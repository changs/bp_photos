//! Carries the original photo's EXIF (date, camera, lens, exposure, GPS) over to exports.
//!
//! Fields that describe the original file rather than the photo are dropped or rewritten:
//! orientation (exports are already upright), pixel size, colour space (exports are sRGB), the
//! embedded thumbnail and maker notes.

use std::io::Cursor;
use std::path::Path;

use exif::{Context, Field, In, Rational, SRational, Tag, Value};

/// EXIF (TIFF-structured bytes) for an image exported from `source` at `size`, or `None` if the
/// source has no readable metadata.
pub fn for_export(source: &Path, size: (u32, u32), keep_gps: bool) -> Option<Vec<u8>> {
    let ext = crate::import::extension(source).unwrap_or_default();
    let mut fields = if crate::loader::RAW_EXTENSIONS.contains(&ext.as_str()) {
        from_raw(source)?
    } else {
        from_container(source)?
    };
    fields.retain(|f| keep_gps || f.tag.context() != Context::Gps);
    fields.extend([
        field(Tag::Software, Value::Ascii(vec![b"bp_photos".to_vec()])),
        field(Tag::ColorSpace, Value::Short(vec![1])), // sRGB
        field(Tag::PixelXDimension, Value::Long(vec![size.0])),
        field(Tag::PixelYDimension, Value::Long(vec![size.1])),
    ]);
    // An Exif IFD needs a version to be valid.
    if !fields.iter().any(|f| f.tag == Tag::ExifVersion) {
        fields.push(field(Tag::ExifVersion, Value::Undefined(b"0232".to_vec(), 0)));
    }
    let mut writer = exif::experimental::Writer::new();
    for f in &fields {
        writer.push_field(f);
    }
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out, false).ok()?;
    Some(out.into_inner())
}

fn field(tag: Tag, value: Value) -> Field {
    Field { tag, ifd_num: In::PRIMARY, value }
}

/// Tags we recompute, or that only make sense for the original file.
fn dropped(tag: Tag) -> bool {
    matches!(
        tag,
        Tag::Orientation
            | Tag::Software
            | Tag::MakerNote
            | Tag::ColorSpace
            | Tag::PixelXDimension
            | Tag::PixelYDimension
    )
}

/// Photo-describing tags from the main (TIFF) IFD; the rest describe the file's structure.
const KEPT_TIFF_TAGS: [Tag; 9] = [
    Tag::Make,
    Tag::Model,
    Tag::DateTime,
    Tag::Artist,
    Tag::Copyright,
    Tag::ImageDescription,
    Tag::XResolution,
    Tag::YResolution,
    Tag::ResolutionUnit,
];

/// JPEG, HEIC, PNG, WebP and TIFF.
fn from_container(source: &Path) -> Option<Vec<Field>> {
    let file = std::fs::File::open(source).ok()?;
    let exif = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)).ok()?;
    Some(
        exif.fields()
            .filter(|f| f.ifd_num == In::PRIMARY && !dropped(f.tag))
            .filter(|f| f.tag.context() != Context::Tiff || KEPT_TIFF_TAGS.contains(&f.tag))
            .cloned()
            .collect(),
    )
}

