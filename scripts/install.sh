#!/usr/bin/env bash
set -euo pipefail

REPO="selfonomy/duckagent"
APP="duck"
VERSION="latest"
INSTALL_DIR=""
INSTALL_DIR_EXPLICIT=false
NO_MODIFY_PATH=false

usage() {
  cat <<EOF
DuckAgent installer

Usage:
  install.sh [--version VERSION] [--install-dir DIR] [--no-modify-path]

Examples:
  curl -fsSL https://raw.githubusercontent.com/selfonomy/duckagent/main/scripts/install.sh | bash
  curl -fsSL https://raw.githubusercontent.com/selfonomy/duckagent/main/scripts/install.sh | bash -s -- --version 0.1.3
EOF
}

step() {
  printf '==> %s\n' "$1"
}

fail() {
  printf 'duck install error: %s\n' "$1" >&2
  exit 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    -v | --version | --release)
      [ "$#" -ge 2 ] || fail "$1 requires a value"
      VERSION="$2"
      shift 2
      ;;
    --install-dir | --dir)
      [ "$#" -ge 2 ] || fail "$1 requires a value"
      INSTALL_DIR="$2"
      INSTALL_DIR_EXPLICIT=true
      shift 2
      ;;
    --no-modify-path)
      NO_MODIFY_PATH=true
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1"
      ;;
  esac
done

resolve_install_dir() {
  if [ "$INSTALL_DIR_EXPLICIT" = true ]; then
    return
  fi

  case "$(uname -s)" in
    Linux)
      if [ "$(id -u)" -eq 0 ]; then
        INSTALL_DIR="/usr/local/bin"
      else
        INSTALL_DIR="$HOME/.local/bin"
      fi
      ;;
    *)
      INSTALL_DIR="$HOME/.local/bin"
      ;;
  esac
}

download_file() {
  url="$1"
  output="$2"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 --retry-delay 1 "$url" -o "$output"
    return
  fi

  if command -v wget >/dev/null 2>&1; then
    wget -q -O "$output" "$url"
    return
  fi

  fail "curl or wget is required"
}

file_sha256() {
  path="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print tolower($1)}'
    return
  fi

  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print tolower($1)}'
    return
  fi

  if command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$path" | awk '{print tolower($NF)}'
    return
  fi

  fail "sha256sum, shasum, or openssl is required to verify downloads"
}

normalize_version() {
  raw="$1"
  case "$raw" in
    "" | latest)
      printf 'latest\n'
      ;;
    v*)
      printf '%s\n' "${raw#v}"
      ;;
    *)
      printf '%s\n' "$raw"
      ;;
  esac
}

detect_target() {
  raw_os="$(uname -s)"
  raw_arch="$(uname -m)"

  case "$raw_os" in
    Darwin)
      os="apple-darwin"
      if [ "$raw_arch" = "x86_64" ] && [ "$(sysctl -in sysctl.proc_translated 2>/dev/null || printf 0)" = "1" ]; then
        raw_arch="arm64"
      fi
      ;;
    Linux)
      os="unknown-linux-musl"
      ;;
    MINGW* | MSYS* | CYGWIN*)
      fail "Windows installs should use scripts/install.ps1"
      ;;
    *)
      fail "unsupported operating system: $raw_os"
      ;;
  esac

  case "$raw_arch" in
    x86_64 | amd64)
      arch="x86_64"
      ;;
    arm64 | aarch64)
      arch="aarch64"
      ;;
    *)
      fail "unsupported CPU architecture: $raw_arch"
      ;;
  esac

  printf '%s-%s\n' "$arch" "$os"
}

download_base_url() {
  normalized="$(normalize_version "$VERSION")"
  if [ "$normalized" = "latest" ]; then
    printf 'https://github.com/%s/releases/latest/download\n' "$REPO"
  else
    printf 'https://github.com/%s/releases/download/v%s\n' "$REPO" "$normalized"
  fi
}

pick_profile() {
  case "$(uname -s):${SHELL:-}" in
    Darwin:*/zsh)
      printf '%s\n' "$HOME/.zprofile"
      ;;
    Darwin:*/bash)
      printf '%s\n' "$HOME/.bash_profile"
      ;;
    Linux:*/zsh)
      printf '%s\n' "$HOME/.zshrc"
      ;;
    Linux:*/bash)
      printf '%s\n' "$HOME/.bashrc"
      ;;
    *)
      printf '%s\n' "$HOME/.profile"
      ;;
  esac
}

path_contains() {
  dir="$1"
  case ":${PATH:-}:" in
    *":$dir:"*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

add_to_path_if_needed() {
  if path_contains "$INSTALL_DIR"; then
    return
  fi

  if [ "$NO_MODIFY_PATH" = true ]; then
    printf 'Add %s to PATH before running duck.\n' "$INSTALL_DIR"
    return
  fi

  profile="$(pick_profile)"
  mkdir -p "$(dirname "$profile")"
  touch "$profile"

  if ! grep -F "$INSTALL_DIR" "$profile" >/dev/null 2>&1; then
    {
      printf '\n# DuckAgent command\n'
      printf 'export PATH="%s:$PATH"\n' "$INSTALL_DIR"
    } >>"$profile"
    printf 'Added %s to PATH in %s.\n' "$INSTALL_DIR" "$profile"
  fi

  printf 'For this shell, run: export PATH="%s:$PATH"\n' "$INSTALL_DIR"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

require_command tar
require_command awk

resolve_install_dir
target="$(detect_target)"
asset="$APP-$target.tar.gz"
base_url="$(download_base_url)"

tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t duck-install)"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

archive="$tmp_dir/$asset"
checksum="$tmp_dir/$asset.sha256"
extract_dir="$tmp_dir/extract"

step "Downloading $asset"
download_file "$base_url/$asset" "$archive"
download_file "$base_url/$asset.sha256" "$checksum"

expected="$(awk '{print tolower($1); exit}' "$checksum")"
if [ "${#expected}" -ne 64 ]; then
  fail "invalid checksum file for $asset"
fi
case "$expected" in
  *[!0123456789abcdef]*)
    fail "invalid checksum file for $asset"
    ;;
esac

actual="$(file_sha256 "$archive")"
if [ "$actual" != "$expected" ]; then
  fail "checksum mismatch for $asset"
fi

mkdir -p "$extract_dir"
tar -xzf "$archive" -C "$extract_dir"
binary_path="$(find "$extract_dir" -type f -name "$APP" | head -n 1)"
[ -n "$binary_path" ] || fail "archive did not contain $APP"

step "Installing duck to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
tmp_bin="$INSTALL_DIR/.$APP.tmp.$$"
cp "$binary_path" "$tmp_bin"
chmod 0755 "$tmp_bin"
mv -f "$tmp_bin" "$INSTALL_DIR/$APP"

if "$INSTALL_DIR/$APP" --version >/dev/null 2>&1; then
  version_output="$("$INSTALL_DIR/$APP" --version)"
  printf 'Installed %s at %s/%s.\n' "$version_output" "$INSTALL_DIR" "$APP"
else
  printf 'Installed duck at %s/%s.\n' "$INSTALL_DIR" "$APP"
fi

add_to_path_if_needed
