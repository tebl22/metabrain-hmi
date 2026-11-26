#!/usr/bin/env bash
# MetaBrain installer for macOS & Linux (100% pure Rust).
# Builds from source (default when run inside a checkout) or installs a verified archive.
#
# Usage:
#   ./install.sh                         # build from source and install into ~/.local/bin
#   ./install.sh --prefix /usr/local     # custom install prefix (binaries in <prefix>/bin)
#   ./install.sh --with-hooks            # also install Claude Code hooks
#   ./install.sh --with-autostart        # also enable login autostart
#   ./install.sh --uninstall             # remove binaries, hooks, and autostart cleanly
set -euo pipefail

PREFIX="${HOME}/.local"
WITH_HOOKS=0
WITH_AUTOSTART=0
UNINSTALL=0
ARCHIVE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix)
      PREFIX="$2"
      shift 2
      ;;
    --archive)
      ARCHIVE="$2"
      shift 2
      ;;
    --with-hooks)
      WITH_HOOKS=1
      shift
      ;;
    --with-autostart)
      WITH_AUTOSTART=1
      shift
      ;;
    --uninstall)
      UNINSTALL=1
      shift
      ;;
    -h|--help)
      sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 2
      ;;
  esac
done

BIN_DIR="${PREFIX}/bin"
DATA_DIR="${METABRAIN_HOME:-${HOME}/.metabrain}"

if [[ "${UNINSTALL}" -eq 1 ]]; then
  echo "Uninstalling MetaBrain from ${BIN_DIR}..."
  if [[ -x "${BIN_DIR}/metabrain-hook" ]]; then
    "${BIN_DIR}/metabrain-hook" hooks uninstall || true
    "${BIN_DIR}/metabrain-hook" autostart off || true
  fi
  rm -f "${BIN_DIR}/MetaBrain" "${BIN_DIR}/metabrain" "${BIN_DIR}/metabrain-hook"
  echo "Done. Your config/history in ${DATA_DIR} was preserved (remove it manually if desired)."
  exit 0
fi

STAGE="$(mktemp -d)"
cleanup() { rm -rf "${STAGE}"; }
trap cleanup EXIT

if [[ -n "${ARCHIVE}" ]]; then
  echo "Extracting ${ARCHIVE}..."
  tar -xzf "${ARCHIVE}" -C "${STAGE}"
  if [[ -f "${STAGE}/SHA256SUMS.txt" ]]; then
    echo "Verifying SHA-256 checksums..."
    if command -v sha256sum >/dev/null 2>&1; then
      (cd "${STAGE}" && sha256sum -c SHA256SUMS.txt)
    elif command -v shasum >/dev/null 2>&1; then
      (cd "${STAGE}" && shasum -a 256 -c SHA256SUMS.txt)
    fi
  fi
  SRC_MAIN="${STAGE}/MetaBrain"
  SRC_HOOK="${STAGE}/metabrain-hook"
else
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  if [[ ! -f "${SCRIPT_DIR}/rust/Cargo.toml" ]]; then
    echo "Error: rust/Cargo.toml not found. Run from the repository root or pass --archive <tar.gz>." >&2
    exit 1
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    echo "Error: Rust toolchain (cargo) not found. Install from https://rustup.rs/" >&2
    exit 1
  fi
  echo "Building MetaBrain release binaries..."
  (cd "${SCRIPT_DIR}/rust" && cargo build --release --locked)
  SRC_MAIN="${SCRIPT_DIR}/rust/target/release/MetaBrain"
  SRC_HOOK="${SCRIPT_DIR}/rust/target/release/metabrain-hook"
fi

mkdir -p "${BIN_DIR}"
install -m 0755 "${SRC_MAIN}" "${BIN_DIR}/MetaBrain.tmp"
install -m 0755 "${SRC_HOOK}" "${BIN_DIR}/metabrain-hook.tmp"
mv -f "${BIN_DIR}/MetaBrain.tmp" "${BIN_DIR}/MetaBrain"
mv -f "${BIN_DIR}/metabrain-hook.tmp" "${BIN_DIR}/metabrain-hook"
ln -sf "${BIN_DIR}/MetaBrain" "${BIN_DIR}/metabrain"

echo "Installed MetaBrain and metabrain-hook to ${BIN_DIR}."

if [[ "${WITH_HOOKS}" -eq 1 ]]; then
  "${BIN_DIR}/metabrain-hook" hooks install
fi

if [[ "${WITH_AUTOSTART}" -eq 1 ]]; then
  "${BIN_DIR}/MetaBrain" autostart on
fi

case ":${PATH}:" in
  *":${BIN_DIR}:"*) ;;
  *)
    echo ""
    echo "Note: ${BIN_DIR} is not on your PATH. Add this to your shell profile:"
    echo "  export PATH=\"${BIN_DIR}:\$PATH\""
    ;;
esac

echo ""
echo "Try it right now with zero credentials:"
echo "  ${BIN_DIR}/MetaBrain serve --demo"
echo ""
echo "Or launch Mission Control normally:"
echo "  ${BIN_DIR}/MetaBrain serve"
