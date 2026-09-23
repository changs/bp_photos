#!/usr/bin/env bash
# Builds dist/BP Photos.app and a .dmg for it.
#
# libheif and its decoders are built here from source rather than taken from Homebrew:
#  - Homebrew's libheif links GPL encoders (x265, x264), which can't ship inside this app.
#    Ours only decodes: HEIC via libde265 (LGPL), AVIF via dav1d (BSD), both linked statically
#    into libheif, which stays a separate, replaceable dylib (as the LGPL asks).
#  - Homebrew's libraries only run on the macOS they were built for; ours target
#    MACOSX_DEPLOYMENT_TARGET, so the app runs on older macOS too.
#
# Needs: Xcode command line tools, Homebrew `cmake meson ninja pkgconf`, Rust.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
HEIF_VERSION=1.23.4
DE265_VERSION=1.1.3
DAV1D_VERSION=1.5.4
ARCH=$(uname -m)
WORK="$PWD/target/macos"
PREFIX="$WORK/deps-heif$HEIF_VERSION-de265$DE265_VERSION-dav1d$DAV1D_VERSION"
APP="dist/BP Photos.app"
export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-12.0}

step() { printf '\n\033[1m%s\033[0m\n' "$*"; }

step "1/5  libheif $HEIF_VERSION with libde265 $DE265_VERSION and dav1d $DAV1D_VERSION (decode-only)"
if [ ! -f "$PREFIX/lib/libheif.dylib" ]; then
  rm -rf "$WORK/src" && mkdir -p "$WORK/src"
  curl -fsSL "https://github.com/strukturag/libde265/releases/download/v$DE265_VERSION/libde265-$DE265_VERSION.tar.gz" | tar xz -C "$WORK/src"
  curl -fsSL "https://downloads.videolan.org/pub/videolan/dav1d/$DAV1D_VERSION/dav1d-$DAV1D_VERSION.tar.xz" | tar xJ -C "$WORK/src"
  curl -fsSL "https://github.com/strukturag/libheif/releases/download/v$HEIF_VERSION/libheif-$HEIF_VERSION.tar.gz" | tar xz -C "$WORK/src"

  cmake -S "$WORK/src/libde265-$DE265_VERSION" -B "$WORK/src/de265-build" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$PREFIX" -DBUILD_SHARED_LIBS=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DENABLE_SDL=OFF -DENABLE_DECODER=OFF -DENABLE_ENCODER=OFF >/dev/null
  cmake --build "$WORK/src/de265-build" -j "$(sysctl -n hw.ncpu)" >/dev/null
  cmake --install "$WORK/src/de265-build" >/dev/null

  meson setup "$WORK/src/dav1d-build" "$WORK/src/dav1d-$DAV1D_VERSION" --prefix="$PREFIX" --libdir=lib \
    --buildtype=release --default-library=static -Denable_tools=false -Denable_tests=false >/dev/null
  ninja -C "$WORK/src/dav1d-build" install >/dev/null

  # Find our static decoders before any installed elsewhere.
  export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
  cmake -S "$WORK/src/libheif-$HEIF_VERSION" -B "$WORK/src/libheif-build" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_PREFIX_PATH="$PREFIX" -DCMAKE_FIND_FRAMEWORK=NEVER \
    -DCMAKE_INSTALL_PREFIX="$PREFIX" -DBUILD_TESTING=OFF -DWITH_EXAMPLES=OFF -DWITH_GDK_PIXBUF=OFF \
    -DENABLE_PLUGIN_LOADING=OFF -DWITH_LIBDE265=ON -DWITH_DAV1D=ON \
    -DWITH_X265=OFF -DWITH_AOM_ENCODER=OFF -DWITH_AOM_DECODER=OFF -DWITH_RAV1E=OFF -DWITH_SvtEnc=OFF \
    -DWITH_KVAZAAR=OFF -DWITH_OpenJPEG_DECODER=OFF -DWITH_OpenJPEG_ENCODER=OFF -DWITH_OPENJPH_ENCODER=OFF \
    -DWITH_OPENJPH_DECODER=OFF -DWITH_JPEG_DECODER=OFF -DWITH_JPEG_ENCODER=OFF -DWITH_FFMPEG_DECODER=OFF \
    -DWITH_LIBSHARPYUV=OFF -DWITH_UNCOMPRESSED_CODEC=OFF -DWITH_HEADER_COMPRESSION=OFF -DWITH_VVDEC=OFF -DWITH_VVENC=OFF \
    -DWITH_X264=OFF -DWITH_OpenH264_DECODER=OFF -DWITH_UVG266=OFF -DWITH_WEBCODECS=OFF -DWITH_GEOTIFF=OFF >/dev/null
  cmake --build "$WORK/src/libheif-build" -j "$(sysctl -n hw.ncpu)" >/dev/null
  cmake --install "$WORK/src/libheif-build" >/dev/null
