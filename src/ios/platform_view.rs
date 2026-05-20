//! iOS platform view implementation.
//!
//! Embeds native iOS `UIView` instances in the GPUI render tree using
//! hybrid composition. Native views are created here and can be inserted
//! into the view hierarchy by the iOS window code when a platform view
//! element is painted.

use crate::platform_view::{
    PlatformView, PlatformViewBounds, PlatformViewFactory, PlatformViewId, PlatformViewParams,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "ios")]
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

#[cfg(target_os = "ios")]
use super::cg_types::ObjcCGRect;

/// View type for the s56 zero-copy CMSampleBuffer path. Creates a
/// UIView containing an `AVSampleBufferDisplayLayer` sublayer + a
/// `CMTimebase` for vsync-aligned video playback. Callers enqueue
/// `CMSampleBuffer`s via `PlatformView::enqueue_sample_buffer` and
/// flush on loop-reopen via `PlatformView::flush_sample_buffer_layer`.
pub const VIEW_TYPE_SAMPLE_BUFFER_DISPLAY: &str = "sample_buffer_display";

/// iOS implementation of a platform view.
///
/// Wraps a `UIView` instance. The view is created during construction but
/// is NOT automatically added to the view hierarchy. Call
/// `native_view_ptr()` to get the raw `*mut AnyObject` pointer, then insert
/// it into the appropriate superview from the iOS window code.
///
/// TODO: The window/paint code should call `native_view_ptr()` and insert
/// the view as a subview of the Metal view's superview (or another
/// appropriate container) at the correct z-order position.
pub struct IosPlatformView {
    id: PlatformViewId,
    view_type: String,
    /// Pointer to the Objective-C UIView instance.
    /// Null if disposed.
    #[cfg(target_os = "ios")]
    native_view: std::sync::Mutex<*mut AnyObject>,
    disposed: AtomicBool,
    /// Whether this view has been inserted into the window's view hierarchy.
    inserted: AtomicBool,
    bounds: std::sync::Mutex<PlatformViewBounds>,
    /// Pointer to a sublayer that needs specialised handling.
    /// Currently only populated for `view_type = "sample_buffer_display"`,
    /// where it points at the `AVSampleBufferDisplayLayer` added below
    /// the UIView's main layer. Null otherwise. Retained via the
    /// sublayer chain — UIView's layer owns it.
    #[cfg(target_os = "ios")]
    aux_layer: std::sync::Mutex<*mut AnyObject>,
    /// Manually-driven `CMTimebase` controlling
    /// `AVSampleBufferDisplayLayer`'s playback clock for the sample-
    /// buffer view type. Reset to PTS=0 on each loop reopen so
    /// PTS=0 buffers in a fresh decode pass match the timebase's
    /// "now". Strong reference held in this `Retained<>` — released
    /// on dispose. Null/uninitialised for other view types.
    #[cfg(target_os = "ios")]
    timebase: std::sync::Mutex<Option<objc2::rc::Retained<objc2_core_media::CMTimebase>>>,
}

// Safety: UIView operations are dispatched to the main thread.
unsafe impl Send for IosPlatformView {}
unsafe impl Sync for IosPlatformView {}

impl IosPlatformView {
    /// Create a new iOS platform view.
    ///
    /// The UIView is allocated and initialized with the given bounds, but
    /// is NOT added to any view hierarchy. The caller is responsible for
    /// inserting the view by using `native_view_ptr()`.
    #[cfg(target_os = "ios")]
    pub fn new(view_type: &str, params: &PlatformViewParams) -> Result<Self, String> {
        let id = PlatformViewId::next();

        let native_view = Self::create_native_view(view_type, &params.bounds, params)?;

        // Probe for the sample-buffer sublayer + create its
        // controlling timebase if this is a sample_buffer_display
        // view. Generic / video_player / webview / camera_preview
        // views leave these unset.
        #[cfg(target_os = "ios")]
        let (aux_layer, timebase) = if view_type == VIEW_TYPE_SAMPLE_BUFFER_DISPLAY {
            unsafe {
                let layer = find_sublayer_of_class(native_view, "AVSampleBufferDisplayLayer");
                let tb = make_host_timebase().ok();
                if let (Some(tb_ref), false) = (tb.as_ref(), layer.is_null()) {
                    // setControlTimebase: takes `CMTimebaseRef`
                    // (opaque-struct pointer, encoding
                    // `^{OpaqueCMTimebase=}`). Casting through
                    // `*mut AnyObject` (encoding `@`) trips objc2's
                    // debug-build type check — the bug went
                    // unnoticed on iPhone device in s56 because
                    // release builds skip the check. Pass the
                    // raw CMTimebase pointer instead.
                    let tb_ptr: *const objc2_core_media::CMTimebase = &**tb_ref;
                    let _: () =
                        msg_send![layer, setControlTimebase: tb_ptr];
                }
                (layer, tb)
            }
        } else {
            (std::ptr::null_mut(), None)
        };

        Ok(Self {
            id,
            view_type: view_type.to_string(),
            native_view: std::sync::Mutex::new(native_view),
            disposed: AtomicBool::new(false),
            inserted: AtomicBool::new(false),
            bounds: std::sync::Mutex::new(params.bounds),
            #[cfg(target_os = "ios")]
            aux_layer: std::sync::Mutex::new(aux_layer),
            #[cfg(target_os = "ios")]
            timebase: std::sync::Mutex::new(timebase),
        })
    }

