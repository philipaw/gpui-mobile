//! Dynamic UIView subclassing that routes UIAccessibility selectors
//! through an [`Adapter`].
//!
//! Mirror of `accesskit_macos::subclass`, with the macOS NSView /
//! NSAccessibility selector set replaced by iOS UIView /
//! UIAccessibilityContainer selectors:
//!
//! - `accessibilityElementCount`
//! - `accessibilityElementAtIndex:`
//! - `indexOfAccessibilityElement:`
//! - `isAccessibilityElement` (forced to `NO` so UIKit treats the view
//!   as a container of accesskit-managed elements rather than a leaf)
//! - `superclass` (returns the original class so super-method lookups
//!   still resolve correctly — same workaround pattern macOS uses for
//!   view classes that ask for `[self superclass]`)
//!
//! Per-view-class subclasses are cached in a static `Mutex<Vec>` so a
//! second `SubclassingAdapter` on a view of the same parent class
//! reuses the existing subclass instead of registering a new one.

use crate::Adapter;
use accesskit::{ActionHandler, ActivationHandler, TreeUpdate};
use objc2::{
    declare::ClassBuilder,
    declare_class,
    ffi::{
        objc_getAssociatedObject, objc_setAssociatedObject, object_setClass,
        OBJC_ASSOCIATION_RETAIN_NONATOMIC,
    },
    msg_send_id,
    mutability::InteriorMutable,
    rc::Id,
    runtime::{AnyClass, Bool, Sel},
    sel, ClassType, DeclaredClass,
};
use objc2_foundation::NSObject;
use objc2_ui_kit::UIView;
use std::{cell::RefCell, ffi::c_void, sync::Mutex};

static SUBCLASSES: Mutex<Vec<(&'static AnyClass, &'static AnyClass)>> = Mutex::new(Vec::new());

static ASSOCIATED_OBJECT_KEY: u8 = 0;

fn associated_object_key() -> *const c_void {
    (&ASSOCIATED_OBJECT_KEY as *const u8).cast()
}

struct AssociatedObjectState {
    adapter: Adapter,
    activation_handler: Box<dyn ActivationHandler>,
}

struct AssociatedObjectIvars {
    state: RefCell<AssociatedObjectState>,
    prev_class: &'static AnyClass,
}

declare_class!(
    struct AssociatedObject;

    unsafe impl ClassType for AssociatedObject {
        type Super = NSObject;
        type Mutability = InteriorMutable;
        const NAME: &'static str = "AccessKitSubclassAssociatedObject";
    }

    impl DeclaredClass for AssociatedObject {
        type Ivars = AssociatedObjectIvars;
    }
);

impl AssociatedObject {
    fn new(
        adapter: Adapter,
        activation_handler: impl 'static + ActivationHandler,
        prev_class: &'static AnyClass,
    ) -> Id<Self> {
        let state = RefCell::new(AssociatedObjectState {
            adapter,
            activation_handler: Box::new(activation_handler),
        });
        let this = Self::alloc().set_ivars(AssociatedObjectIvars { state, prev_class });
        unsafe { msg_send_id![super(this), init] }
    }
}

fn associated_object(view: &UIView) -> &AssociatedObject {
    unsafe {
        (objc_getAssociatedObject(view as *const UIView as *const _, associated_object_key())
            as *const AssociatedObject)
            .as_ref()
    }
    .unwrap()
}

unsafe extern "C" fn superclass(this: &UIView, _cmd: Sel) -> Option<&AnyClass> {
    let associated = associated_object(this);
    associated.ivars().prev_class.superclass()
}

unsafe extern "C" fn is_accessibility_element(_this: &UIView, _cmd: Sel) -> Bool {
    // Force NO: this view itself isn't an a11y leaf; its contents
    // are exposed via the container-protocol selectors below.
    Bool::NO
}

unsafe extern "C" fn accessibility_element_count(this: &UIView, _cmd: Sel) -> isize {
    let associated = associated_object(this);
    let mut state = associated.ivars().state.borrow_mut();
    let state_mut = &mut *state;
    state_mut
        .adapter
        .accessibility_element_count_objc(&mut *state_mut.activation_handler)
}

unsafe extern "C" fn accessibility_element_at_index(
    this: &UIView,
    _cmd: Sel,
    index: isize,
) -> *mut NSObject {
    let associated = associated_object(this);
    let mut state = associated.ivars().state.borrow_mut();
    let state_mut = &mut *state;
    state_mut
        .adapter
        .accessibility_element_at_index_objc(index, &mut *state_mut.activation_handler)
}

unsafe extern "C" fn index_of_accessibility_element(
    this: &UIView,
    _cmd: Sel,
    element: *mut NSObject,
) -> isize {
    let associated = associated_object(this);
    let mut state = associated.ivars().state.borrow_mut();
    let state_mut = &mut *state;
    state_mut
        .adapter
        .index_of_accessibility_element_objc(element, &mut *state_mut.activation_handler)
}

