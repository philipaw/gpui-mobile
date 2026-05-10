use accesskit::{ActionHandler, ActionRequest};
use accesskit_consumer::Tree;
use std::cell::RefCell;
use std::fmt::Debug;
use std::rc::Rc;

#[cfg(target_os = "ios")]
use crate::platform_node::PlatformNode;
#[cfg(target_os = "ios")]
use accesskit_consumer::NodeId;
#[cfg(target_os = "ios")]
use hashbrown::HashMap;
#[cfg(target_os = "ios")]
use objc2::rc::{Id, WeakId};
#[cfg(target_os = "ios")]
use objc2_foundation::MainThreadMarker;
#[cfg(target_os = "ios")]
use objc2_ui_kit::UIView;

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
    #[cfg(target_os = "ios")]
    #[allow(dead_code)]
    pub(crate) view: WeakId<UIView>,
    #[cfg(target_os = "ios")]
    pub(crate) mtm: MainThreadMarker,
    #[cfg(target_os = "ios")]
    pub(crate) platform_nodes: RefCell<HashMap<NodeId, Id<PlatformNode>>>,
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
    #[cfg(not(target_os = "ios"))]
    pub(crate) fn new(
        _view: *mut std::ffi::c_void,
        tree: Tree,
        action_handler: Rc<dyn ActionHandlerNoMut>,
    ) -> Rc<Self> {
        Rc::new(Self {
            tree: RefCell::new(tree),
            action_handler,
        })
    }

    #[cfg(target_os = "ios")]
    pub(crate) fn new(
        view: *mut std::ffi::c_void,
        tree: Tree,
        action_handler: Rc<dyn ActionHandlerNoMut>,
    ) -> Rc<Self> {
        // Safety contract documented on `Adapter::new`: view must be a
        // valid, retained-for-the-adapter's-lifetime UIView pointer,
        // and the call must be on the main thread.
        let mtm = MainThreadMarker::new().expect("Context::new must be called on the main thread");
        let view = unsafe { Id::retain(view as *mut UIView) }.expect("view must be non-null");
        let view = WeakId::from_id(&view);
        Rc::new(Self {
            tree: RefCell::new(tree),
            action_handler,
            view,
            mtm,
            platform_nodes: RefCell::new(HashMap::new()),
        })
    }

    #[cfg(target_os = "ios")]
    pub(crate) fn get_or_create_platform_node(self: &Rc<Self>, id: NodeId) -> Id<PlatformNode> {
        let mut platform_nodes = self.platform_nodes.borrow_mut();
        if let Some(result) = platform_nodes.get(&id) {
            return result.clone();
        }
        let result = PlatformNode::new(Rc::downgrade(self), id, self.mtm);
        platform_nodes.insert(id, result.clone());
        result
    }
}
