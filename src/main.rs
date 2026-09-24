mod app;
mod browse;
mod cli;
mod crop;
mod export;
mod filmstrip;
mod gpu;
mod import;
mod loader;
mod metadata;
mod palette;
mod preset;
mod recommend;
mod theme;

use std::sync::Arc;

use eframe::{egui, egui_wgpu};

/// Prints how long a step took when `BP_PHOTOS_TIMING` is set.
pub fn timing(what: &str, start: std::time::Instant) {
    if std::env::var_os("BP_PHOTOS_TIMING").is_some() {
        eprintln!("{what}: {:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
    }
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| cli::is_command(a)) {
        if let Err(e) = cli::run(&args) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let initial = args.first().map(std::path::PathBuf::from);

    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.device_descriptor = Arc::new(|adapter| {
        let base = if adapter.get_info().backend == wgpu::Backend::Gl {
            wgpu::Limits::downlevel_webgl2_defaults()
        } else {
            wgpu::Limits::default()
        };
        wgpu::DeviceDescriptor {
            label: Some("bp_photos"),
            // Allow full-resolution photos up to what the GPU supports (usually 16384 px).
            required_limits: wgpu::Limits { max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d, ..base },
            ..Default::default()
        }
    });

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: egui_wgpu::WgpuConfiguration { wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(setup), ..Default::default() },
        viewport: egui::ViewportBuilder::default()
            .with_title("BP Photos")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([800.0, 500.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("BP Photos", options, Box::new(|cc| Ok(Box::new(app::PhotoApp::new(cc, initial)))))
}
