//! [`platform_view`] — bind a native view to the gpui layout pass.
//!
//! This is gpui-mobile's answer to SwiftUI's `UIViewRepresentable`,
//! Compose's `AndroidView` and Flutter's `UiKitView`: the ONE seam
//! through which native platform components and custom gpui components
//! compose. A native view becomes just an element —
//!
//! ```rust,ignore
//! div().flex()
//!     .child(cover_card(..))                         // gpui
//!     .child(platform_view(&dock).h(px(56.)))        // UIKit, same layout pass
//!     .child(gem_row(..))                            // gpui
//! ```
//!
//! See `DESIGN-PLATFORM-COMPOSITION.md` for the architecture this
//! implements (tiers, the frame contract, and the per-component rule).
//!
//! ## What the element actually does
//!
//! 1. **Requests layout from its style**, like a `div` — so it takes
//!    part in flex/absolute layout with gpui siblings.
//! 2. **On paint**, pushes its computed WINDOW-SPACE bounds at the
//!    handle, marks it visible, and inserts it into the window once.
//! 3. **When not painted, hides it** — see [`PlatformViewRegistry::begin_frame`].
//! 4. **Occludes hit-testing**, so touches land in the native view
//!    rather than in gpui elements behind it.
//!
//! ## Why it paints nothing
//!
//! The element draws NO pixels into the Metal tier, and deliberately
//! ignores `.bg()` even though [`Styled`] makes it settable: an opaque
//! gpui fill over an underlay's rect hides the native view completely —
//! that is the §17.8 #117 occlusion-bug class, and this element is
//! exactly where it would be easiest to reintroduce. The style is
//! consumed for LAYOUT only.
//!
//! ## The frame contract
//!
//! This element never requests a frame. It is purely reactive to paint:
//! it positions a retained view from a pass that is happening anyway. A
//! settled screen stops painting (§17.8 #123) and that is CORRECT here —
//! nothing moved, so the native view's rect is still right, and the
//! render server keeps animating its content (video, blur) with gpui
//! asleep. That is the entire point of the overlay/underlay tiers.
//!
//! Anything that needs a frame for its own reasons — an external event
//! arriving from a native control — must go through
//! [`crate::request_render`], never an anonymous
//! `window.request_animation_frame()`.

use crate::platform_view::{
    PlatformViewBounds, PlatformViewHandle, PlatformViewRegistry, PlatformViewTier,
};
use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, HitboxBehavior, InspectorElementId,
    IntoElement, LayoutId, ParentElement, Pixels, Refineable as _, Style, StyleRefinement, Styled,
    Window, div,
};
use std::sync::Arc;

/// Map a gpui element rect to the native view's window-space rect.
///
/// **This is the whole layout→bounds contract, as a pure function** —
/// the `[ds] PVIEW1` verdict asserts it directly.
///
/// `element` is the `Bounds` gpui hands to `prepaint`/`paint`. Those are
/// ALREADY window-space absolute: `Window::layout_bounds` folds
/// `element_offset()` into the origin, and a scroll container contributes
/// its scroll offset through exactly that mechanism. So a scrolled child
/// arrives here pre-offset and the mapping to `PlatformViewBounds` is an
/// identity in logical pixels.
///
/// Two things it must NOT do, both of which are live failure modes:
///
/// - **No scale-factor multiply.** `PlatformViewBounds` is documented in
///   *logical* pixels and `UIView.frame` is in points; gpui's `Pixels` is
///   already logical. Scaling here would offset every native view by the
///   device scale (2×/3×) — an error that only appears on device.
/// - **No unoffset origin.** Reaching for a pre-offset rect would strand
///   every native view at its unscrolled position.
///
/// Returns `None` when the view must be HIDDEN rather than positioned:
///
/// - the rect is degenerate (zero/negative extent), or
/// - it lies entirely outside `mask`, the current content mask.
///
/// The mask check is load-bearing, not defensive. A gpui element scrolled
/// out of its container is still painted — gpui just clips it to the
/// mask. A native `UIView` has no such clip: positioned at those bounds
/// it would render at full size outside the container, which is the
/// "spilling over" bug from 2026-05-19.
///
/// A *partially* masked rect returns `Some` with its FULL bounds, so it
/// is shown unclipped and can spill past the container edge. That is a
/// known, accepted gap: clipping a native view to a sub-rect is not
/// expressible through `set_bounds` — shrinking the frame would squash
/// the content rather than crop it. It needs a native clipping container
/// (`masksToBounds` on a parent), which this abstraction does not have.
pub fn window_bounds(element: Bounds<Pixels>, mask: Bounds<Pixels>) -> Option<PlatformViewBounds> {
    let width = element.size.width.as_f32();
    let height = element.size.height.as_f32();
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    // Strictly-degenerate rects never reach `intersects` (it uses strict
    // inequalities, so a zero-extent rect inside the mask would report a
    // hit) — hence the explicit guard above.
    if !element.intersects(&mask) {
        return None;
    }
    Some(PlatformViewBounds {
        x: element.origin.x.as_f32(),
        y: element.origin.y.as_f32(),
        width,
        height,
    })
}

