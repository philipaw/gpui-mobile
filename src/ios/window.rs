//! iOS Window implementation using UIWindow and UIViewController.
//!
//! iOS windows are fundamentally different from desktop windows:
//! - Always fullscreen (or split-screen on iPad)
//! - No title bar or window chrome
//! - Touch-based input
//! - Safe area insets for notch/home indicator
//!
//! The window is backed by a UIWindow containing a UIViewController
//! whose view hosts a CAMetalLayer. Rendering is performed by
//! `gpui_wgpu::WgpuRenderer` which drives wgpu over the Metal backend.

use super::a11y::{A11yState, IosActionHandler, SendSubclassingAdapter, WindowActivationHandler};
use super::events::*;
use super::IosDisplay;
use crate::momentum::{MomentumScroller, VelocityTracker};
use gpui::{
    point, px, size, AnyWindowHandle, AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTile,
    Bounds, Capslock, DevicePixels, DispatchEventResult, GpuSpecs, Modifiers, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point,
    PromptButton, PromptLevel, RequestFrameOptions, Scene, Size, TileId, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowParams,
};
use gpui_wgpu::{GpuContext, WgpuContext, WgpuRenderer, WgpuSurfaceConfig};
use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{class, msg_send, sel};

use super::cg_types::{ObjcCGPoint, ObjcCGRect};
use parking_lot::Mutex;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, UiKitDisplayHandle, UiKitWindowHandle};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::c_void,
    ptr::{self, NonNull},
    rc::Rc,
    sync::Arc,
};

const GPUI_WINDOW_IVAR: &str = "gpui_window_ptr";

/// Lightweight window handle for wgpu surface creation.
/// Stores the raw UIView pointer needed by wgpu to create a Metal surface.
/// Implements the traits required by `WgpuRenderer::new`.
#[derive(Debug, Clone, Copy)]
struct RawIosWindow {
    view: *mut c_void,
}

unsafe impl Send for RawIosWindow {}
unsafe impl Sync for RawIosWindow {}

impl HasWindowHandle for RawIosWindow {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        let view = NonNull::new(self.view).ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = UiKitWindowHandle::new(view);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl HasDisplayHandle for RawIosWindow {
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        let handle = UiKitDisplayHandle::new();
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(handle.into()) })
    }
}

static METAL_VIEW_CLASS_REGISTERED: std::sync::Once = std::sync::Once::new();
static VC_CLASS_REGISTERED: std::sync::Once = std::sync::Once::new();
static TEXT_INPUT_VIEW_CLASS_REGISTERED: std::sync::Once = std::sync::Once::new();

/// Global storage for the current status bar style.
/// 0 = default (dark content), 1 = light content.
/// Accessed from the main thread only.
static STATUS_BAR_STYLE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Register a custom UIViewController subclass that allows overriding
/// `preferredStatusBarStyle` at runtime.
fn register_view_controller_class() -> &'static AnyClass {
    VC_CLASS_REGISTERED.call_once(|| {
        let superclass = class!(UIViewController);
        let mut decl = ClassBuilder::new(c"GPUIViewController", superclass).unwrap();

        // Override preferredStatusBarStyle
        extern "C" fn preferred_status_bar_style(_this: *mut AnyObject, _sel: Sel) -> isize {
            let style = STATUS_BAR_STYLE.load(std::sync::atomic::Ordering::Relaxed);
            if style == 1 {
                1 // UIStatusBarStyleLightContent
            } else {
                3 // UIStatusBarStyleDarkContent (iOS 13+)
            }
        }

        // Override viewDidLayoutSubviews — called by UIKit on rotation,
        // split-screen changes, and any other layout pass.
        //
        // catch_unwind around each handle_layout_change invocation
        // absorbs Rust panics that would otherwise unwind across the
        // extern "C" boundary as `panic_cannot_unwind` and abort the
        // process. Two routine panic sources have been observed:
        //
        // 1. `IOS_WINDOW_LIST` holds raw pointers to IosWindow boxes;
        //    if gpui's `Application::run` drops an IosWindow (which
        //    it does on iOS as soon as `run` returns), the pointer
        //    dangles and dereferencing it reads freed memory.
        //    handle_layout_change then sends `bounds` to a `0xbead...`
        //    sentinel and segfaults — actually a SIGSEGV not a panic,
        //    so catch_unwind doesn't help there. (See sub-commit 6
        //    prereq #2 in accesskit-ios/TODO.md for the real fix.)
        // 2. handle_layout_change's renderer / resize-callback path
        //    can panic mid-frame on RefCell contention or wgpu state
        //    issues. catch_unwind absorbs these.
        extern "C" fn view_did_layout_subviews(this: *mut AnyObject, _sel: Sel) {
            unsafe {
                let superclass = class!(UIViewController);
                let _: () = msg_send![super(this, superclass), viewDidLayoutSubviews];
            }

            if let Some(wrapper) = super::ffi::IOS_WINDOW_LIST.get() {
                unsafe {
                    let windows = &*wrapper.0.get();
                    for &window_ptr in windows.iter() {
                        if !window_ptr.is_null() {
                            let window = &*window_ptr;
                            let _ = std::panic::catch_unwind(
                                std::panic::AssertUnwindSafe(|| {
                                    window.handle_layout_change();
                                }),
                            );
                        }
                    }
                }
            }
        }

        unsafe {
            decl.add_method(
                sel!(preferredStatusBarStyle),
                preferred_status_bar_style as extern "C" fn(*mut AnyObject, Sel) -> isize,
            );
            decl.add_method(
                sel!(viewDidLayoutSubviews),
                view_did_layout_subviews as extern "C" fn(*mut AnyObject, Sel),
            );
        }

        decl.register();
    });

    class!(GPUIViewController)
}

/// Set the iOS status bar content style (light or dark text/icons).
///
/// This updates the stored style and asks the root view controller
/// to re-query `preferredStatusBarStyle`.
pub fn set_status_bar_style(style: crate::StatusBarContentStyle) {
    use crate::StatusBarContentStyle;

    let value = match style {
        StatusBarContentStyle::Light => 1,
        StatusBarContentStyle::Dark => 0,
    };
    STATUS_BAR_STYLE.store(value, std::sync::atomic::Ordering::Relaxed);

    // Ask UIKit to re-query the status bar style
    unsafe {
        if let Some(wrapper) = super::ffi::IOS_WINDOW_LIST.get() {
            let windows = &*wrapper.0.get();
            if let Some(&window_ptr) = windows.last() {
                if !window_ptr.is_null() {
                    let window = &*window_ptr;
                    let vc = window.view_controller;
                    if !vc.is_null() {
                        let _: () = msg_send![vc, setNeedsStatusBarAppearanceUpdate];
                    }
                }
            }
        }
    }
}

/// Register a custom UIView subclass that uses CAMetalLayer as its backing layer.
/// This is required for Metal rendering on iOS.
fn register_metal_view_class() -> &'static AnyClass {
    METAL_VIEW_CLASS_REGISTERED.call_once(|| {
        let superclass = class!(UIView);
        let mut decl = ClassBuilder::new(c"GPUIMetalView", superclass).unwrap();

        // Add ivar to store window pointer for touch handling
        decl.add_ivar::<*mut std::ffi::c_void>(c"gpui_window_ptr");

        // Override layerClass to return CAMetalLayer
        extern "C" fn layer_class(_self: *const AnyClass, _sel: Sel) -> *const AnyClass {
            class!(CAMetalLayer) as *const AnyClass
        }

        // Touch handling methods
        extern "C" fn touches_began(
            this: *mut AnyObject,
            _sel: Sel,
            touches: *mut AnyObject,
            event: *mut AnyObject,
        ) {
            handle_touches(this, touches, event);
        }

        extern "C" fn touches_moved(
            this: *mut AnyObject,
            _sel: Sel,
            touches: *mut AnyObject,
            event: *mut AnyObject,
        ) {
            handle_touches(this, touches, event);
        }

        extern "C" fn touches_ended(
            this: *mut AnyObject,
            _sel: Sel,
            touches: *mut AnyObject,
            event: *mut AnyObject,
        ) {
            handle_touches(this, touches, event);
        }

        extern "C" fn touches_cancelled(
            this: *mut AnyObject,
            _sel: Sel,
            touches: *mut AnyObject,
            event: *mut AnyObject,
        ) {
            handle_touches(this, touches, event);
        }

        // UIPinchGestureRecognizer action target. The recognizer fires this
        // selector on its target on every state transition (Began / Changed /
        // Ended / Cancelled); the recognizer itself is passed as the sender.
        extern "C" fn handle_pinch(
            this: *mut AnyObject,
            _sel: Sel,
            recognizer: *mut AnyObject,
        ) {
            handle_pinch_recognizer(this, recognizer);
        }

        // UIRotationGestureRecognizer action target. Same shape as
        // handle_pinch — fires on every state transition with the
        // recognizer as sender. Pushes a delta onto the rotation
        // side-channel queue (see `rotation` module) rather than
        // emitting a PlatformInput (gpui core has no rotation event).
        extern "C" fn handle_rotation(
            this: *mut AnyObject,
            _sel: Sel,
            recognizer: *mut AnyObject,
        ) {
            handle_rotation_recognizer(this, recognizer);
        }

        // UIGestureRecognizerDelegate. Returning YES here lets the
        // pinch and rotation recognizers fire from the *same*
        // two-finger gesture simultaneously (UIKit's default is to let
        // only one of a competing pair win). The view is set as the
        // delegate of both recognizers below. We return YES
        // unconditionally — the only recognizers we attach are pinch +
        // rotation, and they're meant to compose.
        extern "C" fn should_recognize_simultaneously(
            _this: *mut AnyObject,
            _sel: Sel,
            _recognizer: *mut AnyObject,
            _other: *mut AnyObject,
        ) -> Bool {
            Bool::YES
        }

        unsafe {
            // Add class method for layerClass
            decl.add_class_method(
                sel!(layerClass),
                layer_class as extern "C" fn(*const AnyClass, Sel) -> *const AnyClass,
            );

            // Add touch handling instance methods
            decl.add_method(
                sel!(touchesBegan:withEvent:),
                touches_began as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject),
            );
            decl.add_method(
                sel!(touchesMoved:withEvent:),
                touches_moved as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject),
            );
            decl.add_method(
                sel!(touchesEnded:withEvent:),
                touches_ended as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject),
            );
            decl.add_method(
                sel!(touchesCancelled:withEvent:),
                touches_cancelled
                    as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject),
            );
            decl.add_method(
                sel!(handlePinch:),
                handle_pinch as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject),
            );
            decl.add_method(
                sel!(handleRotation:),
                handle_rotation as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject),
            );
            decl.add_method(
                sel!(gestureRecognizer:shouldRecognizeSimultaneouslyWithGestureRecognizer:),
                should_recognize_simultaneously
                    as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject) -> Bool,
            );
        }

        decl.register();
    });

    class!(GPUIMetalView)
}

