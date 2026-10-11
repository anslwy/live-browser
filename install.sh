#!/usr/bin/env bash
#
# Live Browser installer (Linux + macOS).
#
#   curl -fsSL https://raw.githubusercontent.com/anslwy/live-browser/main/install.sh | bash
#
# What it does by default (fast path):
#   1. installs a browser (distro Chromium/Chrome on Linux, Chrome cask on macOS)
#   2. downloads the latest prebuilt `lb` release (binary + web UI)
#   3. installs to ~/.local (override with PREFIX=/usr/local)
#
# With --from-source it additionally installs the Rust + Node toolchains,
# clones the repo and builds everything locally instead of downloading
# a release:
#
#   curl -fsSL .../install.sh | bash -s -- --from-source
#
set -euo pipefail

REPO="anslwy/live-browser"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
LB_HOME="$PREFIX/share/live-browser"
FROM_SOURCE=0
if [[ "${1:-}" == "--from-source" ]]; then FROM_SOURCE=1; fi

msg() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
err() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

OS="$(uname -s)"
ARCH="$(uname -m)"

# Normalize to release-asset platform/arch names.
case "$OS" in
  Linux) PLATFORM="linux" ;;
  Darwin) PLATFORM="darwin" ;;
  *) err "unsupported OS: $OS (Linux and macOS only — on Windows use install.ps1)" ;;
esac
case "$ARCH" in
  x86_64) ASSET_ARCH="x86_64" ;;
  arm64|aarch64)
    if [[ "$PLATFORM" == "darwin" ]]; then ASSET_ARCH="arm64";
    else ASSET_ARCH="aarch64"; fi ;;
  *) err "unsupported architecture: $ARCH (x86_64, or arm64 on macOS)" ;;
esac
if [[ "$PLATFORM" == "linux" && "$ASSET_ARCH" == "aarch64" ]]; then
  err "prebuilt Linux arm64 binaries are not published yet — re-run with --from-source"
fi

if ! have curl && ! have wget; then err "need curl or wget"; fi
if ! have tar; then err "need tar"; fi

fetch() { # fetch <url> <out>
  if have curl; then curl -fsSL "$1" -o "$2"; else wget -qO "$2" "$1"; fi
}

# ---------------------------------------------------------------- browser ---
install_browser() {
  if have google-chrome-stable || have google-chrome || have chromium || have chromium-browser; then
    msg "browser already installed ($(command -v google-chrome-stable google-chrome chromium chromium-browser 2>/dev/null | head -1))"
    return
  fi
  if [[ "$PLATFORM" == "darwin" ]]; then
    if [[ -x "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ]]; then
      msg "browser already installed (/Applications/Google Chrome.app)"
      return
    fi
    msg "installing Google Chrome…"
    if have brew; then
      brew install --cask google-chrome
    else
      err "no browser found — install Google Chrome from https://www.google.com/chrome/ (or 'brew install --cask google-chrome'), then re-run"
    fi
    return
  fi
  msg "installing a browser…"
  if have apt-get; then
    # Prefer distro Chromium (no third-party repo); fall back to Google Chrome.
    if ! sudo apt-get install -y chromium 2>/dev/null; then
      tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
      fetch "https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb" "$tmp/chrome.deb"
      sudo apt-get install -y "$tmp/chrome.deb"
    fi
  elif have dnf; then
    if ! sudo dnf install -y chromium 2>/dev/null; then
      tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
      fetch "https://dl.google.com/linux/direct/google-chrome-stable_current_x86_64.rpm" "$tmp/chrome.rpm"
      sudo dnf install -y "$tmp/chrome.rpm"
    fi
  elif have pacman; then
    if have yay; then yay -S --noconfirm google-chrome;
    else sudo pacman -S --noconfirm chromium; fi
  else
    err "no supported package manager (apt/dnf/pacman) — install Google Chrome or Chromium manually, then re-run"
  fi
}

# ---------------------------------------------------------------- toolchain --
install_rust() {
  if have cargo; then msg "rust already installed ($(cargo --version))"; return; fi
  msg "installing rust (rustup)…"
  tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
  fetch "https://sh.rustup.rs" "$tmp/rustup.sh"
  sh "$tmp/rustup.sh" -y --default-toolchain stable --profile minimal
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
}

install_node() {
  if have node && [[ "$(node --version | sed 's/v//;s/\..*//')" -ge 20 ]]; then
    msg "node already installed ($(node --version))"; return
  fi
  msg "installing node 22 (user-local, no sudo)…"
  mkdir -p "$PREFIX"
  tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
  # Node dist names: linux-x64, linux-arm64, darwin-x64, darwin-arm64.
  case "${PLATFORM}-${ARCH}" in
    linux-x86_64) NODE_PLAT="linux-x64" ;;
    linux-aarch64|linux-arm64) NODE_PLAT="linux-arm64" ;;
    darwin-arm64|darwin-aarch64) NODE_PLAT="darwin-arm64" ;;
    darwin-x86_64) NODE_PLAT="darwin-x64" ;;
    *) err "unsupported platform for node download: ${PLATFORM}-${ARCH}" ;;
  esac
  NODE_VER="v22.21.0"
  fetch "https://nodejs.org/dist/${NODE_VER}/node-${NODE_VER}-${NODE_PLAT}.tar.xz" "$tmp/node.tar.xz"
  tar -xJf "$tmp/node.tar.xz" -C "$tmp"
  cp -r "$tmp"/node-${NODE_VER}-${NODE_PLAT}/. "$PREFIX/"
  export PATH="$PREFIX/bin:$PATH"
}