fi

step "2/5  bp_photos $VERSION"
# A separate target dir, so everyday builds keep using the system libheif.
export CARGO_TARGET_DIR="$WORK/cargo"
PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" cargo build --release --quiet
BIN="$CARGO_TARGET_DIR/release/bp_photos"

step "3/5  app bundle"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks" "$APP/Contents/Resources/Licenses"
cp "$BIN" "$APP/Contents/MacOS/bp_photos"
FW="$APP/Contents/Frameworks"

# Copies every non-system library a binary needs into Frameworks, and points it there.
resolve() { # an install name → a file on disk
  case "$1" in
    @rpath/*) for d in "$PREFIX/lib" /opt/homebrew/lib /usr/local/lib; do [ -f "$d/${1#@rpath/}" ] && { echo "$d/${1#@rpath/}"; return; }; done ;;
    *) [ -f "$1" ] && echo "$1" ;;
  esac
}
bundle_deps() {
  local file=$1 self dep name src
  self=$(otool -D "$file" | tail -n +2)
  for dep in $(otool -L "$file" | tail -n +2 | awk '{print $1}' | grep -v -E '^(/System/|/usr/lib/)'); do
    [ "$dep" = "$self" ] && continue
    name=$(basename "$dep")
    if [ ! -f "$FW/$name" ]; then
      src=$(resolve "$dep") || { echo "can't find $dep (needed by $file)" >&2; exit 1; }
      cp -L "$src" "$FW/$name"
      chmod u+w "$FW/$name"
      install_name_tool -id "@rpath/$name" "$FW/$name" 2>/dev/null
      bundle_deps "$FW/$name"
    fi
    install_name_tool -change "$dep" "@rpath/$name" "$file" 2>/dev/null
  done
}
bundle_deps "$APP/Contents/MacOS/bp_photos"
# Only look inside the bundle for libraries.
for rpath in $(otool -l "$APP/Contents/MacOS/bp_photos" | awk '/LC_RPATH/ {getline; getline; print $2}'); do
  install_name_tool -delete_rpath "$rpath" "$APP/Contents/MacOS/bp_photos"
done
install_name_tool -add_rpath "@executable_path/../Frameworks" "$APP/Contents/MacOS/bp_photos"

# Oldest macOS every bundled binary supports.
MIN_OS=$(for f in "$APP/Contents/MacOS/bp_photos" "$FW"/*.dylib; do vtool -show-build "$f" | awk '/minos/ {print $2}'; done | sort -V | tail -1)

# Licences of the bundled libraries (libde265 and dav1d are built into libheif).
L="$APP/Contents/Resources/Licenses"
cp "$WORK/src/libheif-$HEIF_VERSION/COPYING" "$L/libheif-LGPL-3.0.txt"
cp "$WORK/src/libde265-$DE265_VERSION/COPYING" "$L/libde265-LGPL-3.0.txt"
cp "$WORK/src/dav1d-$DAV1D_VERSION/COPYING" "$L/dav1d-BSD-2-Clause.txt"

# Icon.
ICONSET="$WORK/AppIcon.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) assets/icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>BP Photos</string>
  <key>CFBundleDisplayName</key><string>BP Photos</string>
  <key>CFBundleExecutable</key><string>bp_photos</string>
  <key>CFBundleIdentifier</key><string>engineering.bp.photos</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>$MIN_OS</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.photography</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

step "4/5  signing (ad hoc)"
for lib in "$FW"/*.dylib; do codesign --force --sign - "$lib" 2>/dev/null; done
codesign --force --sign - "$APP" 2>/dev/null
codesign --verify --strict "$APP"

step "5/5  disk image"
DMG="dist/BP-Photos-$VERSION-macos-$ARCH.dmg"
STAGE="$WORK/dmg"
rm -rf "$STAGE" "$DMG" && mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "BP Photos" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null

echo
echo "  $APP"
echo "  $DMG  ($(du -h "$DMG" | cut -f1))"
echo "  macOS $MIN_OS or later, $ARCH"
