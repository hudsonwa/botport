# Installing botport

botport ships four binaries: `botport`, `grok2hermes` (the former name; same program), `g2h` and `g2hd`
(the talk bridge, see crates/g2h/README.md).

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

    curl -fsSL https://raw.githubusercontent.com/hudsonwa/botport/main/install.sh | sh

The script downloads `botport-<target>.tar.gz` for your platform from the latest GitHub Release, verifies its
SHA-256 checksum, installs `botport`, `g2h` and `g2hd` into `~/.local/bin` and links `grok2hermes -> botport`.
Options (environment variables):

| Variable | Default | Meaning |
|---|---|---|
| `BOTPORT_VERSION` | `latest` | a release tag such as `v0.1.0` |
| `BOTPORT_BIN_DIR` | `~/.local/bin` | where the binaries go |
| `BOTPORT_REPO` | `hudsonwa/botport` | a fork to install from |
| `BOTPORT_BASE_URL` | (GitHub Releases) | a mirror holding the same archive + `.sha256` files |

Prefer to read before you run? Download `install.sh`, read it, then `sh install.sh`. Or install by hand: download
the archive and `SHA256SUMS` from the Releases page, `shasum -a 256 -c SHA256SUMS --ignore-missing`, unpack, and
copy the binaries onto your `PATH`.

## From source with cargo

Needs a Rust toolchain (stable, via rustup):

    cargo install --git https://github.com/hudsonwa/botport --locked botport          # botport + grok2hermes
    cargo install --git https://github.com/hudsonwa/botport --locked g2h               # g2h + g2hd

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

`scripts/build-release.sh [TARGET ...]` writes `dist/release/botport-<target>.tar.gz` + `.sha256`.
`.github/workflows/release.yml` runs it on every `v*` tag for aarch64/x86_64 macOS and aarch64/x86_64 Linux musl
and publishes the archives with a combined `SHA256SUMS`.
