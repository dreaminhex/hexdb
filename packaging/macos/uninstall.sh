#!/bin/sh
# Removes HexDB installed from HexDB.pkg:  sudo /usr/local/hexdb/uninstall.sh
# Your configuration, encryption key and data in
# ~/Library/Application Support/HexDB are left alone; delete that folder too if
# you no longer need the data.
set -e
if [ "$(id -u)" != "0" ]; then
  echo "Run it with sudo: sudo /usr/local/hexdb/uninstall.sh" >&2
  exit 1
fi
for link in /usr/local/bin/hexdb /usr/local/bin/hexdb_api; do
  if [ -L "$link" ] && [ "$(readlink "$link")" = "/usr/local/hexdb/$(basename "$link")" ]; then
    rm -f "$link"
  fi
done
rm -rf /usr/local/hexdb
pkgutil --forget com.dreaminhex.hexdb >/dev/null 2>&1 || true
echo "HexDB removed. Your data in ~/Library/Application Support/HexDB is still there."