# ----------------------------------------------------------------- release ---
install_release() {
  msg "fetching latest live-browser release…"
  # NOTE: fetch the whole body before parsing. Piping curl straight into
  # `grep -m1` breaks under `set -o pipefail`: grep quits after the first
  # match, curl gets SIGPIPE writing the rest (exit 23) once the JSON
  # exceeds the pipe buffer — which it now does with 4 release assets.
  if have curl; then
    REL_JSON="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest")" \
      || err "could not reach api.github.com (network? rate limit?)"
  else
    REL_JSON="$(wget -qO- "https://api.github.com/repos/${REPO}/releases/latest")" \
      || err "could not reach api.github.com (network? rate limit?)"
  fi
  TAG="$(printf '%s' "$REL_JSON" | grep '"tag_name"' | head -1 | cut -d'"' -f4)"
  [[ -n "${TAG:-}" ]] || err "could not determine latest release (network? rate limit?)"
  ASSET="live-browser-${TAG}-${PLATFORM}-${ASSET_ARCH}.tar.gz"
  msg "installing ${ASSET}…"
  tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
  fetch "https://github.com/${REPO}/releases/download/${TAG}/${ASSET}" "$tmp/pkg.tar.gz" \
    || err "no prebuilt binary for ${PLATFORM}-${ASSET_ARCH} yet — re-run with --from-source"
  rm -rf "$LB_HOME"
  mkdir -p "$LB_HOME" "$BIN_DIR"
  tar -xzf "$tmp/pkg.tar.gz" -C "$tmp"
  cp -r "$tmp"/live-browser-*/* "$LB_HOME/"
  chmod +x "$LB_HOME/lb"
  ln -sf "$LB_HOME/lb" "$BIN_DIR/lb"
}

# -------------------------------------------------------------- from-source --
install_from_source() {
  install_rust
  install_node
  export PATH="$HOME/.cargo/bin:$PREFIX/bin:$PATH"
  work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
  msg "cloning ${REPO}…"
  if [[ "$PLATFORM" == "darwin" ]]; then
    git clone --depth 1 "https://github.com/${REPO}.git" "$work/repo" 2>/dev/null \
      || err "need git (xcode-select --install, or brew install git)"
  else
    git clone --depth 1 "https://github.com/${REPO}.git" "$work/repo" 2>/dev/null \
      || err "need git to build from source (sudo apt-get install -y git)"
  fi
  cd "$work/repo"
  msg "building web UI…"
  (cd web && npm ci && npm run build)
  msg "building lb (release, takes a few minutes)…"
  cargo build --release
  msg "installing to ${LB_HOME}…"
  rm -rf "$LB_HOME"
  mkdir -p "$LB_HOME/web" "$BIN_DIR"
  cp target/release/lb "$LB_HOME/lb"
  cp -r web/dist "$LB_HOME/web/dist"
  chmod +x "$LB_HOME/lb"
  ln -sf "$LB_HOME/lb" "$BIN_DIR/lb"
}

# --------------------------------------------------------------------- main ---
install_browser
if [[ "$FROM_SOURCE" == "1" ]]; then install_from_source; else install_release; fi

msg "done!"
echo
echo "  lb web --port 8765          # start browser + web UI"
echo "  lb open https://example.com    # drive it"
echo
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "NOTE: $BIN_DIR is not on your PATH yet. Add:"; echo "  export PATH=\"\$HOME/.local/bin:\$PATH\"";;
esac
