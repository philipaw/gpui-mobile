use crate::context::{ActionHandlerNoMut, ActionHandlerWrapper, Context};
use accesskit::{
    ActionHandler, ActionRequest, ActivationHandler, Node as NodeProvider, NodeId as LocalNodeId,
    Role, Tree as TreeData, TreeId, TreeUpdate,
};
use accesskit_consumer::{common_filter, Node, Tree, TreeChangeHandler};
use std::ffi::c_void;
use std::fmt::{Debug, Formatter};
use std::rc::Rc;

const PLACEHOLDER_ROOT_ID: LocalNodeId = LocalNodeId(0);

enum State {
    Inactive {
        view: *mut c_void,
        is_view_focused: bool,
        action_handler: Rc<dyn ActionHandlerNoMut>,
    },
    Placeholder {
        view: *mut c_void,
        placeholder_context: Rc<Context>,
        is_view_focused: bool,
        action_handler: Rc<dyn ActionHandlerNoMut>,
    },
    Active {
        view: *mut c_void,
        context: Rc<Context>,
    },
}

impl Debug for State {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Inactive {
                view,
                is_view_focused,
                ..
            } => f
                .debug_struct("Inactive")
                .field("view", view)
                .field("is_view_focused", is_view_focused)
                .finish(),
            State::Placeholder {
                view,
                is_view_focused,
                ..
            } => f
                .debug_struct("Placeholder")
                .field("view", view)
                .field("is_view_focused", is_view_focused)
                .finish(),
            State::Active { view, context } => f
                .debug_struct("Active")
                .field("view", view)
                .field("context", context)
                .finish(),
        }
    }
}

struct PlaceholderActionHandler;

impl ActionHandler for PlaceholderActionHandler {
    fn do_action(&mut self, _request: ActionRequest) {}
}

struct NoopChangeHandler;

impl TreeChangeHandler for NoopChangeHandler {
    fn node_added(&mut self, _node: &Node) {}
    fn node_updated(&mut self, _old_node: &Node, _new_node: &Node) {}
    fn focus_moved(&mut self, _old_node: Option<&Node>, _new_node: Option<&Node>) {}
    fn node_removed(&mut self, _node: &Node) {}
}

#[derive(Debug)]
pub struct Adapter {
    state: State,
}

impl Adapter {
    /// Create a new iOS adapter. Mirror of `accesskit_macos::Adapter::new`.
    /// The view pointer is stored opaquely; sub-commit 2b's UIAccessibility
    /// query helpers will dereference it.
    ///
    /// # Safety
    ///
    /// `view` must be a valid, unreleased pointer to a UIView. The view
    /// must outlive the adapter.
    pub unsafe fn new(
        view: *mut c_void,
        is_view_focused: bool,
        action_handler: impl 'static + ActionHandler,
    ) -> Self {
        let state = State::Inactive {
            view,
            is_view_focused,
            action_handler: Rc::new(ActionHandlerWrapper::new(action_handler)),
        };
        Self { state }
    }

    /// Apply a [`TreeUpdate`] iff the adapter has been activated by an AT.
    /// Mirror of `accesskit_macos::Adapter::update_if_active`. Inactive
    /// state discards the update without invoking the factory; Placeholder
    /// state requires `update_factory` to return a full tree (per the
    /// AccessKit docstring contract).
    pub fn update_if_active(&mut self, update_factory: impl FnOnce() -> TreeUpdate) {
        match &self.state {
            State::Inactive { .. } => {}
            State::Placeholder {
                view,
                is_view_focused,
                action_handler,
                ..
            } => {
                let tree = Tree::new(update_factory(), *is_view_focused);
                let context = Context::new(tree, Rc::clone(action_handler));
                self.state = State::Active {
                    view: *view,
                    context,
                };
            }
            State::Active { context, .. } => {
                let mut handler = NoopChangeHandler;
                let mut tree = context.tree.borrow_mut();
                tree.update_and_process_changes(update_factory(), &mut handler);
            }
        }
    }

