# Writing an adapter

An **adapter** connects one bot platform to the neutral bot model (`BotModel`, see [MODEL.md](MODEL.md)). The core is
the hub; adapters are spokes. Every adapter does two things:

- **read**: native layout on disk -> `BotModel`
- **write**: `BotModel` -> native layout on disk (returns the relative paths it wrote)

**What works today.** `plan`, `apply`, `sync` and `drift` run on the model: a source adapter's `read_source` gives
a `BotModel` and a `Source`, and the Hermes adapter (the one `Target`) plans and applies it (see
[ARCHITECTURE.md](ARCHITECTURE.md#the-seam-srccoreadapterrs)). A new source adapter that passes the conformance
suite can therefore feed Hermes; the CLI only selects the Grok Bot source today (`--bundle`), so wiring a way to
choose your adapter in `cmd_profile` (`src/main.rs`) is part of adding one.

## The trait

`src/core/adapter.rs`:

```rust
pub trait Adapter {
    fn id(&self) -> &'static str;                               // stable id, also its key in BotModel::extras
    fn read(&self, src: &Path) -> Res<BotModel>;
    fn read_source(&self, src: &Path) -> Res<(BotModel, Source)>; // default: read, no files recorded
    fn write(&self, m: &BotModel, dest: &Path) -> Res<Vec<String>>;
}
```

Implement `read_source` too if your adapter is a sync source: `Source.files_read` (relative path -> sha256) is
what the sync digest covers, so `sync` notices when the source changed. Only an adapter that keeps a live install
in step implements `Target` (`plan`/`apply`); Hermes does today. The trait is public API of the `botport` library
(`src/lib.rs`).

## Where files go and naming

- Code: `src/adapters/<id>/mod.rs` (plus helper modules beside it), registered in `src/adapters/mod.rs`.
- `<id>` is lowercase, hyphenated in the id string (`"open-claw"`), snake_case as the Rust module (`open_claw`).
- The type is a unit struct named after the platform (`pub struct OpenClaw;`) with `pub const ID: &str = "...";`.
- Fixtures: `tests/fixtures/conformance/<id>/` (a small, made-up native layout). Golden model JSON, if you add one:
  `tests/fixtures/model/<name>.json`.
- `src/core/` must never mention a platform name (a test enforces it). Platform knowledge lives only in the adapter.

## Mapping rules

- Map what fits into the model's fields (persona, memory, skills, schedules, connectors, credentials, history).
  Add a section for your platform to the mapping tables in [MODEL.md](MODEL.md).
- **Extras, not loss.** Anything the model cannot express goes into `extras["<id>"]` (any JSON shape you like), so
  your own `write` can restore it. Never silently drop input. Data your adapter cannot even parse goes into
  `extras["<id>"]["unparsed"]` (raw text keyed by relative path) rather than failing the whole read.
- Extras of *other* adapters are ignored on write; your write must work with `extras` empty.
- **Credentials are names only.** Record `CredentialRef::new(name)` (e.g. `TELEGRAM_BOT_TOKEN`); nothing about where or what it holds.
  Never read a secret value into the model, extras or written files.
- **Deterministic.** Same input -> byte-identical `to_json()`. Sort directory listings, use `BTreeMap`, no
  timestamps, random ids or hash-map iteration order in output.
- **Portable.** No hard-coded absolute paths, host names, user names or profile names. Paths inside the model are
  relative to the bot root. Everything machine-specific comes from arguments.
- Set `schema` to the current `bot-model/<major>` (use the core constants).

## Conformance tests

`src/core/conformance.rs` holds checks every adapter must pass. Each takes `(adapter, fixture_dir)`:

| check | what it proves |
|---|---|
| `read_is_deterministic` | two reads give byte-identical model JSON |
| `round_trips_portable_fields` | read -> write -> read keeps persona instructions, memory texts and skills |
| `round_trips_schedules`, `round_trips_connectors`, `round_trips_credential_names` | read -> write -> read keeps each one, by value or as a named manual item; run twice, the second time with `extras` cleared, so data must go through the model fields |
| `never_leaks_credentials` | the fixture's `FAKE_SECRET` is absent from the model and every written file |
| `has_no_absolute_paths` | the model holds no fixture path, `$HOME` or temp-dir path |
| `keeps_unknown_data_in_extras` | the fixture's `UNKNOWN_MARKER` ends up in `extras[id]` |
| `has_schema_version` | `schema` is a supported version |
| `check` | all of the above |

The suite also has `round_trips_schedules` (a schedule with a cron keeps its cron and action; an event or manual
`schedule X` item keeps at least its name), `round_trips_connectors` (a connector with a url keeps it; others, or
manual `connector X` items, keep their name) and `round_trips_credential_names`. Each one fails for an adapter
that drops that part (`src/conformance_tests.rs` proves it with deliberately lossy wrappers).

### Adding your fixture

Create `tests/fixtures/conformance/<id>/` with a minimal native layout that contains:

1. a persona/instructions, at least two memory facts and one skill (if the platform has them);
2. the string `CONFORMANCE-FAKE-SECRET-0000` where a real credential would live (e.g. `.env`, a token field);
3. the string `CONFORMANCE-UNKNOWN-MARKER` in a field or file your mapping has no model field for.
4. at least one schedule with a cron, one event schedule (or a manual `schedule X` item), one connector with an
   http url and one credential name. The schedule check also requires that an event never comes back as a cron.

Use made-up content only. Then add one line to `src/conformance_tests.rs`:

```rust
suite!(open_claw, OpenClaw, "open-claw", [read_is_deterministic, round_trips_portable_fields,
    round_trips_schedules, round_trips_connectors, round_trips_credential_names, never_leaks_credentials,
    has_no_absolute_paths, keeps_unknown_data_in_extras, has_schema_version]);
```

A check that cannot pass yet may be marked `name #[ignore = "fails: <reason>"]` in that list, with an issue to fix it.

### Running

```sh
cargo test conformance                      # all adapters
cargo test conformance_tests::open_claw     # one adapter
cargo clippy --all-targets -- -D warnings && cargo fmt --check
```

## Checklist for a new adapter PR

- [ ] `src/adapters/<id>/` with `read` + `write`, registered in `src/adapters/mod.rs`
- [ ] fixture under `tests/fixtures/conformance/<id>/`, made-up data, seeded secret + unknown marker, a cron
  schedule, an http connector and a credential name
- [ ] conformance suite line, all checks passing (or ignored with a reason and an issue)
- [ ] MODEL.md mapping-table section for the platform
- [ ] no secrets, no machine paths, no platform names in `src/core/`
