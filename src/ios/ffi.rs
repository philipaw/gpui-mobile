//! FFI (Foreign Function Interface) module for iOS.
//!
//! This module exposes C-compatible functions that can be called from
//! Objective-C code in the iOS app delegate to initialize and control
//! the GPUI application lifecycle.
//!
//! ## Typical call sequence from Obj-C
//!
//! ```text
//! gpui_ios_run_demo()          // sets up platform + invokes finish-launching
//! gpui_ios_get_window()        // retrieve the GPUI window pointer
//! gpui_ios_request_frame(ptr)  // called every CADisplayLink tick
//! ```

use gpui::{App, AppContext, Application, RequestFrameOptions, WindowOptions};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::OnceLock;

/// Absorb Rust panics inside an `extern "C"` FFI body so the process
/// doesn't `abort()` on the unwind across the C boundary
/// (`panic_cannot_unwind`). Logs the location + payload at error level
/// when one fires; the FFI returns normally.
///
/// Wrap the body of every `pub extern "C" fn gpui_ios_*` entry point
/// in this — gpui's render/touch/lifecycle code paths regularly
/// encounter UIKit re-entrancy or RefCell contention that surfaces
/// as a Rust panic mid-callback; without this guard, the simulator
/// produces a macOS crash dialog and the app dies. See sub-commit 6
/// prereq follow-ups in `accesskit-ios/TODO.md`.
fn ffi_panic_guard(name: &'static str, body: impl FnOnce()) {
    let result = catch_unwind(AssertUnwindSafe(body));
    if let Err(payload) = result {
        let msg = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        log::error!("GPUI iOS FFI panic absorbed in {name}: {msg}");
    }
}

/// Global storage for the GPUI application state.
/// This is set during initialization and used by FFI callbacks.
static IOS_APP_STATE: OnceLock<IosAppState> = OnceLock::new();

/// Holds the state needed for iOS FFI callbacks.
/// Note: On iOS, all UI code runs on the main thread, so we use a RefCell
/// instead of Mutex and don't require Send.
struct IosAppState {
    /// The callback to invoke when the app finishes launching.
    /// This is the closure passed to Application::run().
    /// Using std::cell::UnsafeCell since this is only accessed from the main thread.
    finish_launching: std::cell::UnsafeCell<Option<Box<dyn FnOnce()>>>,
}

// Safety: On iOS, all GPUI operations happen on the main thread.
// The FFI functions are only called from the iOS app delegate which runs on main thread.
// We implement both Send and Sync because OnceLock requires Send for its value type,
// and we need Sync for the static. The actual access is always single-threaded.
unsafe impl Send for IosAppState {}
unsafe impl Sync for IosAppState {}

// Safety wrapper for window list - only accessed from main thread
pub(crate) struct WindowListWrapper(
    pub(crate) std::cell::UnsafeCell<Vec<*const super::window::IosWindow>>,
);
unsafe impl Send for WindowListWrapper {}
unsafe impl Sync for WindowListWrapper {}

pub(crate) static IOS_WINDOW_LIST: OnceLock<WindowListWrapper> = OnceLock::new();

/// Initialize the GPUI iOS application.
///
/// This should be called from `application:didFinishLaunchingWithOptions:`
/// in the iOS app delegate, before any other GPUI functions.
///
/// Returns a pointer to the app state that should be passed to other FFI functions.
/// Returns null if initialization fails.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_initialize() -> *mut c_void {
    log::info!("GPUI iOS: Initializing");

    // Initialize the app state
    let state = IosAppState {
        finish_launching: std::cell::UnsafeCell::new(None),
    };

    if IOS_APP_STATE.set(state).is_err() {
        log::error!("GPUI iOS: Already initialized");
        return std::ptr::null_mut();
    }

    // Initialize the window list
    let _ = IOS_WINDOW_LIST.set(WindowListWrapper(std::cell::UnsafeCell::new(Vec::new())));

    // Return a non-null pointer to indicate success
    // The actual state is stored in the static
    std::ptr::dangling_mut::<c_void>()
}

/// Register a window with the FFI layer.
///
/// This is called internally when a new IosWindow is created.
/// The window pointer can then be retrieved by Objective-C code.
///
/// # Safety
/// This must only be called from the main thread.
pub(crate) fn register_window(window: *const super::window::IosWindow) {
    if let Some(wrapper) = IOS_WINDOW_LIST.get() {
        unsafe {
            (*wrapper.0.get()).push(window);
            log::info!("GPUI iOS: Registered window {:p}", window);
        }
    }
}

