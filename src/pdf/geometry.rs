use hayro::kurbo::{Affine, Rect};

/// Rotate the cropped user space into an upright, y-up layout coordinate system.
pub fn layout_transform(crop: Rect, rotation: i32) -> Affine {
    match rotation.rem_euclid(360) {
        90 => Affine::new([0.0, -1.0, 1.0, 0.0, -crop.y0, crop.x1]),
        180 => Affine::new([-1.0, 0.0, 0.0, -1.0, crop.x1, crop.y1]),
        270 => Affine::new([0.0, 1.0, -1.0, 0.0, crop.y1, -crop.x0]),
        _ => Affine::IDENTITY,
    }
}