    /// Notify the adapter that the host view's focus state changed.
    /// Mirror of `accesskit_macos::Adapter::update_view_focus_state`.
    pub fn update_view_focus_state(&mut self, is_focused: bool) {
        match &mut self.state {
            State::Inactive {
                is_view_focused, ..
            } => {
                *is_view_focused = is_focused;
            }
            State::Placeholder {
                is_view_focused, ..
            } => {
                *is_view_focused = is_focused;
            }
            State::Active { context, .. } => {
                let mut handler = NoopChangeHandler;
                let mut tree = context.tree.borrow_mut();
                tree.update_host_focus_state_and_process_changes(is_focused, &mut handler);
            }
        }
    }

    /// Force the adapter to transition out of [`State::Inactive`] by
    /// pulling the initial tree from the activation handler. If the
    /// handler returns `Some`, the adapter goes to [`State::Active`];
    /// if `None`, it falls back to [`State::Placeholder`] (a synthetic
    /// `Role::Window` tree) until the next call to
    /// [`Self::update_if_active`].
    ///
    /// Sub-commit 2b's UIAccessibility query helpers will call this
    /// internally; exposing it now allows unit tests to drive the
    /// state machine without UIKit linkage.
    pub fn ensure_initialized<H: ActivationHandler + ?Sized>(
        &mut self,
        activation_handler: &mut H,
    ) {
        let _ = self.get_or_init_context(activation_handler);
    }

    fn get_or_init_context<H: ActivationHandler + ?Sized>(
        &mut self,
        activation_handler: &mut H,
    ) -> Rc<Context> {
        match &self.state {
            State::Inactive {
                view,
                is_view_focused,
                action_handler,
            } => match activation_handler.request_initial_tree() {
                Some(initial_state) => {
                    let tree = Tree::new(initial_state, *is_view_focused);
                    let context = Context::new(tree, Rc::clone(action_handler));
                    let result = Rc::clone(&context);
                    self.state = State::Active {
                        view: *view,
                        context,
                    };
                    result
                }
                None => {
                    let placeholder_update = TreeUpdate {
                        nodes: vec![(PLACEHOLDER_ROOT_ID, NodeProvider::new(Role::Window))],
                        tree: Some(TreeData::new(PLACEHOLDER_ROOT_ID)),
                        tree_id: TreeId::ROOT,
                        focus: PLACEHOLDER_ROOT_ID,
                    };
                    let placeholder_tree = Tree::new(placeholder_update, false);
                    let placeholder_context = Context::new(
                        placeholder_tree,
                        Rc::new(ActionHandlerWrapper::new(PlaceholderActionHandler {})),
                    );
                    let result = Rc::clone(&placeholder_context);
                    self.state = State::Placeholder {
                        view: *view,
                        placeholder_context,
                        is_view_focused: *is_view_focused,
                        action_handler: Rc::clone(action_handler),
                    };
                    result
                }
            },
            State::Placeholder {
                placeholder_context,
                ..
            } => Rc::clone(placeholder_context),
            State::Active { context, .. } => Rc::clone(context),
        }
    }

    /// Number of accessibility elements directly under the AccessKit
    /// root, after the common AccessKit filter (excludes generic
    /// containers, hidden nodes, etc.). Mirror of UIAccessibility's
    /// `accessibilityElementCount` selector.
    ///
    /// Sub-commit 2c will wrap this with the `*mut NSObject`-returning
    /// shim that UIKit invokes via the `UIAccessibilityContainer`
    /// informal protocol.
    pub fn accessibility_element_count<H: ActivationHandler + ?Sized>(
        &mut self,
        activation_handler: &mut H,
    ) -> isize {
        let context = self.get_or_init_context(activation_handler);
        let tree = context.tree.borrow();
        tree.state()
            .root()
            .filtered_children(common_filter)
            .count() as isize
    }

    /// [`LocalNodeId`] of the nth filtered child of the root, or
    /// `None` if `index` is negative or out of range. Sub-commit 2c
    /// will wrap this with a `*mut NSObject` shim returning a
    /// `PlatformNode` for `accessibilityElementAtIndex:`.
    pub fn accessibility_element_at_index<H: ActivationHandler + ?Sized>(
        &mut self,
        index: isize,
        activation_handler: &mut H,
    ) -> Option<LocalNodeId> {
        if index < 0 {
            return None;
        }
        let context = self.get_or_init_context(activation_handler);
        let tree = context.tree.borrow();
        tree.state()
            .root()
            .filtered_children(common_filter)
            .nth(index as usize)
            .map(|n| n.locate().0)
    }

