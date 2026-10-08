#!/bin/sh
# Install HexDB from the latest GitHub release (Linux and macOS):
#   curl -fsSL https://raw.githubusercontent.com/dreaminhex/hexdb/main/packaging/install.sh | sh
# Installs hexdb_api and hexdb to ~/.local/bin (or $HEXDB_BIN), the admin UI
# and a sample config to ~/.local/share/hexdb (or $HEXDB_HOME).
set -eu

repo="dreaminhex/hexdb"
bin_dir="${HEXDB_BIN:-$HOME/.local/bin}"
home_dir="${HEXDB_HOME:-$HOME/.local/share/hexdb}"

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  *) echo "No HexDB build for $os/$arch; build from source with cargo." >&2; exit 1 ;;
esac

tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
[ -n "$tag" ] || { echo "Couldn't find the latest release." >&2; exit 1; }
name="hexdb-$tag-$target"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading HexDB $tag for $target..."
curl -fsSL -o "$tmp/$name.tar.gz" "https://github.com/$repo/releases/download/$tag/$name.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "https://github.com/$repo/releases/download/$tag/SHA256SUMS"
expected=$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null; then actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1); else actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1); fi
[ "$expected" = "$actual" ] || { echo "Checksum mismatch; not installing." >&2; exit 1; }

tar xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$bin_dir" "$home_dir"
cp "$tmp/$name/hexdb_api" "$tmp/$name/hexdb" "$bin_dir/"
rm -rf "$home_dir/ui"
cp -r "$tmp/$name/ui" "$home_dir/ui"
if [ ! -f "$home_dir/hexdb.toml" ]; then
  cp "$tmp/$name/hexdb.toml" "$home_dir/hexdb.toml"
  printf '[storage]\nencryption_key = "%s"\n' "$("$bin_dir/hexdb" secret)" > "$home_dir/hexdb.local.toml"
  chmod 600 "$home_dir/hexdb.local.toml"
fi
echo "Installed to $bin_dir. Start it with: hexdb start --config $home_dir/hexdb.toml"
echo "Back up $home_dir/hexdb.local.toml: it holds the key your data is encrypted with."