    /// Create the native UIView via Objective-C runtime.
    ///
    /// Dispatches to type-specific creation for known view types:
    /// - "video_player": Creates UIView with AVPlayerLayer (requires player_id in params)
    /// - "webview": Creates WKWebView (uses url/html from params)
    /// - "camera_preview": Creates UIView with AVCaptureVideoPreviewLayer (requires session_id in params)
    /// - Default: Creates a generic UIView container
    #[cfg(target_os = "ios")]
    fn create_native_view(
        view_type: &str,
        bounds: &PlatformViewBounds,
        params: &PlatformViewParams,
    ) -> Result<*mut AnyObject, String> {
        unsafe {
            let frame = ObjcCGRect::new(
                bounds.x as f64,
                bounds.y as f64,
                bounds.width as f64,
                bounds.height as f64,
            );

            let view: *mut AnyObject = match view_type {
                "video_player" => Self::create_video_player_view(frame, params)?,
                "webview" => Self::create_webview_view(frame, params)?,
                "camera_preview" => Self::create_camera_preview_view(frame, params)?,
                VIEW_TYPE_SAMPLE_BUFFER_DISPLAY => {
                    Self::create_sample_buffer_display_view(frame)?
                }
                _ => Self::create_generic_view(frame)?,
            };

            if view.is_null() {
                return Err(format!("Failed to create UIView for type '{}'", view_type));
            }

            // Clip to bounds
            let _: () = msg_send![view, setClipsToBounds: true];

            log::info!(
                "IosPlatformView: created native UIView for type '{}' at ({}, {}, {}, {})",
                view_type,
                bounds.x,
                bounds.y,
                bounds.width,
                bounds.height,
            );

            Ok(view)
        }
    }

    /// Create a generic transparent UIView container.
    #[cfg(target_os = "ios")]
    unsafe fn create_generic_view(frame: ObjcCGRect) -> Result<*mut AnyObject, String> {
        let uiview_class = class!(UIView);
        let view: *mut AnyObject = msg_send![uiview_class, alloc];
        let view: *mut AnyObject = msg_send![view, initWithFrame: frame];
        if view.is_null() {
            return Err("Failed to create UIView".into());
        }
        let clear_color: *mut AnyObject = msg_send![class!(UIColor), clearColor];
        let _: () = msg_send![view, setBackgroundColor: clear_color];
        Ok(view)
    }

    /// Create a UIView with an AVPlayerLayer for video playback.
    #[cfg(target_os = "ios")]
    unsafe fn create_video_player_view(
        frame: ObjcCGRect,
        params: &PlatformViewParams,
    ) -> Result<*mut AnyObject, String> {
        // Create a container UIView
        let uiview_class = class!(UIView);
        let view: *mut AnyObject = msg_send![uiview_class, alloc];
        let view: *mut AnyObject = msg_send![view, initWithFrame: frame];
        if view.is_null() {
            return Err("Failed to create UIView for video_player".into());
        }

        let black_color: *mut AnyObject = msg_send![class!(UIColor), blackColor];
        let _: () = msg_send![view, setBackgroundColor: black_color];

        // If a player_id is provided, try to get the AVPlayer and create AVPlayerLayer
        if let Some(player_id_str) = params.creation_params.get("player_id") {
            if let Ok(player_id) = player_id_str.parse::<u32>() {
                // Get AVPlayer from the video_player package's PLAYERS map
                if let Some(player_ptr) = crate::packages::video_player::ios_get_player(player_id) {
                    let player_layer: *mut AnyObject =
                        msg_send![class!(AVPlayerLayer), playerLayerWithPlayer: player_ptr];
                    if !player_layer.is_null() {
                        let _: () = msg_send![player_layer, setFrame: frame];
                        // Set video gravity to aspect fit. `make_nsstring`
                        // returns an AUTORELEASED NSString — do not
                        // `release` it (double-release fault during
                        // autorelease pool drain, EXC_BAD_ACCESS in
                        // `objc_release`).
                        let gravity = Self::make_nsstring("AVLayerVideoGravityResizeAspect");
                        let _: () = msg_send![player_layer, setVideoGravity: gravity];
                        let view_layer: *mut AnyObject = msg_send![view, layer];
                        let _: () = msg_send![view_layer, addSublayer: player_layer];
                    }
                }
            }
        }

        Ok(view)
    }

