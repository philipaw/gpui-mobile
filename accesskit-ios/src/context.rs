use accesskit::{ActionHandler, ActionRequest};
use accesskit_consumer::Tree;
use std::cell::RefCell;
use std::fmt::Debug;
use std::rc::Rc;

pub(crate) trait ActionHandlerNoMut {
    #[allow(dead_code)]
    fn do_action(&self, request: ActionRequest);
}

pub(crate) struct ActionHandlerWrapper<H: ActionHandler>(RefCell<H>);

impl<H: 'static + ActionHandler> ActionHandlerWrapper<H> {
    pub(crate) fn new(inner: H) -> Self {
        Self(RefCell::new(inner))
    }
}

impl<H: ActionHandler> ActionHandlerNoMut for ActionHandlerWrapper<H> {
    fn do_action(&self, request: ActionRequest) {
        self.0.borrow_mut().do_action(request)
    }
}

pub(crate) struct Context {
    pub(crate) tree: RefCell<Tree>,
    #[allow(dead_code)]
    pub(crate) action_handler: Rc<dyn ActionHandlerNoMut>,
}

impl Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("tree", &self.tree)
            .field("action_handler", &"ActionHandler")
            .finish()
    }
}

impl Context {
    pub(crate) fn new(tree: Tree, action_handler: Rc<dyn ActionHandlerNoMut>) -> Rc<Self> {
        Rc::new(Self {
            tree: RefCell::new(tree),
            action_handler,
        })
    }
}
