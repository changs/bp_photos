//! Thumbnails of the photos in the current folder, loaded in the background, nearest first.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

/// Longest edge of filmstrip thumbnails, in pixels (enough for a 2× display).
const THUMB_PX: u32 = 256;
const WORKERS: usize = 3;
/// Thumbnails further than this from the current photo are dropped to bound memory.
const KEEP_WITHIN: usize = 150;

enum Slot {
    Queued,
    Ready(TextureHandle, [usize; 2]),
    Failed,
}

/// Paths waiting for a worker, each with a priority (lower = sooner).
#[derive(Default)]
struct Queue {
    jobs: Vec<(PathBuf, usize)>,
}

pub struct Filmstrip {
    slots: HashMap<PathBuf, Slot>,
    queue: Arc<(Mutex<Queue>, Condvar)>,
    rx: Receiver<(PathBuf, Result<image::RgbaImage, String>)>,
    tx: Sender<(PathBuf, Result<image::RgbaImage, String>)>,
    started: bool,
}

impl Filmstrip {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self { slots: HashMap::new(), queue: Arc::default(), rx, tx, started: false }
    }

    fn start_workers(&mut self, ctx: &egui::Context) {
        self.started = true;
        for _ in 0..WORKERS {
            let (queue, tx, ctx) = (self.queue.clone(), self.tx.clone(), ctx.clone());
            std::thread::spawn(move || loop {
                let path = {
                    let (lock, cvar) = &*queue;
                    let mut q = lock.lock().unwrap();
                    while q.jobs.is_empty() {
                        q = cvar.wait(q).unwrap();
                    }
                    let best = (0..q.jobs.len()).min_by_key(|&i| q.jobs[i].1).unwrap();
                    q.jobs.swap_remove(best).0
                };
                let t = std::time::Instant::now();
                let result = crate::loader::load_thumbnail(&path, THUMB_PX);
                crate::timing(&format!("filmstrip thumbnail {}", path.file_name().unwrap_or_default().to_string_lossy()), t);
                if tx.send((path, result)).is_err() {
                    return;
                }
                ctx.request_repaint();
            });
        }
    }

    /// Queues thumbnails for `files` (all of the folder), nearest to `current` first, and turns
    /// finished ones into textures. Call once per frame while the filmstrip is shown.
    pub fn update(&mut self, ctx: &egui::Context, files: &[PathBuf], current: Option<usize>) {
        if !self.started {
            self.start_workers(ctx);
        }
        while let Ok((path, result)) = self.rx.try_recv() {
            let slot = match result {
                Ok(img) => {
                    let size = [img.width() as usize, img.height() as usize];
                    let image = ColorImage::from_rgba_unmultiplied(size, img.as_raw());
                    let name = path.to_string_lossy().into_owned();
                    Slot::Ready(ctx.load_texture(name, image, TextureOptions::LINEAR), size)
                }
                Err(_) => Slot::Failed,
            };
            if self.slots.contains_key(&path) {
                self.slots.insert(path, slot);
            }
        }

        let cur = current.unwrap_or(0);
        let near = |i: usize| i.abs_diff(cur) <= KEEP_WITHIN;
        // Forget far-away thumbnails (and files no longer in the folder).
        let wanted: HashMap<&Path, usize> = files.iter().enumerate().filter(|(i, _)| near(*i)).map(|(i, p)| (p.as_path(), i)).collect();
        self.slots.retain(|p, _| wanted.contains_key(p.as_path()));

        let (lock, cvar) = &*self.queue;
        let mut q = lock.lock().unwrap();
        q.jobs.retain(|(p, _)| wanted.contains_key(p.as_path()));
        for (p, prio) in q.jobs.iter_mut() {
            *prio = wanted[p.as_path()].abs_diff(cur);
        }
        let mut added = false;
        for (path, &i) in &wanted {
            if !self.slots.contains_key(*path) {
                self.slots.insert(path.to_path_buf(), Slot::Queued);
                q.jobs.push((path.to_path_buf(), i.abs_diff(cur)));
                added = true;
            }
        }
        if added {
            cvar.notify_all();
        }
    }

    /// The thumbnail texture and its pixel size, once loaded.
    pub fn get(&self, path: &Path) -> Option<(&TextureHandle, [usize; 2])> {
        match self.slots.get(path)? {
            Slot::Ready(t, size) => Some((t, *size)),
            _ => None,
        }
    }

    /// Some thumbnails are still being made.
    pub fn loading(&self) -> bool {
        self.slots.values().any(|s| matches!(s, Slot::Queued))
    }

    pub fn failed(&self, path: &Path) -> bool {
        matches!(self.slots.get(path), Some(Slot::Failed))
    }
}
