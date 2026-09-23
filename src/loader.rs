//! Photo decoding (JPEG/PNG/TIFF/WebP via `image`, HEIC/HEIF via libheif, camera RAW via `rawler`).

use std::path::Path;

use image::{DynamicImage, ImageDecoder, ImageReader, metadata::Orientation};

pub const RAW_EXTENSIONS: [&str; 17] = [
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef", "dng", "srw", "3fr", "iiq", "erf",
];
pub const IMAGE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "tif", "tiff", "webp"];
pub const HEIF_EXTENSIONS: [&str; 4] = ["heic", "heif", "hif", "avif"];

pub fn is_photo(path: &Path) -> bool {
    crate::import::extension(path).is_some_and(|e| {
        let e = e.as_str();
        RAW_EXTENSIONS.contains(&e) || IMAGE_EXTENSIONS.contains(&e) || HEIF_EXTENSIONS.contains(&e)
    })
}

pub enum Pixels {
    /// sRGB-encoded RGBA8, uploaded as `Rgba8UnormSrgb`.
    Srgb8(Vec<u8>),
    /// Linear-light RGBA as f16 bits, uploaded as `Rgba16Float`.
    LinearF16(Vec<u16>),
}

pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
    /// 8-bit Display P3 data, converted to sRGB on the GPU.
    pub p3: bool,
}

/// Decodes a photo, applying its orientation and fitting it within `max_dim`.
pub fn load(path: &Path, max_dim: u32) -> Result<Decoded, String> {
    let t = std::time::Instant::now();
    let ext = crate::import::extension(path).unwrap_or_default();
    let mut p3 = false;
    let img = if RAW_EXTENSIONS.contains(&ext.as_str()) {
        load_raw(path)?
    } else if let Some(img) = (ext == "jpg" || ext == "jpeg").then(|| load_jpeg(path)).flatten() {
        img
    } else if HEIF_EXTENSIONS.contains(&ext.as_str()) {
        let (img, is_p3) = load_heif(path)?;
        p3 = is_p3;
        img
    } else {
        load_image(path)?
    };
    let img = if img.width() > max_dim || img.height() > max_dim {
        img.resize(max_dim, max_dim, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    crate::timing("  decode", t);
    let t = std::time::Instant::now();
    let (width, height) = (img.width(), img.height());
    let pixels = match img {
        DynamicImage::ImageRgb32F(buf) => {
            let mut out = vec![0u16; buf.len() / 3 * 4];
            par_zip(&mut out, 4, buf.as_raw(), 3, |o, px| {
                o.copy_from_slice(&[f16(px[0]), f16(px[1]), f16(px[2]), f16(1.0)]);
            });
            Pixels::LinearF16(out)
        }
        // High bit depth: linearise on the CPU to keep the precision (and convert P3 there).
        other @ (DynamicImage::ImageRgb16(_) | DynamicImage::ImageRgba16(_)) => {
            let out = Pixels::LinearF16(linear_f16(&other.into_rgba16(), p3));
            p3 = false;
            out
        }
        // 8-bit: upload as-is; a P3 → sRGB matrix is applied on the GPU.
        DynamicImage::ImageRgba8(buf) => Pixels::Srgb8(buf.into_raw()),
        DynamicImage::ImageRgb8(buf) => {
            let mut out = vec![255u8; buf.len() / 3 * 4];
            par_zip(&mut out, 4, buf.as_raw(), 3, |o, px| o[..3].copy_from_slice(px));
            Pixels::Srgb8(out)
        }
        other => Pixels::Srgb8(other.into_rgba8().into_raw()),
    };
    crate::timing("  convert", t);
    Ok(Decoded { width, height, pixels, p3 })
}

/// Per-pixel conversion over all cores: `f(out_pixel, in_pixel)` with the given channel counts.
fn par_zip<O: Send, I: Sync>(out: &mut [O], out_ch: usize, input: &[I], in_ch: usize, f: impl Fn(&mut [O], &[I]) + Sync) {
    let pixels = out.len() / out_ch;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let per = pixels.div_ceil(threads).max(1);
    std::thread::scope(|s| {
        for (o, i) in out.chunks_mut(per * out_ch).zip(input.chunks(per * in_ch)) {
            let f = &f;
            s.spawn(move || {
                for (op, ip) in o.chunks_exact_mut(out_ch).zip(i.chunks_exact(in_ch)) {
                    f(op, ip);
                }
            });
        }
    });
}

/// Decodes a JPEG straight to RGBA (what the GPU wants), skipping an RGB → RGBA pass.
/// Returns `None` for anything unusual (e.g. CMYK), so the generic path handles it.
fn load_jpeg(path: &Path) -> Option<DynamicImage> {
    use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};

    let data = std::fs::read(path).ok()?;
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        .set_max_width(1 << 16)
        .set_max_height(1 << 16);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(&data), options);
    let pixels = decoder.decode().ok()?;
    if decoder.output_colorspace()? != ColorSpace::RGBA {
        return None;
    }
    let info = decoder.info()?;
    let orientation = decoder.exif().and_then(|e| Orientation::from_exif_chunk(e)).unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::ImageRgba8(image::RgbaImage::from_raw(info.width as u32, info.height as u32, pixels)?);
    img.apply_orientation(orientation);
    Some(img)
}