/// Uses dynamic Objective-C subclassing to implement the iOS
/// UIAccessibilityContainer informal protocol on an existing UIView
/// without requiring the host to override the selectors at compile
/// time.
pub struct SubclassingAdapter {
    view: Id<UIView>,
    associated: Id<AssociatedObject>,
}

impl SubclassingAdapter {
    /// Create an adapter that dynamically subclasses the specified view.
    /// This must be done before the view is shown or focused for the
    /// first time — UIKit caches the accessibility tree on first
    /// query, and a class swap after that point may not be picked up.
    ///
    /// The action handler will always be called on the main thread.
    ///
    /// # Safety
    ///
    /// `view` must be a valid, unreleased pointer to a `UIView`.
    pub unsafe fn new(
        view: *mut c_void,
        activation_handler: impl 'static + ActivationHandler,
        action_handler: impl 'static + ActionHandler,
    ) -> Self {
        let view = view as *mut UIView;
        let retained_view = unsafe { Id::retain(view) }.unwrap();
        Self::new_internal(retained_view, activation_handler, action_handler)
    }

    fn new_internal(
        retained_view: Id<UIView>,
        activation_handler: impl 'static + ActivationHandler,
        action_handler: impl 'static + ActionHandler,
    ) -> Self {
        let view = Id::as_ptr(&retained_view) as *mut UIView;
        if !unsafe {
            objc_getAssociatedObject(view as *const UIView as *const _, associated_object_key())
        }
        .is_null()
        {
            panic!("subclassing adapter already instantiated on view {view:?}");
        }
        let adapter = unsafe { Adapter::new(view as *mut c_void, false, action_handler) };
        // SAFETY: We know the class will live as long as the instance,
        // and we only use this reference while the instance is alive.
        let prev_class = unsafe { &*((*view).class() as *const AnyClass) };
        let associated = AssociatedObject::new(adapter, activation_handler, prev_class);
        unsafe {
            objc_setAssociatedObject(
                view as *mut _,
                associated_object_key(),
                Id::as_ptr(&associated) as *mut _,
                OBJC_ASSOCIATION_RETAIN_NONATOMIC,
            )
        };
        let mut subclasses = SUBCLASSES.lock().unwrap();
        let subclass = match subclasses.iter().find(|entry| entry.0 == prev_class) {
            Some(entry) => entry.1,
            None => {
                let name = format!("AccessKitSubclassOf{}", prev_class.name());
                let mut builder = ClassBuilder::new(&name, prev_class).unwrap();
                unsafe {
                    builder.add_method(
                        sel!(superclass),
                        superclass as unsafe extern "C" fn(_, _) -> _,
                    );
                    builder.add_method(
                        sel!(isAccessibilityElement),
                        is_accessibility_element as unsafe extern "C" fn(_, _) -> _,
                    );
                    builder.add_method(
                        sel!(accessibilityElementCount),
                        accessibility_element_count as unsafe extern "C" fn(_, _) -> _,
                    );
                    builder.add_method(
                        sel!(accessibilityElementAtIndex:),
                        accessibility_element_at_index as unsafe extern "C" fn(_, _, _) -> _,
                    );
                    builder.add_method(
                        sel!(indexOfAccessibilityElement:),
                        index_of_accessibility_element as unsafe extern "C" fn(_, _, _) -> _,
                    );
                }
                let class = builder.register();
                subclasses.push((prev_class, class));
                class
            }
        };
        // SAFETY: Changing the view's class is only safe because the
        // subclass adds no instance variables — it stores its state
        // on an associated object.
        unsafe { object_setClass(view as *mut _, (subclass as *const AnyClass).cast()) };
        Self {
            view: retained_view,
            associated,
        }
    }

    /// Apply a [`TreeUpdate`] iff the adapter has been activated by
    /// an AT. See [`Adapter::update_if_active`] for the contract.
    pub fn update_if_active(&mut self, update_factory: impl FnOnce() -> TreeUpdate) {
        let mut state = self.associated.ivars().state.borrow_mut();
        state.adapter.update_if_active(update_factory)
    }

    /// Notify the adapter that the host view's
    /// `becomeFirstResponder` / `resignFirstResponder` state changed.
    pub fn update_view_focus_state(&mut self, is_focused: bool) {
        let mut state = self.associated.ivars().state.borrow_mut();
        state.adapter.update_view_focus_state(is_focused)
    }
}

impl Drop for SubclassingAdapter {
    fn drop(&mut self) {
        let prev_class = self.associated.ivars().prev_class;
        let view = Id::as_ptr(&self.view) as *mut UIView;
        unsafe { object_setClass(view as *mut _, (prev_class as *const AnyClass).cast()) };
        unsafe {
            objc_setAssociatedObject(
                view as *mut _,
                associated_object_key(),
                std::ptr::null_mut(),
                OBJC_ASSOCIATION_RETAIN_NONATOMIC,
            )
        };
    }
}
