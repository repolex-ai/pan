//! Forget every embedding in every configured store so the embed stage
//! remakes them. Run with pand STOPPED (the graph is single-writer):
//!
//!     pand stop && cargo run --release --example wipe_embeddings && pand start
use pan::daemon::config::DaemonConfig;
use pan::daemon::registry;
use pan::Pan;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cfg = DaemonConfig::load()?;
    for e in registry::resolve_all(&cfg.stores)? {
        let media_root = cfg.media_root_for(&e.id);
        let pan = Pan::open_with(&e.root, &e.id, media_root.as_deref())?;
        let n = pan.wipe_embeddings()?;
        println!("{}  wiped embeddings on {n} image(s)", e.id);
    }
    Ok(())
}
