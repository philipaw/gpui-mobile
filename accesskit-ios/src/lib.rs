//! AccessKit ↔ UIAccessibility adapter for iOS.
//!
//! Mirror of `accesskit_macos` 0.26's `SubclassingAdapter`. The host
//! UIView is dynamically subclassed at adapter-construction time; the
//! subclass intercepts UIAccessibility selectors
//! (`accessibilityElementCount`, `accessibilityElementAtIndex:`,
//! `indexOfAccessibilityElement:`) and routes them through the
//! adapter to the AccessKit tree. The application feeds the adapter
//! `accesskit::TreeUpdate`s via `update_if_active`; the activation
//! handler provides the initial tree on first AT access; the action
//! handler receives `accesskit::ActionRequest`s when an AT acts on a
//! node.
//!
//! Stub crate at this commit — API surface only, all method bodies
//! are `todo!()`. See `DESIGN.md` §10.4 Layer 3 for the implementation
//! plan and design decisions carried forward from the macOS adapter
//! (Layer 2).

mod adapter;
mod context;

pub use adapter::Adapter;
pub use accesskit::{
    ActionHandler, ActionRequest, ActivationHandler, NodeId, Tree, TreeUpdate,
};

/// Pending platform events the adapter wants the host to dispatch
/// (e.g. UIAccessibility notifications). Mirror of
/// `accesskit_macos::QueuedEvents`. Currently a placeholder.
pub struct QueuedEvents {
    _private: (),
}

/// Dynamically subclasses a UIView and intercepts UIAccessibility
/// selectors, routing them through the AccessKit tree fed via
/// `update_if_active`. Equivalent to
/// `accesskit_macos::SubclassingAdapter`.
pub struct SubclassingAdapter {
    _private: (),
}

impl SubclassingAdapter {
    /// Construct the adapter for the given UIView. Performs
    /// `objc_setClass` on the view (must be done before the view is
    /// shown or focused for the first time, mirroring the macOS
    /// constraint). The activation handler is invoked once on first
    /// AT access; the action handler receives any subsequent
    /// `ActionRequest`s.
    ///
    /// # Safety
    ///
    /// `view` must be a valid, unreleased pointer to a UIView. The
    /// view must outlive the adapter.
    pub unsafe fn new(
        _view: *mut std::ffi::c_void,
        _activation_handler: impl ActivationHandler + 'static,
        _action_handler: impl ActionHandler + 'static,
    ) -> Self {
        todo!(
            "Layer 3 next commits: declare AccessKit-iOS subclass via \
             objc2_runtime ClassBuilder, add UIAccessibility selector \
             impls, swap the view's class via objc_setClass, retain the \
             handlers on an associated object."
        )
    }

    /// Apply a `TreeUpdate` to the adapter's internal tree if the
    /// adapter has been activated by an AT (otherwise the update is
    /// discarded — first activation will pull the initial tree from
    /// the activation handler). Mirror of
    /// `accesskit_macos::SubclassingAdapter::update_if_active`.
    pub fn update_if_active(
        &mut self,
        _update_factory: impl FnOnce() -> TreeUpdate,
    ) -> Option<QueuedEvents> {
        todo!(
            "Layer 3: forward to underlying accesskit_consumer Tree's \
             update method when the adapter is active; collect any \
             notifications the application should post."
        )
    }

    /// Notify the adapter that the host UIView's
    /// `becomeFirstResponder` / `resignFirstResponder` state changed.
    /// AccessKit uses this to know whether keyboard focus should be
    /// reported through the adapter's tree.
    pub fn update_view_focus_state(
        &mut self,
        _is_focused: bool,
    ) -> Option<QueuedEvents> {
        todo!("Layer 3: forward the focus-state change to accesskit_consumer.")
    }
}
