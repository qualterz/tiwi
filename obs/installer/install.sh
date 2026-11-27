#!/bin/sh
set -eu

REPO="qualterz/tiwi"

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Error: required command '$1' not found." >&2
    exit 1
  fi
}

need_cmd curl
need_cmd tar

OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Darwin)
    ASSET_SUFFIX="macos-universal.tar.gz"
    ;;
  Linux)
    if [ "$ARCH" != "x86_64" ]; then
      echo "Error: unsupported Linux architecture '$ARCH' (only x86_64 prebuilt plugin is currently available)." >&2
      exit 1
    fi
    ASSET_SUFFIX="linux-x64.tar.gz"
    ;;
  MINGW*|MSYS*|CYGWIN*)
    echo "Error: Windows detected. Use install.ps1 or the .msi installer instead." >&2
    exit 1
    ;;
  *)
    echo "Error: unsupported OS '$OS'." >&2
    exit 1
    ;;
esac

TMP_DIR=$(mktemp -d)
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

TAG=$(printf '%s' "${TIWI_VERSION:-}" | tr -d '[:space:]')
if [ -z "$TAG" ]; then
  REDIRECT_URL=$(curl -fsSI -o /dev/null -w '%{redirect_url}' "https://github.com/$REPO/releases/latest")
  REDIRECT_URL="${REDIRECT_URL%%[?#]*}"
  case "$REDIRECT_URL" in
    */releases/tag/*) TAG="${REDIRECT_URL##*/}" ;;
  esac
fi

if [ -z "$TAG" ]; then
  echo "Error: could not determine latest Tiwi release." >&2
  exit 1
fi

case "$TAG" in
  v*) ;;
  *) TAG="v$TAG" ;;
esac
VERSION="${TAG#v}"
ASSET="tiwi-obs-${VERSION}-${ASSET_SUFFIX}"

BASE_URL="https://github.com/$REPO/releases/download/$TAG"
ARCHIVE_PATH="$TMP_DIR/$ASSET"
SUMS_PATH="$TMP_DIR/SHA256SUMS.txt"

echo "Downloading $ASSET ($TAG)..."
curl -fsSL "$BASE_URL/$ASSET" -o "$ARCHIVE_PATH"
curl -fsSL "$BASE_URL/SHA256SUMS.txt" -o "$SUMS_PATH"

EXPECTED_HASH=$(
  awk -v f="$ASSET" '
    {
      sub(/\r$/, "")
      file = $NF
      sub(/^\*/, "", file)
      if (file == f && length($1) == 64 && $1 ~ /^[0-9A-Fa-f]+$/) {
        print tolower($1)
        exit
      }
    }
  ' "$SUMS_PATH"
)
if [ -z "$EXPECTED_HASH" ]; then
  echo "Error: checksum for $ASSET not found in SHA256SUMS.txt." >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_HASH=$(sha256sum "$ARCHIVE_PATH" | awk '{ print tolower($1) }')
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_HASH=$(shasum -a 256 "$ARCHIVE_PATH" | awk '{ print tolower($1) }')
else
  echo "Error: neither sha256sum nor shasum found." >&2
  exit 1
fi

if [ "$ACTUAL_HASH" != "$EXPECTED_HASH" ]; then
  echo "Error: SHA256 mismatch for $ASSET (expected $EXPECTED_HASH, got $ACTUAL_HASH)." >&2
  exit 1
fi

EXTRACT_DIR="$TMP_DIR/extract"
mkdir -p "$EXTRACT_DIR"
tar -xzf "$ARCHIVE_PATH" -C "$EXTRACT_DIR"

install_plugin() {
  src="$1"
  dest="$2"
  mkdir -p "$(dirname "$dest")"
  staging="${dest}.tmp.$$"
  rm -rf "$staging"
  if ! cp -RPp "$src" "$staging"; then
    rm -rf "$staging"
    exit 1
  fi
  rm -rf "$dest"
  mv "$staging" "$dest"
  echo "Installed Tiwi for OBS Studio ($TAG) to $dest."
}

if [ "$OS" = "Darwin" ]; then
  SRC="$EXTRACT_DIR/tiwi-obs.plugin"
  if [ ! -d "$SRC" ]; then
    echo "Error: extracted archive does not contain tiwi-obs.plugin." >&2
    exit 1
  fi
  DEST="$HOME/Library/Application Support/obs-studio/plugins/tiwi-obs.plugin"
  install_plugin "$SRC" "$DEST"
  xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true
else
  SRC="$EXTRACT_DIR/tiwi-obs"
  if [ ! -f "$SRC/bin/64bit/tiwi-obs.so" ]; then
    echo "Error: extracted archive does not contain tiwi-obs/bin/64bit/tiwi-obs.so." >&2
    exit 1
  fi

  FLATPAK_ROOT="$HOME/.var/app/com.obsproject.Studio"
  NATIVE_OBS="${XDG_CONFIG_HOME:-$HOME/.config}/obs-studio"
  INSTALLED=0

  if [ -d "$FLATPAK_ROOT" ] || { command -v flatpak >/dev/null 2>&1 && flatpak info com.obsproject.Studio >/dev/null 2>&1; }; then
    install_plugin "$SRC" "$FLATPAK_ROOT/config/obs-studio/plugins/tiwi-obs"
    INSTALLED=1
  fi

  if [ -d "$NATIVE_OBS" ] || [ "$INSTALLED" -eq 0 ]; then
    install_plugin "$SRC" "$NATIVE_OBS/plugins/tiwi-obs"
  fi
fi

echo "Restart OBS to use Tiwi."