/// Register a custom UIView subclass that implements UIKeyInput protocol.
///
/// iOS requires the first-responder view to conform to `UIKeyInput` in order
/// for the software keyboard to actually route typed characters back to the
/// app.  Without this, `becomeFirstResponder` silently fails and no keyboard
/// appears.
///
/// The three required methods:
/// - `hasText` → always returns YES (simplifies things; no harm)
/// - `insertText:` → forwards the text to `IosWindow::handle_text_input`
/// - `deleteBackward` → dispatches a backspace via `crate::dispatch_text_input`
fn register_text_input_view_class() -> &'static AnyClass {
    TEXT_INPUT_VIEW_CLASS_REGISTERED.call_once(|| {
        let superclass = class!(UIView);
        let mut decl = ClassBuilder::new(c"GPUITextInputView", superclass).unwrap();

        // Declare protocol conformance so iOS knows this view can receive
        // keyboard text input.
        if let Some(protocol) = objc2::runtime::AnyProtocol::get(c"UIKeyInput") {
            decl.add_protocol(protocol);
        }

        // Store the IosWindow pointer so callbacks can reach the Rust window.
        decl.add_ivar::<*mut std::ffi::c_void>(c"gpui_window_ptr");

        // UITextInputTraits property storage — UIView doesn't provide these,
        // but iOS reads them from the first responder to configure the keyboard.
        decl.add_ivar::<isize>(c"_keyboardType"); // UIKeyboardType
        decl.add_ivar::<isize>(c"_autocorrectionType"); // UITextAutocorrectionType
        decl.add_ivar::<isize>(c"_autocapitalizationType"); // UITextAutocapitalizationType

        // --- UIKeyInput protocol methods ---

        // Bool hasText
        unsafe extern "C" fn has_text(_this: *mut AnyObject, _sel: Sel) -> Bool {
            Bool::YES
        }

        // void insertText:(NSString *)text
        unsafe extern "C" fn insert_text(this: *mut AnyObject, _sel: Sel, text: *mut AnyObject) {
            #[allow(deprecated)]
            let window_ptr: *mut std::ffi::c_void = *(*this).get_ivar(GPUI_WINDOW_IVAR);
            if window_ptr.is_null() || text.is_null() {
                return;
            }
            let window = &*(window_ptr as *const IosWindow);
            window.handle_text_input(text);
        }

        // void deleteBackward
        unsafe extern "C" fn delete_backward(this: *mut AnyObject, _sel: Sel) {
            #[allow(deprecated)]
            let window_ptr: *mut std::ffi::c_void = *(*this).get_ivar(GPUI_WINDOW_IVAR);
            if window_ptr.is_null() {
                return;
            }
            let window = &*(window_ptr as *const IosWindow);
            window.handle_delete_backward();
        }

        // canBecomeFirstResponder must return Bool::YES
        unsafe extern "C" fn can_become_first_responder(_this: *mut AnyObject, _sel: Sel) -> Bool {
            Bool::YES
        }

        // --- UITextInputTraits property accessors ---
        #[allow(deprecated)]
        unsafe extern "C" fn get_keyboard_type(this: *mut AnyObject, _sel: Sel) -> isize {
            *(*this).get_ivar::<isize>("_keyboardType")
        }
        #[allow(deprecated)]
        unsafe extern "C" fn set_keyboard_type(this: *mut AnyObject, _sel: Sel, val: isize) {
            *(*this).get_mut_ivar::<isize>("_keyboardType") = val;
        }
        #[allow(deprecated)]
        unsafe extern "C" fn get_autocorrection_type(this: *mut AnyObject, _sel: Sel) -> isize {
            *(*this).get_ivar::<isize>("_autocorrectionType")
        }
        #[allow(deprecated)]
        unsafe extern "C" fn set_autocorrection_type(this: *mut AnyObject, _sel: Sel, val: isize) {
            *(*this).get_mut_ivar::<isize>("_autocorrectionType") = val;
        }
        #[allow(deprecated)]
        unsafe extern "C" fn get_autocapitalization_type(this: *mut AnyObject, _sel: Sel) -> isize {
            *(*this).get_ivar::<isize>("_autocapitalizationType")
        }
        #[allow(deprecated)]
        unsafe extern "C" fn set_autocapitalization_type(
            this: *mut AnyObject,
            _sel: Sel,
            val: isize,
        ) {
            *(*this).get_mut_ivar::<isize>("_autocapitalizationType") = val;
        }

        unsafe {
            decl.add_method(
                sel!(hasText),
                has_text as unsafe extern "C" fn(*mut AnyObject, Sel) -> Bool,
            );
            decl.add_method(
                sel!(insertText:),
                insert_text as unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject),
            );
            decl.add_method(
                sel!(deleteBackward),
                delete_backward as unsafe extern "C" fn(*mut AnyObject, Sel),
            );
            decl.add_method(
                sel!(canBecomeFirstResponder),
                can_become_first_responder as unsafe extern "C" fn(*mut AnyObject, Sel) -> Bool,
            );

            // UITextInputTraits property methods
            decl.add_method(
                sel!(keyboardType),
                get_keyboard_type as unsafe extern "C" fn(*mut AnyObject, Sel) -> isize,
            );
            decl.add_method(
                sel!(setKeyboardType:),
                set_keyboard_type as unsafe extern "C" fn(*mut AnyObject, Sel, isize),
            );
            decl.add_method(
                sel!(autocorrectionType),
                get_autocorrection_type as unsafe extern "C" fn(*mut AnyObject, Sel) -> isize,
            );
            decl.add_method(
                sel!(setAutocorrectionType:),
                set_autocorrection_type as unsafe extern "C" fn(*mut AnyObject, Sel, isize),
            );
            decl.add_method(
                sel!(autocapitalizationType),
                get_autocapitalization_type as unsafe extern "C" fn(*mut AnyObject, Sel) -> isize,
            );
            decl.add_method(
                sel!(setAutocapitalizationType:),
                set_autocapitalization_type as unsafe extern "C" fn(*mut AnyObject, Sel, isize),
            );
        }

        decl.register();
    });

    class!(GPUITextInputView)
}

/// Handle touch events from the GPUIMetalView
fn handle_touches(view: *mut AnyObject, touches: *mut AnyObject, event: *mut AnyObject) {
    unsafe {
        // Get the window pointer from the view's ivar
        #[allow(deprecated)]
        let window_ptr: *mut std::ffi::c_void = *(*view).get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            log::warn!("GPUI iOS: Touch event but no window pointer set");
            return;
        }

        let window = &*(window_ptr as *const IosWindow);

        // Get all touches from the set
        let all_touches: *mut AnyObject = msg_send![touches, allObjects];
        let count: usize = msg_send![all_touches, count];

        for i in 0..count {
            let touch: *mut AnyObject = msg_send![all_touches, objectAtIndex: i];
            window.handle_touch(touch, event);
        }
    }
}

/// Route UIPinchGestureRecognizer callbacks to `IosWindow::handle_pinch`.
fn handle_pinch_recognizer(view: *mut AnyObject, recognizer: *mut AnyObject) {
    unsafe {
        #[allow(deprecated)]
        let window_ptr: *mut std::ffi::c_void = *(*view).get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            log::warn!("GPUI iOS: Pinch event but no window pointer set");
            return;
        }
        let window = &*(window_ptr as *const IosWindow);
        window.handle_pinch(recognizer);
    }
}

/// Route UIRotationGestureRecognizer callbacks to
/// `IosWindow::handle_rotation`.
fn handle_rotation_recognizer(view: *mut AnyObject, recognizer: *mut AnyObject) {
    unsafe {
        #[allow(deprecated)]
        let window_ptr: *mut std::ffi::c_void = *(*view).get_ivar(GPUI_WINDOW_IVAR);
        if window_ptr.is_null() {
            log::warn!("GPUI iOS: Rotation event but no window pointer set");
            return;
        }
        let window = &*(window_ptr as *const IosWindow);
        window.handle_rotation(recognizer);
    }
}

