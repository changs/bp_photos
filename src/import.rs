//! Loading presets from Lightroom `.xmp` / `.lrtemplate` files and `.cube` LUTs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::preset::{Adjustments, BANDS, Curve, Lut3d, Preset};

pub const PRESET_EXTENSIONS: [&str; 3] = ["xmp", "lrtemplate", "cube"];

pub fn is_preset_file(path: &Path) -> bool {
    extension(path).is_some_and(|e| PRESET_EXTENSIONS.contains(&e.as_str()))
}

pub fn extension(path: &Path) -> Option<String> {
    path.extension().map(|e| e.to_string_lossy().to_lowercase())
}

/// Recursively loads every preset under `dir`, grouping by sub-folder name.
pub fn load_dir(dir: &Path) -> (Vec<Preset>, Vec<String>) {
    let mut files = Vec::new();
    collect_files(dir, &mut files);
    files.sort();
    let mut presets = Vec::new();
    let mut errors = Vec::new();
    for file in files {
        match load_file(&file, &group_for(dir, &file)) {
            Ok(p) => presets.push(p),
            Err(e) => errors.push(format!("{}: {e}", file.display())),
        }
    }
    (presets, errors)
}

/// Group name from the folders between the presets root and the file, e.g. "Film LUTs / bw".
pub fn group_for(root: &Path, file: &Path) -> String {
    let rel = file.parent().and_then(|p| p.strip_prefix(root).ok());
    let parts: Vec<String> = rel.into_iter().flat_map(|r| r.iter()).map(|c| c.to_string_lossy().into_owned()).collect();
    if parts.is_empty() { "Imported".into() } else { parts.join(" / ") }
}

pub fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else if is_preset_file(&path) {
            out.push(path);
        }
    }
}

pub fn load_file(path: &Path, group: &str) -> Result<Preset, String> {
    let text = std::fs::read(path).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&text);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let (name, adj, lut) = match extension(path).as_deref() {
        Some("cube") => {
            let (title, lut) = parse_cube(&text)?;
            (title.unwrap_or(stem), Adjustments::default(), Some(lut))
        }
        Some("xmp") => {
            let (name, settings) = parse_xmp(&text);
            if settings.is_empty() {
                return Err("no Camera Raw settings found".into());
            }
            (name.unwrap_or(stem), adjustments(&settings), None)
        }
        Some("lrtemplate") => {
            let (name, settings) = parse_lrtemplate(&text);
            if settings.is_empty() {
                return Err("no develop settings found".into());
            }
            (name.unwrap_or(stem), adjustments(&settings), None)
        }
        _ => return Err("unsupported file type".into()),
    };
    Ok(Preset { name, group: group.into(), adj, lut, source: Some(path.to_path_buf()) })
}

#[derive(Debug, Clone)]
enum Value {
    Num(f32),
    Text(String),
    List(Vec<f32>),
}

type Settings = HashMap<String, Value>;

fn value_from_str(s: &str) -> Value {
    let t = s.trim();
    match t.trim_start_matches('+').parse::<f32>() {
        Ok(n) => Value::Num(n),
        Err(_) => Value::Text(t.to_string()),
    }
}

fn numbers(s: &str) -> Vec<f32> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|t| t.trim_start_matches('+').parse().ok())
        .collect()
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Extracts `crs:*` settings from an XMP preset, both attribute form
/// (`crs:Exposure2012="+0.50"`) and element form (curves, names).
fn parse_xmp(xml: &str) -> (Option<String>, Settings) {
    let mut settings = Settings::new();
    let mut name = None;
    let mut rest = xml;
    while let Some(i) = rest.find("crs:") {
        let after = &rest[i + 4..];
        let key_len = after.find(|c: char| !c.is_ascii_alphanumeric() && c != '_').unwrap_or(after.len());
        let key = &after[..key_len];
        let tail = &after[key_len..];
        let is_open_tag = rest[..i].ends_with('<');
        if let Some(val) = tail.strip_prefix("=\"") {
            if let Some(end) = val.find('"') {
                settings.insert(key.to_string(), value_from_str(&unescape(&val[..end])));
            }
        } else if is_open_tag && !key.is_empty() {
            let close = format!("</crs:{key}>");
            if let (Some(start), Some(end)) = (tail.find('>'), tail.find(&close)) {
                if start < end {
                    let inner = &tail[start + 1..end];
                    let items = li_items(inner);
                    if key == "Name" {
                        name = items.first().cloned().filter(|s| !s.is_empty());
                    } else if items.is_empty() {
                        settings.insert(key.to_string(), value_from_str(&unescape(inner)));
                    } else {
                        let nums: Vec<f32> = items.iter().flat_map(|s| numbers(s)).collect();
                        settings.insert(key.to_string(), Value::List(nums));
                    }
                    rest = &tail[end..];
                    continue;
                }
            }
        }
        rest = after;
    }
    (name, settings)
}

