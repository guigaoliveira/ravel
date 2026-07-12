#!/usr/bin/env sh
# Install prebuilt `ravel` for macOS / Linux (CodeGraph-style one-liner).
#
#   curl -fsSL https://raw.githubusercontent.com/<owner>/ravel/main/scripts/install.sh | sh
#
# Env:
#   RAVEL_GITHUB_REPO   owner/repo (default: guigaoliveira/ravel)
#   RAVEL_VERSION       tag without v, or "latest" (default: latest)
#   RAVEL_INSTALL_DIR   install directory (default: ~/.local/bin)
#   RAVEL_FROM_SOURCE=1 force cargo install from this checkout / git
set -eu

REPO="${RAVEL_GITHUB_REPO:-guigaoliveira/ravel}"
VERSION="${RAVEL_VERSION:-latest}"
INSTALL_DIR="${RAVEL_INSTALL_DIR:-${HOME}/.local/bin}"

say() { printf '%s\n' "$*"; }
err() { printf 'error: %s\n' "$*" >&2; exit 1; }

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "need '$1' on PATH"
}

detect_target() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)
  case "$os" in
    linux) os=unknown-linux-gnu ;;
    darwin) os=apple-darwin ;;
    *) err "unsupported OS: $os (use install.ps1 on Windows, or cargo install)" ;;
  esac
  case "$arch" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) err "unsupported arch: $arch" ;;
  esac
  echo "${arch}-${os}"
}

download() {
  url="$1"
  dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  else
    err "need curl or wget"
  fi
}

install_from_cargo() {
  need_cmd cargo
  say "building from source via cargo install…"
  if [ -f Cargo.toml ] && [ -d crates/ravel-cli ]; then
    cargo install --path crates/ravel-cli --locked --force
  else
    cargo install --git "https://github.com/${REPO}.git" ravel-cli --locked --force 2>/dev/null \
      || cargo install --git "https://github.com/${REPO}.git" --locked --force
  fi
  say "installed (cargo). Ensure ~/.cargo/bin is on PATH."
  say "next: ravel install --yes && cd <project> && ravel index"
  exit 0
}

if [ "${RAVEL_FROM_SOURCE:-0}" = "1" ]; then
  install_from_cargo
fi

TARGET=$(detect_target)
ASSET="ravel-${TARGET}.tar.gz"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

if [ "$VERSION" = "latest" ]; then
  BASE="https://github.com/${REPO}/releases/latest/download"
else
  BASE="https://github.com/${REPO}/releases/download/v${VERSION#v}"
fi
URL="${BASE}/${ASSET}"

say "repo:    ${REPO}"
say "target:  ${TARGET}"
say "url:     ${URL}"

if ! download "$URL" "${TMP}/${ASSET}" 2>/dev/null; then
  say "prebuilt asset not found — falling back to cargo install from source"
  install_from_cargo
fi

tar -xzf "${TMP}/${ASSET}" -C "$TMP"
BIN=""
for cand in ravel ravel.exe; do
  if [ -f "${TMP}/${cand}" ]; then BIN="${TMP}/${cand}"; break; fi
  found=$(find "$TMP" -type f -name "$cand" 2>/dev/null | head -n1 || true)
  if [ -n "${found:-}" ]; then BIN="$found"; break; fi
done
[ -n "$BIN" ] || err "archive did not contain ravel binary"

mkdir -p "$INSTALL_DIR"
install -m 755 "$BIN" "${INSTALL_DIR}/ravel"
say "installed: ${INSTALL_DIR}/ravel"

case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    say ""
    say "NOTE: ${INSTALL_DIR} is not on PATH. Add:"
    say "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac

say ""
say "Wire agents (Claude / Cursor / Codex / OpenCode / Gemini / …):"
say "  ravel install --yes"
say "Then in each project:"
say "  cd your-repo && ravel index && ravel status"