fn f16(v: f32) -> u16 {
    half::f16::from_f32(v).to_bits()
}

/// A fast, low-resolution first look: the preview embedded in HEIC and RAW files.
/// Shown while the full image decodes; `None` if the file has none (or is fast anyway).
pub fn load_quick(path: &Path) -> Option<Decoded> {
    let t = std::time::Instant::now();
    let ext = crate::import::extension(path)?;
    let (img, p3) = if HEIF_EXTENSIONS.contains(&ext.as_str()) {
        heif_thumbnail(path)?
    } else if RAW_EXTENSIONS.contains(&ext.as_str()) {
        (raw_preview(path)?, false)
    } else {
        return None;
    };
    let img = img.into_rgba8();
    crate::timing("  quick preview", t);
    Some(Decoded { width: img.width(), height: img.height(), pixels: Pixels::Srgb8(img.into_raw()), p3 })
}

fn heif_thumbnail(path: &Path) -> Option<(DynamicImage, bool)> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

    let ctx = HeifContext::read_from_file(&path.to_string_lossy()).ok()?;
    let handle = ctx.primary_image_handle().ok()?;
    let mut ids = [0; 4];
    let n = handle.thumbnail_ids(&mut ids);
    // The largest embedded thumbnail (libheif applies its rotation when decoding).
    let thumb = ids[..n].iter().filter_map(|&id| handle.thumbnail(id).ok()).max_by_key(|t| t.width())?;
    let image = LibHeif::new().decode(&thumb, ColorSpace::Rgb(RgbChroma::Rgba), None).ok()?;
    let plane = image.planes().interleaved?;
    let (w, h) = (plane.width, plane.height);
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for row in plane.data.chunks(plane.stride).take(h as usize) {
        data.extend_from_slice(&row[..(w * 4) as usize]);
    }
    let img = DynamicImage::ImageRgba8(image::RgbaImage::from_raw(w, h, data)?);
    Some((img, is_p3(&handle)))
}

fn is_p3(handle: &libheif_rs::ImageHandle) -> bool {
    use libheif_rs::ColorPrimaries;
    handle.color_profile_nclx().is_some_and(|n| n.color_primaries() == ColorPrimaries::SMPTE_EG_432_1)
        || handle.color_profile_raw().is_some_and(|icc| icc_is_display_p3(&icc.data))
}

/// The camera's own JPEG preview, oriented like the photo.
fn raw_preview(path: &Path) -> Option<DynamicImage> {
    raw_embedded(path, false)
}

