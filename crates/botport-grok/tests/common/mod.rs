#![allow(dead_code)]
use botport_grok::clock::{FakeClock, SharedClock};
use botport_grok::policy::{Caller, Policy};
use botport_grok::transport::fake::FakeTransport;
use botport_grok::transport::Transport;
use botport_grok::Engine;
use std::path::{Path, PathBuf};

pub const MAIN: &str = "00000000-0000-4000-8000-000000000001";
pub const TEMP: &str = "00000000-0000-4000-8000-000000000035";

pub fn fx() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/app-1.1.0")
}

pub fn clock() -> SharedClock {
    FakeClock::new(1_800_000_000_000)
}

pub fn engine_from(f: FakeTransport, clock: SharedClock, policy: Policy, caller: Caller) -> Engine {
    Engine::new(
        vec![Box::new(f) as Box<dyn Transport>],
        policy,
        clock,
        caller,
    )
}

/// The idle snapshot with `turns` queued (each send starts the next one), fast preset, CLI caller.
pub fn eng(turns: &[&str]) -> Engine {
    let c = clock();
    let mut f = FakeTransport::load(&fx(), c.clone()).unwrap();
    for t in turns {
        f = f.with_turn(t);
    }
    engine_from(f, c, Policy::default(), Caller::Cli { tty: false })
}

pub fn eng_awaiting() -> Engine {
    let c = clock();
    let f = FakeTransport::load_awaiting(&fx(), c.clone()).unwrap();
    engine_from(f, c, Policy::default(), Caller::Cli { tty: false })
}

/// A fresh scratch dir under the system temp dir (removed by name by the caller's guard).
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("botport-grok-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copy the fixture dir (two levels) into `to`.
pub fn copy_fixtures(to: &Path) {
    for sub in ["", "turns"] {
        let from = fx().join(sub);
        std::fs::create_dir_all(to.join(sub)).unwrap();
        for e in std::fs::read_dir(&from).unwrap().flatten() {
            if e.path().is_file() {
                std::fs::copy(e.path(), to.join(sub).join(e.file_name())).unwrap();
            }
        }
    }
}
