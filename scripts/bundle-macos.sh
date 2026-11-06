#!/bin/bash
# Build "Amlogic Flash.app" (and a .dmg) into dist/
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP="dist/Amlogic Flasher.app"

cargo build --release
rm -rf "$APP" && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/amlogic-flash "$APP/Contents/MacOS/amlogic-flash"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Amlogic Flasher</string>
  <key>CFBundleDisplayName</key><string>Amlogic Flasher</string>
  <key>CFBundleIdentifier</key><string>com.srkraut.amlogic-flash</string>
  <key>CFBundleExecutable</key><string>amlogic-flash</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleDocumentTypes</key><array><dict>
    <key>CFBundleTypeName</key><string>Amlogic burn image</string>
    <key>CFBundleTypeExtensions</key><array><string>img</string></array>
    <key>CFBundleTypeRole</key><string>Viewer</string>
  </dict></array>
</dict></plist>
PLIST
[ -f assets/AppIcon.icns ] && cp assets/AppIcon.icns "$APP/Contents/Resources/" && \
  /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string AppIcon" "$APP/Contents/Info.plist"
codesign --force --deep -s - "$APP"
rm -f "dist/Amlogic-Flasher-${VERSION}.dmg"
hdiutil create -quiet -volname "Amlogic Flasher" -srcfolder "$APP" -ov -format UDZO "dist/Amlogic-Flasher-${VERSION}.dmg"
echo "built: $APP and dist/Amlogic-Flasher-${VERSION}.dmg"