/// iOS Window backed by UIWindow + UIViewController.
/// Distance (logical px) the finger must travel before a touch
/// is promoted from a potential tap to a scroll gesture.
const SCROLL_SLOP: f32 = 8.0;

/// Tracks the current touch gesture state machine.
///
/// This distinguishes taps (short, stationary touches) from scroll gestures
/// (finger drags). The same pattern is used on Android.
#[derive(Clone, Copy, Debug)]
enum TouchState {
    /// No active touch.
    Idle,
    /// Finger is down but hasn't moved beyond the slop threshold.
    Pending { start_x: f32, start_y: f32 },
    /// Finger has moved beyond the threshold — we are scrolling.
    Scrolling { prev_x: f32, prev_y: f32 },
}

#[allow(clippy::type_complexity)]
pub(crate) struct IosWindow {
    /// The UIWindow object. Held as `Retained<AnyObject>` so the
    /// window's +1 retain count from `[UIWindow alloc] initWithFrame:`
    /// outlives the surrounding autorelease pool. Without this, the
    /// window deallocates as soon as `IosWindow::new`'s autorelease
    /// pool drains, which cascades down the
    /// setRootViewController → setView → addSubview retain chain
    /// and dangles `view_controller` / `view` / `text_input_view`
    /// (visible as a segfault in `handle_layout_change` once
    /// UIKit's first layout pass fires).
    window: Retained<AnyObject>,
    /// The UIViewController
    view_controller: *mut AnyObject,
    /// The Metal-backed UIView
    view: *mut AnyObject,
    /// The hidden text input view for keyboard input
    text_input_view: *mut AnyObject,
    /// Current bounds in pixels
    bounds: Cell<Bounds<Pixels>>,
    /// Scale factor
    scale_factor: Cell<f32>,
    /// Input handler for text input
    input_handler: RefCell<Option<PlatformInputHandler>>,
    /// Callback for frame requests
    /// Note: pub(super) to allow ffi.rs to access this for the display link callback
    pub(super) request_frame_callback: RefCell<Option<Box<dyn FnMut(RequestFrameOptions)>>>,
    /// Callback for input events
    input_callback: RefCell<Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>>,
    /// Callback for active status changes
    active_status_callback: RefCell<Option<Box<dyn FnMut(bool)>>>,
    /// Callback for hover status changes (not really applicable on iOS)
    hover_status_callback: RefCell<Option<Box<dyn FnMut(bool)>>>,
    /// Callback for resize events
    resize_callback: RefCell<Option<Box<dyn FnMut(Size<Pixels>, f32)>>>,
    /// Callback for move events (not applicable on iOS)
    moved_callback: RefCell<Option<Box<dyn FnMut()>>>,
    /// Callback for should close
    should_close_callback: RefCell<Option<Box<dyn FnMut() -> bool>>>,
    /// Callback for hit test
    hit_test_callback: RefCell<Option<Box<dyn FnMut() -> Option<WindowControlArea>>>>,
    /// Callback for close
    close_callback: RefCell<Option<Box<dyn FnOnce()>>>,
    /// Callback for appearance changes
    appearance_changed_callback: RefCell<Option<Box<dyn FnMut()>>>,
    /// Current mouse position (from touch)
    mouse_position: Cell<Point<Pixels>>,
    /// Current modifiers
    modifiers: Cell<Modifiers>,
    /// Track if a touch is currently pressed
    touch_pressed: Cell<bool>,
    /// Touch gesture state machine — distinguishes taps from scroll drags.
    touch_state: Cell<TouchState>,
    /// Velocity tracker — records recent touch samples during drag gestures
    /// so we can compute the release velocity when the finger lifts.
    velocity_tracker: RefCell<VelocityTracker>,
    /// Momentum scroller — produces decelerating scroll deltas after a fling
    /// gesture, driven by the CADisplayLink frame callback.
    momentum_scroller: RefCell<MomentumScroller>,
    /// The wgpu renderer (Metal backend on iOS).
    /// Wrapped in a `Mutex<Option<…>>` so that `draw()` (called from the
    /// `request_frame` callback) can acquire a mutable reference without
    /// conflicting with the outer `&self` borrow.
    renderer: Mutex<Option<WgpuRenderer>>,
    /// AccessKit ↔ UIAccessibility bridge. Dynamically subclasses the
    /// Metal view at construction time so the iOS UIAccessibilityContainer
    /// selectors route through the AccessKit tree we feed via
    /// `update_if_active`. Mirror of `gpui_macos::MacWindowState::a11y_adapter`.
    a11y_adapter: Arc<Mutex<SendSubclassingAdapter>>,
    /// Per-frame shared state with the activation + action handlers
    /// owned by `a11y_adapter`. Mirror of `gpui_macos::A11yState`.
    /// Cloned by `take_accessibility_handler` for the writer closure
    /// and by `debug_inject_a11y_action` for in-process tests.
    a11y_state: Arc<Mutex<A11yState>>,
}

// Required for raw_window_handle
unsafe impl Send for IosWindow {}
unsafe impl Sync for IosWindow {}

impl Drop for IosWindow {
    fn drop(&mut self) {
        // §10.4 Layer 3 sub-commit 6 prereq #2 workaround: gpui's
        // `Application::run` on iOS consumes `Application` by value
        // and returns immediately (the IosPlatform impl just stashes
        // the launch callback for the FFI to invoke). When `run`
        // returns, the gpui App drops, cascading down through
        // Window → Box<dyn PlatformWindow> → IosWindow → here, which
        // would ordinarily release the UIWindow's `Retained` and run
        // SubclassingAdapter's Drop (reverting the UIView class
        // swap). With the live UIWindow gone, UIKit shows a blank
        // screen and Accessibility Inspector / VoiceOver see no
        // a11y tree.
        //
        // Workaround: bump the refcount on each long-lived field so
        // the inner resources leak instead of dying with us. The
        // UIWindow stays alive (one extra `Retained` clone leaked),
        // the SubclassingAdapter stays alive (one extra Arc clone
        // leaked — its inner Drop, which reverts the class swap,
        // never runs), and the A11yState stays alive (so the
        // activation handler owned by the adapter still has a
        // backing store).
        //
        // Real fix is gpui-side: a `pub fn Application::run_until(
        // &mut self, ...)` companion to `run` that doesn't consume
        // self, so iOS embedders can hold the App past the run
        // callback. See accesskit-ios/TODO.md sub-commit 6 prereq #2
        // for the full sketch and the four-option weighing.
        std::mem::forget(self.window.clone());
        std::mem::forget(self.a11y_adapter.clone());
        std::mem::forget(self.a11y_state.clone());
    }
}

