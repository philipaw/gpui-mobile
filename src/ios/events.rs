//! iOS event handling - converting UIKit events to GPUI's event types.
//!
//! iOS uses touch-based input rather than mouse input, so we need to map
//! touch gestures to appropriate GPUI events:
//! - Single tap → MouseDown + MouseUp (left button)
//! - Pan gesture → ScrollWheel events
//! - Touch move → MouseMove events

use gpui::{px, Pixels, Point, TouchPhase};
use objc2::msg_send;
use objc2::runtime::AnyObject;

use super::cg_types::ObjcCGPoint;

/// Touch phase from UIKit
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum UITouchPhase {
    Began = 0,
    Moved = 1,
    Stationary = 2,
    Ended = 3,
    Cancelled = 4,
}

impl From<i64> for UITouchPhase {
    fn from(value: i64) -> Self {
        match value {
            0 => UITouchPhase::Began,
            1 => UITouchPhase::Moved,
            2 => UITouchPhase::Stationary,
            3 => UITouchPhase::Ended,
            4 => UITouchPhase::Cancelled,
            _ => UITouchPhase::Cancelled,
        }
    }
}

impl From<UITouchPhase> for TouchPhase {
    fn from(phase: UITouchPhase) -> Self {
        match phase {
            UITouchPhase::Began => TouchPhase::Started,
            UITouchPhase::Moved => TouchPhase::Moved,
            UITouchPhase::Stationary => TouchPhase::Moved,
            UITouchPhase::Ended => TouchPhase::Ended,
            UITouchPhase::Cancelled => TouchPhase::Ended,
        }
    }
}

/// Convert a UITouch to a mouse position
pub fn touch_location_in_view(touch: *mut AnyObject, view: *mut AnyObject) -> Point<Pixels> {
    unsafe {
        let location: ObjcCGPoint = msg_send![touch, locationInView: view];
        Point::new(px(location.x as f32), px(location.y as f32))
    }
}

/// Get the touch phase from a UITouch
pub fn touch_phase(touch: *mut AnyObject) -> UITouchPhase {
    unsafe {
        let phase: i64 = msg_send![touch, phase];
        UITouchPhase::from(phase)
    }
}

/// Get the number of taps for a touch (for detecting double-tap, etc.)
pub fn touch_tap_count(touch: *mut AnyObject) -> u32 {
    unsafe {
        let count: i64 = msg_send![touch, tapCount];
        count as u32
    }
}

/// `UITouch.force`. Reports `0.0` for fingers on Haptic-Touch hardware
/// (iPhone 11+); 3D-Touch devices report a value in `0..=max_force`.
/// Apple Pencil reports its own non-zero scale.
pub fn touch_force(touch: *mut AnyObject) -> f32 {
    unsafe {
        let f: f64 = msg_send![touch, force];
        f as f32
    }
}

/// `UITouch.altitudeAngle`. Radians from the surface to the stylus
/// (`π/2` = perpendicular / "tip down"). Fingers always report `π/2`.
pub fn touch_altitude(touch: *mut AnyObject) -> f32 {
    unsafe {
        let a: f64 = msg_send![touch, altitudeAngle];
        a as f32
    }
}

/// `UITouch.azimuthAngleInView:`. Radians around the surface normal in
/// the given view's coordinate system. `0` for fingers.
pub fn touch_azimuth(touch: *mut AnyObject, view: *mut AnyObject) -> f32 {
    unsafe {
        let a: f64 = msg_send![touch, azimuthAngleInView: view];
        a as f32
    }
}

/// `UITouch.type` raw value. UIKit's `UITouchType`: Direct=0,
/// Indirect=1, Pencil=2, IndirectPointer=3.
pub fn touch_type_raw(touch: *mut AnyObject) -> i64 {
    unsafe { msg_send![touch, type] }
}
