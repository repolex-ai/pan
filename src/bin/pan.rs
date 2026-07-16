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
         USAGE:\n  pan serve [--root DIR] [--bind ADDR] [--port N]\n  pan info  [--root DIR]\n\n\
         Defaults: --root ~/.pan  --bind {DEFAULT_BIND}  --port {DEFAULT_PORT}\n\
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
        _ => usage(),
    }
}
