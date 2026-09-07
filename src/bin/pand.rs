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
        ["status"] => status(),
        _ => {
            eprintln!(
                "usage: pand | pand start | pand stop | pand status\n  (no flags; configure in {})",
                pan::daemon::config::config_dir().join("config.yml").display()
            );
            std::process::exit(2);
        }
    }
}

fn serve() -> Result<()> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    // Every line goes two places: the terminal that started pand, and
    // ~/.pan/logs/pand.log (appended across starts) — the file launchd used to
    // fill, kept now that a terminal owns the process (Rob, 2026-09-04).
    let log_dir = pan::daemon::config::default_store_dir().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join("pand.log");
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=info".into());
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(file)))
        .init();
    tracing::info!(log = %log_path.display(), "pand logging here as well as to this terminal");

    let cfg = pan::daemon::config::DaemonConfig::load()?;
    tracing::info!(config = %cfg.path.display(), stores = cfg.stores.len(), stages = cfg.models.len(), "pand starting");
    let daemon = Arc::new(pan::daemon::Daemon::open(cfg)?);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(pan::daemon::http::serve(daemon))
}

/// Is pand running, and is it making model calls? Plain words, exit 0 when it
/// is running and answering, 1 otherwise — so a script can ask too.
fn status() -> Result<()> {
    let cfg = pan::daemon::config::DaemonConfig::load()?;
    let url = format!("{}/health", cfg.base_url());
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(3)).build()?;
    let pids: Vec<String> = std::process::Command::new("pgrep")
        .args(["-x", "pand"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_string()).filter(|p| !p.is_empty() && *p != std::process::id().to_string()).collect())
        .unwrap_or_default();
    match client.get(&url).send().and_then(|r| r.error_for_status()).and_then(|r| r.json::<serde_json::Value>()) {
        Ok(h) => {
            let up = h["uptime_secs"].as_u64().unwrap_or(0);
            let stages: Vec<String> = h["stages"].as_object().map(|m| m.iter().map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or("?"))).collect()).unwrap_or_default();
            println!(
                "pand is RUNNING — pid {}, up {}h {:02}m {:02}s, version {}",
                h["pid"].as_u64().unwrap_or(0),
                up / 3600,
                (up % 3600) / 60,
                up % 60,
                h["version"].as_str().unwrap_or("?")
            );
            println!("  serving {} for {} store(s), default {}", cfg.base_url(), h["stores"].as_array().map(|a| a.len()).unwrap_or(0), h["default"].as_str().unwrap_or("?"));
            println!("  since start: {} image(s) stored, {} model call(s) made", h["images_stored"].as_u64().unwrap_or(0), h["model_calls"].as_u64().unwrap_or(0));
            if let Some(w) = h["windows"].as_object() {
                let mut ws: Vec<String> = w.iter().map(|(k, v)| format!("{k} {}", v.as_str().unwrap_or("?"))).collect();
                ws.sort();
                println!("  in flight (window/ceiling): {}", ws.join(", "));
            }
            if let Some(rows) = h["counts"].as_array() {
                println!("  {:<8} {:>7} {:>7} {:>8} {:>7} {:>6} {:>7}", "store", "images", "thumbs", "captions", "embeds", "poses", "regions");
                for r in rows {
                    let g = |k: &str| r[k].as_u64().unwrap_or(0);
                    println!(
                        "  {:<8} {:>7} {:>7} {:>8} {:>7} {:>6} {:>7}",
                        r["store"].as_str().unwrap_or("?").chars().take(6).collect::<String>(),
                        g("images"), g("thumbnails"), g("captions"), g("embeddings"), g("poses"), g("regions")
                    );
                }
            }
            if stages.is_empty() {
                println!("  model stages: none configured (ingest only)");
            } else {
                println!("  model stages: {}", stages.join("; "));
            }
            println!("  log: {}", pan::daemon::config::default_store_dir().join("logs").join("pand.log").display());
            if pids.len() > 1 {
                println!("  WARNING: {} pand processes exist ({}); `pand stop` kills them all", pids.len(), pids.join(", "));
            }
            Ok(())
        }
        Err(_) if pids.is_empty() => {
            println!("pand is NOT running (nothing answers on {} and no pand process exists). Start it with: pand start", cfg.base_url());
            std::process::exit(1);
        }
        Err(e) => {
            println!(
                "pand is NOT answering on {} but a pand process exists (pid {}): {e}\n  `pand stop` kills it; then `pand start`",
                cfg.base_url(),
                pids.join(", ")
            );
            std::process::exit(1);
        }
    }
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
