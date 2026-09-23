//! Headless commands: batch-apply a preset, or render a contact sheet of all presets.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::gpu::{FULL_CROP, Gpu};
use crate::{app, export, import, loader, preset};

const USAGE: &str = "usage:
  bp_photos [PHOTO]                                   open the app
  bp_photos apply --preset NAME [--amount 1.0] [--crop X0,Y0,X1,Y1] [--size SIZE] [--fit] [--quality 92] [--no-gps] IN... --out DIR
      --crop    crop edges as fractions 0..1 of width/height
      --size    original, instagram-portrait, instagram-square, instagram-landscape, instagram-story,
                x-post, x-large, facebook, web, or a number (long edge in px)
      --fit     fit inside fixed formats instead of cropping to fill them
      --no-gps  leave the photo's location out of the exported metadata
  bp_photos sheet PHOTO OUT.jpg [FILTER]              contact sheet of every preset (optionally filtered)
  bp_photos presets                                   list installed presets and any that fail to load
  bp_photos recommend PHOTO [SHEET.jpg]               presets recommended for a photo (and a sheet of them)";

pub fn is_command(arg: &str) -> bool {
    matches!(arg, "apply" | "sheet" | "presets" | "bench" | "recommend" | "--help" | "-h")
}

pub fn run(args: &[String]) -> Result<(), String> {
    match args[0].as_str() {
        "apply" => apply(&args[1..]),
        "presets" => {
            let dir = app::presets_dir();
            let (presets, errors) = import::load_dir(&dir);
            let mut groups: Vec<(String, usize)> = Vec::new();
            for p in &presets {
                match groups.iter_mut().find(|(g, _)| *g == p.group) {
                    Some((_, n)) => *n += 1,
                    None => groups.push((p.group.clone(), 1)),
                }
            }
            println!("{}: {} presets in {} groups", dir.display(), presets.len(), groups.len());
            for (g, n) in groups {
                println!("  {n:>4}  {g}");
            }
            for e in &errors {
                println!("  failed: {e}");
            }
            Ok(())
        }
        "recommend" => {
            let photo = args.get(1).ok_or(USAGE)?;
            let gpu = headless_gpu()?;
            let presets = all_presets();
            let gps: Vec<_> = presets.iter().map(|p| gpu.create_preset(p)).collect();
            let src = gpu.upload(&loader::load(Path::new(photo), gpu.max_dim())?);
            let start = Instant::now();
            let (recs, _) = crate::recommend::for_photo(&gpu, &src, &gps, FULL_CROP, &presets, true)?;
            println!("recommended in {} ms (render + score {} presets)", start.elapsed().as_millis(), presets.len());
            for r in &recs {
                let p = &presets[r.index];
                println!("  {:5.1}  {:<40} {:<32} {}", r.score, p.name, p.group, r.reasons.join(", "));
            }
            if let Some(out) = args.get(2) {
                let w = 360;
                let h = (w as f32 * src.height as f32 / src.width as f32) as u32;
                let mut sheet = image::RgbImage::from_pixel(4 * (w + 8), (recs.len() as u32 + 1).div_ceil(4) * (h + 8), image::Rgb([30, 30, 30]));
                for (i, idx) in std::iter::once(0).chain(recs.iter().map(|r| r.index)).enumerate() {
                    let img = gpu.render_image(&src, &gps[idx], 1.0, FULL_CROP, w, h)?;
                    image::imageops::overlay(&mut sheet, &img, ((i as u32 % 4) * (w + 8) + 4) as i64, ((i as u32 / 4) * (h + 8) + 4) as i64);
                }
                app::save_image(&sheet, Path::new(out), 90, None)?;
            }
            Ok(())
        }
        "bench" => {
            // Times the same steps the app does when opening a photo.
            // SAFETY: single-threaded at this point.
            unsafe { std::env::set_var("BP_PHOTOS_TIMING", "1") };
            let gpu = headless_gpu()?;
            let presets = all_presets();
            let t = Instant::now();
            let gps: Vec<_> = presets.iter().map(|p| gpu.create_preset(p)).collect();
            crate::timing(&format!("create {} GPU presets", gps.len()), t);
            let t = Instant::now();
            gpu.warm_up(&gps[1]);
            crate::timing("warm up", t);
            for photo in &args[1..] {
                println!("{photo}");
                let total = Instant::now();
                if let Some(q) = loader::load_quick(Path::new(photo)) {
                    let src = gpu.upload(&q);
                    let preview = gpu.create_target(q.width, q.height, wgpu::TextureUsages::empty());
                    let mut enc = gpu.device.create_command_encoder(&Default::default());
                    gpu.render(&mut enc, &src, &gps[1], &preview, 1.0, FULL_CROP);
                    gpu.queue.submit([enc.finish()]);
                    gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| e.to_string())?;
                    crate::timing(&format!(" => quick preview on screen ({}×{})", q.width, q.height), total);
                }
                let t = Instant::now();
                let decoded = loader::load(Path::new(photo), gpu.max_dim())?;
                crate::timing(" load", t);
                let t = Instant::now();
                let src = gpu.upload(&decoded);
                gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| e.to_string())?;
                crate::timing(" upload + mips", t);
                let full = FULL_CROP;
                let t = Instant::now();
                let preview = gpu.create_target(3072.min(src.width), 3072.min(src.height), wgpu::TextureUsages::empty());
                let mut enc = gpu.device.create_command_encoder(&Default::default());
                gpu.render(&mut enc, &src, &gps[1], &preview, 1.0, full);
                gpu.queue.submit([enc.finish()]);
                gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| e.to_string())?;
                crate::timing(" preview render", t);
                crate::timing(" => first preview", total);
                let t = Instant::now();
                let thumbs: Vec<_> = gps.iter().map(|_| gpu.create_target(280, 187, wgpu::TextureUsages::empty())).collect();
                crate::timing(" create thumb targets", t);
                let t = Instant::now();
                let mut enc = gpu.device.create_command_encoder(&Default::default());
                for (g, th) in gps.iter().zip(&thumbs) {
                    gpu.render(&mut enc, &src, g, th, 1.0, full);
                }
                gpu.queue.submit([enc.finish()]);
                gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| e.to_string())?;
                crate::timing(" render thumbs", t);
                crate::timing(" => everything", total);
            }
            Ok(())
        }
        "sheet" => match &args[1..] {
            [photo, out] => sheet(Path::new(photo), Path::new(out), ""),
            [photo, out, filter] => sheet(Path::new(photo), Path::new(out), filter),
            _ => Err(USAGE.into()),
        },
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}

