//! pan — the command line. A thin client of pand; it never touches a store's
//! files or graph itself. Every answer it prints came from the graph.
//!
//!   pan put   [<user-id>] <file> [key=value …] → <pan/Image/id> or <pan/Video/id>; the fields set on it at once
//!   pan info  <pan/Image/id>         → the facts the graph holds about it
//!   pan state <pan/Image/id>         → what has been done to it (per stage)
//!   pan query [<user-id>] "<sparql>" → W3C results JSON
//!   pan stores                       → the stores this machine's pand manages
//!   pan set   <pan/Image/id> key=value … → write any fact pan.ttl declares on an image (rating=4 isPicked=true)
//!   pan unset <pan/Image/id> key …       → remove them
//!   pan redo  <pan/Image/id> embed
//!   pan imageset create [<user-id>] "<description>" → <pan/ImageSet/id>
//!   pan imageset list   [<user-id>]                 → every set in the store
//!   pan imageset show   <pan/ImageSet/id>           → its facts and its media
//!   pan imageset add    <pan/ImageSet/id> <pan/Image/id>
//!   pan imageset remove <pan/ImageSet/id> <pan/Image/id>
//!
//! `<user-id>` names a store by its six-character id (the start of a soul's
//! genesis SHA, or of a bare store id);
//! absent = pand's configured default. No flags.

use anyhow::{anyhow, Context, Result};
use std::path::Path;

