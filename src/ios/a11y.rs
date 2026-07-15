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
use gpui::FocusId;
use gpui::accessibility::PendingA11yAction;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
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
    /// Full-tree `TreeUpdate` snapshot rebuilt on EVERY drain by
    /// [`A11yState::absorb_drain`], so `accesskit_consumer::Tree::new`
    /// initializes consistently no matter WHEN the AT activates.
    ///
    /// This used to cache only the FIRST drain, which went stale as
    /// soon as the element tree mutated (screen changes) while the
    /// adapter was inactive: a late activation then initialized the
    /// consumer from the stale snapshot, and the next diff referenced
    /// nodes it never received — panicking `validate_global` with
    /// "Focused ID #… is not in the node list" (gem §17.8 #120, found
    /// live by the ds/0f shell-chrome AT walk, 2026-07-11).
    pub(crate) initial_update: Option<TreeUpdate>,
    /// Latest data for every drained node, pruned after each absorb to
    /// the set reachable from the root (dead screens don't linger).
    snapshot_nodes: HashMap<accesskit::NodeId, accesskit::Node>,
    /// Tree descriptor from the most recent update carrying one (gpui
    /// emits it on the first drain; it only names the root).
    snapshot_tree: Option<accesskit::Tree>,
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

impl A11yState {
    /// Fold one drained (possibly diff) `TreeUpdate` into the running
    /// full-tree snapshot and rebuild `initial_update` from it, so an
    /// AT activating at ANY later point starts from a tree consistent
    /// with the next diff it will receive.
    ///
    /// Three moves:
    /// - **merge**: every node in the update replaces its entry in
    ///   `snapshot_nodes` (diffs re-emit any node whose data changed);
    /// - **prune**: walk the root-reachable set — nodes a diff dropped
    ///   (their parent stopped listing them) become unreachable and
    ///   are removed, so dead screens leave no ghosts;
    /// - **clamp**: the snapshot's focus must exist in the snapshot
    ///   (`validate_global` panics otherwise) — fall back to the root.
    pub(crate) fn absorb_drain(&mut self, update: &TreeUpdate) {
        for (id, node) in &update.nodes {
            self.snapshot_nodes.insert(*id, node.clone());
        }
        if update.tree.is_some() {
            self.snapshot_tree = update.tree.clone();
        }
        let Some(tree) = self.snapshot_tree.clone() else {
            return;
        };
        let mut reachable: Vec<(accesskit::NodeId, accesskit::Node)> = Vec::new();
        let mut seen: HashSet<accesskit::NodeId> = HashSet::new();
        let mut stack = vec![tree.root];
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(node) = self.snapshot_nodes.get(&id) {
                stack.extend(node.children().iter().copied());
                reachable.push((id, node.clone()));
            }
        }
        self.snapshot_nodes.retain(|id, _| seen.contains(id));
        let focus = if seen.contains(&update.focus) {
            update.focus
        } else {
            tree.root
        };
        self.initial_update = Some(TreeUpdate {
            // gpui stamps every drain with `TreeId::ROOT`; carry the
            // live update's id through so the snapshot matches.
            tree_id: update.tree_id.clone(),
            nodes: reachable,
            tree: Some(tree),
            focus,
        });
    }

    /// Debug hook: current snapshot size (root-reachable node count).
    /// Read by `DEBUG_A11Y_SNAPSHOT=1` logging in `window.rs` so hosts
    /// can assert pruning from run logs.
    pub(crate) fn snapshot_len(&self) -> usize {
        self.snapshot_nodes.len()
    }
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
                state
                    .pending_actions
                    .push(PendingA11yAction::Focus(focus_id));
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
