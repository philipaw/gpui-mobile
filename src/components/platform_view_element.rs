//! GPUI element for embedding native platform views.
//!
//! `PlatformViewElement` is a GPUI component that reserves layout space for
//! a native view (Android `View` or iOS `UIView`) and synchronizes its
//! position and size during the paint phase.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use gpui_mobile::components::platform_view_element::platform_view_element;
//! use gpui_mobile::platform_view::PlatformViewHandle;
//!
//! fn my_component(handle: Arc<PlatformViewHandle>) -> impl IntoElement {
//!     div()
//!         .flex()
//!         .child(platform_view_element(handle).size_full())
//! }
//! ```

use crate::platform_view::{PlatformViewBounds, PlatformViewHandle};
use gpui::{ParentElement, Styled, div};
use std::sync::Arc;

/// Create a GPUI element that hosts a native platform view.
///
/// The element reserves layout space and updates the platform view's
/// position and size on every paint. The native view is shown when
/// painted and hidden when the element is removed from the tree.
///
/// The returned element can be styled with `.w()`, `.h()`, `.size()`,
/// `.flex_grow()`, etc. to control how much space it occupies in the layout.
pub fn platform_view_element(handle: Arc<PlatformViewHandle>) -> gpui::Div {
    platform_view_element_rotated(handle, 0.0)
}

/// Like [`platform_view_element`], but also applies a `radians` rotation
/// to the hosted native view about its center on every paint.
///
/// `radians == 0.0` is byte-for-byte equivalent to
/// `platform_view_element` (the iOS impl short-circuits an unchanged-0
/// rotation and keeps the axis-aligned `setFrame:` path). Non-zero
/// rotates the `UIView` via a `CGAffineTransform`, matching how
/// GPUI-side `SolidRect`/image widgets rotate about their rect center.
/// Used by the gem-ios video widget so `Transform2D.rot` rotates a live
/// `AVPlayerLayer`/`AVSampleBufferDisplayLayer`-backed view.
pub fn platform_view_element_rotated(handle: Arc<PlatformViewHandle>, radians: f32) -> gpui::Div {
    div().child(
        gpui::canvas(
            // Prepaint: capture bounds
            move |bounds, _window, _cx| bounds,
            // Paint: synchronize native view position
            move |_bounds, prepaint_bounds, _window, _cx| {
                let logical_bounds = PlatformViewBounds {
                    x: prepaint_bounds.origin.x.as_f32(),
                    y: prepaint_bounds.origin.y.as_f32(),
                    width: prepaint_bounds.size.width.as_f32(),
                    height: prepaint_bounds.size.height.as_f32(),
                };
                // Set rotation before bounds: `set_bounds` re-applies the
                // stored rotation via the native frame update, so the
                // angle must be current first. An unchanged rotation is a
                // cheap no-op on the iOS side.
                handle.set_rotation(radians);
                handle.set_bounds(logical_bounds);
                handle.set_visible(true);
                // First-paint insertion: the iOS impl guards against
                // re-insertion via an AtomicBool, so this is cheap on
                // every subsequent frame. Android no-ops by default —
                // its insertion happens at create time via the JNI
                // bridge.
                if let Err(e) = handle.insert_into_window() {
                    log::warn!("platform_view_element: insert_into_window failed: {}", e);
                }
            },
        )
        .size_full(),
    )
}

/// A higher-level wrapper that creates the platform view from the registry
/// and manages its lifecycle.
///
/// The view is created on first render and disposed when dropped.
pub struct ManagedPlatformView {
    handle: Option<Arc<PlatformViewHandle>>,
    view_type: String,
    creation_params: std::collections::HashMap<String, String>,
}

impl ManagedPlatformView {
    /// Create a new managed platform view for the given type.
    pub fn new(view_type: impl Into<String>) -> Self {
        Self {
            handle: None,
            view_type: view_type.into(),
            creation_params: std::collections::HashMap::new(),
        }
    }

    /// Add a creation parameter.
    pub fn with_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.creation_params.insert(key.into(), value.into());
        self
    }

    /// Get or create the platform view handle.
    pub fn ensure_created(&mut self) -> Result<Arc<PlatformViewHandle>, String> {
        if let Some(handle) = &self.handle {
            return Ok(handle.clone());
        }

        let registry = crate::platform_view::PlatformViewRegistry::global();
        let params = crate::platform_view::PlatformViewParams {
            bounds: PlatformViewBounds::default(),
            creation_params: self.creation_params.clone(),
        };

        let handle = Arc::new(registry.create_view(&self.view_type, params)?);
        self.handle = Some(handle.clone());
        Ok(handle)
    }

    /// Get the handle if the view has been created.
    pub fn handle(&self) -> Option<&Arc<PlatformViewHandle>> {
        self.handle.as_ref()
    }

    /// Render the platform view element.
    ///
    /// Returns a div with an error message if the view fails to create.
    pub fn render(&mut self) -> gpui::Div {
        match self.ensure_created() {
            Ok(handle) => platform_view_element(handle),
            Err(e) => {
                log::error!("Failed to create platform view '{}': {}", self.view_type, e);
                div()
            }
        }
    }
}

impl Drop for ManagedPlatformView {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.dispose();
        }
    }
}
