//! The egui application: preset grid, preview, import and export.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use eframe::egui::{self, Color32, Key, Rect, Sense, Stroke, TextureId, Vec2};

use crate::crop::{self, Handle};
use crate::export;
use crate::gpu::{Crop, FULL_CROP, Gpu, GpuPreset, Source, Target};
use crate::loader::{self, Decoded};
use crate::preset::{self, Preset};
use crate::import;

/// Longest edge of the on-screen preview, in pixels.
const PREVIEW_MAX: u32 = 3072;
/// Longest edge of preset thumbnails, in pixels.
const THUMB_MAX: u32 = 280;
const CELL: f32 = 148.0;

struct Photo {
    path: PathBuf,
    source: Source,
    /// The committed crop, applied to the preview, thumbnails and export.
    crop: Crop,
    /// Showing the embedded preview while the full image decodes.
    provisional: bool,
    /// The crop the preview/original targets were sized for (the full frame while cropping).
    view_crop: Crop,
    preview: Target,
    preview_id: TextureId,
    /// The untouched photo at preview size, for the before/after view.
    original: Target,
    original_id: TextureId,
    /// The crop the thumbnails were rendered with.
    thumb_crop: Crop,
    thumb_size: (u32, u32),
    /// Per preset; created and rendered only once the thumbnail is on screen.
    thumbs: Vec<Option<Thumb>>,
}

struct Thumb {
    target: Target,
    id: TextureId,
    /// Rendered for the current photo and crop.
    fresh: bool,
}

impl Photo {
    fn size(&self) -> (u32, u32) {
        (self.source.width, self.source.height)
    }
}

/// Choices in the export dialog, kept for the session.
struct ExportSettings {
    size: usize,
    custom_edge: u32,
    fill: bool,
    /// 0 = JPEG, 1 = PNG, 2 = TIFF.
    format: usize,
    quality: u8,
    /// Copy the photo's GPS position into the export.
    keep_gps: bool,
}

const FORMATS: [(&str, &str); 3] = [("JPEG", "jpg"), ("PNG", "png"), ("TIFF", "tif")];

/// Zoomed-in view: the photo point (normalised) at the centre, and screen pixels per photo pixel.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Zoom {
    center: [f32; 2],
    scale: f32,
}

/// Deepest pinch zoom, in screen pixels per photo pixel.
const MAX_ZOOM: f32 = 8.0;

/// Render targets for the 100% view: the visible region of the photo, edited and original.
struct ZoomViews {
    size: (u32, u32),
    region: Crop,
    edited: (Target, TextureId),
    original: (Target, TextureId),
}

/// State of the crop tool while it's open.
struct CropEdit {
    /// Crop to restore on cancel.
    before: Crop,
    /// Active drag: handle, crop at drag start, pointer at drag start.
    drag: Option<(Handle, Crop, egui::Pos2)>,
}

/// A decoded image from the loader thread; `quick` marks the low-res embedded preview.
struct LoadResult {
    path: PathBuf,
    result: Result<Decoded, String>,
    start: Instant,
    quick: bool,
}

pub struct PhotoApp {
    gpu: Gpu,
    renderer: std::sync::Arc<egui::mutex::RwLock<eframe::egui_wgpu::Renderer>>,
    presets: Vec<Preset>,
    gpu_presets: Vec<GpuPreset>,
    selected: usize,
    strength: f32,
    photo: Option<Photo>,
    loading: Option<Receiver<LoadResult>>,
    exporting: Option<Receiver<Result<PathBuf, String>>>,
    /// Resized RGBA image on its way to the clipboard.
    copying: Option<Receiver<(u32, u32, Vec<u8>)>>,
    /// Kept for the app's lifetime: on Linux, clipboard contents live only as long as their owner.
    clipboard: Option<arboard::Clipboard>,
    status: String,
    filter: String,
    show_original: bool,
    side_by_side: bool,
    cropping: Option<CropEdit>,
    /// Index into `crop::RATIOS`, and whether it's rotated 90°.
    crop_ratio: usize,
    crop_flip: bool,
    /// When the amount was last changed by scrolling, to show and fade its overlay.
    amount_changed_at: Option<f64>,
    export_dialog: bool,
    export_settings: ExportSettings,
    preview_dirty: bool,
    original_dirty: bool,
    /// Thumbnails that became visible and need rendering this frame.
    thumb_queue: Vec<usize>,
    /// `BP_PHOTOS_SCREENSHOT=out.png`: once the photo and recommendations are ready, select the
    /// top pick, capture the window and quit (used for the README screenshot).
    screenshot: Option<(PathBuf, u32)>,
    /// Preset cells as laid out last frame (preset, section id, rect), for arrow-key navigation.
    nav_cells: Vec<(usize, String, Rect)>,
    /// Section the selection was made in (a preset can appear in "Recommended" and its group).
    selected_section: String,
    egui_ctx: egui::Context,
    /// Zoomed view (100% via Z/double-click, any level via pinch); `None` = fit to window.
    zoom: Option<Zoom>,
    /// Drag-to-pan: pointer position and zoom centre when the drag began.
    zoom_drag: Option<(egui::Pos2, [f32; 2])>,
    zoom_views: Option<ZoomViews>,
    zoom_dirty: bool,
    /// The folder of the current photo, for stepping to the previous/next one.
    browser: crate::browse::Browser,
    /// The photo most recently asked for (it may still be loading).
    target: Option<PathBuf>,
    filmstrip: crate::filmstrip::Filmstrip,
    palette: crate::palette::Palette,
    show_filmstrip: bool,
    /// The photo the filmstrip last scrolled to, so it follows the current photo.
    filmstrip_at: Option<PathBuf>,
    /// Presets recommended for the current photo, best first.
    recs: Vec<crate::recommend::Rec>,
    /// Recommendations need recomputing (new photo, crop or presets).
    recs_dirty: bool,
    /// GPU render of every preset in flight, then CPU scoring in a thread.
    recs_job: Option<(crate::gpu::Readback, (u32, u32))>,
    recs_rx: Option<Receiver<Vec<crate::recommend::Rec>>>,
    /// When the photo being shown was opened, until its first frame is rendered (for timing).
    opened_at: Option<(Instant, bool)>,
    scroll_to_selected: bool,
    presets_dir: PathBuf,
}

pub fn presets_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("bp_photos").join("presets")
}

fn fit(size: (u32, u32), max: u32) -> (u32, u32) {
    let (w, h) = size;
    let scale = (max as f32 / w.max(h) as f32).min(1.0);
    (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1))
}

fn fit_rect(outer: Rect, size: (u32, u32)) -> Rect {
    let aspect = size.0 as f32 / size.1 as f32;
    let mut s = outer.size();
    if s.x / s.y > aspect {
        s.x = s.y * aspect;
    } else {
        s.y = s.x / aspect;
    }
    Rect::from_center_size(outer.center(), s)
}

