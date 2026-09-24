<p align="center">
  <img src="assets/icon.png" width="128" alt="">
</p>

<h1 align="center">bp_photos</h1>

A small desktop app for trying presets on photos. Open a photo, see every preset applied to it,
pick one, export. It's the preset browser from Lightroom and nothing else.

Rust + egui + wgpu. Everything runs on the GPU, so the preview and all thumbnails update instantly.
Works on macOS and Linux.

![bp_photos with a photo and the presets recommended for it](docs/screenshot.jpg)

![Before/after comparison with the folder's filmstrip below](docs/screenshot-compare.jpg)

## Features

- Opens JPEG, PNG, TIFF, WebP, HEIC/AVIF and camera RAW (CR2/CR3, NEF, ARW, RAF, DNG, …).
  HEIC and RAW show their embedded preview first, then the full image.
- Reads Lightroom presets (`.xmp`, `.lrtemplate`) and `.cube` LUTs. Drop them on the window or use
  *Import presets*; they're kept in `~/Library/Application Support/bp_photos/presets`
  (`~/.config/bp_photos/presets` on Linux), one group per folder.
- Suggests about a dozen presets per photo. Every preset is rendered small and scored for clipping,
  exposure, contrast, saturation and skin tones; hover a suggestion to see why it was picked.
- Amount slider (0–150%), or scroll over the photo.
- Before/after view, or hold the mouse on the photo to see the original. Zoom to 100% to check
  grain and sharpness.
- Step through the photos in a folder; the next and previous ones are decoded in the background.
  An optional filmstrip shows the whole folder, using the thumbnails already embedded in the files.
- Crop with aspect ratios (free, original, 1:1, 4:5, 2:3, 16:9).
- Export at full size or for Instagram, X, Facebook or a custom size, as JPEG, PNG or TIFF.
  Or copy to the clipboard. JPEG and PNG exports keep the date, camera, lens and exposure info,
  and the location unless you untick it.
- Picks up your Ghostty font and colour theme, if you use Ghostty.

Lightroom's processing isn't public, so presets come out close to Lightroom, not identical. Masks,
profiles, sharpening, noise reduction and lens corrections are ignored.

## Download

macOS 12 or later, Apple silicon: grab the `.dmg` from
[Releases](https://github.com/changs/bp_photos/releases) and drag the app to Applications.

The app isn't notarised by Apple, so the first time macOS will refuse to open it. Open it once
from System Settings → Privacy & Security → *Open Anyway*, or run
`xattr -dr com.apple.quarantine "/Applications/BP Photos.app"`.

## Build

```sh
brew install libheif pkgconf        # macOS
sudo apt install libheif-dev pkg-config libxkbcommon-dev libwayland-dev libvulkan1   # Debian/Ubuntu

cargo run --release -- photo.jpg
```

`rust-toolchain.toml` pins the Rust version; rustup installs it on first build.

`scripts/bundle-macos.sh` makes `dist/BP Photos.app` and a `.dmg` (needs `brew install cmake meson ninja`).
It builds libheif with only its decoders, so no GPL encoder code ends up in the app.

## Keys

![The command palette](docs/screenshot-palette.jpg)

| | |
|---|---|
| `Shift+⌘P` or `⌘K` | command palette |
| `⌘O` / `Shift+⌘O` | open a photo / a folder (opens its first photo, with the filmstrip) |
| arrows | move through presets |
| `[` / `]` | previous / next photo in the folder (preloaded, so it's instant) |
| `F` | filmstrip |
| `Z` or double-click | 100% view, drag to pan (both sides in before/after) |
| pinch, or `⌘`/`Ctrl` + scroll | zoom from fit to 800% around the pointer |
| scroll | change amount |
| `\` (hold) | show original |
| `Y` | before / after |
| `C` | crop (`X` rotates the ratio, `Enter` applies, `Esc` cancels) |
| `⌘E` / `Ctrl+E` | export |
| `⌘C` / `Ctrl+C` | copy to clipboard |

## Command line

```sh
bp_photos apply --preset "Portra-ish" --size instagram-portrait *.heic --out insta/
bp_photos recommend photo.jpg        # suggested presets and why
bp_photos presets                    # list installed presets
bp_photos bench photo.heic           # time each loading step
```

## Presets

A couple of dozen are built in. For more, these work well and are MIT licensed:

- [peva3/Lightroom-Presets](https://github.com/peva3/Lightroom-Presets): ~450 film looks (`.xmp`)
- [YahiaAngelo/Film-Luts](https://github.com/YahiaAngelo/Film-Luts): ~300 film LUTs (`.cube`)

The screenshot uses a public-domain sample from [raw.pixls.us](https://raw.pixls.us).
