//! `UIAccessibilityElement` subclass that bridges UIAccessibility
//! selectors to a node in an [`accesskit_consumer::Tree`].
//!
//! The instance owns a [`Weak<Context>`] back-pointer and an
//! [`accesskit_consumer::NodeId`]. Each AT query upgrades the weak
//! reference, borrows the tree, locates the node, and returns the
//! property the AT requested. If the context has been dropped (the
//! adapter was torn down) every query returns a benign default — the
//! AT will simply see the element disappear on its next pass.
//!
//! Mirror of `accesskit_macos::node::PlatformNode`, scoped down to the
//! minimum surface a `Role::Button` needs for VoiceOver to announce
//! and activate it: label, traits, frame, activate. Other roles will
//! pick up the same defaults until they're handled explicitly in a
//! later sub-commit.

#![allow(non_upper_case_globals)]

use crate::context::Context;
use accesskit::{Action, ActionRequest, Role};
use accesskit_consumer::{common_filter, Node, NodeId, Tree};
use objc2::runtime::AnyObject;
use objc2::{
    declare_class, msg_send_id, mutability::MainThreadOnly, rc::Id, ClassType, DeclaredClass,
};
use objc2_foundation::MainThreadMarker;
use objc2_foundation::{CGPoint, CGRect, CGSize, NSString};
use objc2_ui_kit::{
    UIAccessibilityElement, UIAccessibilityTraitButton, UIAccessibilityTraitHeader,
    UIAccessibilityTraitImage, UIAccessibilityTraitLink, UIAccessibilityTraitNone,
    UIAccessibilityTraitNotEnabled, UIAccessibilityTraitSelected, UIAccessibilityTraitStaticText,
    UIAccessibilityTraits,
};
use std::rc::{Rc, Weak};

pub(crate) struct PlatformNodeIvars {
    context: Weak<Context>,
    node_id: NodeId,
}

declare_class!(
    #[derive(Debug)]
    pub(crate) struct PlatformNode;

    unsafe impl ClassType for PlatformNode {
        #[inherits(objc2_foundation::NSObject)]
        type Super = UIAccessibilityElement;
        type Mutability = MainThreadOnly;
        const NAME: &'static str = "AccessKitNode";
    }

    impl DeclaredClass for PlatformNode {
        type Ivars = PlatformNodeIvars;
    }

    unsafe impl PlatformNode {
        #[method_id(accessibilityLabel)]
        fn label(&self) -> Option<Id<NSString>> {
            self.resolve(|node: &Node| node.label().map(|s| NSString::from_str(&s)))
                .flatten()
        }

        #[method(accessibilityTraits)]
        fn traits(&self) -> UIAccessibilityTraits {
            self.resolve(|node: &Node| traits_for(node))
                .unwrap_or_else(|| unsafe { UIAccessibilityTraitNone })
        }

        #[method(accessibilityFrame)]
        fn frame(&self) -> CGRect {
            // First-cut: use AccessKit's bounding box directly. The
            // gpui paint layer publishes nodes in screen-equivalent
            // coordinates for the spike (single window at origin).
            // Real coordinate conversion (view → screen, retina
            // scaling) happens in sub-commit 5 once we can observe
            // VoiceOver's focus rect on a real Simulator.
            self.resolve(|node: &Node| {
                node.bounding_box().map(|r| {
                    CGRect::new(
                        CGPoint::new(r.x0, r.y0),
                        CGSize::new(r.x1 - r.x0, r.y1 - r.y0),
                    )
                })
            })
            .flatten()
            .unwrap_or(CGRect::ZERO)
        }

        #[method(accessibilityActivate)]
        fn activate(&self) -> bool {
            self.resolve_with_context(|node: &Node, _: &Tree, context: &Rc<Context>| {
                if !node.supports_action(Action::Click, &common_filter) {
                    return false;
                }
                let (local_id, tree_id) = node.locate();
                context.action_handler.do_action(ActionRequest {
                    action: Action::Click,
                    target_tree: tree_id,
                    target_node: local_id,
                    data: None,
                });
                true
            })
            .unwrap_or(false)
        }
    }
);

impl PlatformNode {
    pub(crate) fn new(context: Weak<Context>, node_id: NodeId, mtm: MainThreadMarker) -> Id<Self> {
        // iOS 26 rejects bare `[super init]` on UIAccessibilityElement
        // with `NSInvalidArgumentException: Use initWithAccessibilityContainer:`.
        // Resolve the host UIView from the context (kept alive by the
        // window → vc → view retain chain rooted at IosWindow.window's
        // Retained) BEFORE alloc — once `set_ivars` returns a
        // `PartialInit`, `ivars()` isn't accessible until init returns.
        let container = context.upgrade().and_then(|ctx| ctx.view.load());
        let container_ptr = container
            .as_ref()
            .map(|view| Id::as_ptr(view) as *mut AnyObject)
            .unwrap_or(std::ptr::null_mut());

        let this = mtm
            .alloc::<Self>()
            .set_ivars(PlatformNodeIvars { context, node_id });

        unsafe {
            if container_ptr.is_null() {
                // Context is gone (e.g. adapter outlived its host) — fall
                // back to bare init. iOS 26+ raises an exception here
                // too, so this path is essentially "AT will crash" but
                // we can't do better without a container.
                msg_send_id![super(this), init]
            } else {
                let container_ref: &AnyObject = &*container_ptr;
                msg_send_id![super(this), initWithAccessibilityContainer: container_ref]
            }
        }
    }

    fn resolve_with_context<F, T>(&self, f: F) -> Option<T>
    where
        F: FnOnce(&Node, &Tree, &Rc<Context>) -> T,
    {
        let context = self.ivars().context.upgrade()?;
        let tree = context.tree.borrow();
        let state = tree.state();
        let node = state.node_by_id(self.ivars().node_id)?;
        Some(f(&node, &tree, &context))
    }

    fn resolve<F, T>(&self, f: F) -> Option<T>
    where
        F: FnOnce(&Node) -> T,
    {
        self.resolve_with_context(|node, _, _| f(node))
    }
}

fn traits_for(node: &Node) -> UIAccessibilityTraits {
    // Each `UIAccessibilityTraitFoo` is an `extern static` — reading
    // it is `unsafe` because it crosses the FFI boundary, but the
    // values themselves are stable UIKit constants.
    let mut traits = unsafe { UIAccessibilityTraitNone };
    match node.role() {
        Role::Button | Role::DefaultButton | Role::MenuItem | Role::DisclosureTriangle => {
            traits |= unsafe { UIAccessibilityTraitButton };
        }
        Role::Link | Role::DocBackLink | Role::DocBiblioRef => {
            traits |= unsafe { UIAccessibilityTraitLink };
        }
        Role::Heading => {
            traits |= unsafe { UIAccessibilityTraitHeader };
        }
        Role::Image => {
            traits |= unsafe { UIAccessibilityTraitImage };
        }
        Role::Label | Role::TitleBar => {
            traits |= unsafe { UIAccessibilityTraitStaticText };
        }
        _ => {}
    }
    if node.is_disabled() {
        traits |= unsafe { UIAccessibilityTraitNotEnabled };
    }
    if node.is_selected() == Some(true) {
        traits |= unsafe { UIAccessibilityTraitSelected };
    }
    traits
}