impl PhotoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        let rs = cc.wgpu_render_state.as_ref().expect("the wgpu renderer is required");
        let gpu = Gpu::new(rs.device.clone(), rs.queue.clone());
        let presets_dir = presets_dir();
        let look = crate::theme::apply_ghostty(&cc.egui_ctx);
        let mut app = Self {
            gpu,
            renderer: rs.renderer.clone(),
            presets: Vec::new(),
            gpu_presets: Vec::new(),
            selected: 0,
            strength: 1.0,
            photo: None,
            loading: None,
            exporting: None,
            copying: None,
            clipboard: None,
            status: match &look {
                Some(l) => format!("Open or drop a photo to start. ({})", l.summary),
                None => "Open or drop a photo to start.".into(),
            },
            filter: String::new(),
            show_original: false,
            side_by_side: false,
            cropping: None,
            crop_ratio: 0,
            crop_flip: false,
            amount_changed_at: None,
            export_dialog: false,
            export_settings: ExportSettings { size: 0, custom_edge: 2560, fill: true, format: 0, quality: 92, keep_gps: true },
            preview_dirty: false,
            original_dirty: false,
            thumb_queue: Vec::new(),
            opened_at: None,
            recs: Vec::new(),
            egui_ctx: cc.egui_ctx.clone(),
            zoom: None,
            zoom_drag: None,
            zoom_views: None,
            zoom_dirty: false,
            browser: crate::browse::Browser::new(),
            target: None,
            filmstrip: crate::filmstrip::Filmstrip::new(),
            palette: Default::default(),
            show_filmstrip: cc.egui_ctx.data_mut(|d| d.get_persisted(egui::Id::new("show_filmstrip")).unwrap_or(false)),
            filmstrip_at: None,
            nav_cells: Vec::new(),
            screenshot: std::env::var_os("BP_PHOTOS_SCREENSHOT").map(|p| (PathBuf::from(p), 0)),
            selected_section: String::new(),
            recs_dirty: false,
            recs_job: None,
            recs_rx: None,
            scroll_to_selected: false,
            presets_dir,
        };
        app.add_presets(preset::builtins());
        app.gpu.warm_up(&app.gpu_presets[1]);
        let (user, errors) = import::load_dir(&app.presets_dir);
        app.add_presets(user);
        if !errors.is_empty() {
            app.status = format!("Skipped {} preset file(s): {}", errors.len(), errors[0]);
        }
        if let Some(path) = initial {
            app.open(path, &cc.egui_ctx);
        }
        app
    }

    fn add_presets(&mut self, presets: Vec<Preset>) {
        for p in presets {
            self.gpu_presets.push(self.gpu.create_preset(&p));
            self.presets.push(p);
        }
        self.recs_dirty = true;
    }

    fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.browser.index(&path);
        self.browser.pending = None;
        self.target = Some(path.clone());
        let (tx, rx) = channel();
        let max = self.gpu.max_dim();
        let ctx = ctx.clone();
        self.status = format!("Loading {}…", path.display());
        std::thread::spawn(move || {
            let start = Instant::now();
            if let Some(quick) = loader::load_quick(&path) {
                _ = tx.send(LoadResult { path: path.clone(), result: Ok(quick), start, quick: true });
                ctx.request_repaint();
            }
            let result = loader::load(&path, max);
            _ = tx.send(LoadResult { path, result, start, quick: false });
            ctx.request_repaint();
        });
        self.loading = Some(rx);
    }

    fn zoom_available(&self) -> bool {
        self.photo.as_ref().is_some_and(|p| !p.provisional) && self.cropping.is_none()
    }

    /// Turns the 100% view on (centred on `at`, or the middle of the crop) or off.
    fn toggle_zoom(&mut self, at: Option<[f32; 2]>) {
        if self.zoom.is_some() {
            self.zoom = None;
            self.zoom_drag = None;
            if let Some(z) = self.zoom_views.take() {
                let mut renderer = self.renderer.write();
                renderer.free_texture(&z.edited.1);
                renderer.free_texture(&z.original.1);
            }
        } else if self.zoom_available()
            && let Some(photo) = &self.photo
        {
            let c = photo.crop;
            let center = at.unwrap_or([(c[0] + c[2]) / 2.0, (c[1] + c[3]) / 2.0]);
            self.zoom = Some(Zoom { center, scale: 1.0 });
        }
    }

    /// Where zoomed views go: the whole area, or two side-by-side halves for before/after.
    fn zoom_cells(&self, rect: Rect) -> Vec<Rect> {
        let area = rect.shrink(12.0);
        if self.side_by_side {
            let w = (area.width() - 12.0) / 2.0;
            let left = Rect::from_min_size(area.min, Vec2::new(w, area.height()));
            vec![left, left.translate(Vec2::new(w + 12.0, 0.0))]
        } else {
            vec![area]
        }
    }

    /// Screen pixels per photo pixel at which the (cropped) photo just fits a zoom cell.
    fn fit_scale(&self, cell: Rect, ppp: f32) -> f32 {
        let photo = self.photo.as_ref().unwrap();
        let (w, h) = crop::pixel_size(photo.crop, photo.size());
        (cell.width() * ppp / w as f32).min(cell.height() * ppp / h as f32)
    }

    /// Pinch (or ⌘/Ctrl + scroll) over the photo: zooms about the pointer, from fit up to 800%.
    /// `fitted` are the on-screen photo rects (and crops they show) when not zoomed.
    fn handle_pinch(&mut self, ui: &egui::Ui, rect: Rect, response: &egui::Response, fitted: &[(Rect, Crop)]) {
        let pinch = ui.input(|i| i.zoom_delta());
        let Some(cursor) = response.hover_pos() else { return };
        if (pinch - 1.0).abs() < 1e-4 || !self.zoom_available() {
            return;
        }
        let ppp = ui.ctx().pixels_per_point();
        let size = self.photo.as_ref().unwrap().size();
        let cells = self.zoom_cells(rect);
        let cell = cells.iter().copied().find(|c| c.contains(cursor)).unwrap_or(*cells.last().unwrap());
        let fit = self.fit_scale(cell, ppp);
        let current = self.zoom.map_or(fit, |z| z.scale);
        let scale = (current * pinch).min(MAX_ZOOM);
        if scale <= fit {
            if self.zoom.is_some() {
                self.toggle_zoom(None);
            }
            return;
        }
        // The photo point under the pointer stays under it.
        let offset = (cursor - cell.center()) * ppp;
        let under = match self.zoom {
            Some(z) => [z.center[0] + offset.x / z.scale / size.0 as f32, z.center[1] + offset.y / z.scale / size.1 as f32],
            None => {
                let Some((r, c)) = fitted.iter().find(|(r, _)| r.contains(cursor)) else { return };
                let u = (cursor - r.min) / r.size();
                [c[0] + u.x * (c[2] - c[0]), c[1] + u.y * (c[3] - c[1])]
            }
        };
        let center = [under[0] - offset.x / scale / size.0 as f32, under[1] - offset.y / scale / size.1 as f32];
        self.zoom = Some(Zoom { center, scale });
    }

    /// The zoomed view (1:1 at 100%), dragged to pan.
    fn zoom_view(&mut self, ui: &mut egui::Ui, rect: Rect, response: &egui::Response) -> Rect {
        let photo = self.photo.as_ref().unwrap();
        let (size, crop) = (photo.size(), photo.crop);
        let ppp = ui.ctx().pixels_per_point();
        let cells = self.zoom_cells(rect);

        // Pan: dragging moves the photo with the pointer.
        let Zoom { mut center, scale } = self.zoom.unwrap();
        if response.drag_started()
            && let Some(p) = ui.input(|i| i.pointer.press_origin())
        {
            self.zoom_drag = Some((p, center));
        }
        if let Some((origin, start)) = self.zoom_drag {
            if let Some(p) = response.interact_pointer_pos() {
                let d = (p - origin) * ppp / scale;
                center = [start[0] - d.x / size.0 as f32, start[1] - d.y / size.1 as f32];
            }
            if !response.dragged() {
                self.zoom_drag = None;
            }
        }
        let cell_px = (cells[0].width() * ppp, cells[0].height() * ppp);
        let (region, center) = zoom_region(center, cell_px, size, crop, scale);
        self.zoom = Some(Zoom { center, scale });
        // Target pixels = screen pixels, so 100% is exactly one photo pixel per screen pixel.
        let px = (
            (((region[2] - region[0]) * size.0 as f32 * scale).round() as u32).max(1),
            (((region[3] - region[1]) * size.1 as f32 * scale).round() as u32).max(1),
        );

        if self.zoom_views.as_ref().is_none_or(|z| z.size != px) {
            let (edited, original) = (self.new_view(px), self.new_view(px));
            if let Some(old) = self.zoom_views.replace(ZoomViews { size: px, region, edited, original }) {
                let mut renderer = self.renderer.write();
                renderer.free_texture(&old.edited.1);
                renderer.free_texture(&old.original.1);
            }
            self.zoom_dirty = true;
        }
        let z = self.zoom_views.as_mut().unwrap();
        if z.region != region {
            z.region = region;
            self.zoom_dirty = true;
        }

        let painter = ui.painter_at(rect);
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let snap = |p: egui::Pos2| egui::pos2((p.x * ppp).round() / ppp, (p.y * ppp).round() / ppp);
        let shown = |cell: Rect| {
            let r = Rect::from_center_size(cell.center(), Vec2::new(px.0 as f32, px.1 as f32) / ppp);
            Rect::from_min_size(snap(r.min), r.size())
        };
        let name = &self.presets[self.selected].name;
        let after = shown(*cells.last().unwrap());
        painter.image(z.edited.1, after, uv, Color32::WHITE);
        if self.side_by_side {
            painter.image(z.original.1, shown(cells[0]), uv, Color32::WHITE);
        }
        let pct = format!("{:.0}%", scale * 100.0);
        if self.side_by_side {
            badge(&painter, shown(cells[0]), &format!("Before · {pct}"));
            badge(&painter, after, &format!("{name} · {pct}"));
        } else if self.show_original {
            badge(&painter, after, &format!("Original · {pct}"));
        } else {
            badge(&painter, after, &pct);
        }
        ui.ctx().set_cursor_icon(if self.zoom_drag.is_some() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        after
    }

    /// Steps to the previous (-1) or next (+1) photo in the folder. Preloaded neighbours show
    /// immediately; one still preloading is picked up when it's ready.
    fn navigate(&mut self, delta: isize, ctx: &egui::Context) {
        let Some(current) = self.target.clone() else { return };
        if let Some(next) = self.browser.neighbour(&current, delta) {
            self.goto(next, ctx);
        }
    }

    /// Shows another photo from the current folder, using its preload if there is one.
    fn goto(&mut self, next: PathBuf, ctx: &egui::Context) {
        if self.target.as_ref() == Some(&next) {
            return;
        }
        self.target = Some(next.clone());
        self.loading = None;
        self.browser.pending = None;
        if let Some(decoded) = self.browser.take(&next) {
            self.finish_load(next, decoded, Instant::now(), false);
        } else if self.browser.is_inflight(&next) {
            self.status = format!("Loading {}…", next.display());
            self.browser.pending = Some(next);
        } else {
            self.open(next, ctx);
        }
    }

    fn run_command(&mut self, command: crate::palette::Command, ctx: &egui::Context) {
        use crate::palette::Command;
        match command {
            Command::OpenFile => {
                let mut exts: Vec<&str> = loader::IMAGE_EXTENSIONS.to_vec();
                exts.extend(loader::HEIF_EXTENSIONS);
                exts.extend(loader::RAW_EXTENSIONS);
                if let Some(path) = rfd::FileDialog::new().add_filter("Photos", &exts).pick_file() {
                    self.open(path, ctx);
                }
            }
            Command::OpenFolder => {
                let Some(dir) = rfd::FileDialog::new().pick_folder() else { return };
                match crate::browse::list_photos(&dir).into_iter().next() {
                    Some(first) => {
                        self.open(first, ctx);
                        self.set_filmstrip(ctx, true);
                    }
                    None => self.status = format!("No photos in {}", dir.display()),
                }
            }
            // Same as ⌘C: the edited photo at the export dialog's size and crop.
            Command::CopyToClipboard => {
                if !self.photo.as_ref().is_some_and(|p| !p.provisional) {
                    self.status = "Open a photo first (or wait for it to finish loading).".into();
                } else if self.copying.is_none()
                    && let Some(plan) = self.export_plan()
                {
                    self.copy_to_clipboard(plan);
                }
            }
        }
    }

    /// Shortcuts that work everywhere: the palette itself, and the commands in it.
    fn global_shortcuts(&mut self, ctx: &egui::Context) {
        use crate::palette::Command;
        use egui::{KeyboardShortcut, Modifiers};
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let (palette, palette_k, open_folder, open_file) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&KeyboardShortcut::new(cmd_shift, Key::P)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::K)),
                i.consume_shortcut(&KeyboardShortcut::new(cmd_shift, Key::O)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::O)),
            )
        });
        if palette || palette_k {
            self.palette.toggle();
        } else if open_folder {
            self.run_command(Command::OpenFolder, ctx);
        } else if open_file {
            self.run_command(Command::OpenFile, ctx);
        }
    }

    fn set_filmstrip(&mut self, ctx: &egui::Context, show: bool) {
        self.show_filmstrip = show;
        self.filmstrip_at = None; // scroll to the current photo when it appears
        ctx.data_mut(|d| d.insert_persisted(egui::Id::new("show_filmstrip"), show));
    }

    /// The folder's photos as thumbnails; the current one highlighted, click to open.
    fn filmstrip_panel(&mut self, ui: &mut egui::Ui) {
        let files = self.browser.files.clone();
        let current = self.target.as_ref().and_then(|t| self.browser.position(t));
        self.filmstrip.update(ui.ctx(), &files, current);
        if files.is_empty() {
            ui.label(egui::RichText::new("Open a photo to see the rest of its folder here.").weak());
            return;
        }
        let follow = self.target != self.filmstrip_at;
        self.filmstrip_at = self.target.clone();
        let (cell, label_h) = (Vec2::new(116.0, 92.0), 16.0);
        let mut clicked = None;
        egui::ScrollArea::horizontal().auto_shrink([false, true]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for (i, path) in files.iter().enumerate() {
                    let (rect, response) = ui.allocate_exact_size(cell, Sense::click());
                    let is_current = current == Some(i);
                    if is_current && follow {
                        response.scroll_to_me(Some(egui::Align::Center));
                    }
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    let painter = ui.painter_at(rect.expand(2.0));
                    let img_rect = Rect::from_min_size(rect.min, Vec2::new(cell.x, cell.y - label_h));
                    painter.rect_filled(img_rect, 4.0, ui.visuals().extreme_bg_color);
                    if let Some((texture, [w, h])) = self.filmstrip.get(path) {
                        let r = fit_rect(img_rect.shrink(2.0), (w as u32, h as u32));
                        painter.image(texture.id(), r, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                    } else {
                        let text = if self.filmstrip.failed(path) { "?" } else { "…" };
                        painter.text(img_rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(14.0), ui.visuals().weak_text_color());
                    }
                    let stroke = if is_current {
                        Stroke::new(2.0, ui.visuals().selection.bg_fill)
                    } else if response.hovered() {
                        Stroke::new(1.0, ui.visuals().widgets.hovered.fg_stroke.color)
                    } else {
                        Stroke::NONE
                    };
                    painter.rect_stroke(img_rect, 4.0, stroke, egui::StrokeKind::Outside);
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let color = if is_current { ui.visuals().strong_text_color() } else { ui.visuals().weak_text_color() };
                    let galley = painter.layout(name.clone(), egui::FontId::proportional(10.5), color, cell.x);
                    let pos = egui::pos2(rect.center().x - galley.size().x.min(cell.x) / 2.0, img_rect.max.y + 2.0);
                    painter.with_clip_rect(rect).galley(pos, galley, Color32::WHITE);
                    if response.on_hover_text(name).clicked() {
                        clicked = Some(path.clone());
                    }
                }
            });
        });
        if let Some(path) = clicked {
            self.goto(path, &ui.ctx().clone());
        }
    }

    /// Creates a render target and registers it with egui.
    fn new_view(&self, size: (u32, u32)) -> (Target, TextureId) {
        let t = self.gpu.create_target(size.0, size.1, wgpu::TextureUsages::empty());
        let id = self.renderer.write().register_native_texture(&self.gpu.device, &t.display_view, wgpu::FilterMode::Linear);
        (t, id)
    }

    fn finish_load(&mut self, path: PathBuf, decoded: Decoded, start: Instant, provisional: bool) {
        let source = self.gpu.upload(&decoded);
        let size = (decoded.width, decoded.height);
        // Upgrading the embedded preview to the full image keeps the crop (and crop tool) as is.
        let upgrade = self.photo.as_ref().filter(|p| p.provisional && p.path == path).map(|p| p.crop);
        if upgrade.is_none() {
            self.cropping = None;
            self.recs.clear();
        }
        if let Some(old) = self.photo.take() {
            let mut renderer = self.renderer.write();
            renderer.free_texture(&old.preview_id);
            renderer.free_texture(&old.original_id);
            for t in old.thumbs.into_iter().flatten() {
                renderer.free_texture(&t.id);
            }
        }
        let (preview, preview_id) = self.new_view(fit(size, PREVIEW_MAX));
        let (original, original_id) = self.new_view(fit(size, PREVIEW_MAX));
        self.photo = Some(Photo {
            path: path.clone(),
            source,
            crop: upgrade.unwrap_or(FULL_CROP),
            provisional,
            view_crop: FULL_CROP,
            preview,
            preview_id,
            original,
            original_id,
            thumb_crop: FULL_CROP,
            thumb_size: fit(size, THUMB_MAX),
            thumbs: Vec::new(),
        });
        self.preview_dirty = true;
        self.original_dirty = true;
        self.recs_dirty = true;
        self.zoom_dirty = true;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let ms = start.elapsed().as_millis();
        self.status = if provisional {
            format!("{name} — preview in {ms} ms, loading full resolution…")
        } else {
            format!("{name} — {}×{} — loaded in {ms} ms", size.0, size.1)
        };
        self.opened_at = Some((start, provisional));
        if !provisional {
            let ctx = self.egui_ctx.clone();
            self.browser.prefetch(&path, self.gpu.max_dim(), &ctx);
        }
    }

    /// Resizes the preview and thumbnail targets when the displayed crop changes.
    fn sync_targets(&mut self) {
        let cropping = self.cropping.is_some();
        let Some(photo) = &self.photo else { return };
        let (size, crop, view_crop, thumb_crop) = (photo.size(), photo.crop, photo.view_crop, photo.thumb_crop);
        // While cropping, show the whole frame so there is something to crop into.
        let want = if cropping { FULL_CROP } else { crop };
        if view_crop != want {
            let view_size = fit(crop::pixel_size(want, size), PREVIEW_MAX);
            let (preview, preview_id) = self.new_view(view_size);
            let (original, original_id) = self.new_view(view_size);
            let photo = self.photo.as_mut().unwrap();
            let mut renderer = self.renderer.write();
            renderer.free_texture(&photo.preview_id);
            renderer.free_texture(&photo.original_id);
            (photo.preview, photo.preview_id, photo.original, photo.original_id) = (preview, preview_id, original, original_id);
            photo.view_crop = want;
            self.preview_dirty = true;
            self.original_dirty = true;
        }
        if !cropping && thumb_crop != crop {
            let photo = self.photo.as_mut().unwrap();
            let mut renderer = self.renderer.write();
            for t in photo.thumbs.drain(..).flatten() {
                renderer.free_texture(&t.id);
            }
            photo.thumb_size = fit(crop::pixel_size(crop, size), THUMB_MAX);
            photo.thumb_crop = crop;
            self.recs_dirty = true;
        }
    }

    /// Starts (re)computing recommendations when needed; one job at a time.
    fn start_recommendations(&mut self) {
        if !self.recs_dirty || self.recs_job.is_some() || self.recs_rx.is_some() || self.cropping.is_some() {
            return;
        }
        let Some(photo) = &self.photo else { return };
        self.recs_dirty = false;
        self.recs_job = Some(crate::recommend::start(&self.gpu, &photo.source, &self.gpu_presets, photo.crop));
    }

    fn poll_recommendations(&mut self, ctx: &egui::Context) {
        if let Some((readback, cell)) = &self.recs_job
            && let Some(rgba) = readback.try_take(&self.gpu.device)
        {
            let (width, cell) = (readback.width, *cell);
            let groups: Vec<String> = self.presets.iter().map(|p| p.group.clone()).collect();
            let (tx, rx) = channel();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let t = Instant::now();
                _ = tx.send(crate::recommend::finish(&rgba, width, cell, &groups));
                crate::timing("score presets", t);
                ctx.request_repaint();
            });
            self.recs_job = None;
            self.recs_rx = Some(rx);
        }
        if let Some(rx) = &self.recs_rx
            && let Ok(recs) = rx.try_recv()
        {
            self.recs = recs;
            self.recs_rx = None;
        }
        if self.recs_job.is_some() || self.recs_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    /// Renders whatever is out of date, in a single GPU submission.
    fn render(&mut self) {
        self.sync_targets();
        if self.photo.is_none() || !(self.preview_dirty || self.original_dirty || self.zoom_dirty || !self.thumb_queue.is_empty()) {
            return;
        }
        let photo = self.photo.as_ref().unwrap();
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        let thumbs = self.thumb_queue.len();
        for i in self.thumb_queue.drain(..) {
            if let Some(Some(t)) = photo.thumbs.get(i) {
                self.gpu.render(&mut encoder, &photo.source, &self.gpu_presets[i], &t.target, 1.0, photo.thumb_crop);
            }
        }
        if self.original_dirty {
            self.gpu.render(&mut encoder, &photo.source, &self.gpu_presets[0], &photo.original, 0.0, photo.view_crop);
        }
        let strength = if self.show_original { 0.0 } else { self.strength };
        let preset = &self.gpu_presets[self.selected];
        self.gpu.render(&mut encoder, &photo.source, preset, &photo.preview, strength, photo.view_crop);
        if let Some(z) = &self.zoom_views
            && (self.zoom_dirty || self.preview_dirty)
        {
            self.gpu.render(&mut encoder, &photo.source, preset, &z.edited.0, strength, z.region);
            if self.zoom_dirty {
                self.gpu.render(&mut encoder, &photo.source, &self.gpu_presets[0], &z.original.0, 0.0, z.region);
            }
        }
        self.zoom_dirty = false;
        self.gpu.queue.submit([encoder.finish()]);
        if let Some((start, quick)) = self.opened_at.take() {
            let what = if quick { "open → quick preview frame" } else { "open → full image frame" };
            crate::timing(&format!("{what} ({thumbs} thumbnails)"), start);
        }
        self.preview_dirty = false;
        self.original_dirty = false;
    }

    fn start_crop(&mut self) {
        if self.zoom.is_some() {
            self.toggle_zoom(None);
        }
        if let Some(photo) = &self.photo {
            self.cropping = Some(CropEdit { before: photo.crop, drag: None });
            self.side_by_side = false;
        }
    }

    fn finish_crop(&mut self, keep: bool) {
        let Some(edit) = self.cropping.take() else { return };
        let Some(photo) = &mut self.photo else { return };
        if !keep {
            photo.crop = edit.before;
        }
        let (w, h) = crop::pixel_size(photo.crop, photo.size());
        self.status = if photo.crop == FULL_CROP { "No crop.".into() } else { format!("Cropped to {w}×{h}.") };
    }

    /// Normalised w/h ratio of the selected aspect preset, if one is locked.
    fn crop_lock(&self) -> Option<f32> {
        let size = self.photo.as_ref()?.size();
        crop::pixel_ratio(self.crop_ratio, self.crop_flip, size).map(|r| crop::normalised_ratio(r, size))
    }

    fn apply_crop_ratio(&mut self) {
        if let Some(r) = self.crop_lock()
            && let Some(photo) = &mut self.photo
        {
            photo.crop = crop::fit_ratio(photo.crop, r);
        }
    }

    fn crop_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Crop").strong());
            ui.separator();
            let mut changed = false;
            for (i, (label, ..)) in crop::RATIOS.iter().enumerate() {
                changed |= ui.selectable_value(&mut self.crop_ratio, i, *label).changed();
            }
            let fixed = crop::RATIOS[self.crop_ratio].1 != 0 && crop::RATIOS[self.crop_ratio].1 != crop::RATIOS[self.crop_ratio].2;
            if ui.add_enabled(fixed, egui::Button::new("⟲ Rotate")).on_hover_text("Swap portrait / landscape (X)").clicked() {
                self.crop_flip = !self.crop_flip;
                changed = true;
            }
            if changed {
                self.apply_crop_ratio();
            }
            ui.separator();
            if ui.button("Reset").clicked()
                && let Some(photo) = &mut self.photo
            {
                photo.crop = FULL_CROP;
                self.apply_crop_ratio();
            }
            if let Some(photo) = &self.photo {
                let (w, h) = crop::pixel_size(photo.crop, photo.size());
                ui.label(egui::RichText::new(format!("{w} × {h} px")).weak());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✔ Done").on_hover_text("Enter").clicked() {
                    self.finish_crop(true);
                }
                if ui.button("Cancel").on_hover_text("Esc").clicked() {
                    self.finish_crop(false);
                }
            });
        });
    }

    /// Crop overlay and handles over the full-frame preview.
    fn crop_view(&mut self, ui: &mut egui::Ui, rect: Rect, response: &egui::Response) {
        let lock = self.crop_lock();
        let (Some(photo), Some(edit)) = (&mut self.photo, &mut self.cropping) else { return };
        let painter = ui.painter_at(rect);
        let img = fit_rect(rect.shrink(24.0), photo.size());
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        painter.image(photo.preview_id, img, uv, Color32::WHITE);

        let to_screen = |c: Crop| {
            Rect::from_min_max(
                img.min + Vec2::new(c[0] * img.width(), c[1] * img.height()),
                img.min + Vec2::new(c[2] * img.width(), c[3] * img.height()),
            )
        };
        let hit = |c: Crop, p: egui::Pos2| {
            let r = to_screen(c);
            crop::hit_test([r.min.x, r.min.y, r.max.x, r.max.y], [p.x, p.y], 10.0)
        };

        // Dragging.
        if response.drag_started()
            && let Some(p) = ui.input(|i| i.pointer.press_origin())
        {
            edit.drag = hit(photo.crop, p).map(|h| (h, photo.crop, p));
        }
        if let Some((handle, start, origin)) = edit.drag {
            if let Some(p) = response.interact_pointer_pos() {
                let d = [(p.x - origin.x) / img.width(), (p.y - origin.y) / img.height()];
                photo.crop = crop::drag(start, handle, d, lock);
            }
            if !response.dragged() {
                edit.drag = None;
            }
        }

        // Cursor feedback.
        let active = edit.drag.map(|(h, ..)| h).or_else(|| response.hover_pos().and_then(|p| hit(photo.crop, p)));
        if let Some(h) = active {
            ui.ctx().set_cursor_icon(match h {
                Handle::Move => egui::CursorIcon::Grab,
                Handle::N | Handle::S => egui::CursorIcon::ResizeVertical,
                Handle::E | Handle::W => egui::CursorIcon::ResizeHorizontal,
                Handle::NW | Handle::SE => egui::CursorIcon::ResizeNwSe,
                Handle::NE | Handle::SW => egui::CursorIcon::ResizeNeSw,
            });
        }

        // Shade outside the crop, then the frame, thirds and corner handles.
        let c = to_screen(photo.crop);
        let shade = Color32::from_black_alpha(160);
        for r in [
            Rect::from_min_max(img.min, egui::pos2(img.max.x, c.min.y)),
            Rect::from_min_max(egui::pos2(img.min.x, c.max.y), img.max),
            Rect::from_min_max(egui::pos2(img.min.x, c.min.y), egui::pos2(c.min.x, c.max.y)),
            Rect::from_min_max(egui::pos2(c.max.x, c.min.y), egui::pos2(img.max.x, c.max.y)),
        ] {
            painter.rect_filled(r, 0.0, shade);
        }
        let thin = Stroke::new(1.0, Color32::from_white_alpha(110));
        for i in 1..3 {
            let f = i as f32 / 3.0;
            painter.line_segment([egui::pos2(c.min.x + c.width() * f, c.min.y), egui::pos2(c.min.x + c.width() * f, c.max.y)], thin);
            painter.line_segment([egui::pos2(c.min.x, c.min.y + c.height() * f), egui::pos2(c.max.x, c.min.y + c.height() * f)], thin);
        }
        painter.rect_stroke(c, 0.0, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Middle);
        let (len, thick) = (16.0_f32.min(c.width() / 3.0).min(c.height() / 3.0), Stroke::new(4.0, Color32::WHITE));
        for (corner, dx, dy) in [(c.left_top(), 1.0, 1.0), (c.right_top(), -1.0, 1.0), (c.left_bottom(), 1.0, -1.0), (c.right_bottom(), -1.0, -1.0)] {
            painter.line_segment([corner, corner + Vec2::new(dx * len, 0.0)], thick);
            painter.line_segment([corner, corner + Vec2::new(0.0, dy * len)], thick);
        }

        if response.double_clicked() && ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| c.contains(p)) {
            self.finish_crop(true);
        }
    }

    fn select(&mut self, index: usize) {
        if index != self.selected {
            self.selected = index;
            self.preview_dirty = true;
        }
    }

    /// Copies preset files into the presets folder (so they persist) and loads them.
    fn import_paths(&mut self, paths: Vec<PathBuf>) {
        let mut added = Vec::new();
        let mut errors = Vec::new();
        for path in paths {
            let (dest_dir, files) = if path.is_dir() {
                let name = path.file_name().map(|n| n.to_owned()).unwrap_or_else(|| "Imported".into());
                let mut files = Vec::new();
                import::collect_files(&path, &mut files);
                (self.presets_dir.join(name), files.into_iter().map(|f| (f.strip_prefix(&path).unwrap_or(&f).to_path_buf(), f)).collect::<Vec<_>>())
            } else {
                (self.presets_dir.join("Imported"), vec![(PathBuf::from(path.file_name().unwrap_or_default()), path.clone())])
            };
            for (rel, file) in files {
                let dest = dest_dir.join(&rel);
                if self.presets.iter().any(|p| p.source.as_deref() == Some(dest.as_path())) {
                    continue;
                }
                let copied = dest.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::copy(&file, &dest));
                let group = import::group_for(&self.presets_dir, &dest);
                match copied.map_err(|e| e.to_string()).and_then(|_| import::load_file(&dest, &group)) {
                    Ok(p) => added.push(p),
                    Err(e) => {
                        _ = std::fs::remove_file(&dest);
                        errors.push(format!("{}: {e}", file.display()));
                    }
                }
            }
        }
        let count = added.len();
        let first_new = self.presets.len();
        self.add_presets(added);
        if count > 0 {
            self.select(first_new);
            self.scroll_to_selected = true;
        }
        self.status = match errors.first() {
            None => format!("Imported {count} preset(s)."),
            Some(e) => format!("Imported {count} preset(s), {} failed — {e}", errors.len()),
        };
    }

    fn export_plan(&self) -> Option<export::Plan> {
        let photo = self.photo.as_ref()?;
        let st = &self.export_settings;
        let size = match export::SIZES[st.size].size {
            export::Size::LongEdge(0) => export::Size::LongEdge(st.custom_edge),
            s => s,
        };
        Some(export::plan(size, photo.crop, photo.size(), st.fill))
    }

    fn export_dialog(&mut self, ctx: &egui::Context) {
        let Some(plan) = self.export_plan() else {
            self.export_dialog = false;
            return;
        };
        let mut go = false;
        let mut copy = false;
        let modal = egui::Modal::new(egui::Id::new("export")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Export");
            ui.add_space(8.0);
            let st = &mut self.export_settings;
            egui::Grid::new("export_grid").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
                ui.label("Size");
                egui::ComboBox::from_id_salt("export_size")
                    .width(260.0)
                    .selected_text(export::SIZES[st.size].label)
                    .show_ui(ui, |ui| {
                        for (i, s) in export::SIZES.iter().enumerate() {
                            ui.selectable_value(&mut st.size, i, s.label);
                        }
                    });
                ui.end_row();
                if st.size == export::CUSTOM {
                    ui.label("Long edge");
                    ui.add(egui::DragValue::new(&mut st.custom_edge).range(64..=16384).suffix(" px"));
                    ui.end_row();
                }
                if matches!(export::SIZES[st.size].size, export::Size::Exact(..)) {
                    ui.label("");
                    ui.checkbox(&mut st.fill, "Crop to fill the format")
                        .on_hover_text("Trim the photo (centred, within your crop) to the format's shape.\nOff: fit the whole photo inside it.");
                    ui.end_row();
                }
                ui.label("Format");
                ui.horizontal(|ui| {
                    for (i, (name, _)) in FORMATS.iter().enumerate() {
                        ui.selectable_value(&mut st.format, i, *name);
                    }
                });
                ui.end_row();
                if st.format == 0 {
                    ui.label("Quality");
                    ui.add(egui::Slider::new(&mut st.quality, 50..=100));
                    ui.end_row();
                }
                if st.format != 2 {
                    ui.label("Metadata");
                    ui.checkbox(&mut st.keep_gps, "Keep location")
                        .on_hover_text("Date, camera and exposure details are always kept.\nUntick to leave out where the photo was taken.");
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            let (w, h) = plan.output;
            let note = if plan.output != plan.render {
                format!("{w} × {h} px (resized from {} × {})", plan.render.0, plan.render.1)
            } else {
                format!("{w} × {h} px")
            };
            ui.label(egui::RichText::new(note).weak());
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    ui.close();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new("💾 Export…")).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                        go = true;
                        ui.close();
                    }
                    if ui.button("📋 Copy").on_hover_text("Copy to the clipboard at this size (⌘C / Ctrl+C)").clicked() {
                        copy = true;
                        ui.close();
                    }
                });
            });
        });
        if modal.should_close() {
            self.export_dialog = false;
        }
        if go {
            self.export_dialog = false;
            self.export(plan);
        }
        if copy {
            self.export_dialog = false;
            self.copy_to_clipboard(plan);
        }
    }

    /// Renders like an export (size and crop from the export settings) onto the clipboard.
    fn copy_to_clipboard(&mut self, plan: export::Plan) {
        let Some(photo) = &self.photo else { return };
        let (w, h) = plan.render;
        match self.gpu.render_image(&photo.source, &self.gpu_presets[self.selected], self.strength, plan.crop, w, h) {
            Ok(img) => {
                let (tx, rx) = channel();
                self.status = "Copying…".into();
                std::thread::spawn(move || {
                    let img = resize(img, plan.output);
                    let (w, h) = img.dimensions();
                    let mut rgba = vec![255u8; (w * h * 4) as usize];
                    for (o, i) in rgba.chunks_exact_mut(4).zip(img.as_raw().chunks_exact(3)) {
                        o[..3].copy_from_slice(i);
                    }
                    _ = tx.send((w, h, rgba));
                });
                self.copying = Some(rx);
            }
            Err(e) => self.status = format!("Copy failed: {e}"),
        }
    }

    fn export(&mut self, plan: export::Plan) {
        let Some(photo) = &self.photo else { return };
        let st = &self.export_settings;
        let preset = &self.presets[self.selected];
        let stem = photo.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "photo".into());
        let (format, ext) = FORMATS[st.format];
        let suffix = match export::SIZES[st.size].slug {
            "original" => String::new(),
            "custom" => format!(" - {}px", plan.output.0.max(plan.output.1)),
            slug => format!(" - {slug}"),
        };
        let Some(dest) = rfd::FileDialog::new()
            .set_file_name(format!("{stem} - {}{suffix}.{ext}", preset.name))
            .add_filter(format, &[ext])
            .save_file()
        else {
            return;
        };
        let start = Instant::now();
        let (w, h) = plan.render;
        let quality = st.quality;
        let (source, keep_gps) = (photo.path.clone(), st.keep_gps);
        match self.gpu.render_image(&photo.source, &self.gpu_presets[self.selected], self.strength, plan.crop, w, h) {
            Ok(img) => {
                let (tx, rx) = channel();
                self.status = format!("Exporting {}…", dest.display());
                std::thread::spawn(move || {
                    let img = resize(img, plan.output);
                    let exif = crate::metadata::for_export(&source, img.dimensions(), keep_gps);
                    _ = tx.send(save_image(&img, &dest, quality, exif).map(|_| dest));
                });
                self.exporting = Some(rx);
                log_time("render full-res", start);
            }
            Err(e) => self.status = format!("Export failed: {e}"),
        }
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        if let Some((path, result)) = self.browser.poll() {
            match result {
                Ok(decoded) => self.finish_load(path, decoded, Instant::now(), false),
                Err(e) => self.status = format!("Could not open {}: {e}", path.display()),
            }
        }
        if self.browser.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        while let Some(msg) = self.loading.as_ref().and_then(|rx| rx.try_recv().ok()) {
            if !msg.quick {
                self.loading = None;
            }
            match msg.result {
                Ok(decoded) => self.finish_load(msg.path, decoded, msg.start, msg.quick),
                Err(e) => self.status = format!("Could not open {}: {e}", msg.path.display()),
            }
        }
        if self.loading.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        self.poll_recommendations(ctx);
        if let Some(rx) = &self.copying {
            if let Ok((w, h, rgba)) = rx.try_recv() {
                self.copying = None;
                if self.clipboard.is_none() {
                    self.clipboard = arboard::Clipboard::new().ok();
                }
                let image = arboard::ImageData { width: w as usize, height: h as usize, bytes: rgba.into() };
                self.status = match self.clipboard.as_mut().map(|c| c.set_image(image)) {
                    Some(Ok(())) => format!("Copied {w}×{h} to the clipboard."),
                    Some(Err(e)) => format!("Copy failed: {e}"),
                    None => "Copy failed: no clipboard available.".into(),
                };
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(30));
            }
        }
        if let Some(rx) = &self.exporting {
            if let Ok(result) = rx.try_recv() {
                self.exporting = None;
                self.status = match result {
                    Ok(p) => format!("Exported {}", p.display()),
                    Err(e) => format!("Export failed: {e}"),
                };
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if let Some(photo) = dropped.iter().find(|p| loader::is_photo(p)) {
            self.open(photo.clone(), ctx);
        }
        let presets: Vec<PathBuf> = dropped.into_iter().filter(|p| p.is_dir() || import::is_preset_file(p)).collect();
        if !presets.is_empty() {
            self.import_paths(presets);
        }

        if self.export_dialog || self.palette.is_open() || ctx.egui_wants_keyboard_input() {
            return; // a dialog or the palette is open, or typing in the search box
        }
        let can_export = self.photo.as_ref().is_some_and(|p| !p.provisional);
        if can_export && self.cropping.is_none() && ctx.input(|i| i.modifiers.command && i.key_pressed(Key::E)) {
            self.export_dialog = true;
            return;
        }
        // ⌘C / Ctrl+C: copy the edited photo, using the export dialog's size.
        let copy = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        if can_export && self.cropping.is_none() && self.copying.is_none() && copy {
            if let Some(plan) = self.export_plan() {
                self.copy_to_clipboard(plan);
            }
            return;
        }
        if self.cropping.is_some() {
            let (enter, esc, c, x) = ctx.input(|i| {
                (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape), plain_key(i, Key::C), plain_key(i, Key::X))
            });
            if enter || c {
                self.finish_crop(true);
            } else if esc {
                self.finish_crop(false);
            } else if x {
                self.crop_flip = !self.crop_flip;
                self.apply_crop_ratio();
            }
            return; // arrows etc. stay inactive while cropping
        }
        if ctx.input(|i| plain_key(i, Key::C)) {
            self.start_crop();
        }
        if ctx.input(|i| plain_key(i, Key::F)) {
            self.set_filmstrip(ctx, !self.show_filmstrip);
        }
        if ctx.input(|i| plain_key(i, Key::Z)) {
            self.toggle_zoom(None);
        }
        if self.zoom.is_some() && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.toggle_zoom(None);
        }
        if ctx.input(|i| plain_key(i, Key::OpenBracket)) {
            self.navigate(-1, ctx);
        }
        if ctx.input(|i| plain_key(i, Key::CloseBracket)) {
            self.navigate(1, ctx);
        }
        if ctx.input(|i| plain_key(i, Key::Y)) {
            self.side_by_side = !self.side_by_side;
        }
        let keys = ctx.input(|i| {
            [Key::ArrowLeft, Key::ArrowRight, Key::ArrowUp, Key::ArrowDown].map(|k| i.key_pressed(k))
        });
        if keys.contains(&true) {
            self.move_selection(keys);
        }
    }

    /// Moves the selection through the grid as it's shown: left/right step through cells,
    /// up/down go to the nearest cell in the row above/below. Collapsed groups are skipped.
    fn move_selection(&mut self, keys: [bool; 4]) {
        if self.nav_cells.is_empty() {
            return;
        }
        let current = self
            .nav_cells
            .iter()
            .position(|(i, sec, _)| *i == self.selected && *sec == self.selected_section)
            .or_else(|| self.nav_cells.iter().position(|(i, ..)| *i == self.selected));
        let rects: Vec<Rect> = self.nav_cells.iter().map(|(.., r)| *r).collect();
        let (index, section, _) = self.nav_cells[grid_step(&rects, current, keys)].clone();
        self.selected_section = section;
        self.select(index);
        self.scroll_to_selected = true;
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("📂 Open…").on_hover_text(format!("Open a photo ({}) · all commands: {}", crate::palette::shortcut("Cmd+O"), crate::palette::shortcut("Shift+Cmd+P"))).clicked() {
                self.run_command(crate::palette::Command::OpenFile, ui.ctx());
            }
            if let Some(target) = self.target.clone()
                && let Some(pos) = self.browser.position(&target)
            {
                let count = self.browser.files.len();
                if ui.add_enabled(pos > 0, egui::Button::new("◀")).on_hover_text("Previous photo ([)").clicked() {
                    self.navigate(-1, ui.ctx());
                }
                ui.label(egui::RichText::new(format!("{} / {count}", pos + 1)).weak());
                if ui.add_enabled(pos + 1 < count, egui::Button::new("▶")).on_hover_text("Next photo (])").clicked() {
                    self.navigate(1, ui.ctx());
                }
                ui.separator();
            }
            let can_export = self.photo.as_ref().is_some_and(|p| !p.provisional) && self.exporting.is_none();
            if ui.add_enabled(can_export, egui::Button::new("💾 Export…")).on_hover_text("Export (⌘E / Ctrl+E)").clicked() {
                self.export_dialog = true;
            }
            ui.separator();
            ui.menu_button("➕ Import presets", |ui| {
                if ui.button("Files (.xmp, .lrtemplate, .cube)…").clicked() {
                    if let Some(files) = rfd::FileDialog::new().add_filter("Presets", &import::PRESET_EXTENSIONS).pick_files() {
                        self.import_paths(files);
                    }
                    ui.close();
                }
                if ui.button("Folder…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.import_paths(vec![dir]);
                    }
                    ui.close();
                }
                if ui.button("Show presets folder").clicked() {
                    _ = std::fs::create_dir_all(&self.presets_dir);
                    _ = open_in_file_manager(&self.presets_dir);
                    ui.close();
                }
            });
            ui.separator();
            ui.label("Amount");
            let slider = ui.add(egui::Slider::new(&mut self.strength, 0.0..=1.5).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
            if slider.changed() {
                self.preview_dirty = true;
            }
            if slider.double_clicked() {
                self.strength = 1.0;
                self.preview_dirty = true;
            }
            ui.separator();
            ui.add_enabled_ui(self.cropping.is_none(), |ui| {
                ui.toggle_value(&mut self.side_by_side, "◫ Before / After").on_hover_text("Show the original next to the edit (Y)");
            });
            if ui.add(egui::Button::selectable(self.show_filmstrip, "🎞 Filmstrip")).on_hover_text("Show the folder's photos along the bottom (F)").clicked() {
                let show = !self.show_filmstrip;
                self.set_filmstrip(ui.ctx(), show);
            }
            let zoomed = self.zoom.is_some();
            if ui.add_enabled(self.zoom_available(), egui::Button::selectable(zoomed, "🔍 100%")).on_hover_text("Zoom to 100% (Z, or double-click the photo)").clicked() {
                self.toggle_zoom(None);
            }
            let mut cropping = self.cropping.is_some();
            if ui.add_enabled(self.photo.is_some(), egui::Button::selectable(cropping, "✂ Crop")).on_hover_text("Crop the photo (C)").clicked() {
                cropping = !cropping;
                if cropping { self.start_crop() } else { self.finish_crop(true) }
            }
            ui.separator();
            ui.label(egui::RichText::new(&self.presets[self.selected].name).strong());

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Saved with the rest of egui's state, so the choice survives restarts.
                let mut theme = ui.ctx().options(|o| o.theme_preference);
                let before = theme;
                ui.selectable_value(&mut theme, egui::ThemePreference::System, "💻").on_hover_text("Follow system");
                ui.selectable_value(&mut theme, egui::ThemePreference::Light, "☀").on_hover_text("Light mode");
                ui.selectable_value(&mut theme, egui::ThemePreference::Dark, "🌙").on_hover_text("Dark mode");
                if theme != before {
                    ui.ctx().set_theme(theme);
                }
            });
        });
    }

    fn preset_panel(&mut self, ui: &mut egui::Ui, visible: &[usize]) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter presets").desired_width(f32::INFINITY));
        });
        ui.add_space(4.0);

        let mut groups: Vec<String> = Vec::new();
        for &i in visible {
            if !groups.contains(&self.presets[i].group) {
                groups.push(self.presets[i].group.clone());
            }
        }
        let filtering = !self.filter.is_empty();
        // (title, id, presets, open by default, is the recommendations section)
        let mut sections: Vec<(String, String, Vec<usize>, bool, bool)> = Vec::new();
        if !filtering && self.photo.is_some() && !self.recs.is_empty() {
            let items = self.recs.iter().map(|r| r.index).collect();
            sections.push(("★ Recommended for this photo".into(), "recommended".into(), items, true, true));
        }
        for group in &groups {
            let items: Vec<usize> = visible.iter().copied().filter(|&i| self.presets[i].group == *group).collect();
            let builtin = items.first().is_some_and(|&i| self.presets[i].source.is_none());
            sections.push((group.clone(), group.clone(), items, builtin, false));
        }
        let mut clicked = None;
        self.nav_cells.clear();
        let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
        // Screenshot mode starts at the top, whatever scroll position was saved last session.
        if self.screenshot.as_ref().is_some_and(|(_, frames)| *frames <= 1) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        scroll.show(ui, |ui| {
            for (title, id, items, open, recommended) in &sections {
                egui::CollapsingHeader::new(format!("{title}  ({})", items.len()))
                    .id_salt(id)
                    .default_open(*open)
                    .open(filtering.then_some(true))
                    .show(ui, |ui| {
                    // Measured inside the header, which indents its body: rows must never be
                    // wider than the panel, or egui grows the panel back after a resize.
                    let width = ui.available_width();
                    let cols = ((width + 6.0) / (CELL + 6.0)).floor().max(1.0) as usize;
                    let cell_w = ((width - 6.0 * (cols as f32 - 1.0)) / cols as f32).floor();
                    for row in items.chunks(cols) {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            for &i in row {
                                let note = recommended
                                    .then(|| self.recs.iter().find(|r| r.index == i))
                                    .flatten()
                                    .map(|r| r.reasons.join(" · "))
                                    .filter(|n| !n.is_empty());
                                let response = self.preset_cell(ui, i, cell_w, note, id);
                                self.nav_cells.push((i, id.clone(), response.rect));
                                if response.clicked() {
                                    clicked = Some((i, id.clone()));
                                }
                            }
                        });
                    }
                });
            }
            if visible.is_empty() {
                ui.label("No presets match.");
            }
        });
        if let Some((i, section)) = clicked {
            self.selected_section = section;
            self.select(i);
        }
    }

    /// The thumbnail texture for a preset, creating it and queueing a render if needed.
    /// Called while laying out the grid, so only thumbnails on screen are ever rendered.
    fn thumb_for(&mut self, index: usize) -> Option<TextureId> {
        let photo = self.photo.as_ref()?;
        let missing = photo.thumbs.get(index).is_none_or(Option::is_none);
        let view = missing.then(|| self.new_view(photo.thumb_size));
        let photo = self.photo.as_mut()?;
        if photo.thumbs.len() < self.presets.len() {
            photo.thumbs.resize_with(self.presets.len(), || None);
        }
        if let Some((target, id)) = view {
            photo.thumbs[index] = Some(Thumb { target, id, fresh: false });
        }
        let thumb = photo.thumbs[index].as_mut()?;
        if !thumb.fresh {
            thumb.fresh = true;
            self.thumb_queue.push(index);
        }
        Some(thumb.id)
    }

    fn preset_cell(&mut self, ui: &mut egui::Ui, index: usize, width: f32, note: Option<String>, section: &str) -> egui::Response {
        let label_h = 18.0;
        let img_h = width * 0.72;
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, img_h + label_h), Sense::click());
        let selected = index == self.selected;
        if selected && self.scroll_to_selected && (section == self.selected_section || self.selected_section.is_empty()) {
            response.scroll_to_me(None);
            self.scroll_to_selected = false;
        }
        let painter = ui.painter_at(rect.expand(2.0));
        let img_rect = Rect::from_min_size(rect.min, Vec2::new(width, img_h));
        painter.rect_filled(img_rect, 4.0, ui.visuals().extreme_bg_color);
        // Off-screen cells in the scroll area are laid out but never drawn; skip their thumbnails.
        if ui.is_rect_visible(rect)
            && let Some(id) = self.thumb_for(index)
            && let Some(photo) = &self.photo
        {
            let r = fit_rect(img_rect.shrink(2.0), photo.thumb_size);
            painter.image(id, r, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        } else if self.presets[index].lut.is_some() {
            painter.text(img_rect.center(), egui::Align2::CENTER_CENTER, "LUT", egui::FontId::proportional(14.0), ui.visuals().weak_text_color());
        }
        let stroke = if selected {
            Stroke::new(2.0, ui.visuals().selection.bg_fill)
        } else if response.hovered() {
            Stroke::new(1.0, ui.visuals().widgets.hovered.fg_stroke.color)
        } else {
            Stroke::NONE
        };
        painter.rect_stroke(img_rect, 4.0, stroke, egui::StrokeKind::Outside);
        let name = &self.presets[index].name;
        let galley = ui.painter().layout(
            name.clone(),
            egui::FontId::proportional(12.0),
            if selected { ui.visuals().strong_text_color() } else { ui.visuals().text_color() },
            width,
        );
        let text_pos = egui::pos2(rect.center().x - galley.size().x.min(width) / 2.0, img_rect.max.y + 2.0);
        painter.with_clip_rect(rect).galley(text_pos, galley, Color32::WHITE);
        match note {
            Some(note) => response.on_hover_text(format!("{name}\n{note}")),
            None => response.on_hover_text(name),
        }
    }

    fn preview(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        if self.cropping.is_some() {
            self.crop_view(ui, rect, &response);
            return;
        }
        if self.zoom.is_some() && !self.zoom_available() {
            // e.g. the next photo is still showing its quick preview: show it fitted meanwhile.
            self.zoom_views = None;
        }
        let zoomed = self.zoom.is_some() && self.zoom_available();
        let painter = ui.painter_at(rect);
        let mut edited_rect = None;
        // Where on screen the (fitted) photo is, for zooming into a spot: (rect, crop shown).
        let mut fitted: Vec<(Rect, Crop)> = Vec::new();
        match &self.photo {
            Some(_) if zoomed => {
                edited_rect = Some(self.zoom_view(ui, rect, &response));
            }
            Some(photo) if self.side_by_side => {
                let size = (photo.preview.width, photo.preview.height);
                let (before, after) = split_for(rect.shrink(12.0), size, 12.0);
                let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                let (b, a) = (fit_rect(before, size), fit_rect(after, size));
                painter.image(photo.original_id, b, uv, Color32::WHITE);
                painter.image(photo.preview_id, a, uv, Color32::WHITE);
                badge(&painter, b, "Before");
                badge(&painter, a, &self.presets[self.selected].name);
                edited_rect = Some(a);
                fitted = vec![(b, photo.view_crop), (a, photo.view_crop)];
            }
            Some(photo) => {
                let r = fit_rect(rect.shrink(12.0), (photo.preview.width, photo.preview.height));
                painter.image(photo.preview_id, r, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                if self.show_original {
                    badge(&painter, r, "Original");
                }
                edited_rect = Some(r);
                fitted = vec![(r, photo.view_crop)];
            }
            None => {
                let text = if self.loading.is_some() { "Loading…" } else { "Drop a photo here, or click Open.\nDrop .xmp / .lrtemplate / .cube files to import presets." };
                painter.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(18.0), ui.visuals().weak_text_color());
            }
        }
        // Scrolling over the photo changes the amount: 5% per mouse-wheel notch, smooth on trackpads.
        if self.photo.is_some() && response.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                self.strength = (self.strength + dy * 0.00125).clamp(0.0, 1.5);
                self.preview_dirty = true;
                self.amount_changed_at = Some(ui.input(|i| i.time));
            }
        }
        if let (Some(t), Some(r)) = (self.amount_changed_at, edited_rect) {
            let age = ui.input(|i| i.time) - t;
            let alpha = (1.0 - ((age - 0.8) / 0.4).clamp(0.0, 1.0)) as f32;
            if alpha > 0.0 {
                amount_overlay(&painter, r, self.strength, alpha);
                ui.ctx().request_repaint();
            } else {
                self.amount_changed_at = None;
            }
        }

        self.handle_pinch(ui, rect, &response, &fitted);
        // Double-click zooms to 100% at that spot, or back out.
        if response.double_clicked() && self.photo.is_some() {
            let at = response.interact_pointer_pos().and_then(|p| {
                let (r, c) = fitted.iter().find(|(r, _)| r.contains(p))?;
                let u = (p - r.min) / r.size();
                Some([c[0] + u.x * (c[2] - c[0]), c[1] + u.y * (c[3] - c[1])])
            });
            self.toggle_zoom(at);
        }
        // Holding the mouse shows the original, except at 100% where dragging pans.
        let held = response.is_pointer_button_down_on() && !zoomed;
        let original = !self.side_by_side && (held || ui.input(|i| i.key_down(Key::Backslash)));
        if original != self.show_original {
            self.show_original = original;
            self.preview_dirty = true;
        }
        if self.amount_changed_at.is_none() {
            let tip = if zoomed {
                "Drag to pan · pinch to zoom · double-click or Z for the whole photo · hold \\ to see the original"
            } else {
                "Scroll to change the amount · hold the mouse button (or \\) to see the original · pinch, double-click or Z to zoom"
            };
            response.on_hover_text(tip);
        }
    }
}

