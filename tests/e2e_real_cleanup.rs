//! `tests/e2e-real/run.sh` cleanup safety, with a stand-in `hermes` and a temp HOME/HERMES_HOME
//! (never the real Hermes home): the trap deletes only a profile the run itself created.
mod common;
use common::Tmp;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The stand-in logs every call; `profile create` fails unless `CREATE_OK=1`; it never deletes anything.
const STUB: &str = r#"#!/bin/sh
echo "$*" >> "$STUB_LOG"
case "$1 $2" in
  "profile create") [ "$CREATE_OK" = 1 ] || exit 1 ;;
esac
exit 0
"#;

fn exe(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run the script with `botport` = `/usr/bin/false` (export fails before the create step) or a
/// stand-in that succeeds (the run reaches `hermes profile create`). Returns the output and the hermes call log.
fn run(t: &Path, botport_ok: bool, create_ok: bool) -> (Output, String) {
    let (mut c, log) = command(t, botport_ok, create_ok);
    let o = c.output().unwrap();
    (o, std::fs::read_to_string(&log).unwrap_or_default())
}

fn command(t: &Path, botport_ok: bool, create_ok: bool) -> (Command, PathBuf) {
    let bin = t.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    exe(&bin.join("hermes"), STUB);
    let botport: PathBuf = if botport_ok {
        exe(&bin.join("botport-ok"), "#!/bin/sh\nexit 0\n");
        bin.join("botport-ok")
    } else {
        "/usr/bin/false".into()
    };
    let log = t.join("hermes.log");
    let mut c = Command::new("bash");
    c.arg(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/e2e-real/run.sh"
    ))
    .arg(t.join("out"))
    .env("HOME", t.join("home"))
    .env("HERMES_HOME", t.join("hh"))
    .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
    .env("STUB_LOG", &log)
    .env("CREATE_OK", if create_ok { "1" } else { "0" })
    .env("BOTPORT_E2E_REAL", "1")
    .env("BOTPORT_E2E_CLONE_FROM", "src-bot")
    .env("BOTPORT_E2E_PROFILE", "e2e-x")
    .env("BOTPORT_BIN", &botport);
    (c, log)
}

#[test]
fn an_existing_profile_is_refused_and_never_deleted() {
    let t = Tmp::new("e2e-existing");
    let pd = t.0.as_path().join("hh/profiles/e2e-x");
    std::fs::create_dir_all(&pd).unwrap();
    std::fs::write(pd.join("SOUL.md"), "mine").unwrap();
    let (o, log) = run(t.0.as_path(), true, true);
    assert_eq!(o.status.code(), Some(2), "{o:?}");
    assert!(String::from_utf8_lossy(&o.stderr).contains("already exists"));
    assert!(log.is_empty(), "hermes must not be called: {log}");
    assert_eq!(std::fs::read_to_string(pd.join("SOUL.md")).unwrap(), "mine");
}

#[test]
fn a_failed_create_deletes_nothing() {
    let t = Tmp::new("e2e-create-fails");
    let (o, log) = run(t.0.as_path(), true, false);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(log.contains("profile create e2e-x"), "{log}");
    assert!(!log.contains("delete"), "{log}");
    assert!(String::from_utf8_lossy(&o.stdout).contains("not created by this run"));
}

#[test]
fn an_export_failure_before_create_deletes_nothing() {
    let t = Tmp::new("e2e-export-fails");
    let (o, log) = run(t.0.as_path(), false, true);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(log.is_empty(), "{log}");
}

#[test]
fn a_profile_this_run_created_is_deleted_on_sigterm() {
    let t = Tmp::new("e2e-created");
    let (mut c, log) = command(t.0.as_path(), true, true);
    let child = c
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // the create succeeded once the run starts the gateway; it then waits for the socket
    let start = std::time::Instant::now();
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("gateway run")
    {
        assert!(
            start.elapsed().as_secs() < 30,
            "never reached the gateway step"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let pid = child.id().to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    let o = child.wait_with_output().unwrap();
    let log = std::fs::read_to_string(&log).unwrap();
    assert!(log.contains("profile delete -y e2e-x"), "{log}");
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("deleted profile e2e-x"),
        "{o:?}"
    );
}