fn usage() -> ! {
    eprintln!(
        "pan {} — talks to pand\n\n\
         USAGE:\n  \
           pan put   [<user-id>] <file> [key=value …]\n      \
               Put an image or a video in the Pan. The file is sent as it is;\n      \
               <user-id> is the six-character store id from `pan stores`, and\n      \
               without it the file lands in pand's default store. Each key=value\n      \
               sets one field on the new media once it is in, the same as\n      \
               `pan set`: the keys are the properties pan.ttl declares on an\n      \
               image or on any media (rating, description, renderPrompt,\n      \
               mediaCreatedDate, isPicked, …). A key it does not declare is\n      \
               refused, and the refusal lists every key it does.\n  \
           pan info  <pan/Image/id>\n      \
               Every fact pand holds about one image or video (<pan/Video/id>\n      \
               works wherever <pan/Image/id> does).\n  \
           pan state <pan/Image/id>\n      \
               Which stages have run for it and which are still queued.\n  \
           pan query [<user-id>] \"<sparql>\"\n      \
               Run a SPARQL query against one store's graph; the default store\n      \
               when no store is named.\n  \
           pan stores\n      \
               List the stores pand serves, with the default marked.\n  \
           pan set   <pan/Image/id> rating=4 isPicked=true isRejected=false\n      \
               Set fields on media already in the Pan; same keys as `pan put`.\n      \
               Numbers and true/false are typed, everything else is text.\n  \
           pan unset <pan/Image/id> rating\n      \
               Remove a field you set.\n  \
           pan redo  <pan/Image/id> embed\n      \
               Run one stage again: caption, embed, segment, pose, depth or face.\n      \
               Images only; videos do not go through the stages.\n  \
           pan imageset create [<user-id>] \"<description>\"\n      \
               Start a named set of images in a store.\n  \
           pan imageset list   [<user-id>]\n      \
               The sets in one store; the default store when none is named.\n  \
           pan imageset show   <pan/ImageSet/id>\n      \
               The images in a set.\n  \
           pan imageset add    <pan/ImageSet/id> <pan/Image/id>\n  \
           pan imageset remove <pan/ImageSet/id> <pan/Image/id>\n      \
               Put an image in a set, or take it out.\n\n\
         pand must be running (start it with: pand). Config: {}",
        env!("CARGO_PKG_VERSION"),
        pan::daemon::config::config_dir()
            .join("config.yml")
            .display()
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
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
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
    id.replace('%', "%25")
        .replace('/', "%2F")
        .replace('<', "%3C")
        .replace('>', "%3E")
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
        // "put": put it in the Pan (goodlux, 2026-11-11). Not "store", which
        // is what the folder and its graph are called.
        "put" => {
            // Positional words are the store and the file; anything with an
            // `=` is a field to set once the file is in, the same key=value
            // as `pan set` (goodlux, 2026-11-11: a generator that bypasses
            // the render queue hands over a bare file and its prompt).
            let (words, pairs): (Vec<&String>, Vec<&String>) =
                rest.iter().partition(|a| !a.contains('='));
            let (user, file) = match words.as_slice() {
                [file] => (None, Path::new(file.as_str())),
                [user, file] => (Some((*user).clone()), Path::new(file.as_str())),
                _ => usage(),
            };
            let mut fields = serde_json::Map::new();
            for pair in &pairs {
                let (k, v) = pair.split_once('=').expect("partitioned on '='");
                if k.is_empty() {
                    return Err(anyhow!("expected key=value, got {pair} (example: renderPrompt=\"a cat\")"));
                }
                fields.insert(k.to_string(), json_value(v));
            }
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
            let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("?").to_string();
            if !fields.is_empty() {
                // The file is in; now the fields. A refused field is reported
                // with the id, so the stored file is not lost from view.
                check(
                    c.post(format!("{base}/media/{}/set", encode_id(&id)))
                        .json(&fields)
                        .send()
                        .map_err(not_running)?,
                )
                .with_context(|| format!("{id} is stored; setting its fields failed"))?;
            }
            println!("{id}");
            Ok(())
        }
        "info" | "state" => {
            let [id] = rest.as_slice() else { usage() };
            let tail = if cmd == "info" { "facts" } else { "state" };
            let v = check(
                c.get(format!("{base}/media/{}/{tail}", encode_id(id)))
                    .send()
                    .map_err(not_running)?,
            )?;
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
            let Some((id, pairs)) = rest.split_first() else {
                usage()
            };
            if pairs.is_empty() {
                usage();
            }
            let mut body = serde_json::Map::new();
            for pair in pairs {
                let Some((k, v)) = pair.split_once('=') else {
                    return Err(anyhow!(
                        "expected key=value, got {pair} (example: rating=4)"
                    ));
                };
                body.insert(k.to_string(), json_value(v));
            }
            let v = check(
                c.post(format!("{base}/media/{}/set", encode_id(id)))
                    .json(&body)
                    .send()
                    .map_err(not_running)?,
            )?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        "unset" => {
            let Some((id, keys)) = rest.split_first() else {
                usage()
            };
            if keys.is_empty() {
                usage();
            }
            let v = check(
                c.post(format!("{base}/media/{}/unset", encode_id(id)))
                    .json(&keys)
                    .send()
                    .map_err(not_running)?,
            )?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        "redo" => {
            let [id, stage] = rest.as_slice() else {
                usage()
            };
            let v = check(
                c.post(format!("{base}/media/{}/redo", encode_id(id)))
                    .json(&serde_json::json!({ "stage": stage }))
                    .send()
                    .map_err(not_running)?,
            )?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        "imageset" => {
            let Some((sub, args)) = rest.split_first() else {
                usage()
            };
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
                    let v = check(
                        c.post(url)
                            .json(&serde_json::json!({ "description": description }))
                            .send()
                            .map_err(not_running)?,
                    )?;
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
                            s.get("created_date")
                                .and_then(|x| x.as_str())
                                .unwrap_or("?"),
                            s.get("description").and_then(|x| x.as_str()).unwrap_or(""),
                        );
                    }
                    Ok(())
                }
                ("show", [id]) => {
                    let v = check(
                        c.get(format!("{base}/imagesets/{}", encode_id(id)))
                            .send()
                            .map_err(not_running)?,
                    )?;
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
            let v = check(
                c.get(format!("{base}/stores"))
                    .send()
                    .map_err(not_running)?,
            )?;
            for s in v.as_array().into_iter().flatten() {
                println!(
                    "{}{}  {}",
                    s.get("id").and_then(|x| x.as_str()).unwrap_or("?"),
                    if s.get("is_default")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false)
                    {
                        " (default)"
                    } else {
                        ""
                    },
                    s.get("root").and_then(|x| x.as_str()).unwrap_or("?"),
                );
            }
            Ok(())
        }
        _ => usage(),
    }
}
