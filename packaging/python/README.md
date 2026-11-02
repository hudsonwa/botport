# botport

Port, sync and bridge AI bots between a Grok Bot and a Hermes Agent bot: export and import in both directions,
two-way sync with drift detection (your Hermes-side edits are never overwritten), and a direct chat bridge (g2h).

This wheel installs the `botport` binary and its `grok2hermes` alias; no Rust toolchain is needed. The g2h bridge
(`g2h`, `g2hd`) is not in the wheel: use the release archive, Homebrew or cargo for it.

Docs, install options and source: https://github.com/hudsonwa/botport