/// An embedded RAW image: the smallest usable one (`small`, for thumbnails) or the largest.
/// Tried lazily, in order, since decoding each one costs time.
fn raw_embedded(path: &Path, small: bool) -> Option<DynamicImage> {
    use rawler::decoders::{Decoder, RawDecodeParams};
    use rawler::rawsource::RawSource;

    type Get = fn(&dyn Decoder, &RawSource, &RawDecodeParams) -> rawler::Result<Option<DynamicImage>>;
    let source = RawSource::new(path).ok()?;
    let decoder = rawler::get_decoder(&source).ok()?;
    let params = RawDecodeParams::default();
    let preview: Get = |d, s, p| d.preview_image(s, p);
    let full: Get = |d, s, p| d.full_image(s, p);
    let thumb: Get = |d, s, p| d.thumbnail_image(s, p);
    let order = if small { [thumb, preview, full] } else { [preview, full, thumb] };
    let mut img = order.into_iter().find_map(|get| get(decoder.as_ref(), &source, &params).ok().flatten())?;
    let exif = decoder.raw_metadata(&source, &params).ok()?.exif.orientation;
    img.apply_orientation(exif.and_then(|o| Orientation::from_exif(o as u8)).unwrap_or(Orientation::NoTransforms));
    Some(img)
}

/// A small, upright thumbnail (longest edge about `max`) for the filmstrip, taken from the
/// file's embedded preview when it has one, so it's much faster than a full decode.
pub fn load_thumbnail(path: &Path, max: u32) -> Result<image::RgbaImage, String> {
    let ext = crate::import::extension(path).unwrap_or_default();
    let img = if RAW_EXTENSIONS.contains(&ext.as_str()) {
        raw_embedded(path, true).map_or_else(|| load_raw(path), Ok)?
    } else if HEIF_EXTENSIONS.contains(&ext.as_str()) {
        heif_thumbnail(path).map(|(i, _)| i).map_or_else(|| load_heif(path).map(|(i, _)| i), Ok)?
    } else if ext == "jpg" || ext == "jpeg" {
        match exif_thumbnail(path) {
            Some(img) => img,
            None => load_jpeg(path).map_or_else(|| load_image(path), Ok)?,
        }
    } else {
        load_image(path)?
    };
    Ok(img.thumbnail(max, max).into_rgba8())
}

/// The JPEG thumbnail stored in EXIF (usually 160×120), rotated like the photo.
fn exif_thumbnail(path: &Path) -> Option<DynamicImage> {
    use exif::{In, Tag};

    let file = std::fs::File::open(path).ok()?;
    let exif = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)).ok()?;
    let offset = exif.get_field(Tag::JPEGInterchangeFormat, In::THUMBNAIL)?.value.get_uint(0)? as usize;
    let len = exif.get_field(Tag::JPEGInterchangeFormatLength, In::THUMBNAIL)?.value.get_uint(0)? as usize;
    let bytes = exif.buf().get(offset..offset + len)?;
    let mut img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).ok()?;
    let orientation = exif.get_field(Tag::Orientation, In::PRIMARY).and_then(|f| f.value.get_uint(0));
    img.apply_orientation(orientation.and_then(|o| Orientation::from_exif(o as u8)).unwrap_or(Orientation::NoTransforms));
    Some(img)
}

/// sRGB-encoded 16-bit → linear f16, optionally converting Display P3 primaries to sRGB.
fn linear_f16(img: &image::ImageBuffer<image::Rgba<u16>, Vec<u16>>, p3: bool) -> Vec<u16> {
    const P3_TO_SRGB: [[f32; 3]; 3] = [[1.2249, -0.2247, 0.0], [-0.0420, 1.0419, 0.0], [-0.0197, -0.0786, 1.0979]];
    let lut: Vec<f32> = (0..=u16::MAX)
        .map(|v| {
            let c = v as f32 / 65535.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        })
        .collect();
    let mut out = vec![0u16; img.len()];
    par_zip(&mut out, 4, img.as_raw(), 4, |o, px| {
        let rgb = [lut[px[0] as usize], lut[px[1] as usize], lut[px[2] as usize]];
        let rgb = if p3 { P3_TO_SRGB.map(|row| row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2]) } else { rgb };
        o.copy_from_slice(&[f16(rgb[0]), f16(rgb[1]), f16(rgb[2]), f16(1.0)]);
    });
    out
}

