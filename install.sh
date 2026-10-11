#!/usr/bin/env bash
#
# Live Browser installer (Linux x86_64).
#
#   curl -fsSL https://raw.githubusercontent.com/anslwy/live-browser/main/install.sh | bash
#
# What it does by default (fast path):
#   1. installs a browser (distro Chromium, or Google Chrome as fallback)
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

ARCH="$(uname -m)"
[[ "$ARCH" == "x86_64" ]] || err "only x86_64 is supported yet (found: $ARCH)"
[[ "$(uname -s)" == "Linux" ]] || err "only Linux is supported yet"

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
  fetch "https://nodejs.org/dist/v22.21.0/node-v22.21.0-linux-x64.tar.xz" "$tmp/node.tar.xz"
  tar -xJf "$tmp/node.tar.xz" -C "$tmp"
  cp -r "$tmp"/node-v22.21.0-linux-x64/. "$PREFIX/"
  export PATH="$PREFIX/bin:$PATH"
}

# ----------------------------------------------------------------- release ---
install_release() {
  msg "fetching latest live-browser release…"
  if have curl; then
    TAG="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep -m1 '"tag_name"' | cut -d'"' -f4)"
  else
    TAG="$(wget -qO- "https://api.github.com/repos/${REPO}/releases/latest" | grep -m1 '"tag_name"' | cut -d'"' -f4)"
  fi
  [[ -n "${TAG:-}" ]] || err "could not determine latest release (network? rate limit?)"
  ASSET="live-browser-${TAG}-linux-x86_64.tar.gz"
  msg "installing ${ASSET}…"
  tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
  fetch "https://github.com/${REPO}/releases/download/${TAG}/${ASSET}" "$tmp/pkg.tar.gz"
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
  git clone --depth 1 "https://github.com/${REPO}.git" "$work/repo" 2>/dev/null \
    || err "need git to build from source (sudo apt-get install -y git)"
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
echo "  lb daemon --port 8765          # start browser + web UI"
echo "  lb open https://example.com    # drive it"
echo
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "NOTE: $BIN_DIR is not on your PATH yet. Add:"; echo "  export PATH=\"\$HOME/.local/bin:\$PATH\"";;
esac
