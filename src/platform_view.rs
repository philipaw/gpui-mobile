//! Platform Views — embedding native views in the GPUI render tree.
//!
//! Platform views allow native UI components (video players, maps, camera
//! previews, web views) to be embedded alongside GPUI-rendered content.
//!
//! ## Architecture
//!
//! This follows Flutter's platform view approach:
//!
//! 1. **`PlatformView`** — trait representing a live native view instance.
//! 2. **`PlatformViewFactory`** — creates `PlatformView` instances by type.
//! 3. **`PlatformViewRegistry`** — global registry of view factories.
//! 4. **`PlatformViewHandle`** — opaque handle for managing a platform view.
//!
//! ## Composition Modes
//!
//! - **Hybrid Composition** (default): The native view is placed in the
//!   platform's view hierarchy and positioned to match the GPUI element's
//!   screen coordinates. Best compatibility, slight performance overhead.
//!
//! - **Texture-based** (future): The native view renders to an offscreen
//!   texture that GPUI composites. Better performance but some input and
//!   accessibility trade-offs.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use gpui_mobile::platform_view::{PlatformViewRegistry, PlatformViewParams};
//!
//! // Register a factory (typically in package init)
//! PlatformViewRegistry::global().register("video_player", Box::new(MyVideoFactory));
//!
//! // Create a view
//! let handle = PlatformViewRegistry::global()
//!     .create_view("video_player", PlatformViewParams::default())
//!     .unwrap();
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Unique identifier for a platform view instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlatformViewId(pub u64);

impl PlatformViewId {
    pub fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for PlatformViewId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlatformView({})", self.0)
    }
}

/// Bounds for positioning a platform view, in logical pixels.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformViewBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Which side of the ONE gpui Metal layer a native view composites on.
///
/// gpui renders to exactly one `CAMetalLayer`, so a native view is a
/// *separate* layer that can go above it or below it — there is no way
/// to slot it *between* two gpui elements. Hence two tiers, not a z
/// continuum (see `DESIGN-PLATFORM-COMPOSITION.md`).
///
/// The tier is bound at the first successful insertion and cannot be
/// changed afterwards — the insertion guard makes later calls no-ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlatformViewTier {
    /// Below the Metal layer: gpui content draws ON TOP of the native
    /// view. For video and webviews — the render server composites them
    /// while gpui sleeps, and gpui paints selection chrome over them.
    ///
    /// The cost of this tier is hole-punching: an opaque gpui fill over
    /// the same rect hides the native view completely (§17.8 #117).
    #[default]
    Underlay,
    /// Above the Metal layer: the native view covers ALL gpui content.
    /// For chrome that is topmost by design, and the only tier where a
    /// real system blur can see live pixels beneath it.
    Overlay,
}

/// Parameters for creating a platform view.
#[derive(Debug, Clone, Default)]
pub struct PlatformViewParams {
    /// Initial bounds in logical pixels.
    pub bounds: PlatformViewBounds,
    /// Creation arguments (type-specific, e.g. video URL, map coordinates).
    pub creation_params: HashMap<String, String>,
}

/// Trait representing a live native platform view.
///
/// Implementors wrap a platform-specific native view (Android `View`,
/// iOS `UIView`) and manage its lifecycle.
pub trait PlatformView: Send + Sync {
    /// Returns the unique identifier for this view.
    fn id(&self) -> PlatformViewId;

    /// Returns the view type string (e.g. "video_player", "map").
    fn view_type(&self) -> &str;

    /// Update the view's position and size to match GPUI layout.
    ///
    /// Called on every frame where the element's bounds have changed.
    /// Coordinates are in logical pixels relative to the window origin.
    fn set_bounds(&self, bounds: PlatformViewBounds);

    /// Show or hide the native view.
    ///
    /// Called when the GPUI element enters/leaves the visible area or
    /// when the view is explicitly hidden.
    fn set_visible(&self, visible: bool);