impl PhotoApp {
    /// `BP_PHOTOS_TOUR=1`: step to the next photo every 1.5 s and quit at the end of the folder
    /// (with `BP_PHOTOS_TIMING`, measures real navigation, preloading included).
    fn drive_tour(&mut self, ctx: &egui::Context) {
        if std::env::var_os("BP_PHOTOS_TOUR").is_none() {
            return;
        }
        let now = ctx.input(|i| i.time);
        let last = ctx.data_mut(|d| *d.get_temp_mut_or(egui::Id::new("tour"), now));
        if now - last > 1.5 && self.photo.as_ref().is_some_and(|p| !p.provisional) {
            ctx.data_mut(|d| d.insert_temp(egui::Id::new("tour"), now));
            let at_end = self.target.as_ref().and_then(|t| self.browser.neighbour(t, 1)).is_none();
            if at_end {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                eprintln!("tour: next photo");
                self.navigate(1, ctx);
            }
        }
        ctx.request_repaint();
    }

    fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let Some((_, frames)) = &self.screenshot else { return };
        // Full image loaded and its recommendations settled (not the quick preview's).
        let settled = !self.recs_dirty && self.recs_job.is_none() && self.recs_rx.is_none();
        let ready = self.photo.as_ref().is_some_and(|p| !p.provisional) && !self.recs.is_empty() && settled;
        // Wait for filmstrip thumbnails too, once it's shown (from frame 1).
        let ready = ready && !(*frames > 0 && self.show_filmstrip && self.filmstrip.loading());
        let frames = if ready { frames + 1 } else { *frames };
        if let Some((_, f)) = &mut self.screenshot {
            *f = frames;
        }
        if ready {
            if frames == 1 {
                self.selected_section = "recommended".into();
                self.selected = self.recs[0].index;
                self.preview_dirty = true;
                self.side_by_side = std::env::var_os("BP_PHOTOS_SCREENSHOT_COMPARE").is_some();
                self.show_filmstrip = std::env::var_os("BP_PHOTOS_SCREENSHOT_FILMSTRIP").is_some();
                if std::env::var_os("BP_PHOTOS_SCREENSHOT_PALETTE").is_some() {
                    self.palette.toggle();
                }
                if std::env::var_os("BP_PHOTOS_SCREENSHOT_ZOOM").is_some() {
                    self.toggle_zoom(Some([0.62, 0.45]));
                    if let (Some(z), Some(s)) = (&mut self.zoom, std::env::var("BP_PHOTOS_SCREENSHOT_ZOOM").ok().and_then(|v| v.parse::<f32>().ok())) {
                        z.scale = s;
                    }
                }
            }
            // A few frames for thumbnails to render, then capture.
            if frames == 12 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            }
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = shot
            && let Some((path, _)) = &self.screenshot
        {
            let [w, h] = image.size;
            let rgba = image::RgbaImage::from_raw(w as u32, h as u32, image.as_raw().to_vec());
            match rgba.map(|img| img.save(&*path)) {
                Some(Ok(())) => eprintln!("screenshot saved to {}", path.display()),
                _ => eprintln!("screenshot failed"),
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

impl eframe::App for PhotoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drive_screenshot(&ctx);
        self.drive_tour(&ctx);
        self.poll_background(&ctx);

        let filter = self.filter.to_lowercase();
        let visible: Vec<usize> = (0..self.presets.len())
            .filter(|&i| filter.is_empty() || self.presets[i].name.to_lowercase().contains(&filter) || self.presets[i].group.to_lowercase().contains(&filter))
            .collect();
        self.global_shortcuts(&ctx);
        // Before the rest of the input: the palette takes arrows/Enter/Esc while it's open.
        if let Some(command) = self.palette.show(&ctx) {
            self.run_command(command, &ctx);
        }
        self.handle_input(&ctx);

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(2.0);
        });
        if self.cropping.is_some() {
            egui::Panel::top("crop_toolbar").show(ui, |ui| {
                ui.add_space(3.0);
                self.crop_toolbar(ui);
                ui.add_space(2.0);
            });
        }
        egui::Panel::bottom("status").show(ui, |ui| ui.label(egui::RichText::new(&self.status).small()));
        if self.show_filmstrip {
            egui::Panel::bottom("filmstrip").resizable(false).show(ui, |ui| {
                ui.add_space(4.0);
                self.filmstrip_panel(ui);
                ui.add_space(2.0);
            });
        }
        egui::Panel::right("presets").resizable(true).default_size(330.0).size_range(200.0..=1200.0).show(ui, |ui| self.preset_panel(ui, &visible));
        egui::CentralPanel::default().show(ui, |ui| self.preview(ui));
        if self.export_dialog {
            self.export_dialog(&ctx);
        }

