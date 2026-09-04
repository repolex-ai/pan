//! pand — the Pan daemon. Zero arguments, zero flags.
//!
//! Everything it needs is in `~/.config/pan/config.yml` (stores, default,
//! port, model endpoints); a missing file means one store at `~/.pan` and no
//! model stages. Foreground; ctrl-c stops it. A supervisor (launchd) runs
//! this exact command.

use anyhow::Result;
use std::sync::Arc;

fn main() -> Result<()> {
    if std::env::args().len() > 1 {
        eprintln!("pand takes no arguments. Configure it in {}", pan::daemon::config::config_dir().join("config.yml").display());
        std::process::exit(2);
    }
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
