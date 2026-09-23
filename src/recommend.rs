//! Per-photo preset recommendations: render every preset small, then score how well each
//! result suits this photo (clipping, exposure, contrast, skin tones, colour) against the original.

use crate::gpu::{Crop, Gpu, GpuPreset, Readback, Source};
use crate::preset::Preset;

pub const GRID_COLS: u32 = 32;
pub const CELL_MAX: u32 = 96;
pub const COUNT: usize = 12;

/// Grid cell size for a crop of the given pixel size.
pub fn cell_size(px: (u32, u32)) -> (u32, u32) {
    let s = CELL_MAX as f32 / px.0.max(px.1) as f32;
    (((px.0 as f32 * s) as u32).max(8), ((px.1 as f32 * s) as u32).max(8))
}

/// Starts rendering the grid for `src`; hand the result to [`finish`].
pub fn start(gpu: &Gpu, src: &Source, presets: &[GpuPreset], crop: Crop) -> (Readback, (u32, u32)) {
    let cell = cell_size(crate::crop::pixel_size(crop, (src.width, src.height)));
    (gpu.render_grid(src, presets, crop, cell, GRID_COLS), cell)
}

/// Scores a finished grid and picks the recommendations.
pub fn finish(rgba: &[u8], width: u32, cell: (u32, u32), groups: &[String]) -> Vec<Rec> {
    let all = score_grid(rgba, width, cell, GRID_COLS, groups.len());
    pick(&all, rgba, width, cell, GRID_COLS, groups, COUNT)
}

/// Blocking version, for the command line.
pub fn for_photo(
    gpu: &Gpu,
    src: &Source,
    gps: &[GpuPreset],
    crop: Crop,
    presets: &[Preset],
    _verbose: bool,
) -> Result<(Vec<Rec>, (u32, u32)), String> {
    let (readback, cell) = start(gpu, src, gps, crop);
    _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    let rgba = readback.try_take(&gpu.device).ok_or("grid readback failed")?;
    let groups: Vec<String> = presets.iter().map(|p| p.group.clone()).collect();
    Ok((finish(&rgba, readback.width, cell, &groups), cell))
}

/// One scored preset.
#[derive(Clone, Debug)]
pub struct Rec {
    pub index: usize,
    pub score: f32,
    pub reasons: Vec<&'static str>,
}

/// Measurements of one rendered cell.
#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    mean_l: f32,
    std_l: f32,
    /// Fraction of pixels with a channel at the top / all channels at the bottom.
    clip_hi: f32,
    clip_lo: f32,
    mean_sat: f32,
    /// 90th-percentile saturation: how vivid the most colourful areas are.
    p90_sat: f32,
    /// Fraction of vivid pixels at (nearly) full saturation.
    oversat: f32,
    /// Strength of the average colour cast.
    cast: f32,
    /// Over skin-like pixels (from the original): how far hues drift from natural, and saturation.
    skin_hue_off: f32,
    skin_sat: f32,
}

fn hue_sat(r: f32, g: f32, b: f32) -> (f32, f32) {
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let d = mx - mn;
    let s = if mx > 1e-4 { d / mx } else { 0.0 };
    if d < 1e-4 {
        return (0.0, s);
    }
    let h = if mx == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if mx == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s)
}

/// Classic YCbCr skin-tone rule, on 0..255 values.
fn is_skin(p: &[u8]) -> bool {
    let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
    let y = 0.299 * r + 0.587 * g + 0.114 * b;
    let cb = 128.0 - 0.168_736 * r - 0.331_264 * g + 0.5 * b;
    let cr = 128.0 + 0.5 * r - 0.418_688 * g - 0.081_312 * b;
    (77.0..=127.0).contains(&cb) && (133.0..=173.0).contains(&cr) && (40.0..=235.0).contains(&y)
}

