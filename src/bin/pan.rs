//! pan — the command line. A thin client of pand; it never touches a store's
//! files or graph itself. Every answer it prints came from the graph.
//!
//!   pan store [<user-id>] <file>     → <pan/Image/id>
//!   pan info  <pan/Image/id>         → the facts the graph holds about it
//!   pan state <pan/Image/id>         → what has been done to it (per stage)
//!   pan query [<user-id>] "<sparql>" → W3C results JSON
//!   pan stores                       → the stores this machine's pand manages
//!   pan set   <pan/Image/id> key=value … → write facts a person owns (rating=4 isPicked=true)
//!   pan unset <pan/Image/id> key …       → remove them
//!   pan imageset create [<user-id>] "<description>" → <pan/ImageSet/id>
//!   pan imageset list   [<user-id>]                 → every set in the store
//!   pan imageset show   <pan/ImageSet/id>           → its facts and its media
//!   pan imageset add    <pan/ImageSet/id> <pan/Image/id>
//!   pan imageset remove <pan/ImageSet/id> <pan/Image/id>
//!
//! `<user-id>` names a store (a soul's genesis SHA or a bare store id);
//! absent = pand's configured default. No flags.

use anyhow::{anyhow, Context, Result};
use std::path::Path;

fn usage() -> ! {
    eprintln!(
        "pan {} — talks to pand\n\n\
         USAGE:\n  \
           pan store [<user-id>] <file>\n  \
           pan info  <pan/Image/id>\n  \
           pan state <pan/Image/id>\n  \
           pan query [<user-id>] \"<sparql>\"\n  \
           pan stores\n  \
           pan set   <pan/Image/id> rating=4 isPicked=true isRejected=false\n  \
           pan unset <pan/Image/id> rating\n  \
           pan imageset create [<user-id>] \"<description>\"\n  \
           pan imageset list   [<user-id>]\n  \
           pan imageset show   <pan/ImageSet/id>\n  \
           pan imageset add    <pan/ImageSet/id> <pan/Image/id>\n  \
           pan imageset remove <pan/ImageSet/id> <pan/Image/id>\n\n\
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

/// `true`/`false` become booleans, whole numbers become numbers, anything
/// else stays text; pand checks the value against the ontology either way.
fn json_value(v: &str) -> serde_json::Value {
    match v {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => match v.parse::<i64>() {
            Ok(n) => serde_json::Value::from(n),
            Err(_) => serde_json::Value::String(v.to_string()),
        },
    }
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
            // The file IS the request: raw bytes, media type in the header.
            // Whatever XMP it carries is its metadata; nothing else is sent.
            let url = match &user {
                Some(u) => format!("{base}/stores/{u}/media"),
                None => format!("{base}/media"),
            };
            let v = check(
                c.post(url)
                    .header(reqwest::header::CONTENT_TYPE, media_type_for(file))
                    .body(bytes)
                    .send()
                    .map_err(not_running)?,
            )?;
            println!("{}", v.get("id").and_then(|i| i.as_str()).unwrap_or("?"));
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
        "set" => {
            let Some((id, pairs)) = rest.split_first() else { usage() };
            if pairs.is_empty() {
                usage();
            }
            let mut body = serde_json::Map::new();
            for pair in pairs {
                let Some((k, v)) = pair.split_once('=') else {
                    return Err(anyhow!("expected key=value, got {pair} (example: rating=4)"));
                };
                body.insert(k.to_string(), json_value(v));
            }
            let v = check(c.post(format!("{base}/media/{}/set", encode_id(id))).json(&body).send().map_err(not_running)?)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        "unset" => {
            let Some((id, keys)) = rest.split_first() else { usage() };
            if keys.is_empty() {
                usage();
            }
            let v = check(c.post(format!("{base}/media/{}/unset", encode_id(id))).json(&keys).send().map_err(not_running)?)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        "imageset" => {
            let Some((sub, args)) = rest.split_first() else { usage() };
            match (sub.as_str(), args) {
                ("create", args) => {
                    let (user, description) = match args {
                        [d] => (None, d.clone()),
                        [user, d] => (Some(user.clone()), d.clone()),
                        _ => usage(),
                    };
                    let url = match &user {
                        Some(u) => format!("{base}/stores/{u}/imagesets"),
                        None => format!("{base}/imagesets"),
                    };
                    let v = check(c.post(url).json(&serde_json::json!({ "description": description })).send().map_err(not_running)?)?;
                    println!("{}", v.get("id").and_then(|i| i.as_str()).unwrap_or("?"));
                    Ok(())
                }
                ("list", args) => {
                    let url = match args {
                        [] => format!("{base}/imagesets"),
                        [user] => format!("{base}/stores/{user}/imagesets"),
                        _ => usage(),
                    };
                    let v = check(c.get(url).send().map_err(not_running)?)?;
                    for s in v.as_array().into_iter().flatten() {
                        println!(
                            "{}  {}  {}",
                            s.get("id").and_then(|x| x.as_str()).unwrap_or("?"),
                            s.get("created_date").and_then(|x| x.as_str()).unwrap_or("?"),
                            s.get("description").and_then(|x| x.as_str()).unwrap_or(""),
                        );
                    }
                    Ok(())
                }
                ("show", [id]) => {
                    let v = check(c.get(format!("{base}/imagesets/{}", encode_id(id))).send().map_err(not_running)?)?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    Ok(())
                }
                ("add" | "remove", [set, media]) => {
                    let v = check(
                        c.post(format!("{base}/imagesets/{}/{sub}", encode_id(set)))
                            .json(&serde_json::json!({ "media": media }))
                            .send()
                            .map_err(not_running)?,
                    )?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    Ok(())
                }
                _ => usage(),
            }
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
