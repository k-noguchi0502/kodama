#!/bin/sh
# Installs Kodama from an unpacked release archive.
#
#   ./install.sh               # into ~/.local (or /usr/local as root)
#   ./install.sh --prefix DIR  # somewhere else
#   ./install.sh --uninstall   # remove what this script installed
set -eu

here=$(cd "$(dirname "$0")" && pwd)
if [ "$(id -u)" -eq 0 ]; then prefix=/usr/local; else prefix="$HOME/.local"; fi
uninstall=0

while [ $# -gt 0 ]; do
  case $1 in
    --prefix) prefix=$2; shift 2 ;;
    --prefix=*) prefix=${1#*=}; shift ;;
    --uninstall) uninstall=1; shift ;;
    -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
    *) echo "install.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

bin="$prefix/bin/kodama"
desktop="$prefix/share/applications/kodama.desktop"
icon="$prefix/share/icons/hicolor/512x512/apps/kodama.png"
lang="$prefix/share/kodama/lang"

if [ "$uninstall" -eq 1 ]; then
  rm -f "$bin" "$desktop" "$icon"
  if [ -d "$here/lang" ]; then
    for f in "$here/lang"/*.lua; do
      [ -f "$f" ] || continue
      rm -f "$lang/$(basename "$f")"
    done
    rmdir "$lang" 2>/dev/null || true
    rmdir "$prefix/share/kodama" 2>/dev/null || true
  fi
  echo "Removed Kodama from $prefix"
else
  install -Dm755 "$here/kodama" "$bin"
  install -Dm644 "$here/kodama.desktop" "$desktop"
  install -Dm644 "$here/kodama.png" "$icon"
  if [ -d "$here/lang" ]; then
    for f in "$here/lang"/*.lua; do
      [ -f "$f" ] || continue
      install -Dm644 "$f" "$lang/$(basename "$f")"
    done
  fi
  echo "Installed Kodama to $bin"
  case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "Note: $prefix/bin is not in PATH" ;;
  esac
fi

command -v gtk-update-icon-cache >/dev/null 2>&1 &&
  gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" 2>/dev/null || true
command -v update-desktop-database >/dev/null 2>&1 &&
  update-desktop-database -q "$prefix/share/applications" 2>/dev/null || true