/// Get the most recently created window pointer.
///
/// Returns the pointer to the IosWindow that was most recently registered,
/// or null if no windows have been created.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_get_window() -> *mut c_void {
    if let Some(wrapper) = IOS_WINDOW_LIST.get() {
        unsafe {
            let windows = &*wrapper.0.get();
            if let Some(&window) = windows.last() {
                log::info!("GPUI iOS: Returning window {:p}", window);
                return window as *mut c_void;
            }
        }
    }
    log::warn!("GPUI iOS: No windows registered");
    std::ptr::null_mut()
}

/// Store the finish launching callback.
///
/// This is called internally by IosPlatform::run() to store the callback
/// that will be invoked when the app finishes launching.
///
/// # Safety
/// This must only be called from the main thread.
pub(crate) fn set_finish_launching_callback(callback: Box<dyn FnOnce()>) {
    if let Some(state) = IOS_APP_STATE.get() {
        // Safety: Only called from main thread
        unsafe {
            *state.finish_launching.get() = Some(callback);
        }
    }
}

/// Called when the iOS app has finished launching.
///
/// This should be called from `application:didFinishLaunchingWithOptions:`
/// in the iOS app delegate, after `gpui_ios_initialize()` returns.
///
/// This invokes the callback passed to Application::run().
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_did_finish_launching(_app_ptr: *mut c_void) {
    ffi_panic_guard("gpui_ios_did_finish_launching", || {
        log::info!("GPUI iOS: Did finish launching");
        if let Some(state) = IOS_APP_STATE.get() {
            // Safety: Only called from main thread
            let callback = unsafe { (*state.finish_launching.get()).take() };
            if let Some(callback) = callback {
                log::info!("GPUI iOS: Invoking finish launching callback");
                callback();
            } else {
                log::warn!("GPUI iOS: No finish launching callback registered");
            }
        } else {
            log::error!("GPUI iOS: Not initialized");
        }
    });
}

/// Called when the iOS app will enter the foreground.
///
/// This should be called from `applicationWillEnterForeground:` in the app delegate.
/// This notifies all GPUI windows that the app is becoming active.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_will_enter_foreground(_app_ptr: *mut c_void) {
    ffi_panic_guard("gpui_ios_will_enter_foreground", || {
        log::info!("GPUI iOS: Will enter foreground");
        notify_all_windows(true);
    });
}

/// Called when the iOS app did become active.
///
/// This should be called from `applicationDidBecomeActive:` in the app delegate.
/// This indicates the app is now in the foreground and receiving events.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_did_become_active(_app_ptr: *mut c_void) {
    ffi_panic_guard("gpui_ios_did_become_active", || {
        log::info!("GPUI iOS: Did become active");
        notify_all_windows(true);
    });
}

/// Called when the iOS app will resign active.
///
/// This should be called from `applicationWillResignActive:` in the app delegate.
/// This indicates the app is about to become inactive (e.g., incoming call, switching apps).
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_will_resign_active(_app_ptr: *mut c_void) {
    ffi_panic_guard("gpui_ios_will_resign_active", || {
        log::info!("GPUI iOS: Will resign active");
        notify_all_windows(false);
    });
}

/// Called when the iOS app did enter the background.
///
/// This should be called from `applicationDidEnterBackground:` in the app delegate.
/// At this point, the app should have already saved any user data and released
/// shared resources. The app will be suspended shortly after this returns.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_did_enter_background(_app_ptr: *mut c_void) {
    ffi_panic_guard("gpui_ios_did_enter_background", || {
        log::info!("GPUI iOS: Did enter background");
        notify_all_windows(false);
    });
}

/// Walk `IOS_WINDOW_LIST` and call `notify_active_status_change` on each
/// registered window. Shared between the four foreground/background
/// lifecycle FFI entries.
fn notify_all_windows(active: bool) {
    if let Some(wrapper) = IOS_WINDOW_LIST.get() {
        unsafe {
            let windows = &*wrapper.0.get();
            for &window_ptr in windows.iter() {
                if !window_ptr.is_null() {
                    let window = &*window_ptr;
                    window.notify_active_status_change(active);
                }
            }
        }
    }
}

/// Called when the iOS app will terminate.
///
/// This should be called from `applicationWillTerminate:` in the app delegate.
/// This is a good place to save any unsaved data.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_will_terminate(_app_ptr: *mut c_void) {
    ffi_panic_guard("gpui_ios_will_terminate", || {
        log::info!("GPUI iOS: Will terminate");
        // Quit callbacks would be invoked here if registered.
    });
}