    /// Rotate the native view about its center by `radians`.
    ///
    /// iOS overrides this to apply a `CGAffineTransform` rotation to the
    /// hosted `UIView` (pivoting on its center, matching how GPUI-side
    /// `SolidRect`/image rotation pivots on the widget-rect center). The
    /// stored angle is re-applied on every `set_bounds` so the per-paint
    /// frame update doesn't clobber the transform. `0.0` restores the
    /// identity transform (axis-aligned, byte-for-byte the unrotated
    /// path). Other platforms no-op by default — rotation of embedded
    /// native views isn't wired on Android yet.
    fn set_rotation(&self, _radians: f32) {}

    /// Insert the native view into the host window's view hierarchy,
    /// BELOW the Metal layer ([`PlatformViewTier::Underlay`]).
    ///
    /// Called from the [`platform_view`] element's paint callback on
    /// every paint; implementations must guard against double-insertion
    /// (the first successful call inserts, subsequent calls are no-ops).
    ///
    /// Default no-op for platforms (e.g. Android) whose view insertion
    /// happens at view-creation time via their own bridge layer. iOS
    /// overrides this to add the UIView below the Metal view.
    ///
    /// [`platform_view`]: crate::components::platform_view_element::platform_view
    fn insert_into_window(&self) -> Result<(), String> {
        Ok(())
    }

    /// Insert the native view into the host window's view hierarchy on
    /// the given [`PlatformViewTier`].
    ///
    /// The default implementation **ignores the tier** and delegates to
    /// [`PlatformView::insert_into_window`], i.e. it always produces an
    /// underlay. Backends that can honour `Overlay` override this; iOS
    /// does. Android does NOT — its views are inserted by the JNI bridge
    /// at creation time and it has no tier control, so an `Overlay`
    /// request there silently composites as whatever the bridge chose.
    /// That gap is real and unfixed; see the module docs.
    fn insert_into_window_at(&self, tier: PlatformViewTier) -> Result<(), String> {
        let _ = tier;
        self.insert_into_window()
    }

    /// Set the z-order of the native view.
    ///
    /// Higher values are drawn on top. GPUI content rendered after the
    /// platform view element can overlay it using this mechanism.
    fn set_z_index(&self, z_index: i32);

    /// Dispose of the native view and release all resources.
    ///
    /// After this call, the view must not be used. The native view is
    /// removed from the view hierarchy.
    fn dispose(&self);

    /// Whether the view is currently disposed.
    fn is_disposed(&self) -> bool;

    /// Set the view's CALayer (or platform equivalent) to display the
    /// given IOSurface zero-copy. iOS overrides this to set
    /// `view.layer.contents = surface`; other platforms no-op.
    ///
    /// `surface` is an `IOSurfaceRef` (CoreFoundation type) cast to
    /// `*mut c_void`. Caller retains ownership; the view internally
    /// holds a reference (CALayer retains its `contents` value).
    /// Pass `null` to clear.
    ///
    /// Used by callers driving decoded video frames into a sibling-
    /// composite CALayer below the Metal view (zero-copy display
    /// path, distinct from gpui's wgpu sprite atlas). Has a known
    /// vsync-alignment limit: the layer displays whatever you set
    /// immediately, no frame-time scheduling. For smooth video
    /// playback use `enqueue_sample_buffer` instead.
    fn set_iosurface_contents(&self, _surface: *mut std::ffi::c_void) {}

    /// Enqueue a `CMSampleBuffer` for vsync-aligned video display.
    /// iOS sample_buffer_display view-type forwards this to an
    /// `AVSampleBufferDisplayLayer`'s `enqueueSampleBuffer:` —
    /// the layer schedules each buffer for display at its PTS,
    /// driven by a `CMTimebase` set up at view creation. Other
    /// view types + other platforms no-op.
    ///
    /// `sample_buffer` is a `CMSampleBufferRef`. Caller retains;
    /// the layer holds its own reference internally.
    fn enqueue_sample_buffer(&self, _sample_buffer: *mut std::ffi::c_void) {}

