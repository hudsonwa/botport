# Installing botport

botport ships four binaries: `botport`, `grok2hermes` (the former name; same program), `g2h` and `g2hd`.
`g2h` and `g2hd` are the optional talk bridge (see crates/g2h/README.md). `botport` alone is enough to export,
import, sync, drift-check and test a bot; install the bridge only if you want the two bots to message each other.

## Requirements

- macOS (arm64 or x86_64) or Linux (x86_64 or aarch64; static musl binaries, any distro).
- **Hermes Agent v0.22.0 or newer** on `PATH` (tested with v0.22.5). Check with `hermes --version`;
  `botport doctor` checks it for you. To install it, use NousResearch's installer (it stays inside your home):

      curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh \
        | bash -s -- --skip-setup --skip-browser --skip-computer-use

  `--skip-setup` skips the interactive wizard; the other two skip the browser and computer-use extras botport
  does not need. Read the script first if you prefer (`--help` lists every flag).
- No root access: everything installs under your home directory.

## Prebuilt binaries (recommended)

Pin a release tag and check what you download before you run it:

    TAG=v0.2.1 T=aarch64-apple-darwin   # or x86_64-apple-darwin, x86_64-unknown-linux-musl, aarch64-unknown-linux-musl
    curl -fsSLO "https://github.com/hudsonwa/botport/releases/download/$TAG/botport-$T.tar.gz"
    curl -fsSLO "https://github.com/hudsonwa/botport/releases/download/$TAG/SHA256SUMS"
    shasum -a 256 -c SHA256SUMS --ignore-missing
    gh attestation verify "botport-$T.tar.gz" --repo hudsonwa/botport \
      --signer-workflow hudsonwa/botport/.github/workflows/release.yml --source-ref "refs/tags/$TAG"
    tar -xzf "botport-$T.tar.gz" && mkdir -p ~/.local/bin && cp "botport-$T/botport" ~/.local/bin/

Or use the installer script **from the same tag** (read it first; never run a script from `main`):

    curl -fsSLO "https://raw.githubusercontent.com/hudsonwa/botport/$TAG/install.sh" && BOTPORT_VERSION="$TAG" sh install.sh

The script downloads `botport-<target>.tar.gz` for your platform from the latest GitHub Release, verifies its
SHA-256 checksum, refuses an archive holding a link, an absolute or `..` path or any name it does not expect, installs `botport`, `g2h` and `g2hd` into `~/.local/bin` and links `grok2hermes -> botport`. The bridge binaries
do nothing until you run `g2h setup`; if you don't want them, delete `g2h` and `g2hd` or use cargo below.
Options (environment variables):

| Variable | Default | Meaning |
|---|---|---|
| `BOTPORT_VERSION` | `latest` | a release tag such as `v0.1.0` |
| `BOTPORT_BIN_DIR` | `~/.local/bin` | where the binaries go |
| `BOTPORT_REPO` | `hudsonwa/botport` | a fork to install from |
| `BOTPORT_BASE_URL` | (GitHub Releases) | a mirror holding the same archive + `.sha256` files |

Prefer to read before you run? Download `install.sh`, read it, then `sh install.sh`. Or install by hand: download
the archive and `SHA256SUMS` from the Releases page, `shasum -a 256 -c SHA256SUMS --ignore-missing` (supported by macOS
`shasum` and GNU `sha256sum -c --ignore-missing`; it skips the archives you did not download), unpack, and
copy the binaries onto your `PATH`.

### Verify a download

