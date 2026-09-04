//! pand — the Pan daemon.
//!
//!   pand start    kill EVERY pand on this machine, then run in the foreground
//!   pand stop     kill EVERY pand on this machine and exit
//!   pand          run in the foreground without the kill-first step
//!
//! There is no launchd job any more (Rob removed it, 2026-09-04): a terminal
//! starts pand and owns it. `stop` still boots out the old label in case a
//! plist ever comes back.
//!
//! No flags. Everything else is in `~/.config/pan/config.yml` (stores,
//! default, port, model endpoints); a missing file means one store at `~/.pan`
//! and no model stages.
//!
//! "Every pand" means: the launchd job `ai.repolex.pand` is booted out so it
//! cannot respawn, then every process named pand (other than this one) gets
//! SIGTERM, then SIGKILL if it is still there five seconds later. Rob,
//! 2026-09-04: two pands alive at once, one under launchd and one in a
//! terminal, cost a morning; start and stop must leave exactly one or zero.

use anyhow::Result;
use std::sync::Arc;

const LAUNCHD_LABEL: &str = "ai.repolex.pand";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => serve(),
        ["start"] => {
            stop_all();
            serve()
        }
        ["stop"] => {
            stop_all();
            Ok(())
        }
        _ => {
            eprintln!(
                "usage: pand | pand start | pand stop\n  (no flags; configure in {})",
                pan::daemon::config::config_dir().join("config.yml").display()
            );
            std::process::exit(2);
        }
    }
}

fn serve() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=info".into()))
        .init();

    let cfg = pan::daemon::config::DaemonConfig::load()?;
    tracing::info!(config = %cfg.path.display(), stores = cfg.stores.len(), stages = cfg.models.len(), "pand starting");
    let daemon = Arc::new(pan::daemon::Daemon::open(cfg)?);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(pan::daemon::http::serve(daemon))
}

/// Kill every other pand on this machine. Prints what it did, in plain words.
fn stop_all() {
    use std::process::Command;

    // 1. The launchd job, so it cannot respawn what we are about to kill.
    let uid = unsafe { libc_getuid() };
    let target = format!("gui/{uid}/{LAUNCHD_LABEL}");
    let out = Command::new("launchctl").args(["bootout", &target]).output();
    match out {
        Ok(o) if o.status.success() => eprintln!("pand stop: launchd job {LAUNCHD_LABEL} booted out (it will not respawn)"),
        Ok(_) => eprintln!("pand stop: no launchd job {LAUNCHD_LABEL} was loaded"),
        Err(e) => eprintln!("pand stop: could not run launchctl: {e}"),
    }

    // 2. Every process named pand, except this one.
    let me = std::process::id();
    let pids = || -> Vec<u32> {
        Command::new("pgrep")
            .args(["-x", "pand"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.trim().parse::<u32>().ok()).filter(|p| *p != me).collect())
            .unwrap_or_default()
    };
    let first = pids();
    if first.is_empty() {
        eprintln!("pand stop: no other pand process was running");
        return;
    }
    for p in &first {
        let _ = Command::new("kill").args(["-TERM", &p.to_string()]).status();
    }
    eprintln!("pand stop: sent SIGTERM to pand process(es) {:?}", first);
    for _ in 0..50 {
        if pids().is_empty() {
            eprintln!("pand stop: all pand processes have exited");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let left = pids();
    for p in &left {
        let _ = Command::new("kill").args(["-KILL", &p.to_string()]).status();
    }
    eprintln!("pand stop: {:?} did not exit in 5 s; sent SIGKILL", left);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let still = pids();
    if still.is_empty() {
        eprintln!("pand stop: all pand processes have exited");
    } else {
        eprintln!("pand stop: STILL RUNNING after SIGKILL: {:?} — check `ps -p {}`", still, still.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(","));
    }
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}