    /// Flush queued sample buffers + reset the controlling
    /// `CMTimebase` to PTS=0. Call when looping decoder back to
    /// the start of a clip so the timebase doesn't drift past the
    /// next loop pass's frame PTSs (otherwise all newly-enqueued
    /// frames would be "in the past" and AVSampleBufferDisplayLayer
    /// would display them immediately = fast-forward).
    fn flush_sample_buffer_layer(&self) {}
}

/// Factory for creating platform views of a specific type.
///
/// Registered with `PlatformViewRegistry` to handle creation of views
/// matching a particular `view_type` string.
pub trait PlatformViewFactory: Send + Sync {
    /// Create a new platform view with the given parameters.
    ///
    /// Returns `Ok(view)` on success or `Err(message)` if creation fails.
    fn create(&self, params: &PlatformViewParams) -> Result<Box<dyn PlatformView>, String>;

    /// The view type this factory handles (e.g. "video_player").
    fn view_type(&self) -> &str;
}

/// Handle for managing a platform view's lifecycle.
///
/// Wraps a `PlatformView` with convenience methods and automatic
/// disposal on drop.
pub struct PlatformViewHandle {
    view: Box<dyn PlatformView>,
}

impl std::fmt::Debug for PlatformViewHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformViewHandle")
            .field("id", &self.view.id())
            .field("view_type", &self.view.view_type())
            .finish()
    }
}

impl PlatformViewHandle {
    /// Create a new handle wrapping the given view.
    pub fn new(view: Box<dyn PlatformView>) -> Self {
        Self { view }
    }

    /// Get the view's unique identifier.
    pub fn id(&self) -> PlatformViewId {
        self.view.id()
    }

    /// Get the view type string.
    pub fn view_type(&self) -> &str {
        self.view.view_type()
    }

    /// Update the view's position and size.
    ///
    /// Also updates the global registry's bounds tracking so that
    /// hit-testing reflects the current position.
    pub fn set_bounds(&self, bounds: PlatformViewBounds) {
        self.view.set_bounds(bounds);
        PlatformViewRegistry::global().update_view_bounds(self.view.id(), bounds);
    }

    /// Show or hide the view.
    pub fn set_visible(&self, visible: bool) {
        self.view.set_visible(visible);
    }

    /// Rotate the view about its center by `radians`. Pass-through to
    /// `PlatformView::set_rotation`. Call alongside `set_bounds` on each
    /// paint; the iOS impl stores the angle and re-applies it after every
    /// frame update so the transform survives the per-paint `setFrame:`.
    pub fn set_rotation(&self, radians: f32) {
        self.view.set_rotation(radians);
    }

    /// Insert the underlying native view into the host window's
    /// view hierarchy. First-call effect; subsequent calls no-op.
    pub fn insert_into_window(&self) -> Result<(), String> {
        self.view.insert_into_window()
    }

    /// Insert the underlying native view on the given tier. First-call
    /// effect; subsequent calls no-op (so the tier is fixed at first
    /// insertion). Pass-through to [`PlatformView::insert_into_window_at`].
    pub fn insert_into_window_at(&self, tier: PlatformViewTier) -> Result<(), String> {
        self.view.insert_into_window_at(tier)
    }

    /// Set the z-order.
    pub fn set_z_index(&self, z_index: i32) {
        self.view.set_z_index(z_index);
    }

    /// Access the underlying `PlatformView` trait object.
    pub fn inner(&self) -> &dyn PlatformView {
        &*self.view
    }

    /// Dispose of the view explicitly.
    ///
    /// Also removes the view from the global registry's bounds tracking.
    pub fn dispose(&self) {
        PlatformViewRegistry::global().remove_view(self.view.id());
        self.view.dispose();
    }

    /// Pass-through to `PlatformView::set_iosurface_contents`.
    pub fn set_iosurface_contents(&self, surface: *mut std::ffi::c_void) {
        self.view.set_iosurface_contents(surface);
    }

    /// Pass-through to `PlatformView::enqueue_sample_buffer`.
    pub fn enqueue_sample_buffer(&self, sample_buffer: *mut std::ffi::c_void) {
        self.view.enqueue_sample_buffer(sample_buffer);
    }

