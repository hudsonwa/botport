#![forbid(unsafe_code)]
use g2h::config::Config;
use g2h::daemon::{self, AnyBackend, DaemonOptions};
use g2h::paths::Paths;
use std::rc::Rc;

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "--version" | "-V" => {
            println!("g2hd {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        "--home" => {
            match Paths::from_env() {
                Some(p) => println!("{}", p.home.display()),
                None => std::process::exit(1),
            }
            return;
        }
        "" => {}
        _ => {
            println!("g2hd {} — g2h broker daemon\n\nUSAGE: g2hd   (home: $G2H_HOME or ~/.g2h; --home prints it; config: <home>/config.toml)", env!("CARGO_PKG_VERSION"));
            return;
        }
    }
    std::process::exit(match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("g2hd: {e}");
            1
        }
    });
}

fn run() -> Result<(), String> {
    let paths = Paths::from_env().ok_or("cannot resolve the g2h home (set HOME or G2H_HOME)")?;
    let config = Config::load(&paths.config())?;
    let backend = AnyBackend::from_options(config.backend_options(&paths.home)?);
    let opts = DaemonOptions::new(paths, config);
    let bound = daemon::bind(&opts)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    tokio::task::LocalSet::new().block_on(&rt, async move {
        let shutdown = Rc::new(tokio::sync::Notify::new());
        let sd = shutdown.clone();
        tokio::task::spawn_local(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut term), Ok(mut int)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
            ) else {
                return;
            };
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
            sd.notify_one();
        });
        bound.run(backend, shutdown).await
    })
}
