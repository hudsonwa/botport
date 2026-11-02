# Contributing to botport

Thanks for looking. botport is an early v0.1 by one maintainer. There is no team behind it yet, and no outside pull
request has been merged so far, so the first contributors will help shape how review works here.

**The best first read is [docs/OWNERSHIP.md](docs/OWNERSHIP.md)**: one page on the ownership rules that are botport's
core (what the lock records, when apply writes, when it refuses). Every change to sync must keep them.

**Recommended first contributions:** a [docs/PLATFORM-CHANGES.md](docs/PLATFORM-CHANGES.md) entry from a bot you
run; [#16](https://github.com/hudsonwa/botport/issues/16) (Hermes adapter write: connector key order, repeated -N
duplicates, stale credential ticks); [#5](https://github.com/hudsonwa/botport/issues/5) (more parity-battery cases).

Two kinds of longer-term help would matter most:
1. **A co-maintainer who runs Grok Bots daily**, to own the export brief ([docs/AGENT-EXPORT.md](docs/AGENT-EXPORT.md))
   and the Grok side. The brief is a contract with a closed platform that changes without notice; it needs someone
   who notices first. Changes go in [docs/PLATFORM-CHANGES.md](docs/PLATFORM-CHANGES.md).
2. **Rust contributors for adapters.** The planned ones are OpenClaw (formerly Moltbot/Clawdbot),
   [#1](https://github.com/hudsonwa/botport/issues/1), and OpenBot, [#2](https://github.com/hudsonwa/botport/issues/2).
   Neither has code and neither is a small first task: each needs an adapter that passes the conformance suite plus a
   CLI source selector, and porting *into* either also needs a `Target` that keeps the rules in
   [docs/OWNERSHIP.md](docs/OWNERSHIP.md) (see "Adding a platform" below).

g2h is an optional, experimental companion and is feature-frozen for v0.1: bug fixes only, no new features.
Separate-user hardening (#17) is post-launch.

You don't need to be a Rust expert for everything else: docs, test cases and bug reports with the `--json` output
attached all help.

## Build
You need a stable Rust toolchain (`rustup` is easiest).

```sh
cargo build --release            # botport, grok2hermes (alias), g2h, g2hd in target/release/
```

Static Linux builds from macOS work too (pure-Rust deps, `rust-lld`):
`cargo build --release --target x86_64-unknown-linux-musl`.

## Test
```sh
cargo test --workspace           # unit, conformance and CLI tests; no network, no real Hermes needed
scripts/ci.sh                    # what CI runs: fmt, build, clippy -D warnings, tests, cargo-deny if installed
```
Run `scripts/ci.sh` before you open a pull request. If it passes locally, CI should pass too.
For a refactor that should not change behaviour (e.g. of `src/adapters/hermes/sync.rs`), run
`scripts/golden.sh BIN OUTDIR` with a binary built before and one built after, then `diff -r` the two output dirs;
it should be empty.

Tests never touch your real `~/.hermes` or `~/.g2h`: they use temp directories. Please keep it that way in new tests.
`tests/e2e-real/` talks to real bots and is skipped unless you opt in; you don't need it for a normal PR.
`shim/` (the agent-importers shim): its integration test needs the upstream agent-importers source and is **skipped**
without it, including in CI (the two unit tests still run); see [docs/REFERENCE.md](docs/REFERENCE.md#agent-importers-source) to run it.

## Pull requests
- One change per PR, with a short description of what and why. Link the issue if there is one.
- Add or update a test for any behaviour change. Bugs: a failing test first, then the fix.
- Keep output terse (10 lines or fewer per command; detail goes to `--details`), and keep output deterministic: the
  same input gives byte-identical output.
- **Never commit secrets or personal data**: no tokens, real bot exports, host names or home paths. Use the made-up
  Acme bot or a new synthetic fixture.
- Template content from other people stays out of the repo; see the [worked example](docs/examples/daily-wisdom.md).
- No CLA and no DCO sign-off. By opening a PR you agree your contribution is licensed under the project's
  [MIT license](LICENSE).
- Update [CHANGELOG.md](CHANGELOG.md) (newest section at the top) when users would notice the change.

## How this code was built
The code and docs were written by Claude agents, directed by Josh, the maintainer. Each change was checked by a
separate Claude agent acting as auditor (a fresh session with its own checklist), plus the tests and CI. Near launch,
three fresh-context Claude critique passes reviewed the repo. The work before the first outside review is one squashed commit; everything after it is kept as separate,
reviewed commits.

## Adding a platform
New platforms are added as adapters. An adapter reads its platform into the neutral model and writes it back, and
`plan`/`apply`/`sync`/`drift` port that model to Hermes. To be merged it must pass the whole conformance suite,
including the schedule, connector and credential-name round trips, which fail for an adapter that drops them;
the CLI also needs a way to select it as a source (today only `--bundle`, the Grok Bot source). Read [docs/ADAPTERS.md](docs/ADAPTERS.md) (the trait, mapping rules and the
conformance tests every adapter must pass) and [docs/MODEL.md](docs/MODEL.md) (the neutral model and the planned
OpenClaw and OpenBot mappings).

## Picking an issue
- Look for the `good first issue` label. Each one lists the files to read and what "done" looks like.
- Comment on the issue that you're taking it, so two people don't do the same work. If you go quiet for two weeks,
  someone else may pick it up; that's fine, and you're welcome back.
- Unsure about an approach? Open a draft PR early, or ask on the issue.
- What is done, in progress and planned is in [ROADMAP.md](ROADMAP.md).

## Security
Please don't report security problems in public issues. Open a private security advisory on GitHub
(Security → Report a vulnerability) instead. The g2h threat model is in [docs/SECURITY.md](docs/SECURITY.md).

## Conduct
Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).