/// Camera RAW, via rawler's parsed metadata (covers CR3 and other non-TIFF formats too).
fn from_raw(source: &Path) -> Option<Vec<Field>> {
    use rawler::decoders::RawDecodeParams;

    let src = rawler::rawsource::RawSource::new(source).ok()?;
    let decoder = rawler::get_decoder(&src).ok()?;
    let md = decoder.raw_metadata(&src, &RawDecodeParams::default()).ok()?;
    let e = &md.exif;
    let mut out = Vec::new();
    let ascii = |s: &str| Value::Ascii(vec![s.as_bytes().to_vec()]);
    let rat = |r: &rawler::formats::tiff::Rational| Rational { num: r.n, denom: r.d };
    let srat = |r: &rawler::formats::tiff::SRational| SRational { num: r.n, denom: r.d };
    let mut push = |tag, value| out.push(field(tag, value));

    push(Tag::Make, ascii(&md.make));
    push(Tag::Model, ascii(&md.model));
    for (tag, v) in [
        (Tag::DateTimeOriginal, &e.date_time_original),
        (Tag::DateTimeDigitized, &e.create_date),
        (Tag::DateTime, &e.modify_date),
        (Tag::OffsetTime, &e.offset_time),
        (Tag::OffsetTimeOriginal, &e.offset_time_original),
        (Tag::OffsetTimeDigitized, &e.offset_time_digitized),
        (Tag::SubSecTimeOriginal, &e.sub_sec_time_original),
        (Tag::Artist, &e.artist),
        (Tag::Copyright, &e.copyright),
        (Tag::LensMake, &e.lens_make),
        (Tag::LensModel, &e.lens_model),
        (Tag::BodySerialNumber, &e.serial_number),
        (Tag::LensSerialNumber, &e.lens_serial_number),
    ] {
        if let Some(s) = v {
            push(tag, ascii(s));
        }
    }
    for (tag, v) in [
        (Tag::ExposureTime, &e.exposure_time),
        (Tag::FNumber, &e.fnumber),
        (Tag::ApertureValue, &e.aperture_value),
        (Tag::MaxApertureValue, &e.max_aperture_value),
        (Tag::FocalLength, &e.focal_length),
        (Tag::SubjectDistance, &e.subject_distance),
    ] {
        if let Some(r) = v {
            push(tag, Value::Rational(vec![rat(r)]));
        }
    }
    for (tag, v) in [(Tag::ExposureBiasValue, &e.exposure_bias), (Tag::ShutterSpeedValue, &e.shutter_speed_value), (Tag::BrightnessValue, &e.brightness_value)] {
        if let Some(r) = v {
            push(tag, Value::SRational(vec![srat(r)]));
        }
    }
    for (tag, v) in [
        (Tag::PhotographicSensitivity, &e.iso_speed_ratings),
        (Tag::ExposureProgram, &e.exposure_program),
        (Tag::MeteringMode, &e.metering_mode),
        (Tag::LightSource, &e.light_source),
        (Tag::Flash, &e.flash),
        (Tag::ExposureMode, &e.exposure_mode),
        (Tag::WhiteBalance, &e.white_balance),
        (Tag::SceneCaptureType, &e.scene_capture_type),
    ] {
        if let Some(n) = v {
            push(tag, Value::Short(vec![*n]));
        }
    }
    if let Some(spec) = &e.lens_spec {
        push(Tag::LensSpecification, Value::Rational(spec.iter().map(rat).collect()));
    }
    if let Some(g) = &e.gps {
        if let (Some(lat), Some(lat_ref), Some(lon), Some(lon_ref)) = (&g.gps_latitude, &g.gps_latitude_ref, &g.gps_longitude, &g.gps_longitude_ref) {
            push(Tag::GPSVersionID, Value::Byte(g.gps_version_id.unwrap_or([2, 3, 0, 0]).to_vec()));
            push(Tag::GPSLatitudeRef, ascii(lat_ref));
            push(Tag::GPSLatitude, Value::Rational(lat.iter().map(rat).collect()));
            push(Tag::GPSLongitudeRef, ascii(lon_ref));
            push(Tag::GPSLongitude, Value::Rational(lon.iter().map(rat).collect()));
            if let (Some(alt), Some(alt_ref)) = (&g.gps_altitude, g.gps_altitude_ref) {
                push(Tag::GPSAltitudeRef, Value::Byte(vec![alt_ref]));
                push(Tag::GPSAltitude, Value::Rational(vec![rat(alt)]));
            }
            if let Some(d) = &g.gps_date_stamp {
                push(Tag::GPSDateStamp, ascii(d));
            }
            if let Some(t) = &g.gps_timestamp {
                push(Tag::GPSTimeStamp, Value::Rational(t.iter().map(rat).collect()));
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG with orientation, GPS and a date: exported EXIF keeps the date and GPS (unless
    /// asked not to), drops the orientation and records the new size.
    #[test]
    fn rewrites_exif_for_export() {
        let src = [
            field(Tag::Make, Value::Ascii(vec![b"Cam".to_vec()])),
            field(Tag::Orientation, Value::Short(vec![6])),
            field(Tag::DateTimeOriginal, Value::Ascii(vec![b"2024:05:01 10:00:00".to_vec()])),
            field(Tag::ExifVersion, Value::Undefined(b"0232".to_vec(), 0)),
            field(Tag::GPSLatitudeRef, Value::Ascii(vec![b"N".to_vec()])),
            field(Tag::GPSLatitude, Value::Rational(vec![Rational { num: 52, denom: 1 }; 3])),
        ];
        let mut w = exif::experimental::Writer::new();
        src.iter().for_each(|f| w.push_field(f));
        let mut tiff = Cursor::new(Vec::new());
        w.write(&mut tiff, false).unwrap();

        let dir = std::env::temp_dir().join(format!("bp_photos_exif_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("in.jpg");
        let mut jpeg = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new(&mut jpeg);
        image::ImageEncoder::set_exif_metadata(&mut enc, tiff.into_inner()).unwrap();
        enc.encode_image(&image::RgbImage::new(8, 8)).unwrap();
        std::fs::write(&path, jpeg).unwrap();

        let read = |bytes: Vec<u8>| exif::Reader::new().read_raw(bytes).unwrap();
        let out = read(for_export(&path, (640, 480), true).unwrap());
        assert!(out.get_field(Tag::Orientation, In::PRIMARY).is_none());
        assert!(out.get_field(Tag::DateTimeOriginal, In::PRIMARY).is_some());
        assert!(out.get_field(Tag::GPSLatitude, In::PRIMARY).is_some());
        assert_eq!(out.get_field(Tag::PixelXDimension, In::PRIMARY).unwrap().value.get_uint(0), Some(640));
        let no_gps = read(for_export(&path, (640, 480), false).unwrap());
        assert!(no_gps.get_field(Tag::GPSLatitude, In::PRIMARY).is_none());
        std::fs::remove_dir_all(dir).ok();
    }
}
