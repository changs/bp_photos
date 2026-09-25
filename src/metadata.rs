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

/// A photo's EXIF as readable sections for the Info tab: (section, [(label, value)]), plus the
/// GPS position (latitude, longitude) when it has one.
pub struct Summary {
    pub sections: Vec<(&'static str, Vec<(&'static str, String)>)>,
    pub location: Option<(f64, f64)>,
}

/// Reads and formats the metadata of `source` (any format we open).
pub fn summary(source: &Path) -> Summary {
    let ext = crate::import::extension(source).unwrap_or_default();
    let fields = if crate::loader::RAW_EXTENSIONS.contains(&ext.as_str()) {
        from_raw(source)
    } else {
        std::fs::File::open(source)
            .ok()
            .and_then(|f| exif::Reader::new().read_from_container(&mut std::io::BufReader::new(f)).ok())
            .map(|e| e.fields().filter(|f| f.ifd_num == In::PRIMARY).cloned().collect())
    }
    .unwrap_or_default();
    summarise(&fields)
}

fn summarise(fields: &[Field]) -> Summary {
    let get = |t: Tag| fields.iter().find(|f| f.tag == t).map(|f| &f.value);
    let text = |t: Tag| match get(t) {
        Some(Value::Ascii(v)) => v.first().map(|s| String::from_utf8_lossy(s).trim().to_string()).filter(|s| !s.is_empty()),
        _ => None,
    };
    let num = |t: Tag| match get(t) {
        Some(Value::Rational(v)) => v.first().map(|r| r.to_f64()),
        Some(Value::SRational(v)) => v.first().map(|r| r.to_f64()),
        Some(v) => v.get_uint(0).map(f64::from),
        None => None,
    }
    .filter(|n| n.is_finite());
    let trim = |n: f64, digits: usize| {
        let s = format!("{n:.digits$}");
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
    };

    let mut camera = Vec::new();
    match (text(Tag::Make), text(Tag::Model)) {
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => camera.push(("Camera", model)),
        (Some(make), Some(model)) => camera.push(("Camera", format!("{make} {model}"))),
        (None, Some(model)) | (Some(model), None) => camera.push(("Camera", model)),
        _ => {}
    }
    if let Some(lens) = text(Tag::LensModel) {
        camera.push(("Lens", lens));
    }

    let mut exposure = Vec::new();
    if let Some(t) = num(Tag::ExposureTime).filter(|t| *t > 0.0) {
        exposure.push(("Shutter", if t < 1.0 { format!("1/{} s", (1.0 / t).round()) } else { format!("{} s", trim(t, 1)) }));
    }
    if let Some(f) = num(Tag::FNumber).filter(|f| *f > 0.0) {
        exposure.push(("Aperture", format!("f/{}", trim(f, 1))));
    }
    if let Some(iso) = num(Tag::PhotographicSensitivity) {
        exposure.push(("ISO", format!("{iso:.0}")));
    }
    if let Some(mm) = num(Tag::FocalLength).filter(|m| *m > 0.0) {
        let equiv = num(Tag::FocalLengthIn35mmFilm).filter(|e| *e > 0.0).map(|e| format!(" ({e:.0} mm equiv.)")).unwrap_or_default();
        exposure.push(("Focal length", format!("{} mm{equiv}", trim(mm, 1))));
    }
    if let Some(ev) = num(Tag::ExposureBiasValue).filter(|e| e.abs() > 0.01) {
        exposure.push(("Exposure comp.", format!("{}{} EV", if ev > 0.0 { "+" } else { "" }, trim(ev, 1))));
    }
    if let Some(flash) = num(Tag::Flash) {
        exposure.push(("Flash", if flash as u32 & 1 == 1 { "Fired".into() } else { "Off".into() }));
    }

    let mut taken = Vec::new();
    if let Some(date) = text(Tag::DateTimeOriginal).or_else(|| text(Tag::DateTime)) {
        taken.push(("Taken", pretty_date(&date)));
    }

    let coord = |value: Tag, reference: Tag| {
        let Some(Value::Rational(dms)) = get(value) else { return None };
        let deg = dms.iter().map(|r| r.to_f64()).zip([1.0, 60.0, 3600.0]).map(|(v, d)| v / d).sum::<f64>();
        let sign = if matches!(text(reference).as_deref(), Some("S" | "W")) { -1.0 } else { 1.0 };
        Some(deg * sign).filter(|d| d.is_finite())
    };
    let location = coord(Tag::GPSLatitude, Tag::GPSLatitudeRef).zip(coord(Tag::GPSLongitude, Tag::GPSLongitudeRef));
    let mut place = Vec::new();
    if let Some((lat, lon)) = location {
        let (ns, ew) = (if lat < 0.0 { 'S' } else { 'N' }, if lon < 0.0 { 'W' } else { 'E' });
        place.push(("Position", format!("{:.5}° {ns}, {:.5}° {ew}", lat.abs(), lon.abs())));
        if let Some(alt) = num(Tag::GPSAltitude) {
            let below = matches!(get(Tag::GPSAltitudeRef), Some(Value::Byte(b)) if b.first() == Some(&1));
            place.push(("Altitude", format!("{}{alt:.0} m", if below { "-" } else { "" })));
        }
    }

    let mut other = Vec::new();
    for (label, tag) in [("Software", Tag::Software), ("Artist", Tag::Artist), ("Copyright", Tag::Copyright)] {
        if let Some(v) = text(tag) {
            other.push((label, v));
        }
    }

    let sections = [("CAMERA", camera), ("EXPOSURE", exposure), ("DATE", taken), ("LOCATION", place), ("OTHER", other)]
        .into_iter()
        .filter(|(_, rows)| !rows.is_empty())
        .collect();
    Summary { sections, location }
}