    /// Pass-through to `PlatformView::flush_sample_buffer_layer`.
    pub fn flush_sample_buffer_layer(&self) {
        self.view.flush_sample_buffer_layer();
    }
}

impl Drop for PlatformViewHandle {
    fn drop(&mut self) {
        if !self.view.is_disposed() {
            PlatformViewRegistry::global().remove_view(self.view.id());
            self.view.dispose();
        }
    }
}

/// Global registry of platform view factories.
///
/// Packages register their factories here during initialization.
/// When a GPUI element needs a native view, it looks up the factory
/// by type string and creates an instance.
pub struct PlatformViewRegistry {
    factories: Mutex<HashMap<String, Box<dyn PlatformViewFactory>>>,
    views: Mutex<HashMap<PlatformViewId, PlatformViewBounds>>,
    /// Views driven by a [`platform_view`] element, for the per-pass
    /// visibility sweep. See [`PlatformViewRegistry::begin_frame`].
    ///
    /// [`platform_view`]: crate::components::platform_view_element::platform_view
    element_views: Mutex<HashMap<PlatformViewId, ElementView>>,
}

/// Per-pass state for one element-driven platform view.
struct ElementView {
    /// Weak so the registry never keeps a native view alive — the
    /// consumer's `Arc` owns the lifecycle, and `PlatformViewHandle`'s
    /// `Drop` still disposes on schedule.
    handle: Weak<PlatformViewHandle>,
    /// Did its element paint since the last `begin_frame`?
    shown: bool,
}

