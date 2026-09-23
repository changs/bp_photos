//! Crop rectangle geometry, in normalised image coordinates (`[x0, y0, x1, y1]`, 0..1).

use crate::gpu::Crop;

/// Smallest crop, as a fraction of the image.
const MIN: f32 = 0.02;

/// Aspect ratio presets shown in the crop toolbar: (label, width, height). `0:0` = free, `1:0` = original.
pub const RATIOS: [(&str, u32, u32); 6] =
    [("Free", 0, 0), ("Original", 1, 0), ("1:1", 1, 1), ("4:5", 4, 5), ("2:3", 2, 3), ("16:9", 16, 9)];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Handle {
    Move,
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Handle {
    /// Which edges a handle moves: -1 = left/top, 1 = right/bottom, 0 = neither.
    fn edges(self) -> (i8, i8) {
        match self {
            Handle::Move => (0, 0),
            Handle::N => (0, -1),
            Handle::S => (0, 1),
            Handle::E => (1, 0),
            Handle::W => (-1, 0),
            Handle::NE => (1, -1),
            Handle::NW => (-1, -1),
            Handle::SE => (1, 1),
            Handle::SW => (-1, 1),
        }
    }
}

/// Pixel aspect ratio (w/h) for a `RATIOS` entry, oriented like the image unless `flip`.
pub fn pixel_ratio(index: usize, flip: bool, image: (u32, u32)) -> Option<f32> {
    let (_, a, b) = RATIOS[index];
    let (iw, ih) = (image.0 as f32, image.1 as f32);
    let r = match (a, b) {
        (0, 0) => return None,
        (_, 0) => iw / ih,
        _ => {
            let r = a as f32 / b as f32;
            // Match the photo's orientation by default: 16:9 on a portrait photo means 9:16.
            if a != b && ((iw >= ih) != (r >= 1.0)) { 1.0 / r } else { r }
        }
    };
    Some(if flip { 1.0 / r } else { r })
}

/// Converts a pixel aspect ratio to the ratio of normalised width to normalised height.
pub fn normalised_ratio(pixel_ratio: f32, image: (u32, u32)) -> f32 {
    pixel_ratio * image.1 as f32 / image.0 as f32
}

/// Largest crop with ratio `r` (normalised w/h) that fits the image, centred on `around`'s centre.
pub fn fit_ratio(around: Crop, r: f32) -> Crop {
    let (w, h) = if r >= 1.0 { (1.0, 1.0 / r) } else { (r, 1.0) };
    let cx = ((around[0] + around[2]) / 2.0).clamp(w / 2.0, 1.0 - w / 2.0);
    let cy = ((around[1] + around[3]) / 2.0).clamp(h / 2.0, 1.0 - h / 2.0);
    [cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0]
}

/// Applies a drag of `d` (normalised) on `handle` to the crop as it was when the drag began.
pub fn drag(start: Crop, handle: Handle, d: [f32; 2], ratio: Option<f32>) -> Crop {
    let [x0, y0, x1, y1] = start;
    if handle == Handle::Move {
        let dx = d[0].clamp(-x0, 1.0 - x1);
        let dy = d[1].clamp(-y0, 1.0 - y1);
        return [x0 + dx, y0 + dy, x1 + dx, y1 + dy];
    }
    let (mx, my) = handle.edges();
    let Some(r) = ratio else {
        let mut c = start;
        match mx {
            -1 => c[0] = (x0 + d[0]).clamp(0.0, x1 - MIN),
            1 => c[2] = (x1 + d[0]).clamp(x0 + MIN, 1.0),
            _ => {}
        }
        match my {
            -1 => c[1] = (y0 + d[1]).clamp(0.0, y1 - MIN),
            1 => c[3] = (y1 + d[1]).clamp(y0 + MIN, 1.0),
            _ => {}
        }
        return c;
    };

    // Aspect-locked: the opposite edge (or corner) stays put.
    let (ax, ay) = (if mx > 0 { x0 } else { x1 }, if my > 0 { y0 } else { y1 });
    let max_w = if mx > 0 { 1.0 - ax } else if mx < 0 { ax } else { 1.0 };
    let max_h = if my > 0 { 1.0 - ay } else if my < 0 { ay } else { 1.0 };
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (mut w, mut h);
    if mx != 0 && my != 0 {
        let px = if mx > 0 { x1 } else { x0 } + d[0];
        let py = if my > 0 { y1 } else { y0 } + d[1];
        w = ((px - ax) * mx as f32).max(MIN);
        h = ((py - ay) * my as f32).max(MIN);
        // Grow to enclose the pointer, then shrink to fit the image.
        if w / h > r { h = w / r } else { w = h * r }
    } else if mx != 0 {
        let px = if mx > 0 { x1 } else { x0 } + d[0];
        w = ((px - ax) * mx as f32).max(MIN);
        h = w / r;
    } else {
        let py = if my > 0 { y1 } else { y0 } + d[1];
        h = ((py - ay) * my as f32).max(MIN);
        w = h * r;
    }
    // Edge drags keep the other axis centred, so it may extend both ways.
    let max_w = if mx == 0 { 2.0 * cx.min(1.0 - cx) } else { max_w };
    let max_h = if my == 0 { 2.0 * cy.min(1.0 - cy) } else { max_h };
    if w > max_w {
        w = max_w;
        h = w / r;
    }
    if h > max_h {
        h = max_h;
        w = h * r;
    }
    let (nx0, nx1) = match mx {
        1 => (ax, ax + w),
        -1 => (ax - w, ax),
        _ => (cx - w / 2.0, cx + w / 2.0),
    };
    let (ny0, ny1) = match my {
        1 => (ay, ay + h),
        -1 => (ay - h, ay),
        _ => (cy - h / 2.0, cy + h / 2.0),
    };
    [nx0, ny0, nx1, ny1]
}

/// Which handle is under `p`, given the crop's on-screen rectangle (all in points).
pub fn hit_test(crop: [f32; 4], p: [f32; 2], margin: f32) -> Option<Handle> {
    let [l, t, r, b] = crop;
    let near = |a: f32, v: f32| (a - v).abs() <= margin;
    let in_x = p[0] >= l - margin && p[0] <= r + margin;
    let in_y = p[1] >= t - margin && p[1] <= b + margin;
    let (nl, nr, nt, nb) = (near(p[0], l) && in_y, near(p[0], r) && in_y, near(p[1], t) && in_x, near(p[1], b) && in_x);
    Some(match (nl, nr, nt, nb) {
        (true, _, true, _) => Handle::NW,
        (_, true, true, _) => Handle::NE,
        (true, _, _, true) => Handle::SW,
        (_, true, _, true) => Handle::SE,
        (true, ..) => Handle::W,
        (_, true, ..) => Handle::E,
        (_, _, true, _) => Handle::N,
        (.., true) => Handle::S,
        _ if p[0] > l && p[0] < r && p[1] > t && p[1] < b => Handle::Move,
        _ => return None,
    })
}

/// Crop size in source pixels.
pub fn pixel_size(crop: Crop, image: (u32, u32)) -> (u32, u32) {
    let w = ((crop[2] - crop[0]) * image.0 as f32).round() as u32;
    let h = ((crop[3] - crop[1]) * image.1 as f32).round() as u32;
    (w.max(1), h.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::FULL_CROP;

    fn close(a: Crop, b: Crop) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn free_corner_drag_moves_two_edges() {
        let c = drag(FULL_CROP, Handle::SE, [-0.25, -0.5], None);
        assert!(close(c, [0.0, 0.0, 0.75, 0.5]));
    }

    #[test]
    fn move_stays_inside_image() {
        let c = drag([0.25, 0.25, 0.75, 0.75], Handle::Move, [0.5, -0.1], None);
        assert!(close(c, [0.5, 0.15, 1.0, 0.65]));
    }

    #[test]
    fn locked_corner_keeps_ratio_and_bounds() {
        let c = drag([0.0, 0.0, 0.5, 0.5], Handle::SE, [0.9, 0.1], Some(1.0));
        assert!(close(c, [0.0, 0.0, 1.0, 1.0]));
        let (w, h) = (c[2] - c[0], c[3] - c[1]);
        assert!((w / h - 1.0).abs() < 1e-4);
    }

    #[test]
    fn locked_edge_grows_centred() {
        let c = drag([0.25, 0.25, 0.5, 0.75], Handle::E, [0.25, 0.0], Some(0.5));
        assert!(close(c, [0.25, 0.0, 0.75, 1.0]));
    }

    #[test]
    fn fit_ratio_is_largest_centred() {
        assert!(close(fit_ratio(FULL_CROP, 2.0), [0.0, 0.25, 1.0, 0.75]));
        assert!(close(fit_ratio([0.0, 0.0, 0.2, 0.2], 0.5), [0.0, 0.0, 0.5, 1.0]));
    }

    #[test]
    fn ratio_follows_photo_orientation() {
        let r = pixel_ratio(5, false, (3000, 4000)).unwrap();
        assert!((r - 9.0 / 16.0).abs() < 1e-5);
        let r = pixel_ratio(5, true, (3000, 4000)).unwrap();
        assert!((r - 16.0 / 9.0).abs() < 1e-5);
    }

    #[test]
    fn hit_test_finds_handles() {
        let c = [100.0, 100.0, 300.0, 200.0];
        assert_eq!(hit_test(c, [102.0, 98.0], 8.0), Some(Handle::NW));
        assert_eq!(hit_test(c, [200.0, 201.0], 8.0), Some(Handle::S));
        assert_eq!(hit_test(c, [200.0, 150.0], 8.0), Some(Handle::Move));
        assert_eq!(hit_test(c, [50.0, 50.0], 8.0), None);
    }
}