        self.render();
        // Queued after the preview, so the photo never waits for the recommendation pass.
        self.start_recommendations();
    }
}

/// A single-letter shortcut, pressed without ⌘/Ctrl/Alt (so ⌘C copies rather than crops).
fn plain_key(i: &egui::InputState, key: Key) -> bool {
    i.key_pressed(key) && !i.modifiers.command && !i.modifiers.ctrl && !i.modifiers.alt
}

/// The region of the photo shown in a view of `view_px` screen pixels at `scale` screen pixels
/// per photo pixel, centred as close to `center` as the crop allows. Returns it and the
/// (clamped) centre.
fn zoom_region(center: [f32; 2], view_px: (f32, f32), image: (u32, u32), within: Crop, scale: f32) -> (Crop, [f32; 2]) {
    let rw = (view_px.0 / scale / image.0 as f32).min(within[2] - within[0]);
    let rh = (view_px.1 / scale / image.1 as f32).min(within[3] - within[1]);
    // Not `clamp`: when the view spans the whole crop, rounding can put min a hair above max.
    let fit = |v: f32, lo: f32, hi: f32| if lo >= hi { (lo + hi) / 2.0 } else { v.max(lo).min(hi) };
    let cx = fit(center[0], within[0] + rw / 2.0, within[2] - rw / 2.0);
    let cy = fit(center[1], within[1] + rh / 2.0, within[3] - rh / 2.0);
    ([cx - rw / 2.0, cy - rh / 2.0, cx + rw / 2.0, cy + rh / 2.0], [cx, cy])
}

