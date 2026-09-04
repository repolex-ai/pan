//! pan — the command line. A thin client of pand; it never touches a store's
//! files or graph itself. Every answer it prints came from the graph.
//!
//!   pan store [<user-id>] <file>     → <pan/Image/id>
//!   pan info  <pan/Image/id>         → the facts the graph holds about it
//!   pan state <pan/Image/id>         → what has been done to it (per stage)
//!   pan query [<user-id>] "<sparql>" → W3C results JSON
//!   pan stores                       → the stores this machine's pand manages
//!
//! `<user-id>` names a store (a soul's genesis SHA or a bare store id);
//! absent = pand's configured default. No flags.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use std::path::Path;

fn usage() -> ! {
    eprintln!(
        "pan {} — talks to pand\n\n\
         USAGE:\n  \
           pan store [<user-id>] <file>\n  \
           pan info  <pan/Image/id>\n  \
           pan state <pan/Image/id>\n  \
           pan query [<user-id>] \"<sparql>\"\n  \
           pan stores\n\n\
         pand must be running (start it with: pand). Config: {}",
        env!("CARGO_PKG_VERSION"),
        pan::daemon::config::config_dir().join("config.yml").display()
    );
    std::process::exit(2);
}

fn base() -> Result<String> {
    Ok(pan::daemon::config::DaemonConfig::load()?.base_url())
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .expect("http client")
}

fn not_running(e: reqwest::Error) -> anyhow::Error {
    if e.is_connect() {
        anyhow!("pand is not running (start it with: pand)")
    } else {
        anyhow!("{e}")
    }
}

fn media_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}

fn check(resp: reqwest::blocking::Response) -> Result<serde_json::Value> {
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or(text);
        return Err(anyhow!("{status}: {msg}"));
    }
    serde_json::from_str(&text).context("pand answered with something that is not JSON")
}

fn encode_id(id: &str) -> String {
    // The bracket form travels in a URL path; encode what a path cannot hold.
    id.replace('%', "%25").replace('/', "%2F").replace('<', "%3C").replace('>', "%3E")
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match argv.split_first() {
        Some((c, r)) => (c.as_str(), r.to_vec()),
        None => usage(),
    };
    let base = base()?;
    let c = client();

    match cmd {
        "store" => {
            let (user, file) = match rest.as_slice() {
                [file] => (None, Path::new(file)),
                [user, file] => (Some(user.clone()), Path::new(file)),
                _ => usage(),
            };
            let bytes = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
            let body = serde_json::json!({
                "soul": user,
                "content_type": media_type_for(file),
                "bytes_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            });
            let v = check(c.post(format!("{base}/media")).json(&body).send().map_err(not_running)?)?;
            println!("{}", v.get("id").and_then(|i| i.as_str()).unwrap_or("?"));
            if let Some(nr) = v.get("not_recorded").and_then(|n| n.as_array()).filter(|a| !a.is_empty()) {
                eprintln!("not recorded (no vocabulary yet): {}", nr.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "));
            }
            Ok(())
        }
        "info" | "state" => {
            let [id] = rest.as_slice() else { usage() };
            let tail = if cmd == "info" { "facts" } else { "state" };
            let v = check(c.get(format!("{base}/media/{}/{tail}", encode_id(id))).send().map_err(not_running)?)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        "query" => {
            let (user, sparql) = match rest.as_slice() {
                [q] => (None, q.clone()),
                [user, q] => (Some(user.clone()), q.clone()),
                _ => usage(),
            };
            let resp = c
                .post(format!("{base}/query"))
                .json(&serde_json::json!({ "store": user, "query": sparql }))
                .send()
                .map_err(not_running)?;
            let status = resp.status();
            let text = resp.text()?;
            if !status.is_success() {
                return Err(anyhow!("{status}: {text}"));
            }
            println!("{text}");
            Ok(())
        }
        "stores" => {
            let v = check(c.get(format!("{base}/stores")).send().map_err(not_running)?)?;
            for s in v.as_array().into_iter().flatten() {
                println!(
                    "{}{}  {}",
                    s.get("id").and_then(|x| x.as_str()).unwrap_or("?"),
                    if s.get("is_default").and_then(|x| x.as_bool()).unwrap_or(false) { " (default)" } else { "" },
                    s.get("root").and_then(|x| x.as_str()).unwrap_or("?"),
                );
            }
            Ok(())
        }
        _ => usage(),
    }
}