    /// Create a WKWebView.
    #[cfg(target_os = "ios")]
    unsafe fn create_webview_view(
        frame: ObjcCGRect,
        params: &PlatformViewParams,
    ) -> Result<*mut AnyObject, String> {
        let config: *mut AnyObject = msg_send![class!(WKWebViewConfiguration), alloc];
        let config: *mut AnyObject = msg_send![config, init];
        if config.is_null() {
            return Err("Failed to create WKWebViewConfiguration".into());
        }

        let js_enabled = params
            .creation_params
            .get("javascript_enabled")
            .map(|v| v == "true")
            .unwrap_or(true);

        let prefs: *mut AnyObject = msg_send![config, preferences];
        if !prefs.is_null() {
            let _: () = msg_send![prefs, setJavaScriptEnabled: js_enabled];
        }

        let webview: *mut AnyObject = msg_send![class!(WKWebView), alloc];
        let webview: *mut AnyObject =
            msg_send![webview, initWithFrame: frame, configuration: config];
        if webview.is_null() {
            return Err("Failed to create WKWebView".into());
        }

        // Load URL or HTML if provided. `make_nsstring` returns
        // autoreleased; do not `release` (over-release otherwise).
        if let Some(url) = params.creation_params.get("url") {
            if !url.is_empty() {
                let ns_url_str = Self::make_nsstring(url);
                let nsurl: *mut AnyObject = msg_send![class!(NSURL), URLWithString: ns_url_str];
                if !nsurl.is_null() {
                    let request: *mut AnyObject =
                        msg_send![class!(NSURLRequest), requestWithURL: nsurl];
                    let _: *mut AnyObject = msg_send![webview, loadRequest: request];
                }
            }
        } else if let Some(html) = params.creation_params.get("html") {
            if !html.is_empty() {
                let ns_html = Self::make_nsstring(html);
                // If a base_url creation_param is provided, build an
                // NSURL from it and pass as baseURL: — critical for
                // embedding services that reject iframes loaded from
                // a null origin (e.g. YouTube returns "Error 153
                // Video player configuration error" without one).
                let base_url: *mut AnyObject = match params
                    .creation_params
                    .get("base_url")
                {
                    Some(s) if !s.is_empty() => {
                        let ns_base_str = Self::make_nsstring(s);
                        let url: *mut AnyObject =
                            msg_send![class!(NSURL), URLWithString: ns_base_str];
                        url
                    }
                    _ => std::ptr::null_mut(),
                };
                let _: *mut AnyObject =
                    msg_send![webview, loadHTMLString: ns_html, baseURL: base_url];
            }
        }

        Ok(webview)
    }