/// Decodes HEIC/HEIF/AVIF. libheif applies the container's rotation/mirroring itself.
/// Returns whether the image uses Display P3 primaries (as iPhone photos do).
fn load_heif(path: &Path) -> Result<(DynamicImage, bool), String> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

    let err = |e: libheif_rs::HeifError| format!("HEIF decode failed: {e}");
    let mut ctx = HeifContext::read_from_file(&path.to_string_lossy()).map_err(err)?;
    // Photos from phones are stored as a grid of tiles, which libheif can decode in parallel.
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get() as u32);
    ctx.set_max_decoding_threads(threads);
    let handle = ctx.primary_image_handle().map_err(err)?;
    let deep = handle.luma_bits_per_pixel() > 8;
    let chroma = if deep { RgbChroma::HdrRgbaLe } else { RgbChroma::Rgba };
    let image = LibHeif::new().decode(&handle, ColorSpace::Rgb(chroma), None).map_err(err)?;
    let plane = image.planes().interleaved.ok_or("HEIF image has no interleaved plane")?;
    let (w, h) = (plane.width, plane.height);

    let img = if deep {
        let max = ((1u32 << plane.bits_per_pixel.clamp(1, 16)) - 1).max(1);
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for row in plane.data.chunks(plane.stride).take(h as usize) {
            for v in row[..(w * 8) as usize].chunks_exact(2) {
                data.push((u16::from_le_bytes([v[0], v[1]]) as u32 * 65535 / max).min(65535) as u16);
            }
        }
        DynamicImage::ImageRgba16(image::ImageBuffer::from_raw(w, h, data).ok_or("bad HEIF dimensions")?)
    } else {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for row in plane.data.chunks(plane.stride).take(h as usize) {
            data.extend_from_slice(&row[..(w * 4) as usize]);
        }
        DynamicImage::ImageRgba8(image::RgbaImage::from_raw(w, h, data).ok_or("bad HEIF dimensions")?)
    };

    Ok((img, is_p3(&handle)))
}

/// ICC descriptions are ASCII (v2) or UTF-16BE (v4 `mluc`); look for "Display P3" in either.
fn icc_is_display_p3(icc: &[u8]) -> bool {
    let ascii = b"Display P3";
    let utf16: Vec<u8> = ascii.iter().flat_map(|&c| [0, c]).collect();
    icc.windows(ascii.len()).any(|w| w == ascii) || icc.windows(utf16.len()).any(|w| w == utf16.as_slice())
}

fn load_image(path: &Path) -> Result<DynamicImage, String> {
    let reader = ImageReader::open(path).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Demosaics a RAW file into linear sRGB-primaries floats.
fn load_raw(path: &Path) -> Result<DynamicImage, String> {
    use rawler::imgop::develop::{Intermediate, ProcessingStep as S, RawDevelop};

    let raw = rawler::decode_file(path).map_err(|e| format!("RAW decode failed: {e}"))?;
    let develop = RawDevelop::new_with(&[
        S::Rescale,
        S::Demosaic,
        S::FujiRotate,
        S::CropActiveArea,
        S::WhiteBalance,
        S::Calibrate,
        S::CropDefault,
    ]);
    let (w, h, rgb) = match develop.develop_intermediate(&raw).map_err(|e| format!("RAW develop failed: {e}"))? {
        Intermediate::ThreeColor(px) => (px.width, px.height, px.into_flatten()),
        Intermediate::Monochrome(px) => (px.width, px.height, px.data.iter().flat_map(|v| [*v; 3]).collect()),
        Intermediate::FourColor(_) => return Err("unsupported 4-colour sensor".into()),
    };
    let buf = image::Rgb32FImage::from_raw(w as u32, h as u32, rgb).ok_or("bad RAW dimensions")?;
    let mut img = DynamicImage::ImageRgb32F(buf);
    img.apply_orientation(match raw.orientation {
        rawler::Orientation::HorizontalFlip => Orientation::FlipHorizontal,
        rawler::Orientation::Rotate180 => Orientation::Rotate180,
        rawler::Orientation::VerticalFlip => Orientation::FlipVertical,
        rawler::Orientation::Transpose => Orientation::Rotate90FlipH,
        rawler::Orientation::Rotate90 => Orientation::Rotate90,
        rawler::Orientation::Transverse => Orientation::Rotate270FlipH,
        rawler::Orientation::Rotate270 => Orientation::Rotate270,
        _ => Orientation::NoTransforms,
    });
    Ok(img)
}