impl IosWindow {
    pub fn new(handle: AnyWindowHandle, _params: WindowParams) -> anyhow::Result<Self> {
        // Create the window on the main screen
        let screen = IosDisplay::main();
        let screen_bounds = screen.bounds();
        let scale_factor = screen.scale();

        unsafe {
            // Create UIWindow
            let screen_obj: *mut AnyObject = msg_send![class!(UIScreen), mainScreen];
            let screen_bounds_cg: ObjcCGRect = msg_send![screen_obj, bounds];
            let window: *mut AnyObject = msg_send![class!(UIWindow), alloc];
            let window: *mut AnyObject = msg_send![window, initWithFrame: screen_bounds_cg];

            // Create our custom UIViewController subclass that supports
            // dynamic `preferredStatusBarStyle` overrides.
            let vc_class = register_view_controller_class();
            let view_controller: *mut AnyObject = msg_send![vc_class, alloc];
            let view_controller: *mut AnyObject = msg_send![view_controller, init];

            // Create our custom Metal view using the registered class
            let metal_view_class = register_metal_view_class();
            let view: *mut AnyObject = msg_send![metal_view_class, alloc];
            let view: *mut AnyObject = msg_send![view, initWithFrame: screen_bounds_cg];

            // Configure the Metal layer — wgpu will use it for rendering but
            // we still need to set contentsScale so the drawable size is correct.
            let layer: *mut AnyObject = msg_send![view, layer];
            let scale: core_graphics::base::CGFloat = msg_send![screen_obj, scale];
            let _: () = msg_send![layer, setContentsScale: scale];

            // Mark the Metal view + layer non-opaque so a below-Metal subview
            // (platform_view: WKWebView, AVPlayerLayer, etc., inserted via
            // `IosPlatformView::insert_into_window`) can composite through
            // the scene paint's alpha hole-punch. Necessary but not
            // sufficient — the GPUI scene paint itself must also leave
            // alpha < 1 in the platform-view bbox region; that's the step
            // 1.5 hole-punch follow-up.
            let _: () = msg_send![view, setOpaque: false];
            let _: () = msg_send![layer, setOpaque: false];

            // Auto-resize the Metal view when the parent view changes size
            // (e.g. rotation). UIViewAutoresizingFlexibleWidth | UIViewAutoresizingFlexibleHeight
            let _: () = msg_send![view, setAutoresizingMask: 18_usize]; // 0x02 | 0x10

            // Enable user interaction on the Metal view for touch handling
            let _: () = msg_send![view, setUserInteractionEnabled: true];
            let _: () = msg_send![view, setMultipleTouchEnabled: true];

            // Attach a UIPinchGestureRecognizer so 2-finger pinches emit
            // PlatformInput::Pinch through the input pipeline. Default
            // `cancelsTouchesInView=YES` is intentional: when the
            // recognizer transitions to Began, UIKit fires touchesCancelled
            // on any in-flight single-touch handlers, so the existing
            // tap/scroll path (handle_touch above) cleanly bows out and
            // downstream gem-side gesture state machines see a clean
            // Cancelled signal.
            let pinch_recognizer: *mut AnyObject =
                msg_send![class!(UIPinchGestureRecognizer), alloc];
            let pinch_recognizer: *mut AnyObject =
                msg_send![pinch_recognizer, initWithTarget: view, action: sel!(handlePinch:)];
            // The view is its own UIGestureRecognizerDelegate so pinch +
            // rotation can fire simultaneously from one two-finger
            // gesture (see `should_recognize_simultaneously` above).
            let _: () = msg_send![pinch_recognizer, setDelegate: view];
            let _: () = msg_send![view, addGestureRecognizer: pinch_recognizer];

            // Attach a UIRotationGestureRecognizer alongside the pinch
            // one so a two-finger twist drives the rotation side-channel
            // (see `rotation` module). Set the same delegate so it
            // recognizes simultaneously with pinch — scale + rotate from
            // a single gesture.
            let rotation_recognizer: *mut AnyObject =
                msg_send![class!(UIRotationGestureRecognizer), alloc];
            let rotation_recognizer: *mut AnyObject =
                msg_send![rotation_recognizer, initWithTarget: view, action: sel!(handleRotation:)];
            let _: () = msg_send![rotation_recognizer, setDelegate: view];
            let _: () = msg_send![view, addGestureRecognizer: rotation_recognizer];

            // Set the view as the view controller's view
            let _: () = msg_send![view_controller, setView: view];

            // Set the root view controller
            let _: () = msg_send![window, setRootViewController: view_controller];

            // Make the window visible
            let _: () = msg_send![window, makeKeyAndVisible];

            // Create a hidden text input view for keyboard handling.
            // Uses our custom GPUITextInputView which implements UIKeyInput
            // so iOS actually routes keyboard text to us.
            let text_input_class = register_text_input_view_class();
            let text_input_view: *mut AnyObject = msg_send![text_input_class, alloc];
            let text_input_frame = ObjcCGRect::new(0.0, 0.0, 1.0, 1.0);
            let text_input_view: *mut AnyObject =
                msg_send![text_input_view, initWithFrame: text_input_frame];
            let _: () = msg_send![text_input_view, setAlpha: 0.01_f64];
            let _: () = msg_send![text_input_view, setUserInteractionEnabled: true];
            let _: () = msg_send![view, addSubview: text_input_view];

            // --- Initialise the wgpu renderer (Metal backend) ---------------
            let pixel_w = (screen_bounds_cg.width * scale) as i32;
            let pixel_h = (screen_bounds_cg.height * scale) as i32;

            // §10.4 Layer 3: spin up the accesskit_ios SubclassingAdapter
            // on the Metal view. The adapter dynamically subclasses the
            // UIView and intercepts the iOS UIAccessibilityContainer
            // selectors (accessibilityElementCount,
            // accessibilityElementAtIndex:, indexOfAccessibilityElement:)
            // itself — we don't manually add those selectors to the Metal
            // view class. The activation and action handlers close over a
            // shared A11yState so we can write the initial TreeUpdate
            // from gpui's drain even though the handlers are owned by the
            // adapter at this point.
            //
            // unsafe fn — the view pointer must outlive the adapter,
            // which holds because both live on IosWindow.
            let a11y_state: Arc<Mutex<A11yState>> = Arc::new(Mutex::new(A11yState::default()));
            let a11y_adapter = accesskit_ios::SubclassingAdapter::new(
                view as *mut c_void,
                WindowActivationHandler {
                    state: a11y_state.clone(),
                },
                IosActionHandler {
                    state: a11y_state.clone(),
                },
            );

            // Take ownership of the UIWindow's +1 retain count from
            // `[UIWindow alloc] initWithFrame:`. From this point on the
            // window outlives `IosWindow::new`'s autorelease pool —
            // see the field's docstring for why this matters.
            let window = Retained::from_raw(window)
                .expect("[UIWindow alloc] initWithFrame: returned null");

            // Explicitly retain `view_controller`, `view`, and
            // `text_input_view` so they don't dangle when the surrounding
            // autorelease pool drains. The setRootViewController →
            // setView → addSubview retain chain rooted at `window` is
            // NOT sufficient on iOS 13+ — orphan UIWindows (not
            // attached to a UIWindowScene) appear to have their VC /
            // view chain torn down independently, leaving raw pointers
            // pointing at freed memory ("byte read Translation fault"
            // on `0xbead...` sentinels). Belt-and-suspenders retain
            // at construction; the IosWindow::Drop impl pairs each
            // with an objc_release on teardown.
            let _: *mut AnyObject = msg_send![view_controller, retain];
            let _: *mut AnyObject = msg_send![view, retain];
            let _: *mut AnyObject = msg_send![text_input_view, retain];

            let _handle = handle; // consumed but not stored
            let ios_window = Self {
                window,
                view_controller,
                view,
                text_input_view,
                bounds: Cell::new(screen_bounds),
                scale_factor: Cell::new(scale_factor),
                input_handler: RefCell::new(None),
                request_frame_callback: RefCell::new(None),
                input_callback: RefCell::new(None),
                active_status_callback: RefCell::new(None),
                hover_status_callback: RefCell::new(None),
                resize_callback: RefCell::new(None),
                moved_callback: RefCell::new(None),
                should_close_callback: RefCell::new(None),
                hit_test_callback: RefCell::new(None),
                close_callback: RefCell::new(None),
                appearance_changed_callback: RefCell::new(None),
                mouse_position: Cell::new(Point::default()),
                modifiers: Cell::new(Modifiers::default()),
                touch_pressed: Cell::new(false),
                touch_state: Cell::new(TouchState::Idle),
                velocity_tracker: RefCell::new(VelocityTracker::new()),
                momentum_scroller: RefCell::new(MomentumScroller::new()),
                renderer: Mutex::new(None),
                a11y_adapter: Arc::new(Mutex::new(SendSubclassingAdapter(a11y_adapter))),
                a11y_state,
            };

            // Create the wgpu renderer using the Metal backend.
            //
            // `gpui_wgpu::WgpuContext::instance()` only enables Vulkan+GL,
            // so we create our own wgpu instance with Metal enabled, build
            // a surface from the UIView's raw window handle, construct the
            // WgpuContext with that instance, and finally create the renderer.
            let config = WgpuSurfaceConfig {
                size: size(DevicePixels(pixel_w), DevicePixels(pixel_h)),
                // Required for below-Metal platform_view composition
                // (§17.8 step-1.5). Selects a non-`Opaque`
                // `CompositeAlphaMode` (iOS sim's surface advertises
                // `[Opaque, PostMultiplied]`; matching gpui_wgpu
                // patch adds `PostMultiplied` to the transparent_alpha_mode
                // picker preferences) so the iOS compositor honors the
                // CAMetalLayer's per-pixel alpha. The main render pass
                // already clears the drawable to
                // `wgpu::Color::TRANSPARENT`, so unpainted regions
                // reveal whatever's below the Metal view — i.e. the
                // platform-view subview inserted in step 1.
                transparent: true,
                preferred_present_mode: None,
            };

            // wgpu 29 requires a non-None `display` on the
            // InstanceDescriptor — `gpui_wgpu::create_surface()` (used
            // internally by `WgpuRenderer::new` to attach a real
            // surface) passes `raw_display_handle: None` precisely to
            // fall back here. Without this, surface creation fails at
            // "No `DisplayHandle` is available to create this surface
            // with" and the renderer is left as None → draw is a
            // no-op → iOS sim shows BLACK even though gpui's element
            // pipeline runs fine. RawIosWindow is a wgt::WgpuHasDisplayHandle
            // (HasDisplayHandle + Debug + Send + Sync + 'static) so
            // boxing it satisfies the trait bound.
            let raw_window_for_instance = RawIosWindow {
                view: ios_window.view as *mut c_void,
            };
            let metal_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::METAL,
                flags: wgpu::InstanceFlags::default(),
                backend_options: wgpu::BackendOptions::default(),
                memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
                display: Some(Box::new(raw_window_for_instance)),
            });

            let raw_window = RawIosWindow {
                view: ios_window.view as *mut c_void,
            };

            // Build a temporary surface for WgpuContext initialisation
            // (adapter selection needs a surface to test compatibility).
            // `raw_display_handle: None` falls back to the display set
            // on `InstanceDescriptor::display` above (wgpu 29 semantics,
            // matches what `gpui_wgpu::create_surface()` does internally).
            let window_handle = raw_window
                .window_handle()
                .expect("iOS window handle unavailable");