    /// Create a UIView with an `AVSampleBufferDisplayLayer` sublayer.
    /// The sublayer's `enqueueSampleBuffer:` API schedules frames for
    /// vsync-aligned display using each `CMSampleBuffer`'s PTS;
    /// drives smooth video playback off raw decoded buffers without
    /// going through `AVPlayer`. Paired with a manually-driven
    /// `CMTimebase` (set up in `new`) so PTS=0 in a fresh decode
    /// pass aligns with timebase-time=0.
    #[cfg(target_os = "ios")]
    unsafe fn create_sample_buffer_display_view(
        frame: ObjcCGRect,
    ) -> Result<*mut AnyObject, String> {
        let uiview_class = class!(UIView);
        let view: *mut AnyObject = msg_send![uiview_class, alloc];
        let view: *mut AnyObject = msg_send![view, initWithFrame: frame];
        if view.is_null() {
            return Err("Failed to create UIView for sample_buffer_display".into());
        }
        let clear_color: *mut AnyObject = msg_send![class!(UIColor), clearColor];
        let _: () = msg_send![view, setBackgroundColor: clear_color];

        let layer: *mut AnyObject = msg_send![class!(AVSampleBufferDisplayLayer), alloc];
        let layer: *mut AnyObject = msg_send![layer, init];
        if layer.is_null() {
            return Err("Failed to create AVSampleBufferDisplayLayer".into());
        }
        let sublayer_frame =
            ObjcCGRect::new(0.0, 0.0, frame.width, frame.height);
        let _: () = msg_send![layer, setFrame: sublayer_frame];
        // `resize` (the literal value of `AVLayerVideoGravityResize`,
        // also valid as a CALayer contentsGravity) — the layer
        // stretches the video to fill the bounds. Spike scene uses
        // 4:3-ish bounds matching the source aspect, so this looks
        // identical to `resizeAspect` in practice.
        let gravity = Self::make_nsstring("resize");
        let _: () = msg_send![layer, setVideoGravity: gravity];
        let view_layer: *mut AnyObject = msg_send![view, layer];
        let _: () = msg_send![view_layer, addSublayer: layer];

        Ok(view)
    }

    /// Create a UIView with AVCaptureVideoPreviewLayer for camera preview.
    #[cfg(target_os = "ios")]
    unsafe fn create_camera_preview_view(
        frame: ObjcCGRect,
        params: &PlatformViewParams,
    ) -> Result<*mut AnyObject, String> {
        let uiview_class = class!(UIView);
        let view: *mut AnyObject = msg_send![uiview_class, alloc];
        let view: *mut AnyObject = msg_send![view, initWithFrame: frame];
        if view.is_null() {
            return Err("Failed to create UIView for camera_preview".into());
        }

        let black_color: *mut AnyObject = msg_send![class!(UIColor), blackColor];
        let _: () = msg_send![view, setBackgroundColor: black_color];

        // If a session_id is provided, try to get the AVCaptureSession and create preview layer
        if let Some(session_id_str) = params.creation_params.get("session_id") {
            if let Ok(session_id) = session_id_str.parse::<usize>() {
                if let Some(session_ptr) = crate::packages::camera::ios_get_session(session_id) {
                    let layer: *mut AnyObject =
                        msg_send![class!(AVCaptureVideoPreviewLayer), alloc];
                    let layer: *mut AnyObject = msg_send![layer, initWithSession: session_ptr];
                    if !layer.is_null() {
                        let _: () = msg_send![layer, setFrame: frame];
                        // `make_nsstring` returns autoreleased; don't
                        // manually `release` (would over-release).
                        let gravity = Self::make_nsstring("AVLayerVideoGravityResizeAspectFill");
                        let _: () = msg_send![layer, setVideoGravity: gravity];
                        let view_layer: *mut AnyObject = msg_send![view, layer];
                        let _: () = msg_send![view_layer, addSublayer: layer];
                    }
                }
            }
        }

        Ok(view)
    }

    #[cfg(target_os = "ios")]
    unsafe fn make_nsstring(s: &str) -> *mut AnyObject {
        crate::ios::util::nsstring(s)
    }

    /// Returns the raw pointer to the underlying `UIView`.
    ///
    /// Use this to insert the view into the iOS view hierarchy from
    /// the window/paint code. Returns null if the view has been disposed.
    #[cfg(target_os = "ios")]
    pub fn native_view_ptr(&self) -> *mut AnyObject {
        *self.native_view.lock().unwrap()
    }