fn headless_gpu() -> Result<Gpu, String> {
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .map_err(|e| format!("no GPU adapter: {e}"))?;
    let limits = wgpu::Limits { max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d, ..Default::default() };
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_limits: limits, ..Default::default() }))
        .map_err(|e| format!("no GPU device: {e}"))?;
    Ok(Gpu::new(device, queue))
}

fn all_presets() -> Vec<preset::Preset> {
    let mut presets = preset::builtins();
    presets.extend(import::load_dir(&app::presets_dir()).0);
    presets
}

fn apply(args: &[String]) -> Result<(), String> {
    let mut name = None;
    let mut amount = 1.0f32;
    let mut out_dir = None;
    let mut crop = FULL_CROP;
    let mut size = export::Size::Original;
    let mut fill = true;
    let mut quality = 92u8;
    let mut keep_gps = true;
    let mut inputs = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--preset" => name = it.next().cloned(),
            "--amount" => amount = it.next().and_then(|v| v.parse().ok()).ok_or("bad --amount")?,
            "--out" => out_dir = it.next().map(PathBuf::from),
            "--size" => {
                let v = it.next().ok_or("--size needs a value")?;
                size = match v.parse::<u32>() {
                    Ok(n) if n > 0 => export::Size::LongEdge(n),
                    _ => export::find(v).ok_or_else(|| format!("unknown size {v:?}"))?,
                };
            }
            "--fit" => fill = false,
            "--no-gps" => keep_gps = false,
            "--quality" => quality = it.next().and_then(|v| v.parse().ok()).filter(|q| (1..=100).contains(q)).ok_or("--quality needs 1-100")?,
            "--crop" => {
                let v: Vec<f32> = it.next().map(|v| v.split(',').filter_map(|n| n.trim().parse().ok()).collect()).unwrap_or_default();
                let valid = v.len() == 4 && v.iter().all(|n| (0.0..=1.0).contains(n)) && v[0] < v[2] && v[1] < v[3];
                if !valid {
                    return Err("--crop needs X0,Y0,X1,Y1 with 0 <= X0 < X1 <= 1 and 0 <= Y0 < Y1 <= 1".into());
                }
                crop = [v[0], v[1], v[2], v[3]];
            }
            _ => inputs.push(PathBuf::from(a)),
        }
    }
    let (Some(name), Some(out_dir)) = (name, out_dir) else { return Err(USAGE.into()) };
    let preset = match Path::new(&name) {
        p if import::is_preset_file(p) && p.exists() => import::load_file(p, "CLI")?,
        _ => all_presets()
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(&name))
            .ok_or_else(|| format!("no preset named {name:?}"))?,
    };
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let gpu = headless_gpu()?;
    let gp = gpu.create_preset(&preset);
    for input in inputs {
        let start = Instant::now();
        let decoded = loader::load(&input, gpu.max_dim())?;
        let decode_ms = start.elapsed().as_millis();
        let src = gpu.upload(&decoded);
        let plan = export::plan(size, crop, (src.width, src.height), fill);
        let img = gpu.render_image(&src, &gp, amount, plan.crop, plan.render.0, plan.render.1)?;
        let img = app::resize(img, plan.output);
        let render_ms = start.elapsed().as_millis() - decode_ms;
        let stem = input.file_stem().unwrap_or_default().to_string_lossy();
        let dest = out_dir.join(format!("{stem}.jpg"));
        let exif = crate::metadata::for_export(&input, img.dimensions(), keep_gps);
        app::save_image(&img, &dest, quality, exif)?;
        println!("{} → {}  (decode {decode_ms} ms, gpu {render_ms} ms, total {} ms)", input.display(), dest.display(), start.elapsed().as_millis());
    }
    Ok(())
}