            let target = wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: None,
                raw_window_handle: window_handle.as_raw(),
            };

            let surface_result = metal_instance.create_surface_unsafe(target);
            match surface_result {
                Ok(surface) => match WgpuContext::new(metal_instance, &surface, None) {
                    Ok(context) => {
                        // Pre-populate gpu_context so WgpuRenderer::new()
                        // reuses our Metal-backed context (and its instance)
                        // instead of creating a Vulkan+GL one.
                        let gpu_context: GpuContext = Rc::new(RefCell::new(Some(context)));
                        drop(surface); // no longer needed — new() creates its own

                        match WgpuRenderer::new(gpu_context, &raw_window, config, None) {
                            Ok(renderer) => {
                                log::info!("iOS wgpu renderer created (Metal)");
                                *ios_window.renderer.lock() = Some(renderer);
                            }
                            Err(e) => {
                                log::error!("Failed to create iOS wgpu renderer: {e:#}");
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to create iOS WgpuContext: {e:#}");
                    }
                },
                Err(e) => {
                    log::error!("Failed to create iOS wgpu Metal surface: {e:#}");
                }
            }

            Ok(ios_window)
        }
    }

    /// Get the raw pointer to the UIViewController.
    ///
    /// Kept for downstream use even though the step-1 platform_view
    /// path uses `metal_view_ptr().superview` (the UIWindow) directly
    /// — future scaffolds that compose into the VC's content view
    /// (rather than alongside the Metal view) will need this.
    #[allow(dead_code)]
    pub fn view_controller_ptr(&self) -> *mut AnyObject {
        self.view_controller
    }

    /// Get the raw pointer to the GPUIMetalView.
    pub fn metal_view_ptr(&self) -> *mut AnyObject {
        self.view
    }

    /// Register this window with the FFI layer after it's been stored.
    /// This must be called after the window is placed at a stable address
    /// (e.g., in a Box or Arc).
    pub(crate) fn register_with_ffi(&self) {
        super::ffi::register_window(self as *const Self);

        // Set the window pointer on the view so touch events can find us,
        // and on the text input view so keyboard input can find us.
        unsafe {
            let window_ptr = self as *const Self as *mut std::ffi::c_void;
            #[allow(deprecated)]
            {
                *(*self.view).get_mut_ivar::<*mut c_void>(GPUI_WINDOW_IVAR) = window_ptr;
            }
            #[allow(deprecated)]
            {
                *(*self.text_input_view).get_mut_ivar::<*mut c_void>(GPUI_WINDOW_IVAR) = window_ptr;
            }
            log::info!(
                "GPUI iOS: Set window pointer {:p} on view {:p} and text input {:p}",
                window_ptr,
                self.view,
                self.text_input_view
            );
        }

        // Listen for keyboard show/hide so we can expose the keyboard height.
        self.register_keyboard_observers();
    }

    /// Register for keyboard show/hide notifications so we can track the
    /// keyboard height and allow the UI to shift content above the keyboard.
    pub(crate) fn register_keyboard_observers(&self) {
        unsafe {
            let notification_center: *mut AnyObject =
                msg_send![class!(NSNotificationCenter), defaultCenter];

            let show_name = crate::ios::util::nsstring("UIKeyboardWillShowNotification");
            let hide_name = crate::ios::util::nsstring("UIKeyboardWillHideNotification");

            // Block that fires when the keyboard appears — extracts the
            // end-frame height and stores it in the global atomic.
            let show_block = block2::RcBlock::new(move |notification: *mut AnyObject| {
                if notification.is_null() {
                    return;
                }
                let user_info: *mut AnyObject = msg_send![notification, userInfo];
                if user_info.is_null() {
                    return;
                }
                let frame_key = crate::ios::util::nsstring("UIKeyboardFrameEndUserInfoKey");
                let frame_value: *mut AnyObject = msg_send![user_info, objectForKey: frame_key];
                // frame_key is autoreleased by util::nsstring — no manual release needed
                let _ = frame_key;
                if frame_value.is_null() {
                    return;
                }
                let frame: ObjcCGRect = msg_send![frame_value, CGRectValue];
                let height = frame.height as f32;
                log::info!("GPUI iOS: Keyboard will show, height={}", height);
                crate::set_keyboard_height(height);
            });

            let hide_block = block2::RcBlock::new(move |_notification: *mut AnyObject| {
                log::info!("GPUI iOS: Keyboard will hide");
                crate::set_keyboard_height(0.0);
            });

            let _: *mut AnyObject = msg_send![notification_center,
                addObserverForName: show_name,
                object: std::ptr::null::<AnyObject>(),
                queue: std::ptr::null::<AnyObject>(),
                usingBlock: &*show_block
            ];
            let _: *mut AnyObject = msg_send![notification_center,
                addObserverForName: hide_name,
                object: std::ptr::null::<AnyObject>(),
                queue: std::ptr::null::<AnyObject>(),
                usingBlock: &*hide_block
            ];
            // show_name and hide_name are autoreleased by util::nsstring

            // Leak the blocks so they live for the app lifetime.
            std::mem::forget(show_block);
            std::mem::forget(hide_block);
        }
    }

    /// Handle a touch event from UIKit.
    ///
    /// Uses a state machine to distinguish **taps** from **drag gestures**:
    ///
    ///   DOWN  → record start position, enter "pending" (NO MouseDown yet)
    ///   MOVE  → if finger moved > threshold → switch to "scrolling",
    ///           emit `ScrollWheel` deltas (for scrollable containers) AND
    ///           `MouseMove` (for interactive canvas screens like Animations)
    ///   UP    → if still "pending" → emit `MouseDown` + `MouseUp` (tap)
    ///           if "scrolling"   → emit final `ScrollWheel` (Ended) +
    ///           `MouseUp` (so drag-to-throw works)
    ///
    /// MouseDown is **deferred** until finger-up so that starting a scroll
    /// near a button or tab doesn't accidentally trigger navigation.
    /// Interactive screens use `MouseMove` to track the finger during drags
    /// and `MouseUp` to detect the end of a throw/drag gesture.
    pub fn handle_touch(&self, touch: *mut AnyObject, _event: *mut AnyObject) {
        let position = touch_location_in_view(touch, self.view);
        let phase = touch_phase(touch);
        let tap_count = touch_tap_count(touch);
        let modifiers = self.modifiers.get();

        let logical_x: f32 = position.x.into();
        let logical_y: f32 = position.y.into();

        self.mouse_position.set(position);

        // Stylus side-channel: push a raw sample for every UITouch
        // BEFORE the tap-vs-scroll discriminator below. Canvas-style
        // hosts drain this queue and consume per-sample pressure /
        // tilt / Pencil-vs-finger discrimination; finger UIs ignore
        // it and keep using MouseDown/Move/Up below as before.
        super::stylus::push(super::stylus::StylusEvent {
            position_x: logical_x,
            position_y: logical_y,
            phase: phase.into(),
            force: touch_force(touch),
            altitude: touch_altitude(touch),
            azimuth: touch_azimuth(touch, self.view),
            kind: match touch_type_raw(touch) {
                2 => super::stylus::TouchKind::Pencil,
                _ => super::stylus::TouchKind::Finger,
            },
        });

        let mut ts = self.touch_state.get();

        let emit = |input: PlatformInput| {
            if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
                callback(input);
            }
        };

        match phase {
            UITouchPhase::Began => {
                self.touch_pressed.set(true);
                // Cancel any active momentum fling — the user touched the
                // screen again, so inertia scrolling must stop immediately.
                self.momentum_scroller.borrow_mut().cancel();
                self.velocity_tracker.borrow_mut().reset();

                ts = TouchState::Pending {
                    start_x: logical_x,
                    start_y: logical_y,
                };
                // Do NOT emit MouseDown here — wait until we know whether
                // this is a tap or a scroll.  Emitting MouseDown immediately
                // causes accidental navigation when the user starts scrolling
                // near a button/tab.
                //
                // - Tap (finger lifts within slop) → emit MouseDown + MouseUp
                //   together in Ended phase.
                // - Scroll (finger exceeds slop) → emit only MouseMove +
                //   ScrollWheel, no MouseDown.
            }

            UITouchPhase::Moved => {
                // Record every move for velocity estimation.
                self.velocity_tracker
                    .borrow_mut()
                    .record(logical_x, logical_y);

                match ts {
                    TouchState::Pending { start_x, start_y } => {
                        let dx = logical_x - start_x;
                        let dy = logical_y - start_y;
                        let distance = (dx * dx + dy * dy).sqrt();

                        if distance > SCROLL_SLOP {
                            // Promote to scrolling — emit the first scroll
                            // delta from the start position.
                            ts = TouchState::Scrolling {
                                prev_x: logical_x,
                                prev_y: logical_y,
                            };
                            emit(PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                                position,
                                delta: gpui::ScrollDelta::Pixels(gpui::point(
                                    gpui::px(dx),
                                    gpui::px(dy),
                                )),
                                modifiers,
                                touch_phase: gpui::TouchPhase::Started,
                            }));
                        }
                        // Always emit MouseMove so interactive screens can
                        // track finger position (e.g. drag line in Animations,
                        // gradient control in Shaders).
                        emit(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                            position,
                            modifiers,
                            pressed_button: Some(gpui::MouseButton::Left),
                        }));
                    }
                    TouchState::Scrolling { prev_x, prev_y } => {
                        let dx = logical_x - prev_x;
                        let dy = logical_y - prev_y;
                        ts = TouchState::Scrolling {
                            prev_x: logical_x,
                            prev_y: logical_y,
                        };
                        // Scroll event for scrollable containers.
                        emit(PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                            position,
                            delta: gpui::ScrollDelta::Pixels(gpui::point(
                                gpui::px(dx),
                                gpui::px(dy),
                            )),
                            modifiers,
                            touch_phase: gpui::TouchPhase::Moved,
                        }));
                        // MouseMove for interactive screens.
                        emit(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                            position,
                            modifiers,
                            pressed_button: Some(gpui::MouseButton::Left),
                        }));
                    }
                    TouchState::Idle => {
                        // Spurious move without a preceding down — ignore.
                    }
                }
            }

            UITouchPhase::Ended | UITouchPhase::Cancelled => {
                self.touch_pressed.set(false);
                match ts {
                    TouchState::Pending { start_x, start_y } => {
                        // Finger lifted without exceeding slop → tap.
                        // Emit MouseDown + MouseUp together at the original
                        // down position so hit-testing matches the initial
                        // touch point.
                        self.velocity_tracker.borrow_mut().reset();
                        let tap_pos = gpui::point(gpui::px(start_x), gpui::px(start_y));
                        emit(PlatformInput::MouseDown(gpui::MouseDownEvent {
                            button: gpui::MouseButton::Left,
                            position: tap_pos,
                            modifiers,
                            click_count: tap_count as usize,
                            first_mouse: false,
                        }));
                        emit(PlatformInput::MouseUp(gpui::MouseUpEvent {
                            button: gpui::MouseButton::Left,
                            position: tap_pos,
                            modifiers,
                            click_count: tap_count as usize,
                        }));
                    }
                    TouchState::Scrolling { prev_x, prev_y } => {
                        // End the active touch-scroll gesture.
                        let dx = logical_x - prev_x;
                        let dy = logical_y - prev_y;
                        emit(PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                            position,
                            delta: gpui::ScrollDelta::Pixels(gpui::point(
                                gpui::px(dx),
                                gpui::px(dy),
                            )),
                            modifiers,
                            touch_phase: gpui::TouchPhase::Ended,
                        }));
                        // Also emit MouseUp so interactive screens can
                        // detect the end of a drag (e.g. fling a ball).
                        emit(PlatformInput::MouseUp(gpui::MouseUpEvent {
                            button: gpui::MouseButton::Left,
                            position,
                            modifiers,
                            click_count: 1,
                        }));

                        // ── Start momentum / inertia scrolling ───────────
                        // Compute release velocity from recent touch samples
                        // and kick off the momentum scroller.  Subsequent
                        // frames will pump synthetic ScrollWheel events via
                        // `pump_momentum()` until velocity decays below the
                        // threshold.
                        let (vx, vy) = self.velocity_tracker.borrow().velocity();
                        self.velocity_tracker.borrow_mut().reset();
                        self.momentum_scroller
                            .borrow_mut()
                            .fling(vx, vy, logical_x, logical_y);
                    }
                    TouchState::Idle => {}
                }
                ts = TouchState::Idle;
            }

            UITouchPhase::Stationary => {
                // No change — ignore.
                return;
            }
        }

        self.touch_state.set(ts);
    }

    /// Handle a UIPinchGestureRecognizer callback. Emits one
    /// `PlatformInput::Pinch` per state transition we care about. We use
    /// the standard Apple pattern of reading `recognizer.scale` (cumulative
    /// since the last reset) and resetting it back to 1.0 — so each emitted
    /// `delta` is the per-event change, matching the gpui::PinchEvent
    /// contract that macOS NSEvent.magnification already satisfies.
    pub fn handle_pinch(&self, recognizer: *mut AnyObject) {
        // UIGestureRecognizerState raw values:
        //   0 = Possible, 1 = Began, 2 = Changed,
        //   3 = Ended, 4 = Cancelled, 5 = Failed
        let state: i64 = unsafe { msg_send![recognizer, state] };
        let scale: f64 = unsafe { msg_send![recognizer, scale] };
        let location: ObjcCGPoint =
            unsafe { msg_send![recognizer, locationInView: self.view] };

        let (phase, delta) = match state {
            1 => (gpui::TouchPhase::Started, (scale - 1.0) as f32),
            2 => (gpui::TouchPhase::Moved, (scale - 1.0) as f32),
            3 | 4 => (gpui::TouchPhase::Ended, (scale - 1.0) as f32),
            _ => return,
        };

        // Normalize the recognizer so the next callback's `scale` is again
        // measured from 1.0 — this gives us per-event deltas naturally.
        let _: () = unsafe { msg_send![recognizer, setScale: 1.0_f64] };

        let position = gpui::point(
            gpui::px(location.x as f32),
            gpui::px(location.y as f32),
        );

        if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
            callback(PlatformInput::Pinch(gpui::PinchEvent {
                position,
                delta,
                modifiers: self.modifiers.get(),
                phase,
            }));
        }
    }

    /// Handle a UIRotationGestureRecognizer callback. Pushes one
    /// `RotationEvent` per state transition onto the `rotation`
    /// side-channel queue. Mirrors `handle_pinch`'s per-event-delta
    /// contract: read `recognizer.rotation` (cumulative radians since
    /// the last reset) and reset it back to 0.0, so each pushed `delta`
    /// is the incremental twist. gpui core has no rotation input event,
    /// so this never emits a `PlatformInput` — host apps drain the
    /// queue from their render loop.
    pub fn handle_rotation(&self, recognizer: *mut AnyObject) {
        // UIGestureRecognizerState raw values:
        //   0 = Possible, 1 = Began, 2 = Changed,
        //   3 = Ended, 4 = Cancelled, 5 = Failed
        let state: i64 = unsafe { msg_send![recognizer, state] };
        let rotation: f64 = unsafe { msg_send![recognizer, rotation] };
        let location: ObjcCGPoint =
            unsafe { msg_send![recognizer, locationInView: self.view] };

        let (phase, delta) = match state {
            1 => (gpui::TouchPhase::Started, rotation as f32),
            2 => (gpui::TouchPhase::Moved, rotation as f32),
            3 | 4 => (gpui::TouchPhase::Ended, rotation as f32),
            _ => return,
        };

        // Normalize so the next callback's `rotation` is measured from
        // 0.0 again — this gives per-event deltas naturally, mirroring
        // handle_pinch's `setScale: 1.0`.
        let _: () = unsafe { msg_send![recognizer, setRotation: 0.0_f64] };

        super::rotation::push(super::rotation::RotationEvent {
            position_x: location.x as f32,
            position_y: location.y as f32,
            phase,
            delta,
        });
    }

    /// Query the safe area insets from the UIView.
    ///
    /// Returns `(top, bottom, left, right)` in logical points.
    /// These represent the areas occupied by system UI (status bar,
    /// home indicator, camera notch) that content should avoid.
    pub fn safe_area_insets(&self) -> (f32, f32, f32, f32) {
        if self.view.is_null() {
            return (0.0, 0.0, 0.0, 0.0);
        }
        unsafe {
            // UIEdgeInsets { top, left, bottom, right } — all CGFloat
            #[repr(C)]
            #[derive(Debug, Clone, Copy)]
            struct UIEdgeInsets {
                top: f64,
                left: f64,
                bottom: f64,
                right: f64,
            }

            unsafe impl Encode for UIEdgeInsets {
                const ENCODING: Encoding = Encoding::Struct(
                    "UIEdgeInsets",
                    &[
                        Encoding::Double,
                        Encoding::Double,
                        Encoding::Double,
                        Encoding::Double,
                    ],
                );
            }

            unsafe impl RefEncode for UIEdgeInsets {
                const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
            }

            let insets: UIEdgeInsets = msg_send![self.view, safeAreaInsets];
            (
                insets.top as f32,
                insets.bottom as f32,
                insets.left as f32,
                insets.right as f32,
            )
        }
    }

    /// Advance the momentum scroller by one frame and emit a synthetic
    /// `ScrollWheel` event if the fling is still active.
    ///
    /// Called from `gpui_ios_request_frame` on every CADisplayLink tick,
    /// **before** the GPUI render callback runs, so that the scroll delta
    /// is picked up during the current frame's layout/paint cycle.
    pub(crate) fn pump_momentum(&self) {
        let mut scroller = self.momentum_scroller.borrow_mut();
        if !scroller.is_active() {
            return;
        }

        if let Some(delta) = scroller.step() {
            let modifiers = self.modifiers.get();
            let position = gpui::point(gpui::px(delta.position_x), gpui::px(delta.position_y));
            let fling_ended = !scroller.is_active();

            if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
                callback(PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                    position,
                    delta: gpui::ScrollDelta::Pixels(gpui::point(
                        gpui::px(delta.dx),
                        gpui::px(delta.dy),
                    )),
                    modifiers,
                    touch_phase: gpui::TouchPhase::Moved,
                }));

                // If this was the last momentum frame, send Ended now.
                if fling_ended {
                    callback(PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                        position,
                        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(0.0))),
                        modifiers,
                        touch_phase: gpui::TouchPhase::Ended,
                    }));
                }
            }
        } else {
            // Fling finished — emit one final Ended event so GPUI knows
            // the scroll gesture is truly complete.
            let position = gpui::point(
                gpui::px(scroller.position_x()),
                gpui::px(scroller.position_y()),
            );
            let modifiers = self.modifiers.get();
            if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
                callback(PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                    position,
                    delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(0.0))),
                    modifiers,
                    touch_phase: gpui::TouchPhase::Ended,
                }));
            }
        }
    }

    /// Show the software keyboard with the specified keyboard type.
    ///
    /// The actual `becomeFirstResponder` call is deferred to the next run-loop
    /// iteration via `performSelector:withObject:afterDelay:` to avoid re-entering
    /// GPUI's event dispatch while an entity lease is active (UIKit's keyboard
    /// presentation can synchronously trigger layout callbacks).
    pub fn show_keyboard_with_type(&self, keyboard_type: crate::KeyboardType) {
        log::info!("GPUI iOS: Showing keyboard (type={:?})", keyboard_type);
        unsafe {
            use crate::KeyboardType;
            let kb_type: isize = match keyboard_type {
                KeyboardType::Default => 0,      // UIKeyboardTypeDefault
                KeyboardType::EmailAddress => 7, // UIKeyboardTypeEmailAddress
                KeyboardType::Phone => 5,        // UIKeyboardTypePhonePad
                KeyboardType::NumberPad => 4,    // UIKeyboardTypeNumberPad
                KeyboardType::URL => 3,          // UIKeyboardTypeURL
                KeyboardType::Decimal => 8,      // UIKeyboardTypeDecimalPad
            };
            log::info!(
                "GPUI iOS: text_input_view={:p}, setKeyboardType: {}",
                self.text_input_view,
                kb_type
            );
            if self.text_input_view.is_null() {
                log::error!("GPUI iOS: text_input_view is NULL!");
                return;
            }
            let _: () = msg_send![self.text_input_view, setKeyboardType: kb_type];
            log::info!("GPUI iOS: setAutocorrectionType");
            let _: () = msg_send![self.text_input_view, setAutocorrectionType: 1_isize];
            log::info!("GPUI iOS: setAutocapitalizationType");
            let _: () = msg_send![self.text_input_view, setAutocapitalizationType: 0_isize];
            log::info!("GPUI iOS: scheduling becomeFirstResponder");

            // Defer becomeFirstResponder to the next run-loop iteration.
            let _: () = msg_send![self.text_input_view,
                performSelector: sel!(becomeFirstResponder),
                withObject: ptr::null::<AnyObject>(),
                afterDelay: 0.0_f64
            ];
            log::info!("GPUI iOS: show_keyboard_with_type done");
        }
    }

    /// Hide the software keyboard.
    ///
    /// Deferred to the next run-loop iteration (like `show_keyboard_with_type`)
    /// to avoid re-entering GPUI event dispatch.
    pub fn hide_keyboard(&self) {
        log::info!("GPUI iOS: Hiding keyboard");
        unsafe {
            let _: () = msg_send![self.text_input_view,
                performSelector: sel!(resignFirstResponder),
                withObject: ptr::null::<AnyObject>(),
                afterDelay: 0.0_f64
            ];
        }
    }

    /// Handle text input from the software keyboard
    pub fn handle_text_input(&self, text: *mut AnyObject) {
        if text.is_null() {
            return;
        }

        unsafe {
            // Convert NSString to Rust String
            let utf8: *const i8 = msg_send![text, UTF8String];
            if utf8.is_null() {
                return;
            }

            let text_str = std::ffi::CStr::from_ptr(utf8)
                .to_string_lossy()
                .into_owned();

            log::info!("GPUI iOS: Text input: {:?}", text_str);

            // Try the global text input callback (for our TextInput components).
            // The text is captured in PENDING_TEXT regardless of whether we also
            // send key events below.
            let dispatched = crate::dispatch_text_input(&text_str);

            // Try the input handler (for GPUI's built-in text fields)
            if !dispatched {
                if let Some(handler) = self.input_handler.borrow_mut().as_mut() {
                    handler.replace_text_in_range(None, &text_str);
                    return;
                }
            }

            // Send key events through GPUI's input callback.
            // Even if dispatch_text_input captured the text, we still send key
            // events so GPUI triggers a re-render cycle (which runs
            // drain_pending_text and updates the UI).
            for c in text_str.chars() {
                let keystroke = gpui::Keystroke {
                    modifiers: Modifiers::default(),
                    key: c.to_string(),
                    key_char: Some(c.to_string()),
                };

                let event = PlatformInput::KeyDown(gpui::KeyDownEvent {
                    keystroke,
                    is_held: false,
                    prefer_character_input: true,
                });

                if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
                    callback(event);
                }
            }
        }
    }

    /// Handle the delete-backward action from the software keyboard.
    ///
    /// This is called by the `GPUITextInputView` when the user taps the
    /// backspace key.  We dispatch a special sentinel ("\x08") through the
    /// global text input callback so the active TextInput component can
    /// remove the last character.
    pub fn handle_delete_backward(&self) {
        log::info!("GPUI iOS: deleteBackward");

        // Try the global callback first (backspace = "\x08")
        crate::dispatch_text_input("\x08");

        // Always send a Backspace KeyDown event through GPUI to trigger
        // a re-render cycle (which runs drain_pending_text).
        let keystroke = gpui::Keystroke {
            modifiers: Modifiers::default(),
            key: "backspace".to_string(),
            key_char: None,
        };
        let event = PlatformInput::KeyDown(gpui::KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        });
        if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
            callback(event);
        }
    }

    /// Handle a key event from an external keyboard
    pub fn handle_key_event(&self, key_code: u32, modifier_flags: u32, is_key_down: bool) {
        use super::text_input::{
            key_code_to_key_down, key_code_to_key_up, key_code_to_string,
            modifier_flags_to_modifiers,
        };

        let key = key_code_to_string(key_code);
        let modifiers = modifier_flags_to_modifiers(modifier_flags);

        log::info!(
            "GPUI iOS: Key event - key: {:?}, modifiers: {:?}, down: {}",
            key,
            modifiers,
            is_key_down
        );

        // On key-down, dispatch cursor-movement control codes through the
        // global text input callback so TextField-based components receive them.
        if is_key_down {
            match key_code {
                0x50 => {
                    crate::dispatch_text_input("\x1b[D");
                } // Left arrow
                0x4F => {
                    crate::dispatch_text_input("\x1b[C");
                } // Right arrow
                0x4A => {
                    crate::dispatch_text_input("\x1b[H");
                } // Home
                0x4D => {
                    crate::dispatch_text_input("\x1b[F");
                } // End
                _ => {}
            }
        }

        let event = if is_key_down {
            key_code_to_key_down(key_code, modifier_flags)
        } else {
            key_code_to_key_up(key_code, modifier_flags)
        };

        if let Some(callback) = self.input_callback.borrow_mut().as_mut() {
            callback(event);
        }
    }

    /// Notify the window of active status changes (foreground/background).
    ///
    /// This is called by the FFI layer when the app transitions between
    /// foreground and background states.
    pub fn notify_active_status_change(&self, is_active: bool) {
        log::info!("GPUI iOS: Window active status changed to: {}", is_active);

        if let Some(callback) = self.active_status_callback.borrow_mut().as_mut() {
            callback(is_active);
        }
    }

    /// Handle a layout change (e.g. rotation, split-screen resize).
    ///
    /// Called from `viewDidLayoutSubviews` on the GPUIViewController.
    /// Queries the current UIView bounds, updates the stored bounds/scale,
    /// reconfigures the Metal layer + wgpu surface, and fires the resize callback.
    pub fn handle_layout_change(&self) {
        unsafe {
            let view_bounds: ObjcCGRect = msg_send![self.view, bounds];
            let screen: *mut AnyObject = msg_send![class!(UIScreen), mainScreen];
            let scale: core_graphics::base::CGFloat = msg_send![screen, scale];

            let new_w = view_bounds.width as f32;
            let new_h = view_bounds.height as f32;
            let new_scale = scale as f32;

            let old_bounds = self.bounds.get();
            let old_scale = self.scale_factor.get();

            let new_size = size(px(new_w), px(new_h));

            // Only process if something actually changed.
            if old_bounds.size == new_size && (old_scale - new_scale).abs() < 0.01 {
                return;
            }

            log::info!(
                "GPUI iOS: Layout changed — {:?} @{:.1}x → {:?} @{:.1}x",
                old_bounds.size,
                old_scale,
                new_size,
                new_scale,
            );

            // Update stored bounds (in logical pixels, matching GPUI convention).
            let new_bounds = Bounds {
                origin: Default::default(),
                size: new_size,
            };
            self.bounds.set(new_bounds);
            self.scale_factor.set(new_scale);

            // Update the Metal layer's contentsScale so the drawable has the
            // correct pixel dimensions.
            let layer: *mut AnyObject = msg_send![self.view, layer];
            let _: () = msg_send![layer, setContentsScale: scale];

            // Update the wgpu renderer's surface configuration.
            let pixel_w = (new_w * new_scale) as i32;
            let pixel_h = (new_h * new_scale) as i32;
            {
                let mut guard = self.renderer.lock();
                if let Some(renderer) = guard.as_mut() {
                    renderer
                        .update_drawable_size(size(DevicePixels(pixel_w), DevicePixels(pixel_h)));
                }
            }

            // Fire the resize callback so GPUI re-layouts at the new size.
            let cb = self.resize_callback.borrow_mut().take();
            if let Some(mut cb) = cb {
                cb(new_size, new_scale);
                // Restore the callback for future resize events.
                let mut slot = self.resize_callback.borrow_mut();
                if slot.is_none() {
                    *slot = Some(cb);
                }
            }
        }
    }

    /// Test-injection hook for the a11y action pipeline. Mirror of
    /// `gpui_macos::MacWindow::debug_inject_a11y_action`. Bypasses
    /// UIKit and synthesizes an `accesskit::ActionRequest` directly
    /// into a fresh `IosActionHandler`. The handler resolves the
    /// `target_node` against the current `focus_inverse_map` and
    /// (for `Action::Focus`) enqueues a `PendingA11yAction::Focus(fid)`
    /// onto the same `pending_actions` queue a real OS-triggered
    /// action would land on; the next gpui draw consumes it via
    /// `take_pending_a11y_actions`.
    ///
    /// Layer 2 lesson carried forward from `gpui_macos`: this hook is
    /// painful to retrofit, so add it from day one. Used by tests +
    /// in-process validation harnesses.
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn debug_inject_a11y_action(&self, request: accesskit::ActionRequest) {
        use accesskit::ActionHandler as _;
        let mut handler = IosActionHandler {
            state: self.a11y_state.clone(),
        };
        handler.do_action(request);
    }
}