    /// Index of the supplied node among the root's filtered children,
    /// or `None` if not present. Sub-commit 2c will translate `None`
    /// into `NSNotFound` for `indexOfAccessibilityElement:`.
    pub fn index_of_accessibility_element<H: ActivationHandler + ?Sized>(
        &mut self,
        node_id: LocalNodeId,
        activation_handler: &mut H,
    ) -> Option<isize> {
        let context = self.get_or_init_context(activation_handler);
        let tree = context.tree.borrow();
        tree.state()
            .root()
            .filtered_children(common_filter)
            .position(|n| n.locate().0 == node_id)
            .map(|i| i as isize)
    }

    #[doc(hidden)]
    pub fn debug_is_active(&self) -> bool {
        matches!(self.state, State::Active { .. })
    }

    #[doc(hidden)]
    pub fn debug_is_placeholder(&self) -> bool {
        matches!(self.state, State::Placeholder { .. })
    }

    #[doc(hidden)]
    pub fn debug_is_inactive(&self) -> bool {
        matches!(self.state, State::Inactive { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::null_mut;

    struct CountingActivation {
        update: Option<TreeUpdate>,
        calls: u32,
    }

    impl ActivationHandler for CountingActivation {
        fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
            self.calls += 1;
            self.update.take()
        }
    }

    struct CountingAction {
        calls: std::sync::Arc<std::sync::Mutex<u32>>,
    }

    impl ActionHandler for CountingAction {
        fn do_action(&mut self, _request: ActionRequest) {
            *self.calls.lock().unwrap() += 1;
        }
    }

    fn full_tree(focus: LocalNodeId) -> TreeUpdate {
        let root = LocalNodeId(1);
        TreeUpdate {
            nodes: vec![(root, NodeProvider::new(Role::Window))],
            tree: Some(TreeData::new(root)),
            tree_id: TreeId::ROOT,
            focus,
        }
    }

    /// Root with `n` button children. Children are LocalNodeId(2..2+n).
    fn tree_with_button_children(n: usize) -> TreeUpdate {
        let root_id = LocalNodeId(1);
        let mut root = NodeProvider::new(Role::Window);
        let child_ids: Vec<LocalNodeId> = (0..n).map(|i| LocalNodeId(2 + i as u64)).collect();
        root.set_children(child_ids.clone());

        let mut nodes = vec![(root_id, root)];
        for id in &child_ids {
            let mut btn = NodeProvider::new(Role::Button);
            btn.set_label(format!("Button {}", id.0));
            nodes.push((*id, btn));
        }

        TreeUpdate {
            nodes,
            tree: Some(TreeData::new(root_id)),
            tree_id: TreeId::ROOT,
            focus: root_id,
        }
    }

    fn make_adapter() -> Adapter {
        unsafe {
            Adapter::new(
                null_mut(),
                false,
                CountingAction {
                    calls: Default::default(),
                },
            )
        }
    }

    #[test]
    fn inactive_update_if_active_is_a_no_op() {
        let mut adapter = make_adapter();
        assert!(adapter.debug_is_inactive());

        // Factory must not be invoked. If update_if_active wrongly
        // forwarded in Inactive state, this panics.
        let mut factory_called = false;
        adapter.update_if_active(|| {
            factory_called = true;
            full_tree(LocalNodeId(1))
        });
        assert!(!factory_called);
        assert!(adapter.debug_is_inactive());
    }

    #[test]
    fn activation_with_full_tree_goes_active() {
        let mut adapter = make_adapter();
        let mut activation = CountingActivation {
            update: Some(full_tree(LocalNodeId(1))),
            calls: 0,
        };
        adapter.ensure_initialized(&mut activation);
        assert_eq!(activation.calls, 1);
        assert!(adapter.debug_is_active());
    }

    #[test]
    fn activation_returning_none_falls_back_to_placeholder() {
        let mut adapter = make_adapter();
        let mut activation = CountingActivation {
            update: None,
            calls: 0,
        };
        adapter.ensure_initialized(&mut activation);
        assert_eq!(activation.calls, 1);
        assert!(adapter.debug_is_placeholder());
    }

    #[test]
    fn placeholder_promotes_to_active_on_next_update() {
        let mut adapter = make_adapter();
        let mut activation = CountingActivation {
            update: None,
            calls: 0,
        };
        adapter.ensure_initialized(&mut activation);
        assert!(adapter.debug_is_placeholder());

        adapter.update_if_active(|| full_tree(LocalNodeId(1)));
        assert!(adapter.debug_is_active());
    }

    #[test]
    fn element_count_matches_filtered_children() {
        let mut adapter = make_adapter();
        let mut activation = CountingActivation {
            update: Some(tree_with_button_children(3)),
            calls: 0,
        };
        assert_eq!(adapter.accessibility_element_count(&mut activation), 3);
    }

    #[test]
    fn element_count_returns_zero_for_empty_root() {
        let mut adapter = make_adapter();
        let mut activation = CountingActivation {
            update: Some(tree_with_button_children(0)),
            calls: 0,
        };
        assert_eq!(adapter.accessibility_element_count(&mut activation), 0);
    }

    #[test]
    fn element_at_index_returns_nth_child_id() {
        let mut adapter = make_adapter();
        let mut activation = CountingActivation {
            update: Some(tree_with_button_children(3)),
            calls: 0,
        };
        // Force activation, then query without re-firing the handler.
        adapter.ensure_initialized(&mut activation);

        assert_eq!(
            adapter.accessibility_element_at_index(0, &mut activation),
            Some(LocalNodeId(2)),
        );
        assert_eq!(
            adapter.accessibility_element_at_index(1, &mut activation),
            Some(LocalNodeId(3)),
        );
        assert_eq!(
            adapter.accessibility_element_at_index(2, &mut activation),
            Some(LocalNodeId(4)),
        );
    }

    #[test]
    fn element_at_index_returns_none_out_of_range() {
        let mut adapter = make_adapter();
        let mut activation = CountingActivation {
            update: Some(tree_with_button_children(2)),
            calls: 0,
        };
        adapter.ensure_initialized(&mut activation);

        assert_eq!(
            adapter.accessibility_element_at_index(2, &mut activation),
            None,
        );
        assert_eq!(
            adapter.accessibility_element_at_index(-1, &mut activation),
            None,
        );
    }

    #[test]
    fn index_of_element_roundtrips_with_at_index() {
        let mut adapter = make_adapter();
        let mut activation = CountingActivation {
            update: Some(tree_with_button_children(3)),
            calls: 0,
        };
        adapter.ensure_initialized(&mut activation);

        assert_eq!(
            adapter.index_of_accessibility_element(LocalNodeId(2), &mut activation),
            Some(0),
        );
        assert_eq!(
            adapter.index_of_accessibility_element(LocalNodeId(4), &mut activation),
            Some(2),
        );
    }

    #[test]
    fn index_of_unknown_element_returns_none() {
        let mut adapter = make_adapter();
        let mut activation = CountingActivation {
            update: Some(tree_with_button_children(2)),
            calls: 0,
        };
        adapter.ensure_initialized(&mut activation);

        assert_eq!(
            adapter.index_of_accessibility_element(LocalNodeId(999), &mut activation),
            None,
        );
        // The synthetic root is not itself a filtered child.
        assert_eq!(
            adapter.index_of_accessibility_element(LocalNodeId(1), &mut activation),
            None,
        );
    }

    #[test]
    fn active_update_applies_diff_without_panic() {
        let mut adapter = make_adapter();
        let mut activation = CountingActivation {
            update: Some(full_tree(LocalNodeId(1))),
            calls: 0,
        };
        adapter.ensure_initialized(&mut activation);
        assert!(adapter.debug_is_active());

        // Diff: tree=None, no nodes — exercising the Layer-2 lesson
        // that a no-op diff after activation must not crash.
        let diff = TreeUpdate {
            nodes: vec![],
            tree: None,
            tree_id: TreeId::ROOT,
            focus: LocalNodeId(1),
        };
        adapter.update_if_active(|| diff);
        assert!(adapter.debug_is_active());
    }
}
