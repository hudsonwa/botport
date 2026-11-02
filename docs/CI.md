# Continuous integration

All checks live in `scripts/ci.sh` (fmt, build, clippy `-D warnings`, `cargo test --workspace`, and
`cargo deny check licenses advisories` when `cargo-deny` is installed). Run it locally before pushing:

    scripts/ci.sh
    CI_MEMCAP="<your-mem-cap-wrapper> <args> --" scripts/ci.sh   # optional: prefix every cargo call

Workflows calling the script:

- `.github/workflows/ci.yml` — GitHub Actions; runs on every push and pull request.
- `.forgejo/workflows/ci.yml` — the same script for the maintainer's optional Forgejo mirror (see below).

## Maintainer's optional mirror: a self-hosted Forgejo runner

Contributors can ignore this section and `.forgejo/`: GitHub Actions is the project's CI. Nothing runs until a runner is registered. Keep it small and reversible:

1. Install `forgejo-runner` (release binary from code.forgejo.org/forgejo/runner) on a host with Docker or,
   for a host-mode runner, a Rust toolchain.
2. Get a registration token: repo **Settings → Actions → Runners → Create new runner**.
3. `forgejo-runner register --no-interactive --instance <forgejo-url> --token <token> --name <name> --labels docker:docker://rust:latest`
   (host mode instead: `--labels docker:host`).
4. `forgejo-runner generate-config > config.yml`; set `runner.capacity: 1` and container memory limits.
5. Run `forgejo-runner daemon -c config.yml` as a service (systemd unit / launchd agent, low priority).
6. To remove: stop the service, delete the runner in the repo settings, delete its `.runner` file.