Each release archive and each binary in it carries a GitHub build attestation: proof that the repository's own
release workflow built it from source. With the [GitHub CLI](https://cli.github.com/):

    TAG=v0.2.1    # the release you downloaded
    gh attestation verify botport-<target>.tar.gz --repo hudsonwa/botport \
      --source-ref "refs/tags/$TAG" --signer-workflow hudsonwa/botport/.github/workflows/release.yml
    gh attestation verify "$(command -v botport)" --repo hudsonwa/botport \
      --source-ref "refs/tags/$TAG" --signer-workflow hudsonwa/botport/.github/workflows/release.yml

`--source-ref` requires that tag's build and `--signer-workflow` the release workflow; add `--source-digest <commit sha>`
to require a given commit. Binaries from pipx or cargo are not attested.

## With pipx (no Rust needed)

    pipx install botport

The Python wheel bundles the prebuilt `botport` binary (and its `grok2hermes` alias); pipx puts both on your
`PATH`. **Not on PyPI yet:** until the first upload, build the wheel yourself (see
[RELEASING.md](RELEASING.md#python-wheel)) and run `pipx install ./botport-<version>-*.whl`. The wheel does **not**
include the optional talk bridge `g2h`/`g2hd` (a separate crate); get those from the curl script, Homebrew or cargo.
Uninstall with `pipx uninstall botport`.

## With Homebrew (after the tap exists)

    brew install hudsonwa/tap/botport

Installs `botport`, `grok2hermes`, `g2h` and `g2hd` from the release archives. The tap repo `hudsonwa/homebrew-tap`
is **not created yet**; the draft formula is in `packaging/homebrew/`.

## From source with cargo

Needs a Rust toolchain (stable, via rustup):

    cargo install --git https://github.com/hudsonwa/botport --locked botport          # botport + grok2hermes
    cargo install --git https://github.com/hudsonwa/botport --locked g2h               # optional: g2h + g2hd

Or from a checkout: `cargo build --release` builds all four into `target/release/`;
add that directory to your `PATH` (`export PATH="$PWD/target/release:$PATH"`).

## Check the install

    botport doctor [--profile NAME] [--spec FILE]

Checks that `hermes` runs and is new enough, that the Hermes home / profile directory exists and is writable,
and that the spec (if given) parses. Each failure prints a `fix:` hint. Exit codes: 0 all good, 1 a check failed,
2 usage error. Paths come from `--hermes-home` / `--profile-dir`, else `$HERMES_HOME`, else `~/.hermes`.

For the bridge, `g2h doctor` checks the daemon, its home (`$G2H_HOME`, else `~/.g2h`) and credentials.
`g2h selftest` sends one message to the Hermes bot and back, so it **needs a running Hermes gateway with a model**,
then logs a dry-run wake (nothing is sent). `g2h selftest --wake-only` checks just the dry-run wake, with no gateway.

### Try the bridge locally (no service installed)

    export HOME="$(mktemp -d)"
    export G2H_HOME="$HOME/.g2h"   # separate line, after HOME: an exported G2H_HOME survives a HOME change
    unset HERMES_HOME                                   # botport and hermes follow it before ~/.hermes
    g2h home                                            # must print a path under the new HOME
    hermes profile create demo                          # a throwaway profile
    export HERMES_HOME="$HOME/.hermes/profiles/demo"   # point hermes at that profile for the next three lines
    hermes config set model.base_url http://127.0.0.1:8080/v1   # any OpenAI-compatible endpoint
    hermes config set model.default your-model-name
    hermes gateway run &                                # optional: only the full selftest needs it
    unset HERMES_HOME
    g2h setup --profile demo --no-service --yes         # writes config + credentials; no launchd/systemd unit
    g2hd &                                              # the daemon in the foreground of this shell
    g2h selftest --wake-only                            # or plain `g2h selftest` with the gateway running

`setup` asks for a webhook URL and sender key; for a dry run any `https://` URL and dummy key will do, because
`dry_run = true` logs wakes and never sends them. Stop with `kill %%` (or `kill` the PIDs) when done.

## Uninstall

    rm ~/.local/bin/botport ~/.local/bin/grok2hermes ~/.local/bin/g2h ~/.local/bin/g2hd

(`g2h uninstall` first if you set up the bridge service.)

## Building release archives

`scripts/build-release.sh [TARGET ...]` writes, per target, `dist/release/botport-<target>.tar.gz`,
`botport-<target>.tar.gz.sha256` (the archive's checksum) and `botport-<target>.sha256` (the checksum of the bare
`botport` binary, renamed `botport-<target>`). `.github/workflows/release.yml` runs it on every `v*` tag and
publishes these files plus a combined `SHA256SUMS` (archives only). The release assets are exactly:

| Target | Archive | Checksums |
|---|---|---|
| `aarch64-apple-darwin` | `botport-aarch64-apple-darwin.tar.gz` | `.tar.gz.sha256`, `botport-aarch64-apple-darwin.sha256` |
| `x86_64-apple-darwin` | `botport-x86_64-apple-darwin.tar.gz` | `.tar.gz.sha256`, `botport-x86_64-apple-darwin.sha256` |
| `x86_64-unknown-linux-musl` | `botport-x86_64-unknown-linux-musl.tar.gz` | `.tar.gz.sha256`, `botport-x86_64-unknown-linux-musl.sha256` |
| `aarch64-unknown-linux-musl` | `botport-aarch64-unknown-linux-musl.tar.gz` | `.tar.gz.sha256`, `botport-aarch64-unknown-linux-musl.sha256` |

plus `SHA256SUMS`. Each archive holds a `botport-<target>/` directory with `botport`, `grok2hermes`, `g2h` and `g2hd`.