/// Called when a touch event occurs.
///
/// This bridges UIKit touch events to GPUI's input system.
/// Parameters:
/// - `window_ptr`: Pointer to the IosWindow
/// - `touch_ptr`: Pointer to the UITouch object
/// - `event_ptr`: Pointer to the UIEvent object
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_handle_touch(
    window_ptr: *mut c_void,
    touch_ptr: *mut c_void,
    event_ptr: *mut c_void,
) {
    if window_ptr.is_null() || touch_ptr.is_null() {
        return;
    }

    // Cast to IosWindow and forward the touch event
    let window = unsafe { &*(window_ptr as *const super::window::IosWindow) };
    window.handle_touch(
        touch_ptr as *mut objc2::runtime::AnyObject,
        event_ptr as *mut objc2::runtime::AnyObject,
    );
}

/// Request a frame to be rendered.
///
/// This should be called from CADisplayLink callback to trigger GPUI rendering.
/// The window_ptr should be the value returned by gpui_ios_get_window().
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_request_frame(window_ptr: *mut c_void) {
    ffi_panic_guard("gpui_ios_request_frame", || request_frame_inner(window_ptr));
}

/// Stash the current `CADisplayLink.targetTimestamp` for consumers
/// to read via [`crate::last_display_link_target`]. Called from the
/// platform's `CADisplayLink` callback (main.m's `renderFrame`)
/// before `gpui_ios_request_frame`. M0 spike #4 L5 instrumentation
/// (s54): host-side code compares this against `CACurrentMediaTime()`
/// at touch arrival to compute touch→present-time latency.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_set_display_link_target(target_timestamp: f64) {
    crate::LATEST_DISPLAY_LINK_TARGET_BITS.store(
        target_timestamp.to_bits(),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// gem D4 native tuning panel — set one glass-composite param
/// (idx 0..15, GlassTuning order) from a `UISlider`'s valueChanged;
/// applied to the live renderer next frame.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_set_glass_tuning_param(idx: u32, value: f32) {
    crate::set_glass_tuning_param(idx as usize, value);
}

/// gem D4 native tuning panel — dump the current 15 params (the panel's
/// "log" button) so the dialed set can be baked into the shader consts.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_log_glass_tuning() {
    eprintln!("[glass-tune] {:?}", crate::glass_tuning_snapshot());
}

fn request_frame_inner(window_ptr: *mut c_void) {
    if window_ptr.is_null() {
        return;
    }

    // Safety: window_ptr must be a valid pointer to an IosWindow
    let window = unsafe { &*(window_ptr as *const super::window::IosWindow) };

    // ── Momentum scrolling ───────────────────────────────────────────────
    // Pump the momentum scroller BEFORE the render callback so that any
    // synthetic ScrollWheel events are processed during this frame's
    // layout/paint cycle.  This produces the smooth, decelerating inertia
    // scroll that users expect on iOS after a fling gesture.
    window.pump_momentum();

    // Check if text input arrived since last frame — if so, force a render
    // so drain_pending_text() runs and the UI updates.
    let text_dirty = crate::TEXT_INPUT_DIRTY.swap(false, std::sync::atomic::Ordering::AcqRel);

    // Take the callback, invoke it, then restore it.
    //
    // `try_borrow_mut` (instead of `borrow_mut`) makes both halves of
    // the take/replace dance fault-tolerant against re-entrancy.
    // Observed scenario: gpui's render-frame callback, mid-flight,
    // ends up triggering another `CADisplayLink` dispatch (or any path
    // back into `gpui_ios_request_frame`) before the original call
    // returns — the second call's `borrow_mut` panics with
    // "RefCell already borrowed", which can't unwind across this
    // `extern "C"` boundary and aborts the process. With `try_borrow_mut`,
    // we silently skip the re-entrant frame request; the original
    // frame finishes normally and `CADisplayLink` will fire again on
    // the next vsync.
    let callback = match window.request_frame_callback.try_borrow_mut() {
        Ok(mut guard) => guard.take(),
        Err(_) => return,
    };
    if let Some(mut cb) = callback {
        cb(RequestFrameOptions {
            force_render: text_dirty,
            ..Default::default()
        });
        // Restore the callback for the next frame. `try_borrow_mut`
        // here too — if anything left a borrow open during cb (e.g.
        // `Window::set_request_frame_handler` was called from within
        // cb), we can't re-install; the next frame will just see no
        // callback registered.
        if let Ok(mut guard) = window.request_frame_callback.try_borrow_mut() {
            *guard = Some(cb);
        }
    }
}