/// Next cell for arrow keys `[left, right, up, down]`, over cells in display order: left/right
/// step through them; up/down pick the nearest row in that direction, then the closest cell in it.
fn grid_step(cells: &[Rect], current: Option<usize>, [left, right, up, down]: [bool; 4]) -> usize {
    let Some(c) = current else { return 0 };
    if left {
        return c.saturating_sub(1);
    }
    if right {
        return (c + 1).min(cells.len() - 1);
    }
    if !(up || down) {
        return c;
    }
    let from = cells[c];
    let in_direction = |r: &Rect| if up { r.max.y <= from.min.y + 1.0 } else { r.min.y >= from.max.y - 1.0 };
    let dy = |r: &Rect| (r.center().y - from.center().y).abs();
    let dx = |r: &Rect| (r.center().x - from.center().x).abs();
    let row = cells.iter().filter(|r| in_direction(r)).map(dy).fold(f32::INFINITY, f32::min);
    cells
        .iter()
        .enumerate()
        .filter(|(_, r)| in_direction(r) && dy(r) - row < 1.0)
        .min_by(|(_, a), (_, b)| dx(a).total_cmp(&dx(b)))
        .map_or(c, |(i, _)| i)
}

/// Splits `rect` into before/after cells, side by side or stacked, whichever shows the photo larger.
fn split_for(rect: Rect, size: (u32, u32), gap: f32) -> (Rect, Rect) {
    let (w, h) = (size.0 as f32, size.1 as f32);
    let (cw, ch) = ((rect.width() - gap) / 2.0, (rect.height() - gap) / 2.0);
    let side_scale = (cw / w).min(rect.height() / h);
    let stack_scale = (rect.width() / w).min(ch / h);
    if side_scale >= stack_scale {
        let left = Rect::from_min_size(rect.min, Vec2::new(cw, rect.height()));
        (left, left.translate(Vec2::new(cw + gap, 0.0)))
    } else {
        let top = Rect::from_min_size(rect.min, Vec2::new(rect.width(), ch));
        (top, top.translate(Vec2::new(0.0, ch + gap)))
    }
}

