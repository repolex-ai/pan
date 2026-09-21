//! ImageSet against a live pand (issue #39). The class, the `pan imageset`
//! commands and the /imagesets routes landed on 2026-09-17 with unit tests,
//! and no set had ever been made through a running daemon.
//!
//! This starts the real HTTP server over two temporary stores on a free port
//! and drives it with the real `pan` binary, the way a person does: store
//! images, make sets, add, remove, show, list, per store. Then it stops the
//! daemon, starts a second one over the same directories, and checks the sets
//! come back from their files and the memberships from the graph.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn make_png(seed: u8) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(8, 8, |x, y| {
        image::Rgba([seed, (x * 30) as u8, (y * 30) as u8, 255])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A home directory holding one pand config and two bare stores.
struct Home {
    dir: tempfile::TempDir,
    port: u16,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        for (name, id) in [("alpha", "aaaaaa"), ("bravo", "bbbbbb")] {
            let s = dir.path().join(name);
            std::fs::create_dir_all(&s).unwrap();
            std::fs::write(s.join("pan.yml"), format!("storage_id: {id}\n")).unwrap();
        }
        let cfg = dir.path().join(".config/pan");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("config.yml"),
            format!(
                "stores:\n  - {a}\n  - {b}\ndefault: {a}\nport: {port}\n",
                a = dir.path().join("alpha").display(),
                b = dir.path().join("bravo").display(),
            ),
        )
        .unwrap();
        Home { dir, port }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config(&self) -> PathBuf {
        self.path().join(".config/pan/config.yml")
    }

    /// Run the real `pan` binary with this home. Returns trimmed stdout.
    fn pan(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new(env!("CARGO_BIN_EXE_pan"))
            .args(args)
            .env("HOME", self.path())
            .output()
            .unwrap();
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }
}

/// A pand serving `home` until dropped.
struct Live {
    rt: tokio::runtime::Runtime,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    done: Option<tokio::task::JoinHandle<()>>,
}

impl Live {
    fn start(home: &Home) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let cfg = pan::daemon::config::DaemonConfig::load_from(&home.config()).unwrap();
        let daemon = Arc::new(pan::daemon::Daemon::open(cfg).unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let port = home.port;
        let listener = rt
            .block_on(tokio::net::TcpListener::bind(("127.0.0.1", port)))
            .unwrap();
        let done = rt.spawn(async move {
            axum::serve(listener, pan::daemon::http::router(daemon))
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Live {
            rt,
            stop: Some(stop),
            done: Some(done),
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(d) = self.done.take() {
            // Every handle on the stores is released before the next daemon
            // opens the same directories.
            let _ = self.rt.block_on(d);
        }
    }
}

fn show(home: &Home, set: &str) -> serde_json::Value {
    serde_json::from_str(&home.pan(&["imageset", "show", set]).unwrap()).unwrap()
}

fn members(v: &serde_json::Value) -> Vec<String> {
    let mut m: Vec<String> = v["media"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    m.sort();
    m
}

#[test]
fn imagesets_work_through_a_running_pand_and_survive_a_restart() {
    let home = Home::new();
    let img = |name: &str, seed: u8| -> String {
        let p = home.path().join(name);
        std::fs::write(&p, make_png(seed)).unwrap();
        p.to_string_lossy().into_owned()
    };
    let (f1, f2, f3) = (img("one.png", 1), img("two.png", 2), img("three.png", 3));

    let (set_a, set_b, a1, a2, b1);
    {
        let _pand = Live::start(&home);

        // Store: two in the default store, one in the other.
        a1 = home.pan(&["store", &f1]).unwrap();
        a2 = home.pan(&["store", &f2]).unwrap();
        b1 = home.pan(&["store", "bbbbbb", &f3]).unwrap();
        assert!(a1.starts_with("<pan/Image/"), "{a1}");

        // Create: one set per store. The command prints the set's id.
        set_a = home
            .pan(&["imageset", "create", "lora candidates"])
            .unwrap();
        set_b = home
            .pan(&["imageset", "create", "bbbbbb", "second store"])
            .unwrap();
        assert!(set_a.starts_with("<pan/ImageSet/"), "{set_a}");

        // The file is where it was decided to be: ImageSet/<id>.nq.
        let bare = set_a
            .trim_start_matches("<pan/ImageSet/")
            .trim_end_matches('>');
        let file = home.path().join(format!("alpha/ImageSet/{bare}.nq"));
        let text =
            std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        assert!(text.contains("lora candidates"), "{text}");

        // List: each store lists its own set and not the other's.
        let list_a = home.pan(&["imageset", "list"]).unwrap();
        let list_b = home.pan(&["imageset", "list", "bbbbbb"]).unwrap();
        assert!(list_a.contains("lora candidates") && !list_a.contains("second store"));
        assert!(list_b.contains("second store") && !list_b.contains("lora candidates"));

        // Add, twice over for one of them: the second add changes nothing.
        home.pan(&["imageset", "add", &set_a, &a1]).unwrap();
        home.pan(&["imageset", "add", &set_a, &a2]).unwrap();
        home.pan(&["imageset", "add", &set_a, &a2]).unwrap();
        let mut both = vec![a1.clone(), a2.clone()];
        both.sort();
        assert_eq!(members(&show(&home, &set_a)), both);

        // A set never reaches across stores.
        let err = home.pan(&["imageset", "add", &set_a, &b1]).unwrap_err();
        assert!(err.contains("404"), "{err}");
        // An image can be in a set in its own store.
        home.pan(&["imageset", "add", &set_b, &b1]).unwrap();
        assert_eq!(members(&show(&home, &set_b)), vec![b1.clone()]);

        // Membership is a fact on the image, and a graph pattern finds it.
        let q = format!(
            "SELECT ?s WHERE {{ ?s pan:relatedToId <https://repolex.ai/pan/ImageSet/{bare}> }}"
        );
        let found = home.pan(&["query", &q]).unwrap();
        assert_eq!(found.matches("/pan/Image/").count(), 2, "{found}");

        // Remove.
        home.pan(&["imageset", "remove", &set_a, &a1]).unwrap();
        assert_eq!(members(&show(&home, &set_a)), vec![a2.clone()]);

        // A set or an image that does not exist is refused, by name.
        let err = home
            .pan(&["imageset", "add", "<pan/ImageSet/nosuchid>", &a1])
            .unwrap_err();
        assert!(err.contains("imageset not found"), "{err}");
    }

    // pand is down. A second one over the same directories: the sets come
    // back from ImageSet/*.nq, the memberships are still in the graph and in
    // each image's own XMP.
    let _pand = Live::start(&home);
    let after = show(&home, &set_a);
    assert_eq!(after["description"], "lora candidates");
    assert_eq!(members(&after), vec![a2.clone()]);
    assert_eq!(members(&show(&home, &set_b)), vec![b1.clone()]);

    // The image file itself says so: one source PNG in the default store
    // names the set in its XMP, and the one that was removed does not.
    let bare = set_a
        .trim_start_matches("<pan/ImageSet/")
        .trim_end_matches('>');
    let needle = format!("pan/ImageSet/{bare}");
    let mut naming = 0;
    let mut stack = vec![home.path().join("alpha/_ignore/media/image/img/source")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if String::from_utf8_lossy(&std::fs::read(&p).unwrap()).contains(&needle) {
                naming += 1;
            }
        }
    }
    assert_eq!(naming, 1, "exactly one image file names the set in its XMP");

    let info = home.pan(&["info", &a2]).unwrap();
    assert!(
        info.contains("ImageSet"),
        "the image says which set it is in: {info}"
    );
}