/// "2025:08:14 19:02:11" → "14 Aug 2025, 19:02".
fn pretty_date(exif: &str) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let p: Vec<&str> = exif.split(|c: char| c == ':' || c == ' ').collect();
    match (p.first(), p.get(1).and_then(|m| m.parse::<usize>().ok()), p.get(2), p.get(3), p.get(4)) {
        (Some(y), Some(m @ 1..=12), Some(d), Some(h), Some(min)) => format!("{} {} {y}, {h}:{min}", d.trim_start_matches('0'), MONTHS[m - 1]),
        _ => exif.to_string(),
    }
}

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
    fn summarises_exif_readably() {
        let fields = [
            field(Tag::Make, Value::Ascii(vec![b"Canon".to_vec()])),
            field(Tag::Model, Value::Ascii(vec![b"Canon EOS R6".to_vec()])),
            field(Tag::ExposureTime, Value::Rational(vec![Rational { num: 1, denom: 250 }])),
            field(Tag::FNumber, Value::Rational(vec![Rational { num: 28, denom: 10 }])),
            field(Tag::FocalLength, Value::Rational(vec![Rational { num: 70, denom: 1 }])),
            field(Tag::DateTimeOriginal, Value::Ascii(vec![b"2025:08:04 19:02:11".to_vec()])),
            field(Tag::GPSLatitudeRef, Value::Ascii(vec![b"S".to_vec()])),
            field(Tag::GPSLatitude, Value::Rational(vec![Rational { num: 33, denom: 1 }, Rational { num: 30, denom: 1 }, Rational { num: 0, denom: 1 }])),
            field(Tag::GPSLongitudeRef, Value::Ascii(vec![b"E".to_vec()])),
            field(Tag::GPSLongitude, Value::Rational(vec![Rational { num: 151, denom: 1 }, Rational { num: 12, denom: 1 }, Rational { num: 0, denom: 1 }])),
        ];
        let s = summarise(&fields);
        let rows: Vec<(&str, String)> = s.sections.iter().flat_map(|(_, r)| r.clone()).collect();
        let get = |l: &str| rows.iter().find(|(k, _)| *k == l).map(|(_, v)| v.as_str());
        assert_eq!(get("Camera"), Some("Canon EOS R6"));
        assert_eq!(get("Shutter"), Some("1/250 s"));
        assert_eq!(get("Aperture"), Some("f/2.8"));
        assert_eq!(get("Focal length"), Some("70 mm"));
        assert_eq!(get("Taken"), Some("4 Aug 2025, 19:02"));
        let (lat, lon) = s.location.unwrap();
        assert!((lat + 33.5).abs() < 1e-9 && (lon - 151.2).abs() < 1e-9);
    }

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