fn sheet(photo: &Path, out: &Path, filter: &str) -> Result<(), String> {
    let gpu = headless_gpu()?;
    let decoded = loader::load(photo, gpu.max_dim())?;
    let src = gpu.upload(&decoded);
    let filter = filter.to_lowercase();
    let presets: Vec<_> = all_presets()
        .into_iter()
        .filter(|p| p.name.to_lowercase().contains(&filter) || p.group.to_lowercase().contains(&filter))
        .collect();
    let scale = 360.0 / src.width.max(src.height) as f32;
    let (w, h) = (((src.width as f32 * scale) as u32).max(1), ((src.height as f32 * scale) as u32).max(1));
    let cols = 6u32;
    let rows = (presets.len() as u32).div_ceil(cols);
    let label_h = 22;
    let mut sheet = image::RgbImage::from_pixel(cols * (w + 8), rows * (h + 8 + label_h), image::Rgb([30, 30, 30]));
    let start = Instant::now();
    for (i, p) in presets.iter().enumerate() {
        let img = gpu.render_image(&src, &gpu.create_preset(p), 1.0, FULL_CROP, w, h)?;
        let (x, y) = ((i as u32 % cols) * (w + 8) + 4, (i as u32 / cols) * (h + 8 + label_h) + 4);
        image::imageops::overlay(&mut sheet, &img, x as i64, y as i64);
    }
    println!("rendered {} presets in {} ms", presets.len(), start.elapsed().as_millis());
    for (i, p) in presets.iter().enumerate() {
        println!("  {:>2}: row {} col {}  {}", i, i as u32 / cols, i as u32 % cols, p.name);
    }
    app::save_image(&sheet, out, 92, None)
}
