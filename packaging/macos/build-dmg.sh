#!/bin/bash
# Builds the macOS installer: a DMG holding HexDB.pkg, which installs universal
# (Apple Silicon and Intel) binaries into /usr/local/hexdb and links hexdb and
# hexdb_api into /usr/local/bin. Run on a Mac; the release workflow runs it on
# every tag.
#
#   packaging/macos/build-dmg.sh <version> <arm64 archive dir> <x86_64 archive dir> <output.dmg>
#
# The two archive dirs are unpacked release archives (hexdb-vX-aarch64-apple-darwin
# and hexdb-vX-x86_64-apple-darwin). Signing and notarization happen when these
# are set (see packaging/RELEASING.md); without them the package is unsigned and
# macOS asks the user to allow it in System Settings, Privacy & Security:
#   MACOS_APP_IDENTITY        "Developer ID Application: Name (TEAMID)"
#   MACOS_INSTALLER_IDENTITY  "Developer ID Installer: Name (TEAMID)"
#   APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD   for notarytool
set -euo pipefail

version="${1#v}"
arm="$2"
intel="$3"
out="$4"
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

root="$work/root/usr/local/hexdb"
mkdir -p "$root/odbc"

echo "Making universal binaries..."
for f in hexdb hexdb_api; do
  lipo -create "$arm/$f" "$intel/$f" -output "$root/$f"
  chmod 755 "$root/$f"
done
lipo -create "$arm/odbc/libhexdb_odbc.dylib" "$intel/odbc/libhexdb_odbc.dylib" -output "$root/odbc/libhexdb_odbc.dylib"
cp -R "$arm/ui" "$root/ui"
cp "$arm/README.md" "$arm/MANUAL.md" "$root/"
[ -f "$arm/LICENSE" ] && cp "$arm/LICENSE" "$root/"
cp "$here/uninstall.sh" "$root/uninstall.sh"
chmod 755 "$root/uninstall.sh"
lipo -info "$root/hexdb"

if [ -n "${MACOS_APP_IDENTITY:-}" ]; then
  echo "Signing the binaries..."
  for f in "$root/hexdb" "$root/hexdb_api" "$root/odbc/libhexdb_odbc.dylib"; do
    codesign --force --options runtime --timestamp --sign "$MACOS_APP_IDENTITY" "$f"
  done
fi

mkdir -p "$work/scripts"
cp "$here/preinstall" "$here/postinstall" "$work/scripts/"
chmod 755 "$work/scripts/"*

pkg="$work/dmg/HexDB.pkg"
mkdir -p "$work/dmg"
sign_args=()
[ -n "${MACOS_INSTALLER_IDENTITY:-}" ] && sign_args=(--sign "$MACOS_INSTALLER_IDENTITY" --timestamp)
pkgbuild --root "$work/root" --install-location / --scripts "$work/scripts" \
  --identifier com.dreaminhex.hexdb --version "$version" ${sign_args[@]+"${sign_args[@]}"} "$pkg"

if [ -n "${APPLE_ID:-}" ] && [ -n "${MACOS_INSTALLER_IDENTITY:-}" ]; then
  echo "Notarizing..."
  xcrun notarytool submit "$pkg" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD" --wait
  xcrun stapler staple "$pkg"
fi

cat > "$work/dmg/Read me.txt" <<EOF
HexDB $version

1. Open HexDB.pkg and follow the installer. It puts HexDB in /usr/local/hexdb
   and the hexdb and hexdb_api commands in /usr/local/bin.
2. Open Terminal and run:  hexdb start
   The first start creates your configuration and encryption key in
   ~/Library/Application Support/HexDB and prints where the generated
   administrator password is.
3. Open http://127.0.0.1:7700/ui/ and sign in as hexdbadmin.

The ODBC driver is in /usr/local/hexdb/odbc. To remove HexDB:
  sudo /usr/local/hexdb/uninstall.sh

Documentation: https://github.com/dreaminhex/hexdb
EOF

rm -f "$out"
hdiutil create -volname "HexDB $version" -srcfolder "$work/dmg" -ov -format UDZO "$out"
if [ -n "${MACOS_APP_IDENTITY:-}" ]; then
  codesign --force --timestamp --sign "$MACOS_APP_IDENTITY" "$out"
fi
echo "Built $out"
