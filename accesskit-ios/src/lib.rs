//! AccessKit ↔ UIAccessibility adapter for iOS.
//!
//! Mirror of `accesskit_macos`. Two ways to integrate:
//!
//! - [`Adapter`] is the manual integration path: the host wires the
//!   three iOS UIAccessibilityContainer selectors
//!   (`accessibilityElementCount`, `accessibilityElementAtIndex:`,
//!   `indexOfAccessibilityElement:`) by hand and delegates each into
//!   the adapter via `*_objc` shim methods. Useful when the host
//!   already owns the `UIView` subclass.
//!
//! - [`SubclassingAdapter`] (iOS-only) does the wiring automatically
//!   by dynamically subclassing the host `UIView` at construction
//!   time and routing the selectors itself. Use this when you don't
//!   control the view's class definition (e.g. integrating into an
//!   existing UIKit app).
//!
//! Both feed the adapter `accesskit::TreeUpdate`s via
//! `update_if_active`; the activation handler provides the initial
//! tree on first AT access; the action handler receives any
//! subsequent `accesskit::ActionRequest`s.

mod adapter;
mod context;
#[cfg(target_os = "ios")]
mod platform_node;
#[cfg(target_os = "ios")]
mod subclass;

pub use accesskit::{ActionHandler, ActionRequest, ActivationHandler, NodeId, Tree, TreeUpdate};
pub use adapter::Adapter;
#[cfg(target_os = "ios")]
pub use subclass::SubclassingAdapter;
