//! The `pan` binary — Mode 1 (standalone).
//!
//! Two parameter-less-friendly commands, no daemon machinery:
//!   pan serve [--root DIR] [--bind ADDR] [--port N]   (foreground; ctrl-c stops)
//!   pan info  [--root DIR]
//!
//! Defaults: root = ~/.pan (zero-permission home-dir default — works without
//! touching OS security), bind = 127.0.0.1, port = 7401.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

const DEFAULT_PORT: u16 = 7401;
const DEFAULT_BIND: &str = "127.0.0.1";

fn default_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".pan"))
        .unwrap_or_else(|| PathBuf::from(".pan"))
}

struct Args {
    root: PathBuf,
    bind: String,
    port: u16,
}

fn parse_flags(rest: &[String]) -> Result<Args> {
    let mut args = Args {
        root: default_root(),
        bind: DEFAULT_BIND.to_string(),
        port: DEFAULT_PORT,
    };
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--root" => {
                i += 1;
                args.root = PathBuf::from(rest.get(i).ok_or_else(|| anyhow!("--root needs a path"))?);
            }
            "--bind" => {
                i += 1;
                args.bind = rest.get(i).ok_or_else(|| anyhow!("--bind needs an address"))?.clone();
            }
            "--port" => {
                i += 1;
                args.port = rest
                    .get(i)
                    .ok_or_else(|| anyhow!("--port needs a number"))?
                    .parse()?;
            }
            other => return Err(anyhow!("unknown flag: {other}")),
        }
        i += 1;
    }
    Ok(args)
}

fn usage() -> ! {
    eprintln!(
        "pan {} — a standalone media store that speaks git-lex\n\n\
         USAGE:\n  \
           pan serve  [--root DIR] [--bind ADDR] [--port N]\n  \
           pan info   [--root DIR]\n  \
           pan import --source POOL_DIR [--root DIR] [--limit N] [--per-month N] [--dry-run]\n\n\
         Defaults: --root ~/.pan  --bind {DEFAULT_BIND}  --port {DEFAULT_PORT}\n\n\
         `import` migrates a Pool store into Pan: media is COPIED (the source is\n\
         never modified), each enricher's output becomes a data file beside the\n\
         blob, and the image carries a reference to every one of them.\n\
         --per-month samples evenly within each month, so a short run still meets\n\
         every metadata shape the source contains.\n\n\
         Swagger (the interface spec): http://{DEFAULT_BIND}:{DEFAULT_PORT}/swagger-ui",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2);
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match argv.split_first() {
        Some((c, r)) => (c.as_str(), r.to_vec()),
        None => usage(),
    };

    match cmd {
        "serve" => {
            let args = parse_flags(&rest)?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(pan::serve::serve(&args.root, &args.bind, args.port))
        }
        "info" => {
            let args = parse_flags(&rest)?;
            let store = pan::Pan::open(&args.root)?;
            println!("storage_id:   {}", store.cfg.storage_id);
            println!("root:         {}", store.layout.root.display());
            println!("storage_root: {}", store.layout.storage_root.display());
            println!("index_id:     {}", store.cfg.index_id);
            let stats = store.index_stats();
            if stats.is_empty() {
                println!("indexes:      (none yet)");
            }
            for (name, s) in stats {
                println!("index {name}: dim={} count={}", s.dim, s.count);
            }
            Ok(())
        }
        "import" => {
            let mut root = default_root();
            let mut source: Option<PathBuf> = None;
            let mut limit: Option<usize> = None;
            let mut per_month: Option<usize> = None;
            let mut dry_run = false;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--root" => {
                        i += 1;
                        root = PathBuf::from(rest.get(i).ok_or_else(|| anyhow!("--root needs a path"))?);
                    }
                    "--source" => {
                        i += 1;
                        source = Some(PathBuf::from(
                            rest.get(i).ok_or_else(|| anyhow!("--source needs a path"))?,
                        ));
                    }
                    "--limit" => {
                        i += 1;
                        limit = Some(rest.get(i).ok_or_else(|| anyhow!("--limit needs a number"))?.parse()?);
                    }
                    "--per-month" => {
                        i += 1;
                        per_month =
                            Some(rest.get(i).ok_or_else(|| anyhow!("--per-month needs a number"))?.parse()?);
                    }
                    "--dry-run" => dry_run = true,
                    other => return Err(anyhow!("unknown flag: {other}")),
                }
                i += 1;
            }
            let source = source.ok_or_else(|| anyhow!("import needs --source POOL_DIR"))?;
            if !source.join("blob/image").is_dir() {
                return Err(anyhow!(
                    "{} does not look like a Pool store (no blob/image)",
                    source.display()
                ));
            }

            let pan = pan::Pan::open(&root)?;
            let opts = pan::import::ImportOptions { source_root: source, limit, per_month, dry_run };
            let stats = pan::import::import_pool(&pan, &opts)?;

            println!("\n── import {} ──", if dry_run { "(dry run)" } else { "complete" });
            println!("scanned:  {}", stats.scanned);
            println!("imported: {}", stats.imported);
            if stats.skipped_already > 0 {
                println!("skipped:  {} (already imported)", stats.skipped_already);
            }
            if stats.failed > 0 {
                println!("FAILED:   {}", stats.failed);
            }
            println!(
                "records:  {} regions · {} poses · {} captions · {} vectors · {} pose overlays",
                stats.regions, stats.poses, stats.captions, stats.vectors, stats.overlays
            );
            if !stats.by_month.is_empty() {
                println!("\nby month:");
                for (m, n) in &stats.by_month {
                    println!("  {m}  {n}");
                }
            }
            if !stats.passthrough_fields.is_empty() {
                println!("\napplication fields carried through (name × images):");
                let mut rows: Vec<_> = stats.passthrough_fields.iter().collect();
                rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                for (name, n) in rows.iter().take(40) {
                    println!("  {n:>5}  {name}");
                }
                if rows.len() > 40 {
                    println!("  … {} more", rows.len() - 40);
                }
            }
            if !stats.warnings.is_empty() {
                println!("\nwarnings ({}):", stats.warnings.len());
                for w in stats.warnings.iter().take(20) {
                    println!("  {w}");
                }
                if stats.warnings.len() > 20 {
                    println!("  … {} more", stats.warnings.len() - 20);
                }
            }
            Ok(())
        }
        _ => usage(),
    }
}
