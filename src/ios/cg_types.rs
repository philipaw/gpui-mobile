//! Local wrappers for CoreGraphics geometry types with objc2::Encode impls.
//!
//! `core-graphics` types (CGRect, CGPoint, CGSize) do not implement
//! `objc2::encode::Encode` because the `core-graphics` crate predates
//! objc2's encoding system. We define `#[repr(C)]` newtype wrappers with the
//! same memory layout and provide the needed impls, then offer cheap
//! conversion helpers.

use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use objc2::encode::{Encode, Encoding, RefEncode};

// ── CGRect ────────────────────────────────────────────────────────────────────

/// objc2-encodable mirror of `core_graphics::geometry::CGRect`.
/// Layout: `{ origin: { x: f64, y: f64 }, size: { width: f64, height: f64 } }`
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjcCGRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

unsafe impl Encode for ObjcCGRect {
    const ENCODING: Encoding = Encoding::Struct(
        "CGRect",
        &[
            Encoding::Struct("CGPoint", &[Encoding::Double, Encoding::Double]),
            Encoding::Struct("CGSize", &[Encoding::Double, Encoding::Double]),
        ],
    );
}

unsafe impl RefEncode for ObjcCGRect {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

impl ObjcCGRect {
    #[allow(dead_code)]
    pub fn from_cg(r: CGRect) -> Self {
        Self {
            x: r.origin.x,
            y: r.origin.y,
            width: r.size.width,
            height: r.size.height,
        }
    }

    #[allow(dead_code)]
    pub fn to_cg(self) -> CGRect {
        CGRect {
            origin: CGPoint {
                x: self.x,
                y: self.y,
            },
            size: CGSize {
                width: self.width,
                height: self.height,
            },
        }
    }

    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

// ── CGAffineTransform ──────────────────────────────────────────────────────────

/// objc2-encodable mirror of `CGAffineTransform`.
///
/// Layout: six `CGFloat`s `{ a, b, c, d, tx, ty }` representing the
/// matrix `[ a b 0; c d 0; tx ty 1 ]`. Used to set a `UIView.transform`
/// (or `CALayer.affineTransform`) for in-place rotation about the view
/// center — the video-widget rotation path. `transform` rotates about
/// the view's `center` (default = bbox center), matching how
/// `paint::build_rotated_quad` rotates a `SolidRect` about its rect
/// center.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ObjcCGAffineTransform {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub tx: f64,
    pub ty: f64,
}

unsafe impl Encode for ObjcCGAffineTransform {
    const ENCODING: Encoding = Encoding::Struct(
        "CGAffineTransform",
        &[
            Encoding::Double,
            Encoding::Double,
            Encoding::Double,
            Encoding::Double,
            Encoding::Double,
            Encoding::Double,
        ],
    );
}

unsafe impl RefEncode for ObjcCGAffineTransform {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

impl ObjcCGAffineTransform {
    /// The identity transform — no rotation/scale/translation. Equivalent
    /// to `CGAffineTransformIdentity`; restoring this un-rotates a view.
    pub fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: 0.0,
            ty: 0.0,
        }
    }

    /// A pure rotation by `radians`, matching `CGAffineTransformMakeRotation`:
    /// `[ cos sin 0; -sin cos 0; 0 0 1 ]`. In UIKit's y-down view space this
    /// rotates the same direction as `paint::build_rotated_quad` /
    /// `paint::rotate_points_around_center` (both y-down screen rotations
    /// `(dx·cosθ − dy·sinθ, dx·sinθ + dy·cosθ)`), so a rotated video spins
    /// the same way as a `SolidRect`/image at the same `rot`.
    pub fn rotation(radians: f64) -> Self {
        let (s, c) = radians.sin_cos();
        Self {
            a: c,
            b: s,
            c: -s,
            d: c,
            tx: 0.0,
            ty: 0.0,
        }
    }
}

// ── CGPoint ───────────────────────────────────────────────────────────────────

/// objc2-encodable mirror of `core_graphics::geometry::CGPoint`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjcCGPoint {
    pub x: f64,
    pub y: f64,
}

unsafe impl Encode for ObjcCGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[Encoding::Double, Encoding::Double]);
}

unsafe impl RefEncode for ObjcCGPoint {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

impl ObjcCGPoint {
    #[allow(dead_code)]
    pub fn to_cg(self) -> CGPoint {
        CGPoint {
            x: self.x,
            y: self.y,
        }
    }
}

// ── CGSize ────────────────────────────────────────────────────────────────────

/// objc2-encodable mirror of `core_graphics::geometry::CGSize`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjcCGSize {
    pub width: f64,
    pub height: f64,
}

unsafe impl Encode for ObjcCGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[Encoding::Double, Encoding::Double]);
}

unsafe impl RefEncode for ObjcCGSize {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

impl ObjcCGSize {
    #[allow(dead_code)]
    pub fn to_cg(self) -> CGSize {
        CGSize {
            width: self.width,
            height: self.height,
        }
    }
}
