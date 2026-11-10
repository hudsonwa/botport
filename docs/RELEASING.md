# Releasing

## Binary archives

Push a `v*` tag. `.github/workflows/release.yml` builds the four targets with `scripts/build-release.sh` and
publishes the archives, their `.sha256` files and `SHA256SUMS`; the asset names are listed in
[INSTALL.md](INSTALL.md#building-release-archives).

## Build attestations

The `build` job attests every archive and every binary in it with `actions/attest-build-provenance` (GitHub's
Sigstore-signed build provenance: repository, workflow, tag and commit). Check a release asset or an installed
binary:

    TAG=v0.2.1
    gh attestation verify botport-<target>.tar.gz --repo hudsonwa/botport \
      --source-ref "refs/tags/$TAG" --signer-workflow hudsonwa/botport/.github/workflows/release.yml
    gh attestation verify "$(command -v botport)" --repo hudsonwa/botport \
      --source-ref "refs/tags/$TAG" --signer-workflow hudsonwa/botport/.github/workflows/release.yml
    # pin the exact commit too: add --source-digest <commit sha>

A binary built anywhere else (cargo, the Python wheel) has no attestation.

## Python wheel

`packaging/python/pyproject.toml` builds a wheel with [maturin](https://www.maturin.rs/) in `bindings = "bin"`
mode: the wheel holds only the compiled `botport` and `grok2hermes` binaries (no Python code), so
`pipx install botport` needs no Rust. `g2h`/`g2hd` are a separate crate and are not included. This is a manual
step; it is not in `release.yml` and nothing is uploaded to PyPI yet.

    python3 -m venv /tmp/mat && /tmp/mat/bin/pip install 'maturin>=1.5,<2'
    PATH=/tmp/mat/bin:$PATH packaging/python/build-wheel.sh                          # this machine's platform
    PATH=/tmp/mat/bin:$PATH packaging/python/build-wheel.sh --target x86_64-apple-darwin
    # build-wheel.sh sets --remap-path-prefix so no host path ($HOME, ~/.cargo, the checkout) ends up in the binary;
    # check with: unzip -p dist/wheels/botport-*.whl '*/scripts/botport' | strings | grep -c "$HOME"   (must be 0)
    # Linux (manylinux/musllinux): run maturin in its Docker image, or add --zig

Wheel names look like `botport-0.2.1-py3-none-macosx_11_0_arm64.whl`; the version comes from `Cargo.toml`.
Check one before uploading:

    python3 -m venv /tmp/wt && /tmp/wt/bin/pip install dist/wheels/botport-*.whl && /tmp/wt/bin/botport --version

Upload (when the project decides to publish): `/tmp/mat/bin/maturin upload dist/wheels/*` or `twine upload`.

## Homebrew

See [packaging/homebrew/README.md](../packaging/homebrew/README.md): copy the draft formula into the tap and fill
its checksums from the release's `SHA256SUMS`.