fn stats(px: &[&[u8]], skin: &[bool]) -> Stats {
    let n = px.len().max(1) as f32;
    let mut s = Stats::default();
    let (mut sum_l, mut sum_l2, mut cast_a, mut cast_b) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let mut skin_n = 0.0f32;
    let mut sat_hist = [0u32; 64];
    for (i, p) in px.iter().enumerate() {
        let (r, g, b) = (p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
        let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        sum_l += l;
        sum_l2 += l * l;
        if r.max(g).max(b) >= 0.99 {
            s.clip_hi += 1.0;
        }
        if r.max(g).max(b) <= 0.02 {
            s.clip_lo += 1.0;
        }
        let (h, sat) = hue_sat(r, g, b);
        s.mean_sat += sat;
        sat_hist[((sat * 63.0) as usize).min(63)] += 1;
        if sat > 0.92 && r.max(g).max(b) > 0.2 {
            s.oversat += 1.0;
        }
        cast_a += r - g;
        cast_b += b - (r + g) / 2.0;
        if skin[i] {
            skin_n += 1.0;
            // Natural skin hues sit roughly between 5° and 50° (red-orange).
            let off = if (5.0..=50.0).contains(&h) { 0.0 } else if h > 180.0 { (365.0 - h).min(60.0) } else { (h - 50.0).min(60.0) };
            s.skin_hue_off += off / 60.0 * sat.min(0.5) * 2.0;
            s.skin_sat += sat;
        }
    }
    let mut seen = 0u32;
    let target = (n * 0.9) as u32;
    s.p90_sat = sat_hist.iter().position(|&c| {
        seen += c;
        seen >= target
    }).unwrap_or(63) as f32 / 63.0;
    s.mean_l = sum_l / n;
    s.std_l = (sum_l2 / n - s.mean_l * s.mean_l).max(0.0).sqrt();
    s.clip_hi /= n;
    s.clip_lo /= n;
    s.mean_sat /= n;
    s.oversat /= n;
    s.cast = ((cast_a / n).powi(2) + (cast_b / n).powi(2)).sqrt();
    if skin_n > 0.0 {
        s.skin_hue_off /= skin_n;
        s.skin_sat /= skin_n;
    }
    s
}

/// Scores result `r` against original `o`. Higher is better; `reasons` explain the good parts.
fn score(o: &Stats, r: &Stats, skin_frac: f32, diff: f32) -> (f32, Vec<&'static str>) {
    let mut score = 100.0;
    let mut reasons = Vec::new();
    let gray = r.mean_sat < 0.03;

    // Clipping the photo didn't already have.
    let hi = (r.clip_hi - o.clip_hi).max(0.0);
    let lo = (r.clip_lo - o.clip_lo).max(0.0);
    score -= hi * 300.0 + lo * 200.0;
    if hi < 0.002 && o.mean_l > 0.45 {
        reasons.push("keeps highlight detail");
    }
    if lo < 0.002 && o.mean_l < 0.4 {
        reasons.push("keeps shadow detail");
    }

    // Exposure: don't push a dark photo darker or a bright one brighter; avoid extremes.
    if o.mean_l < 0.35 && r.mean_l < o.mean_l {
        score -= (o.mean_l - r.mean_l) * 150.0;
    }
    if o.mean_l > 0.65 && r.mean_l > o.mean_l {
        score -= (r.mean_l - o.mean_l) * 150.0;
    }
    score -= (0.15 - r.mean_l).max(0.0) * 200.0 + (r.mean_l - 0.85).max(0.0) * 200.0;
    if o.mean_l < 0.35 && r.mean_l > o.mean_l + 0.03 {
        reasons.push("brightens a dark photo");
    }

    // Contrast: reward depth on flat photos, penalise harshness.
    if o.std_l < 0.16 {
        let gain = (r.std_l - o.std_l).clamp(-0.05, 0.08);
        score += gain * 150.0;
        if gain > 0.02 {
            reasons.push("adds depth to a flat photo");
        }
    }
    score -= (r.std_l - 0.33).max(0.0) * 200.0;

    // Colour.
    score -= (r.oversat - o.oversat).max(0.0) * 150.0;
    // Big saturation jumps look garish even when few pixels hit the ceiling; they usually hit the
    // already-colourful areas, so judge by those (90th percentile) as well as the average.
    score -= (r.mean_sat - o.mean_sat - 0.12).max(0.0) * 150.0;
    score -= (r.p90_sat - o.p90_sat.max(0.6)).max(0.0) * 120.0;
    if !gray && o.mean_sat < 0.2 {
        let gain = (r.mean_sat - o.mean_sat).clamp(-0.1, 0.1);
        score += gain * 60.0;
        if gain > 0.03 {
            reasons.push("livens up muted colours");
        }
    }
    score -= (r.cast - o.cast.max(0.12)).max(0.0) * 100.0;

    // Skin tones, when the photo has them.
    if skin_frac > 0.03 {
        if gray {
            reasons.push("black & white");
        } else {
            let bad = r.skin_hue_off * 40.0 + (r.skin_sat - 0.65).max(0.0) * 60.0 + (0.12 - r.skin_sat).max(0.0) * 80.0;
            score -= bad;
            // The colour rule also catches wood, sand and clay; only claim skin when there's plenty.
            if bad < 1.0 && skin_frac > 0.08 {
                reasons.insert(0, "natural skin tones");
            }
        }
    } else if gray {
        reasons.push("black & white");
    }

    // Presets that barely change this photo aren't much of a recommendation.
    if diff < 0.015 {
        score -= 8.0;
    }
    // Most specific first: what the look is, then what it does for this photo.
    reasons.sort_by_key(|r| match *r {
        "black & white" => 0,
        "natural skin tones" => 1,
        "adds depth to a flat photo" | "livens up muted colours" | "brightens a dark photo" => 2,
        _ => 3,
    });
    reasons.truncate(2);
    (score, reasons)
}

/// Scores every cell of a preset grid; cell 0 must be the unedited original.
pub fn score_grid(rgba: &[u8], width: u32, cell: (u32, u32), cols: u32, count: usize) -> Vec<Rec> {
    let cell_px = |i: usize| -> Vec<&[u8]> {
        let (x0, y0) = ((i as u32 % cols) * cell.0, (i as u32 / cols) * cell.1);
        let mut out = Vec::with_capacity((cell.0 * cell.1) as usize);
        for y in y0..y0 + cell.1 {
            let row = ((y * width + x0) * 4) as usize;
            out.extend(rgba[row..row + (cell.0 * 4) as usize].chunks_exact(4));
        }
        out
    };
    let original = cell_px(0);
    let skin: Vec<bool> = original.iter().map(|p| is_skin(p)).collect();
    let skin_frac = skin.iter().filter(|s| **s).count() as f32 / skin.len().max(1) as f32;
    let o = stats(&original, &skin);

    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let per = count.div_ceil(threads).max(1);
    let mut recs: Vec<Rec> = std::thread::scope(|s| {
        let handles: Vec<_> = (1..count)
            .collect::<Vec<_>>()
            .chunks(per)
            .map(|chunk| {
                let chunk = chunk.to_vec();
                let (cell_px, original, skin, o) = (&cell_px, &original, &skin, &o);
                s.spawn(move || {
                    chunk
                        .into_iter()
                        .map(|i| {
                            let px = cell_px(i);
                            let diff = px
                                .iter()
                                .zip(original.iter())
                                .map(|(a, b)| a[..3].iter().zip(&b[..3]).map(|(x, y)| x.abs_diff(*y) as f32).sum::<f32>())
                                .sum::<f32>()
                                / (px.len().max(1) as f32 * 3.0 * 255.0);
                            let (score, reasons) = score(o, &stats(&px, skin), skin_frac, diff);
                            Rec { index: i, score, reasons }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    recs.sort_by(|a, b| b.score.total_cmp(&a.score));
    recs
}

/// A 4×4 colour signature of a cell, for telling similar-looking results apart.
fn signature(rgba: &[u8], width: u32, cell: (u32, u32), cols: u32, i: usize) -> [f32; 48] {
    let (x0, y0) = ((i as u32 % cols) * cell.0, (i as u32 / cols) * cell.1);
    let mut sig = [0.0f32; 48];
    let mut counts = [0.0f32; 16];
    for y in 0..cell.1 {
        for x in 0..cell.0 {
            let p = (((y0 + y) * width + x0 + x) * 4) as usize;
            let b = ((y * 4 / cell.1) * 4 + x * 4 / cell.0) as usize;
            for c in 0..3 {
                sig[b * 3 + c] += rgba[p + c] as f32 / 255.0;
            }
            counts[b] += 1.0;
        }
    }
    for b in 0..16 {
        for c in 0..3 {
            sig[b * 3 + c] /= counts[b].max(1.0);
        }
    }
    sig
}

/// Picks the best `k`, skipping near-duplicates and limiting how many come from one group
/// (and how many are black & white).
pub fn pick(
    recs: &[Rec],
    rgba: &[u8],
    width: u32,
    cell: (u32, u32),
    cols: u32,
    groups: &[String],
    k: usize,
) -> Vec<Rec> {
    let mut chosen: Vec<(Rec, [f32; 48])> = Vec::new();
    for rec in recs {
        if chosen.len() == k || rec.score < 60.0 {
            break;
        }
        let sig = signature(rgba, width, cell, cols, rec.index);
        let similar = chosen
            .iter()
            .any(|(_, s)| s.iter().zip(&sig).map(|(a, b)| (a - b).abs()).sum::<f32>() / 48.0 < 0.025);
        let same_group = chosen.iter().filter(|(c, _)| groups[c.index] == groups[rec.index]).count();
        let bw = rec.reasons.contains(&"black & white");
        let bw_count = chosen.iter().filter(|(c, _)| c.reasons.contains(&"black & white")).count();
        if !similar && same_group < 3 && !(bw && bw_count >= 2) {
            chosen.push((rec.clone(), sig));
        }
    }
    chosen.into_iter().map(|(r, _)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(l: f32) -> Stats {
        Stats { mean_l: l, std_l: 0.1, mean_sat: 0.3, ..Default::default() }
    }

    #[test]
    fn penalises_new_clipping() {
        let o = flat(0.5);
        let clipped = Stats { clip_hi: 0.1, ..o };
        assert!(score(&o, &clipped, 0.0, 0.1).0 < score(&o, &o, 0.0, 0.1).0 - 20.0);
    }

    #[test]
    fn dark_photo_prefers_brighter_result() {
        let o = flat(0.25);
        let darker = Stats { mean_l: 0.15, ..o };
        let brighter = Stats { mean_l: 0.32, ..o };
        assert!(score(&o, &brighter, 0.0, 0.1).0 > score(&o, &darker, 0.0, 0.1).0);
    }

    #[test]
    fn skin_shift_hurts_portraits() {
        let o = Stats { skin_sat: 0.35, ..flat(0.5) };
        let green_skin = Stats { skin_hue_off: 0.6, ..o };
        assert!(score(&o, &green_skin, 0.2, 0.1).0 < score(&o, &o, 0.2, 0.1).0 - 15.0);
        assert!(score(&o, &o, 0.2, 0.1).1.contains(&"natural skin tones"));
    }

    #[test]
    fn penalises_garish_saturation_jumps() {
        let o = flat(0.5);
        let garish = Stats { mean_sat: 0.7, p90_sat: 0.95, ..o };
        let lively = Stats { mean_sat: 0.4, p90_sat: 0.6, ..o };
        assert!(score(&o, &garish, 0.0, 0.1).0 < score(&o, &lively, 0.0, 0.1).0 - 20.0);
    }

    #[test]
    fn skin_rule_matches_typical_skin() {
        assert!(is_skin(&[224, 172, 140]));
        assert!(!is_skin(&[40, 120, 220]));
    }
}
