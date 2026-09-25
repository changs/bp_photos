//! Free preset collections the app can download (the palette's "Get Free Presets").
//!
//! Each is pinned to a reviewed commit, so what gets installed can't change underneath us.

use std::io::Read;
use std::path::{Path, PathBuf};

pub struct Pack {
    pub name: &'static str,
    /// Folder under the presets directory (its sub-folders become groups).
    pub folder: &'static str,
    pub repo: &'static str,
    pub commit: &'static str,
    /// Folder inside the repository that holds the presets.
    pub subdir: &'static str,
}

pub const PACKS: [Pack; 2] = [
    Pack {
        name: "~450 Lightroom film presets",
        folder: "Film Presets (peva3)",
        repo: "peva3/Lightroom-Presets",
        commit: "f22f4d8057aaaada6df3f0ed7fbe6b952a30db77",
        subdir: "Presets",
    },
    Pack {
        name: "~300 film LUTs (G'MIC)",
        folder: "Film LUTs (GMIC)",
        repo: "YahiaAngelo/Film-Luts",
        commit: "af957b631a304e6be94778b96cb3395de3438e9f",
        subdir: "luts",
    },
];

/// Refuse anything unreasonably large (the packs are ~2 MB and ~26 MB).
const MAX_DOWNLOAD: u64 = 100 << 20;

/// Downloads every pack and unpacks its presets (and licence) into `presets_dir`, reporting
/// progress through `progress`. Returns how many preset files were written.
pub fn install(presets_dir: &Path, progress: impl Fn(String)) -> Result<usize, String> {
    let mut written = 0;
    for (i, pack) in PACKS.iter().enumerate() {
        progress(format!("Downloading {} ({}/{})…", pack.name, i + 1, PACKS.len()));
        let url = format!("https://codeload.github.com/{}/zip/{}", pack.repo, pack.commit);
        let bytes = ureq::get(&url)
            .call()
            .map_err(|e| format!("download failed: {e}"))?
            .body_mut()
            .with_config()
            .limit(MAX_DOWNLOAD)
            .read_to_vec()
            .map_err(|e| format!("download failed: {e}"))?;
        progress(format!("Unpacking {}…", pack.name));
        written += unpack(&bytes, pack, &presets_dir.join(pack.folder))?;
    }
    Ok(written)
}

/// Writes the pack's preset files, keeping their sub-folders, plus LICENSE and SOURCE notes.
fn unpack(zip_bytes: &[u8], pack: &Pack, dest: &Path) -> Result<usize, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).map_err(|e| format!("bad archive: {e}"))?;
    let mut written = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        // `enclosed_name` rejects absolute paths and `..`, so nothing lands outside `dest`.
        let Some(path) = entry.enclosed_name() else { continue };
        // Archives from GitHub wrap everything in a "<repo>-<commit>/" folder.
        let inner: PathBuf = path.components().skip(1).collect();
        let target = if inner == Path::new("LICENSE") {
            dest.join("LICENSE.txt")
        } else if let Ok(rel) = inner.strip_prefix(pack.subdir)
            && crate::import::is_preset_file(rel)
        {
            dest.join(rel)
        } else {
            continue;
        };
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&target, data).map_err(|e| e.to_string())?;
        if target.extension().is_some_and(|e| e != "txt") {
            written += 1;
        }
    }
    let source = format!("From https://github.com/{} at commit {} (MIT licence, see LICENSE.txt).\n", pack.repo, pack.commit);
    std::fs::write(dest.join("SOURCE.txt"), source).map_err(|e| e.to_string())?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn unpacks_only_presets_and_licence_safely() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("repo-abc/LICENSE", "MIT"),
                ("repo-abc/Presets/Film/Portra.xmp", "<x/>"),
                ("repo-abc/Presets/readme.md", "skip"),
                ("repo-abc/other/Thing.xmp", "skip: outside subdir"),
            ] {
                z.start_file(name, opts).unwrap();
                z.write_all(body.as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        let dir = std::env::temp_dir().join(format!("bp_photos_pack_{}", std::process::id()));
        let pack = Pack { name: "t", folder: "T", repo: "r/r", commit: "abc", subdir: "Presets" };
        assert_eq!(unpack(buf.get_ref(), &pack, &dir).unwrap(), 1);
        assert!(dir.join("Film/Portra.xmp").exists());
        assert!(dir.join("LICENSE.txt").exists() && dir.join("SOURCE.txt").exists());
        assert!(!dir.join("readme.md").exists() && !dir.join("Thing.xmp").exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