impl HasWindowHandle for IosWindow {
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        let view = NonNull::new(self.view as *mut c_void)
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = UiKitWindowHandle::new(view);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl HasDisplayHandle for IosWindow {
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        let handle = UiKitDisplayHandle::new();
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(handle.into()) })
    }
}

impl PlatformWindow for IosWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    fn is_maximized(&self) -> bool {
        true // iOS windows are always "maximized"
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds.get())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds.get().size
    }

    fn resize(&mut self, _size: Size<Pixels>) {
        // iOS windows cannot be resized programmatically
    }

    fn scale_factor(&self) -> f32 {
        self.scale_factor.get()
    }

    fn appearance(&self) -> WindowAppearance {
        unsafe {
            let trait_collection: *mut AnyObject = msg_send![self.view, traitCollection];
            let style: i64 = msg_send![trait_collection, userInterfaceStyle];
            match style {
                2 => WindowAppearance::Dark,
                _ => WindowAppearance::Light,
            }
        }
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay::main()))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.mouse_position.get()
    }

    fn modifiers(&self) -> Modifiers {
        self.modifiers.get()
    }

    fn capslock(&self) -> Capslock {
        // Would need to check UIKeyModifierFlags
        Capslock { on: false }
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.input_handler.borrow_mut() = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.input_handler.borrow_mut().take()
    }

    fn take_accessibility_handler(
        &mut self,
    ) -> Option<Box<dyn FnMut(gpui::accessibility::AccessibilityDrain) + Send + 'static>> {
        // §10.4 Layer 3: closure feeds the SubclassingAdapter and stashes
        // per-frame state for the activation + action handlers (which
        // are owned by the adapter and can't be mutated externally).
        //
        // - `absorb_drain` folds EVERY (diff) update into the running
        //   full-tree snapshot behind `initial_update`, so an AT that
        //   activates after the element tree has mutated still gets a
        //   `Tree::new`-consistent snapshot. The previous first-drain-
        //   only cache panicked `validate_global` on late activation
        //   (gem §17.8 #120).
        // - `focus_inverse_map` is replaced every frame so the action
        //   handler always sees the latest mapping.
        let state = self.a11y_state.clone();
        let adapter = self.a11y_adapter.clone();
        let snapshot_log = std::env::var("DEBUG_A11Y_SNAPSHOT")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false);
        let mut last_logged_len = usize::MAX;
        Some(Box::new(move |drain: gpui::accessibility::AccessibilityDrain| {
            let gpui::accessibility::AccessibilityDrain {
                tree_update,
                focus_inverse_map,
            } = drain;
            {
                let mut a11y = state.lock();
                a11y.absorb_drain(&tree_update);
                if snapshot_log {
                    // Log only on change: hosts assert pruning (no
                    // ghost nodes from dead screens) from run logs.
                    let len = a11y.snapshot_len();
                    if len != last_logged_len {
                        last_logged_len = len;
                        eprintln!("[a11y-snapshot] reachable_nodes={len}");
                    }
                }
                a11y.focus_inverse_map = focus_inverse_map;
            }
            adapter.lock().update_if_active(|| tree_update);
        }))
    }

    fn take_pending_a11y_actions(&mut self) -> Vec<gpui::accessibility::PendingA11yAction> {
        // Drain whatever the IosActionHandler enqueued since the last
        // gpui draw. The action handler runs on the UIKit main thread
        // but without `&mut Window/&mut App`; this hook hands the
        // pending list back to gpui core where those are available.
        std::mem::take(&mut self.a11y_state.lock().pending_actions)
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        // Would use UIAlertController
        let (_tx, rx) = futures::channel::oneshot::channel();

        unsafe {
            // Create UIAlertController
            let title = msg;
            let message = detail.unwrap_or("");

            let alert_style: i64 = 1; // UIAlertControllerStyleAlert

            let title_str: *mut AnyObject =
                msg_send![class!(NSString), stringWithUTF8String: title.as_ptr()];
            let message_str: *mut AnyObject =
                msg_send![class!(NSString), stringWithUTF8String: message.as_ptr()];

            let alert: *mut AnyObject = msg_send![
                class!(UIAlertController),
                alertControllerWithTitle: title_str,
                message: message_str,
                preferredStyle: alert_style
            ];

            // Add buttons
            for button in answers.iter() {
                let button_title: *mut AnyObject = msg_send![
                    class!(NSString),
                    stringWithUTF8String: button.label().as_str().as_ptr()
                ];

                let action_style: i64 = if button.is_cancel() { 1 } else { 0 }; // UIAlertActionStyleCancel or Default

                // Note: In production, this would need a block that calls tx.send(index)
                let action: *mut AnyObject = msg_send![
                    class!(UIAlertAction),
                    actionWithTitle: button_title,
                    style: action_style,
                    handler: ptr::null::<AnyObject>()
                ];

                let _: () = msg_send![alert, addAction: action];
            }

            // Present the alert
            let _: () = msg_send![
                self.view_controller,
                presentViewController: alert,
                animated: true,
                completion: ptr::null::<AnyObject>()
            ];
        }

        Some(rx)
    }

    fn activate(&self) {
        unsafe {
            let _: () = msg_send![&*self.window, makeKeyAndVisible];
        }
    }

    fn is_active(&self) -> bool {
        unsafe {
            let app: *mut AnyObject = msg_send![class!(UIApplication), sharedApplication];
            let key_window: *mut AnyObject = msg_send![app, keyWindow];
            Retained::as_ptr(&self.window) as *mut AnyObject == key_window
        }
    }

    fn is_hovered(&self) -> bool {
        // Hover isn't really applicable on iOS
        false
    }

    fn set_title(&mut self, _title: &str) {
        // iOS apps don't have window titles
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {
        // Could adjust view background color
    }

    fn minimize(&self) {
        // iOS apps cannot be minimized
    }

    fn zoom(&self) {
        // iOS apps cannot be zoomed
    }

    fn toggle_fullscreen(&self) {
        // iOS apps are always fullscreen
    }

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        *self.request_frame_callback.borrow_mut() = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        *self.input_callback.borrow_mut() = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.active_status_callback.borrow_mut() = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.hover_status_callback.borrow_mut() = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        *self.resize_callback.borrow_mut() = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        *self.moved_callback.borrow_mut() = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        *self.should_close_callback.borrow_mut() = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        *self.hit_test_callback.borrow_mut() = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        *self.close_callback.borrow_mut() = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        *self.appearance_changed_callback.borrow_mut() = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        let mut guard = self.renderer.lock();
        if let Some(renderer) = guard.as_mut() {
            renderer.draw(scene);
        } else {
            log::trace!("GPUI iOS: draw called but no renderer available");
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        let guard = self.renderer.lock();
        if let Some(renderer) = guard.as_ref() {
            renderer.sprite_atlas().clone()
        } else {
            // Fallback: return a dummy atlas so GPUI doesn't panic before
            // the renderer is initialised.
            Arc::new(FallbackAtlas::new())
        }
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        let guard = self.renderer.lock();
        guard
            .as_ref()
            .map(|r| r.supports_dual_source_blending())
            .unwrap_or(false)
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        let guard = self.renderer.lock();
        guard.as_ref().map(|r| r.gpu_specs())
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // iOS handles IME positioning automatically
    }
}

