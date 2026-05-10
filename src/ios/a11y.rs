//! Per-window AccessKit ↔ gpui-mobile glue. Mirror of `gpui_macos`'s
//! a11y wiring (search the gpui_macos `window.rs` for `A11yState`,
//! `LoggingActionHandler`, `WindowActivationHandler`).
//!
//! This module owns three things:
//!
//! - `A11yState`: per-frame shared state that lives across the platform
//!   handler closure (writer of `initial_update` + `focus_inverse_map`),
//!   the activation handler (reader of `initial_update`), and the
//!   action handler (reader of `focus_inverse_map`, writer of
//!   `pending_actions`). All four parties hold an
//!   `Arc<parking_lot::Mutex<A11yState>>` clone of the same instance.
//! - `WindowActivationHandler`: invoked by `accesskit_consumer` on first
//!   AT activation. Returns the cached `initial_update` so consumer's
//!   `Tree::new` can initialize without panicking.
//! - `IosActionHandler`: invoked when the AT triggers an action on a
//!   node. For `Action::Focus`, resolves `target_node` → `FocusId` via
//!   `focus_inverse_map` and enqueues a `PendingA11yAction::Focus(fid)`
//!   on `pending_actions` so gpui core can apply it on the next draw.

use accesskit::{Action, ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use accesskit_ios::SubclassingAdapter;
use gpui::accessibility::PendingA11yAction;
use gpui::FocusId;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// Send wrapper around `accesskit_ios::SubclassingAdapter`. The
/// adapter retains an `Id<UIView>` internally; `Id` is `!Send` by
/// design because UIKit objects are main-thread-only. `gpui`'s
/// `PlatformWindow::take_accessibility_handler` requires the closure
/// to be `Send`, which transitively demands `Send` on everything it
/// captures. We assert it manually here — the adapter is only ever
/// accessed from the main thread (UIKit invariant), but the type
/// system can't see that.
///
/// Mirror of the `unsafe impl Send for MacWindowState {}` pattern in
/// `gpui_macos`, applied at finer granularity since gpui-mobile's
/// `IosWindow` doesn't have a single state newtype to wrap.
pub(crate) struct SendSubclassingAdapter(pub(crate) SubclassingAdapter);

// SAFETY: the adapter is only accessed on the UIKit main thread; the
// Send impl exists to satisfy gpui's trait bounds.
unsafe impl Send for SendSubclassingAdapter {}

impl Deref for SendSubclassingAdapter {
    type Target = SubclassingAdapter;
    fn deref(&self) -> &SubclassingAdapter {
        &self.0
    }
}

impl DerefMut for SendSubclassingAdapter {
    fn deref_mut(&mut self) -> &mut SubclassingAdapter {
        &mut self.0
    }
}

#[derive(Default)]
pub(crate) struct A11yState {
    /// FIRST `TreeUpdate` from gpui's drain — has `tree: Some(...)` plus
    /// the full nodes list. Required because subsequent emissions are
    /// diffs that can't initialize `accesskit_consumer::Tree::new`.
    pub(crate) initial_update: Option<TreeUpdate>,
    /// Most-recent `NodeId → FocusId` inverse map from gpui's drain.
    /// Replaced each frame. The action handler reads this to resolve
    /// `Action::Focus` requests back to a gpui `FocusHandle`.
    pub(crate) focus_inverse_map: HashMap<accesskit::NodeId, FocusId>,
    /// Cross-thread dispatch queue. The action handler runs on the
    /// UIKit main thread but without `&mut Window/&mut App`; it pushes
    /// here when an AT action resolves to a gpui domain key, and
    /// `IosWindow::take_pending_a11y_actions` drains the queue on each
    /// gpui draw where those mutable references are available.
    pub(crate) pending_actions: Vec<PendingA11yAction>,
}

pub(crate) struct WindowActivationHandler {
    pub(crate) state: Arc<Mutex<A11yState>>,
}

impl ActivationHandler for WindowActivationHandler {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.state.lock().initial_update.clone()
    }
}

pub(crate) struct IosActionHandler {
    pub(crate) state: Arc<Mutex<A11yState>>,
}

impl ActionHandler for IosActionHandler {
    fn do_action(&mut self, request: ActionRequest) {
        if matches!(request.action, Action::Focus) {
            let mut state = self.state.lock();
            if let Some(focus_id) = state.focus_inverse_map.get(&request.target_node).copied() {
                state.pending_actions.push(PendingA11yAction::Focus(focus_id));
                log::info!(
                    "[a11y] Action::Focus resolved target_node={:?} → {:?} (enqueued)",
                    request.target_node,
                    focus_id,
                );
                return;
            }
            log::info!(
                "[a11y] Action::Focus target_node={:?} has no FocusHandle in inverse map",
                request.target_node,
            );
        } else {
            log::info!(
                "[a11y] ActionRequest action={:?} target_node={:?} (no handler yet)",
                request.action,
                request.target_node,
            );
        }
    }
}