/// A gpui [`Element`] that binds a [`PlatformViewHandle`] to gpui layout.
///
/// Build it with [`platform_view`].
pub struct PlatformViewElement {
    handle: Arc<PlatformViewHandle>,
    tier: PlatformViewTier,
    z_index: i32,
    rotation: f32,
    occlude: bool,
    style: StyleRefinement,
}

/// Bind a native view to the gpui layout pass.
///
/// The element reserves layout space from its style and drives the
/// handle's window-space rect from paint. Style it like a `div`:
/// `.size_full()`, `.h(px(56.))`, `.flex_grow()`.
///
/// Defaults: [`PlatformViewTier::Underlay`], z-index 0, no rotation,
/// hit-testing occluded.
///
/// ```rust,ignore
/// platform_view(&handle)
///     .tier(PlatformViewTier::Overlay)
///     .z_index(10)
///     .h(px(56.))
/// ```
pub fn platform_view(handle: &Arc<PlatformViewHandle>) -> PlatformViewElement {
    PlatformViewElement {
        handle: handle.clone(),
        tier: PlatformViewTier::Underlay,
        z_index: 0,
        rotation: 0.0,
        occlude: true,
        style: StyleRefinement::default(),
    }
}

impl PlatformViewElement {
    /// Composite above or below the Metal layer. Bound at first
    /// insertion; later changes are ignored by the backend's
    /// insertion guard.
    pub fn tier(mut self, tier: PlatformViewTier) -> Self {
        self.tier = tier;
        self
    }

    /// Z-order WITHIN the tier (iOS: the view's `layer.zPosition`).
    /// It cannot lift an underlay above the Metal layer — that is what
    /// [`PlatformViewElement::tier`] is for.
    pub fn z_index(mut self, z_index: i32) -> Self {
        self.z_index = z_index;
        self
    }

    /// Rotate the native view about its center, in radians, matching how
    /// gpui-side rotation pivots on the widget-rect center. Re-applied
    /// after each `set_bounds`, so it survives the per-paint frame update.
    pub fn rotation(mut self, radians: f32) -> Self {
        self.rotation = radians;
        self
    }

    /// Whether to block gpui hit-testing over the view's rect
    /// (default `true`).
    ///
    /// This is the GPUI-SIDE half of "touches land in the native view":
    /// it stops gpui elements *behind* the platform view from reacting.
    /// Whether the native view actually RECEIVES the touch is a
    /// platform-side question about the Metal view's own hit-testing,
    /// which this element does not control. Set `false` for a native
    /// view that should be inert to touch (a decorative underlay) so gpui
    /// content behind it stays interactive.
    pub fn occlude(mut self, occlude: bool) -> Self {
        self.occlude = occlude;
        self
    }
}

impl IntoElement for PlatformViewElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for PlatformViewElement {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Element for PlatformViewElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        // Hitboxes must be inserted during prepaint. Only occlude where
        // the view will actually BE — a hidden view must not eat touches.
        if self.occlude && window_bounds(bounds, window.content_mask().bounds).is_some() {
            window.insert_hitbox(bounds, HitboxBehavior::BlockMouse);
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let registry = PlatformViewRegistry::global();
        match window_bounds(bounds, window.content_mask().bounds) {
            Some(rect) => {
                // Rotation BEFORE bounds: the iOS impl re-applies the
                // stored angle inside its frame update, so the angle has
                // to be current first. An unchanged angle is a no-op.
                self.handle.set_rotation(self.rotation);
                self.handle.set_bounds(rect);
                self.handle.set_z_index(self.z_index);
                // First-paint insertion; the backend guards re-insertion
                // with an atomic, so this is cheap on later paints.
                if let Err(e) = self.handle.insert_into_window_at(self.tier) {
                    log::warn!("platform_view: insert_into_window_at failed: {e}");
                }
                self.handle.set_visible(true);
                registry.note_element_view(&self.handle, true);
            }
            None => {
                // Clipped away or degenerate. `begin_frame` already hid
                // it, but assert it explicitly: a consumer that skips the
                // sweep must still not get a stranded view.
                self.handle.set_visible(false);
                registry.note_element_view(&self.handle, false);
            }
        }
    }
}

/// Deprecated shim for the pre-element API — a `div` wrapping a
/// `canvas` that pushed bounds at the handle.
///
/// It never carried a tier, z-index, hit-test occlusion, content-mask
/// clipping or the not-painted sweep, which is why every consumer
/// hand-rolled its own variant instead of using it. Kept as a
/// delegating shim so existing callers keep working; prefer
/// [`platform_view`].
pub fn platform_view_element(handle: Arc<PlatformViewHandle>) -> gpui::Div {
    platform_view_element_rotated(handle, 0.0)
}

/// Deprecated shim. Prefer [`platform_view`] + [`PlatformViewElement::rotation`].
pub fn platform_view_element_rotated(handle: Arc<PlatformViewHandle>, radians: f32) -> gpui::Div {
    div().child(platform_view(&handle).rotation(radians).size_full())
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