/// Show the software keyboard.
///
/// Call this when a text input field gains focus.
/// The window_ptr should be the value returned by gpui_ios_get_window().
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_show_keyboard(window_ptr: *mut c_void) {
    if window_ptr.is_null() {
        return;
    }

    log::info!("GPUI iOS: Show keyboard requested");

    let window = unsafe { &*(window_ptr as *const super::window::IosWindow) };
    window.show_keyboard_with_type(crate::KeyboardType::Default);
}

/// Hide the software keyboard.
///
/// Call this when a text input field loses focus.
/// The window_ptr should be the value returned by gpui_ios_get_window().
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_hide_keyboard(window_ptr: *mut c_void) {
    if window_ptr.is_null() {
        return;
    }

    log::info!("GPUI iOS: Hide keyboard requested");

    let window = unsafe { &*(window_ptr as *const super::window::IosWindow) };
    window.hide_keyboard();
}

/// Handle text input from the software keyboard.
///
/// This is called when the user types on the keyboard.
/// Parameters:
/// - `window_ptr`: Pointer to the IosWindow
/// - `text_ptr`: Pointer to NSString with the entered text
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_handle_text_input(window_ptr: *mut c_void, text_ptr: *mut c_void) {
    if window_ptr.is_null() || text_ptr.is_null() {
        return;
    }

    log::info!("GPUI iOS: Handle text input");

    let window = unsafe { &*(window_ptr as *const super::window::IosWindow) };
    window.handle_text_input(text_ptr as *mut objc2::runtime::AnyObject);
}

/// Handle a key event from an external keyboard.
///
/// Parameters:
/// - `window_ptr`: Pointer to the IosWindow
/// - `key_code`: The key code from UIKeyboardHIDUsage
/// - `modifiers`: Modifier flags from UIKeyModifierFlags
/// - `is_key_down`: true for key down, false for key up
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_handle_key_event(
    window_ptr: *mut c_void,
    key_code: u32,
    modifiers: u32,
    is_key_down: bool,
) {
    if window_ptr.is_null() {
        return;
    }

    log::info!(
        "GPUI iOS: Handle key event - code: {}, modifiers: {}, down: {}",
        key_code,
        modifiers,
        is_key_down
    );

    let window = unsafe { &*(window_ptr as *const super::window::IosWindow) };
    window.handle_key_event(key_code, modifiers, is_key_down);
}

/// Called from ObjC when the app receives a URL to open.
///
/// Parameters:
/// - url_ptr: Pointer to an NSString containing the URL
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_handle_open_url(url_ptr: *mut c_void) {
    if url_ptr.is_null() {
        return;
    }

    let url_string = unsafe {
        use objc2::msg_send;
        let ns_str = url_ptr as *mut objc2::runtime::AnyObject;
        let cstr: *const std::ffi::c_char = msg_send![ns_str, UTF8String];
        if cstr.is_null() {
            return;
        }
        std::ffi::CStr::from_ptr(cstr)
            .to_string_lossy()
            .into_owned()
    };

    log::info!("GPUI iOS: Received deep link: {}", url_string);

    #[cfg(feature = "deeplink")]
    {
        crate::packages::deeplink::ios::handle_open_url(url_string);
    }
}

// ── App callback storage ─────────────────────────────────────────────────────

/// Wrapper around an `UnsafeCell<Option<Box<dyn FnOnce(&mut App)>>>`.
///
/// # Safety
/// On iOS all UI work happens on the main thread.  The FFI entry points
/// (`set_app_callback`, `run_app`, `gpui_ios_run_demo`) are only ever
/// called from the main thread, so interior-mutable access is safe.
#[allow(clippy::type_complexity)]
struct AppCallbackCell(std::cell::UnsafeCell<Option<Box<dyn FnOnce(&mut App)>>>);

// Safety: only accessed from the iOS main thread.
unsafe impl Send for AppCallbackCell {}
unsafe impl Sync for AppCallbackCell {}

static APP_CALLBACK: OnceLock<AppCallbackCell> = OnceLock::new();

/// Register a callback that will be invoked inside `Application::run`.
///
/// This must be called **before** [`run_app`] so that the run-loop
/// has something to do (open a window, create views, etc.).
///
/// # Safety
/// Must be called from the main thread only.
pub fn set_app_callback(cb: Box<dyn FnOnce(&mut App)>) {
    let cell = APP_CALLBACK.get_or_init(|| AppCallbackCell(std::cell::UnsafeCell::new(None)));
    unsafe {
        *cell.0.get() = Some(cb);
    }
}

#[allow(clippy::type_complexity)]
fn take_app_callback() -> Option<Box<dyn FnOnce(&mut App)>> {
    APP_CALLBACK
        .get()
        .and_then(|cell| unsafe { (*cell.0.get()).take() })
}

