#!/bin/sh
# Release archives for every binary (botport, grok2hermes, g2h, g2hd): one botport-<target>.tar.gz per target,
# each with a .sha256 next to it, plus botport-<target>.sha256 for the bare botport binary (the file the Grok
# export brief attaches, renamed botport-<target>). Linux musl targets link with rust-lld (.cargo/config.toml), so
# they build from any host; macOS targets need a macOS host. Needs: rustup target add <target> for each target.
# Usage: scripts/build-release.sh [TARGET ...]   (default: the targets this host can build)
# Output: dist/release/botport-<target>.tar.gz{,.sha256} and dist/release/botport-<target>.sha256
# Optional: CARGO_PREFIX="<mem-cap wrapper> --" to prefix every cargo call.
set -eu
cd "$(dirname "$0")/.."
if [ $# -eq 0 ]; then
  set -- x86_64-unknown-linux-musl aarch64-unknown-linux-musl
  [ "$(uname -s)" = Darwin ] && set -- aarch64-apple-darwin x86_64-apple-darwin "$@"
fi
sum() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi; }
mkdir -p dist/release
for t in "$@"; do
  ${CARGO_PREFIX:-} cargo build --release --locked --workspace --bins --target "$t"
  stage="dist/release/botport-$t"
  mkdir -p "$stage"
  for b in botport grok2hermes g2h g2hd; do cp "target/$t/release/$b" "$stage/"; done
  cp -R LICENSE NOTICE LICENSES README.md "$stage/"
  (cd dist/release && COPYFILE_DISABLE=1 tar --no-xattrs -czf "botport-$t.tar.gz" "botport-$t" && sum "botport-$t.tar.gz" > "botport-$t.tar.gz.sha256")
  # "<hash>  botport-<target>": checks the botport binary taken out of the archive and renamed (docs/AGENT-EXPORT.md)
  (cd "$stage" && sum botport) | sed "s|botport\$|botport-$t|" > "dist/release/botport-$t.sha256"
  echo "built dist/release/botport-$t.tar.gz"
done
