//! Export sizes for common destinations, and how a crop maps onto them.

use std::path::{Path, PathBuf};

use crate::crop;
use crate::gpu::Crop;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Size {
    /// Full resolution of the (cropped) photo.
    Original,
    /// A fixed canvas, e.g. Instagram portrait 1080×1350.
    Exact(u32, u32),
    /// Longest edge at most this many pixels.
    LongEdge(u32),
}

pub struct SizePreset {
    pub label: &'static str,
    /// Short tag for file names and the command line.
    pub slug: &'static str,
    pub size: Size,
}

/// The last entry is the custom long-edge size; its value comes from the UI.
pub const SIZES: [SizePreset; 10] = [
    SizePreset { label: "Original size", slug: "original", size: Size::Original },
    SizePreset { label: "Instagram · Portrait 4:5 (1080×1350)", slug: "instagram-portrait", size: Size::Exact(1080, 1350) },
    SizePreset { label: "Instagram · Square (1080×1080)", slug: "instagram-square", size: Size::Exact(1080, 1080) },
    SizePreset { label: "Instagram · Landscape (1080×566)", slug: "instagram-landscape", size: Size::Exact(1080, 566) },
    SizePreset { label: "Instagram · Story / Reel 9:16 (1080×1920)", slug: "instagram-story", size: Size::Exact(1080, 1920) },
    SizePreset { label: "X · Post 16:9 (1600×900)", slug: "x-post", size: Size::Exact(1600, 900) },
    SizePreset { label: "X · Large (up to 4096 px)", slug: "x-large", size: Size::LongEdge(4096) },
    SizePreset { label: "Facebook (up to 2048 px)", slug: "facebook", size: Size::LongEdge(2048) },
    SizePreset { label: "Web (up to 1920 px)", slug: "web", size: Size::LongEdge(1920) },
    SizePreset { label: "Custom long edge…", slug: "custom", size: Size::LongEdge(0) },
];

pub const CUSTOM: usize = SIZES.len() - 1;

/// What to render: the crop to use, its full-resolution size, and the final (resized) size.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Plan {
    pub crop: Crop,
    pub render: (u32, u32),
    pub output: (u32, u32),
}

/// Largest crop with normalised ratio `r` (w/h) inside `crop`, centred.
fn fit_within(crop: Crop, r: f32) -> Crop {
    let (cw, ch) = (crop[2] - crop[0], crop[3] - crop[1]);
    let (w, h) = if cw / ch > r { (ch * r, ch) } else { (cw, cw / r) };
    let (cx, cy) = ((crop[0] + crop[2]) / 2.0, (crop[1] + crop[3]) / 2.0);
    [cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0]
}

fn scaled(size: (u32, u32), scale: f32) -> (u32, u32) {
    let s = scale.min(1.0); // never enlarge
    (((size.0 as f32 * s).round() as u32).max(1), ((size.1 as f32 * s).round() as u32).max(1))
}

/// Plans an export of `crop` from an image of `image` pixels. With `fill`, fixed-size
/// formats trim the crop to their aspect ratio; otherwise the photo is fitted inside them.
pub fn plan(size: Size, crop: Crop, image: (u32, u32), fill: bool) -> Plan {
    let px = crop::pixel_size(crop, image);
    match size {
        Size::Original | Size::LongEdge(0) => Plan { crop, render: px, output: px },
        Size::LongEdge(n) => Plan { crop, render: px, output: scaled(px, n as f32 / px.0.max(px.1) as f32) },
        Size::Exact(w, h) if fill => {
            let r = crop::normalised_ratio(w as f32 / h as f32, image);
            let c = fit_within(crop, r);
            let render = crop::pixel_size(c, image);
            // Exact target when the photo is big enough; otherwise same aspect at native size.
            let output = if render.0 >= w && render.1 >= h { (w, h) } else { scaled(render, 1.0) };
            Plan { crop: c, render, output }
        }
        Size::Exact(w, h) => {
            let scale = (w as f32 / px.0 as f32).min(h as f32 / px.1 as f32);
            Plan { crop, render: px, output: scaled(px, scale) }
        }
    }
}

/// Formats we can write, by extension.
const WRITABLE: [&str; 6] = ["jpg", "jpeg", "png", "tif", "tiff", "webp"];

/// Where Quick Export saves: `name-edited.ext` next to the original (`name-edited-2.ext` … if
/// taken), keeping its format when we can write it, else JPEG. Also returns the original format's
/// name when it had to change (e.g. "HEIC").
pub fn edited_path(original: &Path) -> (PathBuf, Option<String>) {
    let stem = original.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "photo".into());
    let ext = original.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let (ext, converted) = if WRITABLE.contains(&ext.to_lowercase().as_str()) {
        (ext, None)
    } else {
        ("jpg".to_string(), Some(ext.to_uppercase()))
    };
    let dir = original.parent().unwrap_or(Path::new("."));
    let path = (1..)
        .map(|n| dir.join(if n == 1 { format!("{stem}-edited.{ext}") } else { format!("{stem}-edited-{n}.{ext}") }))
        .find(|p| !p.exists())
        .unwrap();
    (path, converted)
}

pub fn find(slug: &str) -> Option<Size> {
    SIZES.iter().find(|s| s.slug.eq_ignore_ascii_case(slug) && s.slug != "custom").map(|s| s.size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::FULL_CROP;

    #[test]
    fn instagram_portrait_fill_crops_landscape_photo() {
        let p = plan(Size::Exact(1080, 1350), FULL_CROP, (6000, 4000), true);
        assert_eq!(p.output, (1080, 1350));
        assert_eq!(p.render, (3200, 4000));
        assert!((p.crop[0] - (1.0 - 3200.0 / 6000.0) / 2.0).abs() < 1e-4);
    }

    #[test]
    fn fit_keeps_whole_photo_inside_box() {
        let p = plan(Size::Exact(1080, 1350), FULL_CROP, (6000, 4000), false);
        assert_eq!(p.output, (1080, 720));
        assert_eq!(p.crop, FULL_CROP);
    }

    #[test]
    fn long_edge_never_enlarges() {
        assert_eq!(plan(Size::LongEdge(2048), FULL_CROP, (4000, 3000), false).output, (2048, 1536));
        assert_eq!(plan(Size::LongEdge(2048), FULL_CROP, (1200, 800), false).output, (1200, 800));
    }

    #[test]
    fn edited_path_keeps_format_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("bp_photos_edited_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(edited_path(&dir.join("IMG_1.JPG")), (dir.join("IMG_1-edited.JPG"), None));
        assert_eq!(edited_path(&dir.join("a.heic")), (dir.join("a-edited.jpg"), Some("HEIC".into())));
        std::fs::write(dir.join("b-edited.png"), b"").unwrap();
        assert_eq!(edited_path(&dir.join("b.png")).0, dir.join("b-edited-2.png"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn small_photo_fill_keeps_native_size() {
        let p = plan(Size::Exact(1080, 1080), FULL_CROP, (900, 600), true);
        assert_eq!(p.output, (600, 600));
    }
}
