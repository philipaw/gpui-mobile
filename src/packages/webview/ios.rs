use super::WebViewHandle;
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::{class, msg_send, sel};
use std::sync::{Mutex, Once};

#[link(name = "WebKit", kind = "framework")]
extern "C" {}

// ── JS → native message bridge ───────────────────────────────────────
//
// Embedded media providers (YouTube IFrame API, SoundCloud Widget API,
// …) emit playback events in-page. To auto-advance a room playlist
// (DESIGN §6.6) the host needs a "this track ended" signal out of the
// webview. We install a `WKScriptMessageHandler` named `gemPlayer` on
// every webview's content controller; page JS calls
// `window.webkit.messageHandlers.gemPlayer.postMessage(str)` and the
// string lands on `MESSAGES`. The host drains it each frame via
// `drain_messages` and routes it (typically a JSON
// `{event:"ended", widget:"<id>"}`) to `Schedule::signal_done`.

static MESSAGES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Drain every JS→native message posted since the last call. Each is
/// the raw string the page passed to `postMessage`; the host parses.
pub fn drain_messages() -> Vec<String> {
    let mut q = MESSAGES.lock().unwrap();
    std::mem::take(&mut *q)
}

static REGISTER_HANDLER: Once = Once::new();
static mut HANDLER_CLASS: *const AnyClass = std::ptr::null();

/// Lazily build the `WKScriptMessageHandler` NSObject subclass — same
/// `ClassBuilder` pattern as the file-selector / camera delegates.
fn handler_class() -> &'static AnyClass {
    REGISTER_HANDLER.call_once(|| {
        let superclass = class!(NSObject);
        let mut decl = ClassBuilder::new(c"GpuiWebViewMessageHandler", superclass).unwrap();
        unsafe {
            decl.add_method(
                sel!(userContentController:didReceiveScriptMessage:),
                did_receive_message
                    as unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject),
            );
        }
        unsafe { HANDLER_CLASS = decl.register() as *const AnyClass };
    });
    unsafe { &*HANDLER_CLASS }
}

unsafe extern "C" fn did_receive_message(
    _this: *mut AnyObject,
    _sel: Sel,
    _controller: *mut AnyObject,
    message: *mut AnyObject,
) {
    if message.is_null() {
        return;
    }
    // WKScriptMessage.body — for our use it's always an NSString (the
    // JSON the page JSON.stringify'd). Non-string bodies are ignored.
    let body: *mut AnyObject = msg_send![message, body];
    if body.is_null() {
        return;
    }
    let is_str: bool = msg_send![body, isKindOfClass: class!(NSString)];
    if !is_str {
        return;
    }
    let cstr: *const std::ffi::c_char = msg_send![body, UTF8String];
    if cstr.is_null() {
        return;
    }
    let s = std::ffi::CStr::from_ptr(cstr).to_string_lossy().into_owned();
    if let Ok(mut q) = MESSAGES.lock() {
        q.push(s);
    }
}

/// Install the `gemPlayer` script-message handler onto a freshly
/// created `WKWebViewConfiguration`. Safe to call on every webview —
/// pages that never post to `gemPlayer` are unaffected. Called from
/// `ios/platform_view.rs::create_webview_view`.
///
/// # Safety
/// `config` must be a valid `WKWebViewConfiguration` pointer.
pub unsafe fn attach_message_handler(config: *mut AnyObject) {
    if config.is_null() {
        return;
    }
    let controller: *mut AnyObject = msg_send![config, userContentController];
    if controller.is_null() {
        return;
    }
    let handler: *mut AnyObject = msg_send![handler_class(), alloc];
    let handler: *mut AnyObject = msg_send![handler, init];
    let name = crate::ios::util::nsstring("gemPlayer");
    let _: () = msg_send![controller, addScriptMessageHandler: handler, name: name];
}

pub fn evaluate_javascript(handle: &WebViewHandle, script: &str) -> Result<(), String> {
    // The platform view system handles the WKWebView lifecycle.
    // For JS evaluation, we need access to the underlying WKWebView.
    // This is currently a no-op — the platform view manages the webview.
    // TODO: Store a reference to the WKWebView for runtime JS evaluation.
    let _ = (handle, script);
    log::warn!("evaluate_javascript: not yet wired through platform view system");
    Ok(())
}

pub fn go_back(handle: &WebViewHandle) -> Result<(), String> {
    let _ = handle;
    log::warn!("go_back: not yet wired through platform view system");
    Ok(())
}

pub fn reload(handle: &WebViewHandle) -> Result<(), String> {
    let _ = handle;
    log::warn!("reload: not yet wired through platform view system");
    Ok(())
}

pub fn stop_loading(handle: &WebViewHandle) -> Result<(), String> {
    let _ = handle;
    log::warn!("stop_loading: not yet wired through platform view system");
    Ok(())
}
