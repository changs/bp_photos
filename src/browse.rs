//! Stepping through the photos in a folder, with the neighbours decoded ahead of time.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::loader::{self, Decoded, Pixels};

/// Don't keep decoded neighbours larger than this in memory.
const MAX_CACHED_BYTES: usize = 400 << 20;

type Loaded = (PathBuf, Result<Decoded, String>);

pub struct Browser {
    dir: Option<PathBuf>,
    /// Photos in `dir`, sorted by name.
    pub files: Vec<PathBuf>,
    cache: Vec<(PathBuf, Decoded)>,
    inflight: Vec<PathBuf>,
    tx: Sender<Loaded>,
    rx: Receiver<Loaded>,
    /// A photo the user moved to while it was still being preloaded.
    pub pending: Option<PathBuf>,
}

fn size_of(d: &Decoded) -> usize {
    match &d.pixels {
        Pixels::Srgb8(p) => p.len(),
        Pixels::LinearF16(p) => p.len() * 2,
    }
}

/// Photos in a folder, sorted by file name (case-insensitive), skipping hidden files.
pub fn list_photos(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && loader::is_photo(p))
        .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .collect();
    files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    files
}

impl Browser {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self { dir: None, files: Vec::new(), cache: Vec::new(), inflight: Vec::new(), tx, rx, pending: None }
    }

    /// Lists the folder containing `photo` (again, if it changed or the photo is new to us).
    pub fn index(&mut self, photo: &Path) {
        let dir = photo.parent().map(Path::to_path_buf);
        if dir != self.dir || !self.files.iter().any(|f| f == photo) {
            self.files = dir.as_deref().map(list_photos).unwrap_or_default();
            self.cache.clear();
            self.dir = dir;
        }
    }

    pub fn position(&self, photo: &Path) -> Option<usize> {
        self.files.iter().position(|f| f == photo)
    }

    pub fn neighbour(&self, photo: &Path, delta: isize) -> Option<PathBuf> {
        let i = self.position(photo)? as isize + delta;
        (0..self.files.len() as isize).contains(&i).then(|| self.files[i as usize].clone())
    }

    /// A preloaded photo, removed from the cache.
    pub fn take(&mut self, path: &Path) -> Option<Decoded> {
        let i = self.cache.iter().position(|(p, _)| p == path)?;
        Some(self.cache.swap_remove(i).1)
    }

    pub fn is_inflight(&self, path: &Path) -> bool {
        self.inflight.iter().any(|p| p == path)
    }

    /// Starts decoding the photos either side of `current`, and forgets the rest.
    pub fn prefetch(&mut self, current: &Path, max_dim: u32, ctx: &eframe::egui::Context) {
        let wanted: Vec<PathBuf> = [1, -1].into_iter().filter_map(|d| self.neighbour(current, d)).collect();
        self.cache.retain(|(p, _)| wanted.contains(p));
        for path in wanted {
            if self.is_inflight(&path) || self.cache.iter().any(|(p, _)| *p == path) {
                continue;
            }
            self.inflight.push(path.clone());
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let result = loader::load(&path, max_dim);
                _ = tx.send((path, result));
                ctx.request_repaint();
            });
        }
    }

    /// Collects finished preloads; returns the one the user is waiting for, if it arrived.
    pub fn poll(&mut self) -> Option<Loaded> {
        let mut ready = None;
        while let Ok((path, result)) = self.rx.try_recv() {
            self.inflight.retain(|p| *p != path);
            if self.pending.as_ref() == Some(&path) {
                self.pending = None;
                ready = Some((path, result));
            } else if let Ok(decoded) = result
                && size_of(&decoded) <= MAX_CACHED_BYTES
            {
                self.cache.push((path, decoded));
            }
        }
        ready
    }

    pub fn busy(&self) -> bool {
        !self.inflight.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_photos_sorted_and_steps_through_them() {
        let dir = std::env::temp_dir().join(format!("bp_photos_browse_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["b.JPG", "a.heic", "c.txt", ".hidden.jpg", "C.png"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let files: Vec<String> = list_photos(&dir).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(files, ["a.heic", "b.JPG", "C.png"]);

        let mut b = Browser::new();
        b.index(&dir.join("b.JPG"));
        assert_eq!(b.neighbour(&dir.join("b.JPG"), 1), Some(dir.join("C.png")));
        assert_eq!(b.neighbour(&dir.join("a.heic"), -1), None);
        std::fs::remove_dir_all(dir).ok();
    }
}
