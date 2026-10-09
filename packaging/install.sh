#!/bin/sh
# Install HexDB from the latest GitHub release (Linux and macOS), no root needed:
#   curl -fsSL https://raw.githubusercontent.com/dreaminhex/hexdb/main/packaging/install.sh | sh
#
# Puts hexdb_api and hexdb in ~/.local/bin (or $HEXDB_BIN), the admin UI and the
# ODBC driver in ~/.local/share/hexdb (or $HEXDB_SHARE), then runs `hexdb init`,
# which creates your configuration and encryption key in ~/.config/hexdb
# (Linux) or ~/Library/Application Support/HexDB (macOS). Set HEXDB_VERSION
# (for example v1.0.0) to install a specific release.
set -eu

repo="dreaminhex/hexdb"
bin_dir="${HEXDB_BIN:-$HOME/.local/bin}"
share_dir="${HEXDB_SHARE:-$HOME/.local/share/hexdb}"

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  *) echo "No HexDB build for $os/$arch; build from source with cargo." >&2; exit 1 ;;
esac

tag="${HEXDB_VERSION:-}"
if [ -z "$tag" ]; then
  tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$tag" ] || { echo "Couldn't find the latest release." >&2; exit 1; }
name="hexdb-$tag-$target"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading HexDB $tag for $target..."
curl -fsSL -o "$tmp/$name.tar.gz" "https://github.com/$repo/releases/download/$tag/$name.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "https://github.com/$repo/releases/download/$tag/SHA256SUMS"
expected=$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null; then actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1); else actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1); fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || { echo "Checksum mismatch; not installing." >&2; exit 1; }

tar xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$bin_dir" "$share_dir"
cp "$tmp/$name/hexdb_api" "$tmp/$name/hexdb" "$bin_dir/"
rm -rf "$share_dir/ui" "$share_dir/odbc"
cp -r "$tmp/$name/ui" "$share_dir/ui"
cp -r "$tmp/$name/odbc" "$share_dir/odbc"

echo "Installed HexDB $tag: $bin_dir/hexdb and $bin_dir/hexdb_api."
if [ -f "$share_dir/hexdb.toml" ]; then
  # Installed by an earlier version of this script: keep using that config.
  echo "Your existing configuration is $share_dir/hexdb.toml. Start with: hexdb start --config $share_dir/hexdb.toml"
else
  "$bin_dir/hexdb" init
fi
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "Add $bin_dir to your PATH to run hexdb from anywhere." ;;
esac
echo "The ODBC driver is in $share_dir/odbc."