// ── Fallback atlas ────────────────────────────────────────────────────────────

/// A minimal fallback `PlatformAtlas` used until a real Blade/Metal renderer is
/// wired up.  It records tiles in memory but does not upload texture data to the
/// GPU — just enough to satisfy GPUI's atlas queries without panicking.
struct FallbackAtlas {
    state: Mutex<FallbackAtlasState>,
}

struct FallbackAtlasState {
    next_id: u32,
    tiles: HashMap<AtlasKey, AtlasTile>,
}

impl FallbackAtlas {
    fn new() -> Self {
        Self {
            state: Mutex::new(FallbackAtlasState {
                next_id: 1,
                tiles: HashMap::new(),
            }),
        }
    }
}

impl PlatformAtlas for FallbackAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> anyhow::Result<
            Option<(Size<DevicePixels>, std::borrow::Cow<'a, [u8]>)>,
        >,
    ) -> anyhow::Result<Option<AtlasTile>> {
        let mut state = self.state.lock();

        if let Some(tile) = state.tiles.get(key) {
            return Ok(Some(tile.clone()));
        }

        let data = build()?;
        if let Some((size, _pixels)) = data {
            let id = state.next_id;
            state.next_id += 1;

            let tile = AtlasTile {
                texture_id: AtlasTextureId {
                    index: 0,
                    kind: AtlasTextureKind::Monochrome,
                },
                tile_id: TileId(id),
                padding: 0,
                bounds: Bounds {
                    origin: point(DevicePixels(0), DevicePixels(0)),
                    size,
                },
            };

            state.tiles.insert(key.clone(), tile.clone());
            Ok(Some(tile))
        } else {
            Ok(None)
        }
    }

    fn remove(&self, key: &AtlasKey) {
        self.state.lock().tiles.remove(key);
    }
}