/// "Amount 85%" with a small bar, centred near the bottom of the photo.
fn amount_overlay(painter: &egui::Painter, image: Rect, amount: f32, alpha: f32) {
    let fade = |c: Color32| c.gamma_multiply(alpha);
    let text = format!("Amount {:.0}%", amount * 100.0);
    let galley = painter.layout_no_wrap(text, egui::FontId::proportional(16.0), fade(Color32::WHITE));
    let size = Vec2::new(galley.size().x.max(140.0) + 24.0, galley.size().y + 22.0);
    let bg = Rect::from_center_size(egui::pos2(image.center().x, image.max.y - 24.0 - size.y / 2.0), size);
    painter.rect_filled(bg, 8.0, fade(Color32::from_black_alpha(170)));
    painter.galley(egui::pos2(bg.center().x - galley.size().x / 2.0, bg.min.y + 6.0), galley, fade(Color32::WHITE));
    // Bar spans 0..150%, with a tick at 100%.
    let bar = Rect::from_min_size(egui::pos2(bg.min.x + 12.0, bg.max.y - 10.0), Vec2::new(bg.width() - 24.0, 4.0));
    painter.rect_filled(bar, 2.0, fade(Color32::from_white_alpha(60)));
    let filled = Rect::from_min_size(bar.min, Vec2::new(bar.width() * amount / 1.5, bar.height()));
    painter.rect_filled(filled, 2.0, fade(Color32::WHITE));
    let tick = bar.min.x + bar.width() / 1.5;
    painter.line_segment([egui::pos2(tick, bar.min.y - 2.0), egui::pos2(tick, bar.max.y + 2.0)], Stroke::new(1.0, fade(Color32::from_white_alpha(160))));
}

