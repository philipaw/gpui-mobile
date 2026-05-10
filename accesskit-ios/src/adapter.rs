use crate::context::{ActionHandlerNoMut, ActionHandlerWrapper, Context};
use accesskit::{
    ActionHandler, ActionRequest, ActivationHandler, Node as NodeProvider, NodeId as LocalNodeId,
    Role, Tree as TreeData, TreeId, TreeUpdate,
};
use accesskit_consumer::{Node, Tree, TreeChangeHandler};
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
