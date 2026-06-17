//! Two-finger rotation sample stream for canvas-style apps that want a
//! widget-rotate gesture alongside pinch-resize.
//!
//! Gpui's primary input dispatch carries pinch *scale* through
//! `PlatformInput::Pinch` (see `window.rs::handle_pinch`), but
//! `gpui::PinchEvent` has no rotation field — a two-finger twist is
//! simply not represented in gpui core's input enum. Rather than widen
//! that enum in the fork (and have to keep it in sync with upstream),
//! this module exposes a parallel, drainable stream of `RotationEvent`s
//! — exactly the same process-global queue+drain pattern as the
//! `stylus` side-channel (s81). A `UIRotationGestureRecognizer` on the
//! Metal view pushes one sample per state transition; host apps call
//! `drain()` from their render loop and accumulate the per-event delta
//! into whatever they're rotating.
//!
//! The recognizer is configured to recognize *simultaneously* with the
//! pinch recognizer, so a single two-finger gesture can drive scale (via
//! the pinch path) and rotation (via this side-channel) at once — the
//! "scale + rotate together" feel.
//!
//! `push` is `pub` so host apps can L4-verdict the dispatch path
//! deterministically via a force-fire env var, without needing a real
//! two-finger twist on the simulator.

use std::sync::{Mutex, OnceLock};

use gpui::TouchPhase;

/// One rotation-recognizer sample.
///
/// `delta` is the per-event change in radians (positive = counter-...
/// well, it matches `UIRotationGestureRecognizer.rotation`, which is
/// the cumulative rotation in radians since the gesture began, in the
/// view's coordinate system; we reset it to 0 after each callback so
/// `delta` is the incremental change — mirroring how `handle_pinch`
/// normalizes `scale` back to 1.0). Host apps add this directly to a
/// `Transform2D.rot`.
///
/// `position` is the gesture centroid in *logical* pixels in the
/// window's coordinate space — same units as `MouseDownEvent.position`
/// and `PinchEvent.position`. Provided for symmetry with the pinch
/// event; rotation itself is centroid-independent (a pure twist), so
/// most hosts will ignore it.
#[derive(Debug, Clone, Copy)]
pub struct RotationEvent {
    pub position_x: f32,
    pub position_y: f32,
    pub phase: TouchPhase,
    /// Incremental rotation since the previous sample, in radians.
    pub delta: f32,
}

fn queue() -> &'static Mutex<Vec<RotationEvent>> {
    static Q: OnceLock<Mutex<Vec<RotationEvent>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(Vec::new()))
}

/// Drain pending rotation samples. Host apps call this per render frame
/// at the top of their input-handling pass. Returns an owned Vec so the
/// caller can iterate without holding the mutex across processing.
pub fn drain() -> Vec<RotationEvent> {
    let mut q = queue().lock().expect("rotation queue poisoned");
    std::mem::take(&mut *q)
}

/// Soft cap on the unconsumed queue: drop the oldest sample when full.
/// Hosts that never `drain()` would otherwise see unbounded growth
/// under a long twist gesture. The recognizer fires at most once per
/// touch update (~60 Hz), so 1024 is many seconds of slack — plenty for
/// a host that drains per render, harmless for one that ignores
/// rotation entirely.
const QUEUE_CAP: usize = 1024;

/// Push a sample onto the queue. Called from
/// `window.rs::handle_rotation` for every recognizer state transition —
/// `pub` so host apps can also drive synthetic events through this same
/// site (used by a `DEBUG_FORCE_*`-style L4 verdict driver to verify
/// the downstream rotate handler without a real two-finger twist).
pub fn push(ev: RotationEvent) {
    let mut q = queue().lock().expect("rotation queue poisoned");
    if q.len() >= QUEUE_CAP {
        q.remove(0);
    }
    q.push(ev);
}