/// A small label with a dark backing, readable on any photo.
fn badge(painter: &egui::Painter, image: Rect, text: &str) {
    let galley = painter.layout_no_wrap(text.to_owned(), egui::FontId::proportional(13.0), Color32::WHITE);
    let pos = image.left_top() + Vec2::new(10.0, 10.0);
    let bg = Rect::from_min_size(pos, galley.size()).expand2(Vec2::new(6.0, 3.0));
    painter.rect_filled(bg, 4.0, Color32::from_black_alpha(150));
    painter.galley(pos, galley, Color32::WHITE);
}

/// Downsizes with Lanczos3 (sharp, no aliasing) if `size` differs from the image.
pub fn resize(img: image::RgbImage, size: (u32, u32)) -> image::RgbImage {
    if img.dimensions() == size {
        img
    } else {
        image::imageops::resize(&img, size.0, size.1, image::imageops::FilterType::Lanczos3)
    }
}

/// Saves as JPEG, PNG or TIFF (by extension), with `exif` embedded where the format allows
/// (JPEG and PNG).
pub fn save_image(img: &image::RgbImage, dest: &Path, jpeg_quality: u8, exif: Option<Vec<u8>>) -> Result<(), String> {
    use image::ImageEncoder;

    let err = |e: image::ImageError| e.to_string();
    let file = || std::fs::File::create(dest).map(std::io::BufWriter::new).map_err(|e| e.to_string());
    match import::extension(dest).as_deref() {
        Some("jpg" | "jpeg") | None => {
            let mut writer = file()?;
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, jpeg_quality);
            if let Some(exif) = exif {
                _ = encoder.set_exif_metadata(exif);
            }
            encoder.encode_image(img).map_err(err)
        }
        Some("png") => {
            let mut encoder = image::codecs::png::PngEncoder::new(file()?);
            if let Some(exif) = exif {
                _ = encoder.set_exif_metadata(exif);
            }
            encoder.write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8).map_err(err)
        }
        _ => img.save(dest).map_err(err),
    }
}