/// C entry point called from `main.m`'s app delegate.
///
/// Consumer crates should call [`set_app_callback`] **before** this function
/// to register their root view.  If no callback is registered an empty
/// window is opened as a fallback.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_run_demo() {
    run_app();
}

/// Run the GPUI iOS application.
///
/// This initialises the platform, creates the `Application`, and enters the
/// GPUI run loop.  The actual UI is determined by a callback previously
/// registered via [`set_app_callback`].  If no callback was registered a
/// default empty window is opened so the app doesn't crash.
pub fn run_app() {
    log::info!("GPUI iOS: Starting application");

    // Initialise the FFI layer if not already done.
    if IOS_APP_STATE.get().is_none() {
        let state = IosAppState {
            finish_launching: std::cell::UnsafeCell::new(None),
        };
        let _ = IOS_APP_STATE.set(state);
        let _ = IOS_WINDOW_LIST.set(WindowListWrapper(std::cell::UnsafeCell::new(Vec::new())));
    }

    let platform = Rc::new(super::IosPlatform::new());
    // Wire a real HTTP client so `gpui::img(uri)` (and any other
    // URL-sourced asset) actually fetches. Without this gpui defaults
    // to `FakeHttpClient::with_404_response` and every URL silently
    // 404s — banked in gem's §17.8 from the s50 YouTube poster work.
    // `reqwest_client::ReqwestClient` spins up a 1-thread tokio
    // runtime + reqwest+rustls; same impl zed itself uses.
    //
    // Use `proxy_and_user_agent` (not `new`) because `new` configures
    // reqwest with bare `use_rustls_tls()` which gives rustls no CA
    // roots — every HTTPS request fails with
    // `invalid peer certificate: UnknownIssuer`.
    // `proxy_and_user_agent` routes through
    // `http_client_tls::tls_config()` which sets up
    // `rustls-platform-verifier`, hitting iOS's system trust store.
    let http_client: std::sync::Arc<dyn gpui::http_client::HttpClient> = std::sync::Arc::new(
        reqwest_client::ReqwestClient::proxy_and_user_agent(None, "gpui-mobile/0.1")
            .expect("ReqwestClient::proxy_and_user_agent"),
    );
    let mut app = Application::with_platform(platform).with_http_client(http_client);
    // `run_until(&mut self, ...)` (gpui core, philipaw/zed @ bb5281fa7d+):
    // the `&mut self` shape lets us hold `app` past the run callback,
    // which Platform::run on iOS returns from immediately. Plain `run`
    // would consume `app` and drop the underlying Rc<AppCell>, which
    // cascades down through Window → IosWindow → UIKit tree and
    // leaves the simulator showing a snapshot of freed memory.
    app.run_until(|cx: &mut App| {
        if let Some(cb) = take_app_callback() {
            log::info!("GPUI iOS: Invoking user-provided app callback");
            cb(cx);
        } else {
            log::warn!("GPUI iOS: No app callback registered — opening default empty window");
            cx.open_window(
                WindowOptions {
                    window_bounds: None,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| gpui::Empty),
            )
            .expect("Failed to open default window");
            cx.activate(true);
        }
    });

    // On iOS, Application::run_until() stores the launch callback (via
    // IosPlatform::run → set_finish_launching_callback) and returns
    // immediately. Invoke it here synchronously so the AppDelegate
    // doesn't have to (in a properly-structured iOS app the
    // AppDelegate would call gpui_ios_did_finish_launching instead;
    // for the spike, we collapse that into run_app to keep main.m
    // simple).
    if let Some(state) = IOS_APP_STATE.get() {
        let callback = unsafe { (*state.finish_launching.get()).take() };
        if let Some(callback) = callback {
            log::info!("GPUI iOS: Invoking Application::run_until callback");
            callback();
        }
    }

    // Leak the Application so its underlying Rc<AppCell> outlives
    // run_app. Without this, dropping `app` here would cascade down
    // through the gpui App's Windows → Box<dyn PlatformWindow> →
    // IosWindow → UIKit tree, leaving the simulator showing a
    // snapshot of freed memory and the AT walking dangling pointers
    // in IOS_WINDOW_LIST.
    //
    // The leak is deliberate and permanent for the lifetime of the
    // iOS process. A future cleanup could stash `app` in a static
    // for explicit teardown on app termination, but until iOS
    // teardown is meaningful (the OS terminates the process anyway)
    // mem::forget is the simplest correct shape.
    std::mem::forget(app);
}
