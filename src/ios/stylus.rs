//! Raw stylus / touch sample stream for canvas-style apps that need
//! per-sample pressure / tilt / Pencil-vs-finger discrimination.
//!
//! Gpui's primary input dispatch (`PlatformInput::MouseDown` / `MouseMove`
//! / `MouseUp` / `ScrollWheel`) collapses every UITouch into mouse-like
//! events via a tap-vs-scroll slop discriminator (see `window.rs`'s
//! `handle_touch`). That model is correct for UI widgets but loses the
//! data a drawing surface needs: Apple Pencil pressure / altitude /
//! azimuth, and the touch *kind* (finger taps vs Pencil strokes get
//! different gestures in a paint app).
//!
//! This module exposes a parallel stream of raw `StylusEvent`s as a
//! process-global drainable queue. The iOS touch handler pushes onto it
//! for every UITouch (alongside the existing gpui input emit); host apps
//! call `drain()` from their render loop. Same queue+drain pattern as
//! the canvas-ios `remote_commands` module — bound by what the user can
//! touch in one render frame (~6 events at 360 Hz Pencil sampling on a
//! 60 Hz screen).
//!
//! `push` is `pub` so host apps can L4-verdict the dispatch path
//! deterministically via a force-fire env var, without needing real
//! Pencil hardware on the simulator.

use std::sync::{Mutex, OnceLock};

use gpui::TouchPhase;

/// One UITouch sample. Position is in *logical* pixels in the window's
/// coordinate space — same units as `MouseDownEvent.position`. Caller
/// is responsible for mapping into widget-local coords.
///
/// Field values when the source is a finger (no Pencil):
/// - `force = 0.0` on Haptic-Touch devices (iPhone 11 and later); on
///   3D-Touch devices it's a value in `0..=max_force` reported by
///   UIKit. The Pencil reports its own non-zero force scale.
/// - `altitude = π/2` (perpendicular, i.e. "tip down") for fingers;
///   Pencil reports the real tilt.
/// - `azimuth = 0.0` for fingers; Pencil reports the real azimuth
///   relative to the receiving view's coordinate system.
#[derive(Debug, Clone, Copy)]
pub struct StylusEvent {
    pub position_x: f32,
    pub position_y: f32,
    pub phase: TouchPhase,
    pub force: f32,
    pub altitude: f32,
    pub azimuth: f32,
    pub kind: TouchKind,
}

/// `UITouchType` subset we care about. Apple's enum has `Direct` (0),
/// `Indirect` (1), `Pencil` (2), `IndirectPointer` (3); for a canvas
/// app the only useful distinction is finger vs Pencil — `Indirect*`
/// fold into `Finger` (they're trackpad-on-iPad style pointers, which
/// behave like fingers for drawing purposes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchKind {
    Finger,
    Pencil,
}

fn queue() -> &'static Mutex<Vec<StylusEvent>> {
    static Q: OnceLock<Mutex<Vec<StylusEvent>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(Vec::new()))
}

/// Drain pending stylus samples. Host apps call this per render frame
/// at the top of their input-handling pass. Returns owned Vec so the
/// caller can iterate without holding the mutex across processing.
pub fn drain() -> Vec<StylusEvent> {
    let mut q = queue().lock().expect("stylus queue poisoned");
    std::mem::take(&mut *q)
}

/// Soft cap on the unconsumed queue: drop the oldest sample when full.
/// Hosts that don't care about stylus events never `drain()` and
/// would otherwise see the queue grow unbounded under a long Pencil
/// session (~360 Hz × 40 B = ~14 KB/s). 4096 is ~11 s of Pencil at
/// full rate — plenty for a host that drains per render, harmless
/// for a host that ignores stylus entirely.
const QUEUE_CAP: usize = 4096;

/// Push a sample onto the queue. Called from `window.rs::handle_touch`
/// for every UITouch — `pub` so host apps can also drive synthetic
/// events through this same site (used by canvas-ios's
/// `DEBUG_FORCE_STROKE_AT_MS` L4 verdict driver to verify the
/// downstream recorder without real Pencil hardware).
pub fn push(ev: StylusEvent) {
    let mut q = queue().lock().expect("stylus queue poisoned");
    if q.len() >= QUEUE_CAP {
        q.remove(0);
    }
    q.push(ev);
}
