#!/bin/sh
# Install botport (+ grok2hermes alias, g2h, g2hd) from GitHub Releases into ~/.local/bin. No sudo.
#   curl -fsSL https://raw.githubusercontent.com/hudsonwa/botport/main/install.sh | sh
# Env: BOTPORT_VERSION=v0.1.0 (default: latest)  BOTPORT_BIN_DIR=DIR (default: ~/.local/bin)
#      BOTPORT_REPO=owner/name (default: hudsonwa/botport)  BOTPORT_BASE_URL=URL (mirror or local test dir)
set -eu

repo=${BOTPORT_REPO:-hudsonwa/botport}
version=${BOTPORT_VERSION:-latest}
bin_dir=${BOTPORT_BIN_DIR:-"$HOME/.local/bin"}

die() { echo "install.sh: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "needs $1"; }
need curl
need tar
need uname

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  *) die "no prebuilt binary for $(uname -s) $(uname -m); use: cargo install --locked --git https://github.com/$repo botport g2h" ;;
esac

if [ -n "${BOTPORT_BASE_URL:-}" ]; then
  base=$BOTPORT_BASE_URL
elif [ "$version" = latest ]; then
  base="https://github.com/$repo/releases/latest/download"
else
  base="https://github.com/$repo/releases/download/$version"
fi
archive="botport-$target.tar.gz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
echo "downloading $base/$archive"
curl -fsSL -o "$tmp/$archive" "$base/$archive" || die "download failed: $base/$archive"
curl -fsSL -o "$tmp/$archive.sha256" "$base/$archive.sha256" || die "download failed: $archive.sha256"

want=$(cut -d' ' -f1 < "$tmp/$archive.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$archive" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  got=$(shasum -a 256 "$tmp/$archive" | cut -d' ' -f1)
else
  die "needs sha256sum or shasum to verify the download"
fi
[ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $archive (want $want, got $got)"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$bin_dir"
for b in botport g2h g2hd; do
  install -m 0755 "$tmp/botport-$target/$b" "$bin_dir/$b"
done
ln -sf botport "$bin_dir/grok2hermes"
echo "installed botport, grok2hermes (-> botport), g2h, g2hd to $bin_dir"

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "note: $bin_dir is not on PATH; add: export PATH=\"$bin_dir:\$PATH\"" ;;
esac
echo "next: botport doctor"