    /// Insert this view into the GPUI window's view hierarchy.
    ///
    /// Adds the UIView as a subview of the Metal view's superview (the
    /// view controller's view), positioned below the Metal view so that
    /// GPUI content renders on top.
    ///
    /// This should be called once after creation, typically from the
    /// platform view element's first paint. Subsequent calls are no-ops.
    /// Renamed from `insert_into_window` so the trait method on
    /// `PlatformView` can call this without shadowing or recursing.
    #[cfg(target_os = "ios")]
    fn do_insert_into_window(&self) -> Result<(), String> {
        // Guard against double-insertion.
        if self.inserted.swap(true, Ordering::Relaxed) {
            return Ok(());
        }

        let native_view = *self.native_view.lock().unwrap();
        if native_view.is_null() {
            self.inserted.store(false, Ordering::Relaxed);
            return Err("View is null (disposed?)".to_string());
        }

        unsafe {
            if let Some(wrapper) = super::ffi::IOS_WINDOW_LIST.get() {
                let windows = &*wrapper.0.get();
                if let Some(&window_ptr) = windows.last() {
                    if !window_ptr.is_null() {
                        let window = &*window_ptr;
                        // The Metal view IS the view controller's
                        // root view (window.rs sets
                        // `view_controller.setView: metal_view`), so
                        // for "below-Metal" composition we use the
                        // Metal view's *superview* — the UIWindow —
                        // as the insertion parent and insert the
                        // native view below the Metal view there.
                        // Earlier iterations of this scaffold used
                        // `vc.view` as the parent, but that's the
                        // Metal view itself — which would make the
                        // native view a CHILD of the Metal view
                        // (rendered on top of the Metal scene paint,
                        // not below).
                        let metal_view = window.metal_view_ptr();
                        if !metal_view.is_null() {
                            let parent: *mut AnyObject =
                                msg_send![metal_view, superview];
                            if !parent.is_null() {
                                let _: () = msg_send![
                                    parent,
                                    insertSubview: native_view,
                                    belowSubview: metal_view
                                ];
                                log::info!(
                                    "IosPlatformView: inserted view {} below Metal view",
                                    self.id
                                );
                                // Verdict-side dump: walk
                                // `[parent subviews]` (the UIWindow)
                                // and emit one line per subview
                                // (class name + frame). Step-1
                                // platform_view scaffolding verdict
                                // reads these from
                                // `simctl launch --console` to
                                // confirm:
                                //   (a) the inserted view is in the
                                //       subviews list, and
                                //   (b) it appears BEFORE
                                //       GPUIMetalView
                                //       (insertSubview:belowSubview:
                                //       puts the new view earlier
                                //       in the subviews array).
                                // Routed through `eprintln!` (not
                                // log::) so the trace doesn't depend
                                // on a logger being configured.
                                Self::dump_view_hierarchy(parent, self.id);
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        self.inserted.store(false, Ordering::Relaxed);
        Err("No GPUI window available to host platform view".to_string())
    }

    /// Walk a UIView's `subviews` array and print one line per child
    /// to stderr. Step-1 platform_view verdict: lets a host-side
    /// harness scan the line stream emitted by
    /// `simctl launch --console` for `[platform-view-hierarchy]
    /// subview[N]: <Class> ...` and assert structural properties
    /// (e.g. WKWebView present, ordered before GPUIMetalView).
    #[cfg(target_os = "ios")]
    fn dump_view_hierarchy(vc_view: *mut AnyObject, inserted_id: PlatformViewId) {
        use objc2::runtime::AnyClass;
        unsafe {
            let subviews: *mut AnyObject = msg_send![vc_view, subviews];
            if subviews.is_null() {
                eprintln!(
                    "[platform-view-hierarchy] vc.view has nil subviews (insert id={inserted_id})"
                );
                return;
            }
            let count: usize = msg_send![subviews, count];
            eprintln!(
                "[platform-view-hierarchy] inserted id={inserted_id}; Metal view's superview has {count} subview(s) (earlier index = lower z):"
            );
            for i in 0..count {
                let subview: *mut AnyObject = msg_send![subviews, objectAtIndex: i];
                if subview.is_null() {
                    eprintln!("[platform-view-hierarchy] subview[{i}]: <nil>");
                    continue;
                }
                let cls_ptr: *const AnyClass = msg_send![subview, class];
                let name = if cls_ptr.is_null() {
                    "<null-class>".to_string()
                } else {
                    (&*cls_ptr).name().to_string_lossy().into_owned()
                };
                let frame: ObjcCGRect = msg_send![subview, frame];
                eprintln!(
                    "[platform-view-hierarchy] subview[{i}]: class={name} frame=({:.0},{:.0},{:.0},{:.0})",
                    frame.x, frame.y, frame.width, frame.height
                );
            }
        }
    }

    /// Update the native view's frame.
    ///
    /// Also resizes every sublayer of the view's CALayer to match
    /// the new bounds (origin-zero, since sublayer frames are in
    /// superlayer coordinates). CALayer on iOS does NOT honor
    /// `autoresizingMask` (that property is macOS-only), so e.g.
    /// the `AVPlayerLayer` added by `create_video_player_view` and
    /// the `AVCaptureVideoPreviewLayer` added by
    /// `create_camera_preview_view` would otherwise stay frozen at
    /// their initial-creation size (the `bounds.width/height`
    /// passed to `IosPlatformView::new`) while the outer UIView
    /// grew to the gpui-computed widget rect. Symptom: the video /
    /// camera surface renders at 1×1 (or whatever the seed size
    /// was) in the top-left of the visible UIView while the rest
    /// of the bbox stays dark.
    #[cfg(target_os = "ios")]
    fn update_native_frame(&self, bounds: &PlatformViewBounds) {
        let view = *self.native_view.lock().unwrap();
        if view.is_null() {
            return;
        }
        unsafe {
            let frame = ObjcCGRect::new(
                bounds.x as f64,
                bounds.y as f64,
                bounds.width as f64,
                bounds.height as f64,
            );
            let _: () = msg_send![view, setFrame: frame];

            let sublayer_frame =
                ObjcCGRect::new(0.0, 0.0, bounds.width as f64, bounds.height as f64);
            let view_layer: *mut AnyObject = msg_send![view, layer];
            if !view_layer.is_null() {
                let sublayers: *mut AnyObject = msg_send![view_layer, sublayers];
                if !sublayers.is_null() {
                    let count: usize = msg_send![sublayers, count];
                    for i in 0..count {
                        let sub: *mut AnyObject = msg_send![sublayers, objectAtIndex: i];
                        if !sub.is_null() {
                            let _: () = msg_send![sub, setFrame: sublayer_frame];
                        }
                    }
                }
            }
        }
    }
}

impl PlatformView for IosPlatformView {
    fn id(&self) -> PlatformViewId {
        self.id
    }

    fn view_type(&self) -> &str {
        &self.view_type
    }

    fn set_bounds(&self, bounds: PlatformViewBounds) {
        if self.disposed.load(Ordering::Relaxed) {
            return;
        }
        *self.bounds.lock().unwrap() = bounds;
        #[cfg(target_os = "ios")]
        self.update_native_frame(&bounds);
    }

    fn set_visible(&self, visible: bool) {
        if self.disposed.load(Ordering::Relaxed) {
            return;
        }
        #[cfg(target_os = "ios")]
        {
            let view = *self.native_view.lock().unwrap();
            if !view.is_null() {
                unsafe {
                    let _: () = msg_send![view, setHidden: !visible];
                }
            }
        }
        #[cfg(not(target_os = "ios"))]
        {
            let _ = visible;
        }
    }

    fn insert_into_window(&self) -> Result<(), String> {
        if self.disposed.load(Ordering::Relaxed) {
            return Err("View is disposed".into());
        }
        #[cfg(target_os = "ios")]
        {
            return self.do_insert_into_window();
        }
        #[cfg(not(target_os = "ios"))]
        {
            Ok(())
        }
    }

    fn set_z_index(&self, z_index: i32) {
        if self.disposed.load(Ordering::Relaxed) {
            return;
        }
        #[cfg(target_os = "ios")]
        {
            let view = *self.native_view.lock().unwrap();
            if !view.is_null() {
                unsafe {
                    let layer: *mut AnyObject = msg_send![view, layer];
                    if !layer.is_null() {
                        let z = z_index as f64;
                        let _: () = msg_send![layer, setZPosition: z];
                    }
                }
            }
        }
        #[cfg(not(target_os = "ios"))]
        {
            let _ = z_index;
        }
    }

    fn dispose(&self) {
        if self.disposed.swap(true, Ordering::Relaxed) {
            return;
        }
        #[cfg(target_os = "ios")]
        {
            let view = *self.native_view.lock().unwrap();
            if !view.is_null() {
                unsafe {
                    // Remove from superview if it was added to one.
                    let _: () = msg_send![view, removeFromSuperview];
                }
            }
        }
        log::info!("IosPlatformView: disposed view {}", self.id);
    }

    fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::Relaxed)
    }

    /// Enqueue a `CMSampleBuffer` for vsync-aligned display on the
    /// view's `AVSampleBufferDisplayLayer` sublayer. No-op for other
    /// view types (where `aux_layer` is null). Caller retains the
    /// CMSampleBuffer; the layer holds its own reference internally.
    fn enqueue_sample_buffer(&self, sample_buffer: *mut std::ffi::c_void) {
        if self.disposed.load(Ordering::Relaxed) || sample_buffer.is_null() {
            return;
        }
        #[cfg(target_os = "ios")]
        {
            let layer = *self.aux_layer.lock().unwrap();
            if layer.is_null() {
                return;
            }
            unsafe {
                // enqueueSampleBuffer: takes `CMSampleBufferRef`
                // (`^{opaqueCMSampleBuffer=}`), not an Obj-C
                // object (`@`). Casting via `*mut AnyObject`
                // trips objc2's debug-build type check (sim
                // panics; release-build device skipped the
                // check, which is why s56 device "worked").
                let sb_ptr: *const objc2_core_media::CMSampleBuffer =
                    sample_buffer as *const objc2_core_media::CMSampleBuffer;
                let _: () = msg_send![layer, enqueueSampleBuffer: sb_ptr];
            }
        }
        #[cfg(not(target_os = "ios"))]
        {
            let _ = sample_buffer;
        }
    }

    /// Flush queued buffers + reset the controlling `CMTimebase` to
    /// PTS=0. Call when looping decoder back to the start so the
    /// timebase doesn't drift past the next loop pass's frames.
    fn flush_sample_buffer_layer(&self) {
        if self.disposed.load(Ordering::Relaxed) {
            return;
        }
        #[cfg(target_os = "ios")]
        {
            let layer = *self.aux_layer.lock().unwrap();
            if !layer.is_null() {
                unsafe {
                    let _: () = msg_send![layer, flush];
                }
            }
            if let Some(tb) = self.timebase.lock().unwrap().as_ref() {
                reset_timebase_to_zero(tb);
            }
        }
    }

    /// Set `view.layer.contents` to the given IOSurface for zero-copy
    /// display (decoded video frames, externally-rendered Metal
    /// content, etc.). Caller retains the IOSurface; CALayer holds
    /// its own reference internally.
    ///
    /// `surface` is an `IOSurfaceRef` (`__IOSurface*` after toll-free
    /// bridging). Pass null to clear. On first call we also set
    /// `contentsGravity = resize` so the surface fills the layer
    /// regardless of its native pixel size, plus
    /// `masksToBounds = true` so non-rectangular clips don't bleed.
    fn set_iosurface_contents(&self, surface: *mut std::ffi::c_void) {
        if self.disposed.load(Ordering::Relaxed) {
            return;
        }
        #[cfg(target_os = "ios")]
        {
            let view = *self.native_view.lock().unwrap();
            if view.is_null() {
                return;
            }
            unsafe {
                let layer: *mut AnyObject = msg_send![view, layer];
                if layer.is_null() {
                    return;
                }
                // Wrap in CATransaction with implicit actions
                // disabled, otherwise Core Animation crossfades
                // every `contents` change with its default 0.25 s
                // CABasicAnimation. For video playback (30 fps =
                // ~33 ms per frame) that means every new frame
                // overlaps the previous frame's still-running
                // fade-out → visible jitter / ghosting. Observed
                // 2026-05-19 on iPhone 16 Pro Max during s56's
                // IOSurface zero-copy verdict.
                let ca_tx = class!(CATransaction);
                let _: () = msg_send![ca_tx, begin];
                let _: () = msg_send![ca_tx, setDisableActions: true];
                let _: () =
                    msg_send![layer, setContents: surface as *mut AnyObject];
                // Idempotent layer config — cheap enough to set every
                // frame; setting once on first non-null contents is a
                // future optimisation if profiling demands it.
                let gravity = Self::make_nsstring("resize");
                let _: () = msg_send![layer, setContentsGravity: gravity];
                let _: () = msg_send![layer, setMasksToBounds: true];
                let _: () = msg_send![ca_tx, commit];
            }
        }
        #[cfg(not(target_os = "ios"))]
        {
            let _ = surface;
        }
    }
}

/// iOS platform view factory.
pub struct IosPlatformViewFactory {
    view_type: String,
}

impl IosPlatformViewFactory {
    pub fn new(view_type: &str) -> Self {
        Self {
            view_type: view_type.to_string(),
        }
    }
}

/// Walk a UIView's CALayer sublayers and return the first one whose
/// Obj-C class name matches `class_name`. Used to extract the
/// `AVSampleBufferDisplayLayer` we added during view creation so
/// later `enqueue_sample_buffer` calls hit the right layer without
/// rescanning every time (the result is cached in `aux_layer`).
/// Returns null if no match.
#[cfg(target_os = "ios")]
unsafe fn find_sublayer_of_class(view: *mut AnyObject, class_name: &str) -> *mut AnyObject {
    use objc2::runtime::AnyClass;
    if view.is_null() {
        return std::ptr::null_mut();
    }
    let view_layer: *mut AnyObject = msg_send![view, layer];
    if view_layer.is_null() {
        return std::ptr::null_mut();
    }
    let sublayers: *mut AnyObject = msg_send![view_layer, sublayers];
    if sublayers.is_null() {
        return std::ptr::null_mut();
    }
    let count: usize = msg_send![sublayers, count];
    for i in 0..count {
        let layer: *mut AnyObject = msg_send![sublayers, objectAtIndex: i];
        if layer.is_null() {
            continue;
        }
        let cls_ptr: *const AnyClass = msg_send![layer, class];
        if cls_ptr.is_null() {
            continue;
        }
        let name = (&*cls_ptr).name();
        if name.to_string_lossy() == class_name {
            return layer;
        }
    }
    std::ptr::null_mut()
}

// `CMTimebaseCreateWithSourceClock` isn't bound by objc2-core-media
// 0.3.2 (it's marked TODO in the generated code). Declare the C
// signature directly. CoreMedia.framework is already linked
// transitively via other AVFoundation msg_send paths.
#[cfg(target_os = "ios")]
unsafe extern "C" {
    fn CMTimebaseCreateWithSourceClock(
        allocator: *mut std::ffi::c_void,
        source_clock: *const objc2_core_media::CMClock,
        timebase_out: *mut *mut objc2_core_media::CMTimebase,
    ) -> i32;
}

/// Create a host-time-clock-backed `CMTimebase` running at rate 1.0
/// from time 0. `AVSampleBufferDisplayLayer.controlTimebase` reads
/// this to schedule sample-buffer display against the timebase's
/// clock — so PTS=0 in our newly-opened AvfDecoder aligns with
/// timebase-time=0 (= now-at-view-creation). On loop reopen we
/// reset the timebase to 0 again via `reset_timebase_to_zero`.
#[cfg(target_os = "ios")]
unsafe fn make_host_timebase() -> Result<objc2::rc::Retained<objc2_core_media::CMTimebase>, String>
{
    use objc2_core_media::{CMClock, CMTime, CMTimeFlags, CMTimebase};

    let host_clock = CMClock::host_time_clock();
    let mut tb_out: *mut CMTimebase = std::ptr::null_mut();
    let status = CMTimebaseCreateWithSourceClock(
        std::ptr::null_mut(),
        &*host_clock,
        &mut tb_out,
    );
    if status != 0 || tb_out.is_null() {
        return Err(format!("CMTimebaseCreateWithSourceClock status={status}"));
    }
    // CMTimebaseCreate returns +1 retained (CF "Create" rule).
    let tb = objc2::rc::Retained::from_raw(tb_out)
        .ok_or_else(|| "CMTimebase Retained::from_raw nil".to_string())?;
    // Initial state: time=0, rate=1 (playback proceeds at 1× host
    // clock rate from PTS=0).
    let zero = CMTime {
        value: 0,
        timescale: 1_000_000,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    };
    let _ = tb.set_time(zero);
    let _ = tb.set_rate(1.0);
    Ok(tb)
}

/// Reset a `CMTimebase` to time=0 + rate=1. Used at loop-reopen so
/// the newly-decoded frames (PTS starting at 0 again) land at the
/// scheduler's "now" rather than "in the past".
#[cfg(target_os = "ios")]
fn reset_timebase_to_zero(tb: &objc2_core_media::CMTimebase) {
    let zero = objc2_core_media::CMTime {
        value: 0,
        timescale: 1_000_000,
        flags: objc2_core_media::CMTimeFlags::Valid,
        epoch: 0,
    };
    unsafe {
        let _ = tb.set_time(zero);
        let _ = tb.set_rate(1.0);
    }
}

impl PlatformViewFactory for IosPlatformViewFactory {
    fn create(&self, params: &PlatformViewParams) -> Result<Box<dyn PlatformView>, String> {
        #[cfg(target_os = "ios")]
        {
            let view = IosPlatformView::new(&self.view_type, params)?;
            Ok(Box::new(view))
        }
        #[cfg(not(target_os = "ios"))]
        {
            let _ = params;
            Err("iOS platform views are only available on iOS".to_string())
        }
    }

    fn view_type(&self) -> &str {
        &self.view_type
    }
}