fn li_items(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = inner;
    while let Some(i) = rest.find("<rdf:li") {
        let after = &rest[i..];
        let Some(gt) = after.find('>') else { break };
        let body = &after[gt + 1..];
        let Some(end) = body.find("</rdf:li>") else { break };
        out.push(unescape(body[..end].trim()));
        rest = &body[end..];
    }
    out
}

/// Extracts settings from a Lightroom Classic `.lrtemplate` (a Lua table).
fn parse_lrtemplate(lua: &str) -> (Option<String>, Settings) {
    let mut settings = Settings::new();
    let name = lua_string_field(lua, "title").or_else(|| lua_string_field(lua, "internalName"));
    let body = lua.find("settings").map(|i| &lua[i..]).unwrap_or(lua);
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !(bytes[i].is_ascii_alphabetic()) || (i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let key = &body[start..i];
        let rest = body[i..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        if let Some(list) = rest.strip_prefix('{') {
            if let Some(end) = list.find('}') {
                let nums = numbers(&list[..end]);
                if !nums.is_empty() {
                    settings.insert(key.to_string(), Value::List(nums));
                }
            }
        } else if let Some(s) = rest.strip_prefix('"') {
            if let Some(end) = s.find('"') {
                settings.insert(key.to_string(), Value::Text(s[..end].to_string()));
            }
        } else {
            let end = rest.find([',', '\n', '}']).unwrap_or(rest.len());
            settings.insert(key.to_string(), value_from_str(&rest[..end]));
        }
    }
    (name, settings)
}

fn lua_string_field(lua: &str, field: &str) -> Option<String> {
    let i = lua.find(&format!("{field} = \""))? + field.len() + 4;
    let end = lua[i..].find('"')?;
    Some(lua[i..i + end].to_string()).filter(|s| !s.is_empty() && !s.starts_with("$$$"))
}

/// Maps Lightroom / Camera Raw setting names onto our sliders.
fn adjustments(s: &Settings) -> Adjustments {
    let num = |keys: &[&str]| {
        keys.iter().find_map(|k| match s.get(*k) {
            Some(Value::Num(n)) => Some(*n),
            _ => None,
        })
    };
    let n = |keys: &[&str]| num(keys).unwrap_or(0.0);
    let text = |k: &str| match s.get(k) {
        Some(Value::Text(t)) => Some(t.to_lowercase()),
        _ => None,
    };
    let curve = |keys: &[&str]| -> Curve {
        keys.iter()
            .find_map(|k| match s.get(*k) {
                Some(Value::List(v)) if v.len() >= 4 => Some(v.chunks_exact(2).map(|c| [c[0], c[1]]).collect()),
                _ => None,
            })
            .unwrap_or_default()
    };

    let mut a = Adjustments {
        exposure: n(&["Exposure2012", "Exposure"]),
        contrast: n(&["Contrast2012", "Contrast"]),
        highlights: n(&["Highlights2012", "HighlightRecovery"]).clamp(-100.0, 100.0),
        shadows: n(&["Shadows2012", "FillLight"]),
        whites: n(&["Whites2012"]),
        blacks: n(&["Blacks2012"]),
        clarity: n(&["Clarity2012", "Clarity"]),
        dehaze: n(&["Dehaze"]),
        vibrance: n(&["Vibrance"]),
        saturation: n(&["Saturation"]),
        // Split Toning (older presets) and its successor, Color Grading.
        tint_balance: n(&["SplitToningBalance", "ColorGradeBalance"]),
        shadow_tint: [
            n(&["SplitToningShadowHue", "ColorGradeShadowHue"]),
            n(&["SplitToningShadowSaturation", "ColorGradeShadowSat"]),
        ],
        highlight_tint: [
            n(&["SplitToningHighlightHue", "ColorGradeHighlightHue"]),
            n(&["SplitToningHighlightSaturation", "ColorGradeHighlightSat"]),
        ],
        midtone_tint: match (n(&["ColorGradeMidtoneSat"]), n(&["ColorGradeGlobalSat"])) {
            // We have one midtone wheel; fold the global wheel in when midtones are unused.
            (0.0, global) if global != 0.0 => [n(&["ColorGradeGlobalHue"]), global],
            (mid, _) => [n(&["ColorGradeMidtoneHue"]), mid],
        },
        vignette: n(&["PostCropVignetteAmount", "VignetteAmount"]),
        grain: n(&["GrainAmount"]),
        grayscale: text("ConvertToGrayscale").is_some_and(|t| t == "true") || text("Treatment").is_some_and(|t| t == "blackandwhite"),
        curve: curve(&["ToneCurvePV2012", "ToneCurve"]),
        curve_rgb: [curve(&["ToneCurvePV2012Red"]), curve(&["ToneCurvePV2012Green"]), curve(&["ToneCurvePV2012Blue"])],
        parametric: [
            n(&["ParametricShadows"]),
            n(&["ParametricDarks"]),
            n(&["ParametricLights"]),
            n(&["ParametricHighlights"]),
        ],
        ..Default::default()
    };

    // White balance: relative sliders (JPEG presets) or absolute Kelvin (RAW presets).
    // Absolute Kelvin is relative to daylight (5500 K); small signed values are treated as
    // Kelvin offsets, which some hand-written presets use.
    let as_shot = text("WhiteBalance").is_some_and(|t| t == "as shot");
    a.temperature = match (num(&["IncrementalTemperature"]), num(&["Temperature"])) {
        (Some(t), _) => t,
        (None, Some(k)) if !as_shot => {
            let kelvin = if k > 1500.0 { k } else { 5500.0 + k };
            (1e6 / 5500.0 - 1e6 / kelvin.max(1500.0)).clamp(-100.0, 100.0)
        }
        _ => 0.0,
    };
    a.tint = match (num(&["IncrementalTint"]), num(&["Tint"])) {
        (Some(t), _) => t,
        (None, Some(t)) if !as_shot => t / 1.5,
        _ => 0.0,
    };

    for (i, band) in BANDS.iter().enumerate() {
        a.hsl[i] = [
            n(&[&format!("HueAdjustment{band}")]),
            n(&[&format!("SaturationAdjustment{band}")]),
            n(&[&format!("LuminanceAdjustment{band}")]),
        ];
        a.gray_mix[i] = n(&[&format!("GrayMixer{band}")]);
    }
    a
}

/// Parses an Adobe / Resolve `.cube` 3D LUT.
fn parse_cube(text: &str) -> Result<(Option<String>, Lut3d), String> {
    let mut size = 0u32;
    let mut title = None;
    let mut min = [0.0f32; 3];
    let mut max = [1.0f32; 3];
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let first = parts.next().unwrap_or_default();
        match first {
            "TITLE" => title = Some(line[5..].trim().trim_matches('"').to_string()).filter(|t| !t.is_empty()),
            "LUT_3D_SIZE" => size = parts.next().and_then(|s| s.parse().ok()).ok_or("bad LUT_3D_SIZE")?,
            "LUT_1D_SIZE" => return Err("1D LUTs are not supported".into()),
            "DOMAIN_MIN" | "DOMAIN_MAX" => {
                let v = numbers(&line[first.len()..]);
                if v.len() == 3 {
                    let t = if first == "DOMAIN_MIN" { &mut min } else { &mut max };
                    *t = [v[0], v[1], v[2]];
                }
            }
            _ if first.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.') => {
                let v = numbers(line);
                if v.len() == 3 {
                    data.push([
                        (v[0] - min[0]) / (max[0] - min[0]),
                        (v[1] - min[1]) / (max[1] - min[1]),
                        (v[2] - min[2]) / (max[2] - min[2]),
                    ]);
                }
            }
            _ => {}
        }
    }
    if size < 2 || data.len() != (size * size * size) as usize {
        return Err(format!("expected {}³ entries, found {}", size, data.len()));
    }
    Ok((title, Lut3d { size, data }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xmp_attributes_and_curves() {
        let xml = r#"<x:xmpmeta><rdf:RDF><rdf:Description crs:Exposure2012="+0.50" crs:Contrast2012="-12"
            crs:ConvertToGrayscale="True" crs:HueAdjustmentBlue="-20">
            <crs:Name><rdf:Alt><rdf:li xml:lang="x-default">My Look</rdf:li></rdf:Alt></crs:Name>
            <crs:ToneCurvePV2012><rdf:Seq><rdf:li>0, 20</rdf:li><rdf:li>255, 240</rdf:li></rdf:Seq></crs:ToneCurvePV2012>
            </rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let (name, s) = parse_xmp(xml);
        let a = adjustments(&s);
        assert_eq!(name.as_deref(), Some("My Look"));
        assert_eq!(a.exposure, 0.5);
        assert_eq!(a.contrast, -12.0);
        assert!(a.grayscale);
        assert_eq!(a.hsl[5][0], -20.0);
        assert_eq!(a.curve, vec![[0.0, 20.0], [255.0, 240.0]]);
    }

    #[test]
    fn parses_lrtemplate() {
        let lua = r#"s = { id = "X", internalName = "Old Look", title = "Old Look",
            value = { settings = { Exposure2012 = 0.3, Vibrance = 15, ToneCurvePV2012 = { 0, 10, 255, 250, }, WhiteBalance = "As Shot", }, uuid = "U", }, version = 0, }"#;
        let (name, s) = parse_lrtemplate(lua);
        let a = adjustments(&s);
        assert_eq!(name.as_deref(), Some("Old Look"));
        assert_eq!(a.exposure, 0.3);
        assert_eq!(a.vibrance, 15.0);
        assert_eq!(a.curve.len(), 2);
    }

    #[test]
    fn parses_cube() {
        let mut text = String::from("TITLE \"Id\"\nLUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    text += &format!("{r} {g} {b}\n");
                }
            }
        }
        let (title, lut) = parse_cube(&text).unwrap();
        assert_eq!(title.as_deref(), Some("Id"));
        assert_eq!(lut.data[1], [1.0, 0.0, 0.0]);
    }
}