fn open_in_file_manager(path: &Path) -> std::io::Result<std::process::Child> {
    let cmd = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    std::process::Command::new(cmd).arg(path).spawn()
}

fn log_time(what: &str, start: Instant) {
    if std::env::var_os("BP_PHOTOS_TIMING").is_some() {
        eprintln!("{what}: {:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two sections: a 2×2 grid, then (after a header gap) a row of 2 — like the preset panel.
    fn cells() -> Vec<Rect> {
        let cell = |x: f32, y: f32| Rect::from_min_size(egui::pos2(x, y), Vec2::new(100.0, 80.0));
        vec![cell(0.0, 0.0), cell(110.0, 0.0), cell(0.0, 90.0), cell(110.0, 90.0), cell(0.0, 210.0), cell(110.0, 210.0)]
    }

    #[test]
    fn zoom_region_is_one_to_one_and_stays_inside_the_crop() {
        // 1000×500 view on a 4000×2000 photo: a quarter of each side, one pixel per pixel.
        let (r, c) = zoom_region([0.5, 0.5], (1000.0, 500.0), (4000, 2000), FULL_CROP, 1.0);
        assert_eq!(r, [0.375, 0.375, 0.625, 0.625]);
        assert_eq!(c, [0.5, 0.5]);
        // Panned past the corner: clamped so the view stays on the photo.
        let (r, _) = zoom_region([0.0, 1.0], (1000.0, 500.0), (4000, 2000), FULL_CROP, 1.0);
        assert_eq!(r, [0.0, 0.75, 0.25, 1.0]);
        // View bigger than a small crop: shows the whole crop.
        let (r, _) = zoom_region([0.5, 0.5], (4000.0, 4000.0), (4000, 2000), [0.2, 0.2, 0.4, 0.6], 1.0);
        assert!(r.iter().zip([0.2, 0.2, 0.4, 0.6]).all(|(a, b)| (a - b).abs() < 1e-6), "{r:?}");
        // At 200% the same view shows half as much of the photo.
        let (r, _) = zoom_region([0.5, 0.5], (1000.0, 500.0), (4000, 2000), FULL_CROP, 2.0);
        assert_eq!(r, [0.4375, 0.4375, 0.5625, 0.5625]);
    }

    #[test]
    fn left_right_follow_display_order() {
        assert_eq!(grid_step(&cells(), Some(1), [false, true, false, false]), 2);
        assert_eq!(grid_step(&cells(), Some(0), [true, false, false, false]), 0);
        assert_eq!(grid_step(&cells(), Some(5), [false, true, false, false]), 5);
    }

    #[test]
    fn up_down_keep_the_column_across_sections() {
        assert_eq!(grid_step(&cells(), Some(1), [false, false, false, true]), 3);
        assert_eq!(grid_step(&cells(), Some(3), [false, false, false, true]), 5);
        assert_eq!(grid_step(&cells(), Some(4), [false, false, true, false]), 2);
        assert_eq!(grid_step(&cells(), Some(0), [false, false, true, false]), 0);
    }
}