impl PlatformViewRegistry {
    /// Get the global registry singleton.
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<PlatformViewRegistry> = OnceLock::new();
        INSTANCE.get_or_init(|| Self {
            factories: Mutex::new(HashMap::new()),
            views: Mutex::new(HashMap::new()),
            element_views: Mutex::new(HashMap::new()),
        })
    }

    /// Hide every element-driven platform view. **Call once at the top
    /// of the render pass**, before the element tree is built.
    ///
    /// This is the immediate-mode ↔ retained-view bridge. A gpui element
    /// vanishes by simply not being emitted; a `UIView` does not — it
    /// stays inserted at its last window-space rect, so a widget panned
    /// out of view leaves its native view stranded on screen, spilling
    /// wherever the new gpui content doesn't happen to cover it (the bug
    /// surfaced 2026-05-19). So: hide everything here, and let each
    /// element that *does* paint this pass re-show itself.
    ///
    /// Both halves run inside ONE runloop turn, and UIKit commits its
    /// `CATransaction` at the end of that turn — so the hide/re-show
    /// pair collapses to no visual change for views that are still on
    /// screen. Only the genuinely-gone ones stay hidden.
    ///
    /// **This is per-render-pass, NOT per-frame-of-wall-clock.** Since
    /// the app only renders when something is animating (§17.8 #123), a
    /// settled screen stops painting entirely — `begin_frame` stops
    /// being called too, so on-screen views correctly stay visible. The
    /// sweep is driven by "we are rebuilding the tree", which is exactly
    /// when the answer can change.
    pub fn begin_frame(&self) {
        let mut map = self.element_views.lock().unwrap();
        // Drop entries whose consumer released the handle; hide the rest.
        map.retain(|_, ev| match ev.handle.upgrade() {
            Some(handle) => {
                handle.set_visible(false);
                ev.shown = false;
                true
            }
            None => false,
        });
    }

    /// Record an element-driven view's visibility for this pass, and
    /// start tracking it if this is its first paint. Called by the
    /// [`platform_view`] element; `shown` mirrors the `set_visible` the
    /// element just issued.
    ///
    /// [`platform_view`]: crate::components::platform_view_element::platform_view
    pub fn note_element_view(&self, handle: &Arc<PlatformViewHandle>, shown: bool) {
        self.element_views.lock().unwrap().insert(
            handle.id(),
            ElementView {
                handle: Arc::downgrade(handle),
                shown,
            },
        );
    }

    /// Is this element-driven view on screen as of the last render pass?
    ///
    /// The honest replacement for a consumer-side `visible` flag: once
    /// the render loop can sleep, paint is not a per-frame heartbeat, so
    /// anything driven off the display link (a decoder pump) must ASK
    /// rather than assume it was re-told this frame.
    pub fn is_element_view_showing(&self, id: PlatformViewId) -> bool {
        self.element_views
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|ev| ev.shown)
    }

    /// Register a factory for a view type.
    ///
    /// If a factory for this type already exists, it is replaced.
    pub fn register(&self, view_type: &str, factory: Box<dyn PlatformViewFactory>) {
        log::info!(
            "PlatformViewRegistry: registered factory for '{}'",
            view_type
        );
        self.factories
            .lock()
            .unwrap()
            .insert(view_type.to_string(), factory);
    }

    /// Unregister a factory for a view type.
    pub fn unregister(&self, view_type: &str) {
        self.factories.lock().unwrap().remove(view_type);
    }

    /// Check if a factory is registered for the given view type.
    pub fn has_factory(&self, view_type: &str) -> bool {
        self.factories.lock().unwrap().contains_key(view_type)
    }

    /// List all registered view types.
    pub fn registered_types(&self) -> Vec<String> {
        self.factories.lock().unwrap().keys().cloned().collect()
    }

    /// Create a platform view of the specified type.
    ///
    /// Returns a `PlatformViewHandle` that manages the view's lifecycle.
    pub fn create_view(
        &self,
        view_type: &str,
        params: PlatformViewParams,
    ) -> Result<PlatformViewHandle, String> {
        let factories = self.factories.lock().unwrap();
        let factory = factories
            .get(view_type)
            .ok_or_else(|| format!("No factory registered for view type '{}'", view_type))?;

        let view = factory.create(&params)?;
        let id = view.id();
        let initial_bounds = params.bounds;
        self.views.lock().unwrap().insert(id, initial_bounds);
        log::debug!(
            "PlatformViewRegistry: created view {} of type '{}'",
            id,
            view_type
        );
        Ok(PlatformViewHandle::new(view))
    }

    /// Update the bounds of a tracked platform view.
    ///
    /// Called whenever a platform view's position or size changes. This keeps
    /// the registry's bounds map in sync for hit-testing purposes.
    pub fn update_view_bounds(&self, id: PlatformViewId, bounds: PlatformViewBounds) {
        if let Some(entry) = self.views.lock().unwrap().get_mut(&id) {
            *entry = bounds;
        }
    }

    /// Remove a platform view from the registry's tracking.
    ///
    /// Called when a platform view is disposed. After removal the view's
    /// bounds will no longer participate in hit-testing.
    pub fn remove_view(&self, id: PlatformViewId) {
        self.views.lock().unwrap().remove(&id);
    }

    /// Check if a point hits any active platform view.
    ///
    /// Returns `true` if the point (in logical pixels, relative to the
    /// window origin) falls within any registered platform view's bounds.
    ///
    /// On Android with `NativeActivity`, all touch events go to the native
    /// surface first rather than to the Java view hierarchy. This method
    /// lets the input handler detect touches on platform views so it can
    /// skip GPUI dispatch and let the native views handle them instead.
    ///
    /// On iOS the OS's `UIView` hit-testing handles this natively, but
    /// this method is available as a consistent cross-platform check.
    pub fn hit_test(&self, x: f32, y: f32) -> bool {
        let views = self.views.lock().unwrap();
        for bounds in views.values() {
            if x >= bounds.x
                && x <= bounds.x + bounds.width
                && y >= bounds.y
                && y <= bounds.y + bounds.height
            {
                return true;
            }
        }
        false
    }

    /// Returns the number of currently tracked views.
    ///
    /// Useful for quick checks — if zero, hit-testing can be skipped entirely.
    pub fn active_view_count(&self) -> usize {
        self.views.lock().unwrap().len()
    }
}
