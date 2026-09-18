//! The model-call log — one JSON line per call pand makes to a model.
//!
//! Metadata only, never the image and never the answer: when, which store and
//! image, which stage and model, where it went, how many bytes each way, the
//! HTTP status, how long it took, and how it ended. One file per local day
//! under `<config dir>/logs/calls/YYYY-MM-DD.jsonl`, so a day's cost and
//! failures are one `jq` away; files older than `log_keep_days` are removed
//! at start and whenever the day rolls over (goodlux, 2026-09-07: the shape
//! is a formal location beside the config, by date, pruned automatically,
//! retention a setting).
//!
//! A failed log write is a warning once, never a stage failure: the log
//! describes the work, it does not gate it.

use chrono::{Local, NaiveDate, SecondsFormat};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Days of call logs kept when `log_keep_days` is not in config.yml.
pub const DEFAULT_KEEP_DAYS: u32 = 30;

/// Where the files live, relative to the config directory.
pub const CALLS_SUBDIR: &str = "logs/calls";

/// What the HTTP layer measured about one call. Filled in by the client
/// (`iris.rs`) where the bytes and the clock are; read by the stage where
/// the outcome is known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallMeta {
    pub url: String,
    /// The HTTP status, or None when the call never got an answer
    /// (connection refused, reset, timed out).
    pub status: Option<u16>,
    pub latency_ms: u64,
    /// Payload bytes sent: the image plus any text or JSON body. Not the
    /// wire size (multipart framing is not counted).
    pub request_bytes: u64,
    pub response_bytes: u64,
    /// Chat-completions replies only (the caption stage): why the model
    /// stopped (`stop`, `length`, …) and the token counts the server
    /// reported. None on every other route and when the server sent no
    /// `usage`. Asked for by m3rc (2026-09-18) to tell a cut-off answer
    /// (`length`) from a wandering one (`stop` with prose).
    pub finish_reason: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

/// One call's measurements, handed to the client empty and read back after.
#[derive(Default)]
pub struct Meter(Mutex<Option<CallMeta>>);

impl Meter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self, m: CallMeta) {
        *crate::locked(&self.0) = Some(m);
    }
    pub fn take(&self) -> Option<CallMeta> {
        crate::locked(&self.0).take()
    }
}

/// One line of the log. Field order is the column order a reader sees.
#[derive(Debug, Serialize)]
pub struct CallLine<'a> {
    /// RFC3339 with milliseconds, system local time.
    pub time: String,
    pub store: &'a str,
    pub id: &'a str,
    pub stage: &'a str,
    pub model: &'a str,
    pub url: &'a str,
    /// `primary` or `fallback`.
    pub via: &'a str,
    pub request_bytes: u64,
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub response_bytes: u64,
    /// `recorded`, `busy`, `backend_down`, `quota`, `transient`, `terminal`.
    pub outcome: &'a str,
    pub error: Option<&'a str>,
    /// Chat-completions replies only; null elsewhere and when the server
    /// sent no `usage` (see `CallMeta`).
    pub finish_reason: Option<&'a str>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

impl CallLine<'_> {
    /// The current local time in the log's format.
    pub fn now() -> String {
        Local::now().to_rfc3339_opts(SecondsFormat::Millis, false)
    }
}

pub struct CallLog {
    dir: PathBuf,
    keep_days: u32,
    /// The open file and the local date it belongs to; a new date closes it.
    open: Mutex<Option<(String, BufWriter<File>)>>,
    warned: AtomicBool,
}

impl CallLog {
    /// `config_dir` is the directory config.yml lives in. Prunes at once.
    pub fn new(config_dir: &Path, keep_days: u32) -> Self {
        let log = CallLog {
            dir: config_dir.join(CALLS_SUBDIR),
            keep_days,
            open: Mutex::new(None),
            warned: AtomicBool::new(false),
        };
        let removed = log.prune_now();
        tracing::info!(dir = %log.dir.display(), keep_days, removed, "model-call log");
        log
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn keep_days(&self) -> u32 {
        self.keep_days
    }

    /// Append one line. Never fails the caller: the first write error is a
    /// warning, later ones are silent until the next successful write.
    pub fn record(&self, line: &CallLine<'_>) {
        match self.append(line) {
            Ok(()) => self.warned.store(false, Ordering::Relaxed),
            Err(e) => {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(dir = %self.dir.display(), "model-call log not written: {e}");
                }
            }
        }
    }

    fn append(&self, line: &CallLine<'_>) -> io::Result<()> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let mut g = crate::locked(&self.open);
        let rolled = g.as_ref().map(|(d, _)| d != &today).unwrap_or(true);
        if rolled {
            fs::create_dir_all(&self.dir)?;
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(file_name(&today)))?;
            *g = Some((today, BufWriter::new(f)));
        }
        let (_, w) = g.as_mut().expect("opened above");
        serde_json::to_writer(&mut *w, line).map_err(io::Error::other)?;
        w.write_all(b"\n")?;
        w.flush()?;
        drop(g);
        if rolled {
            // The day rolled over (or this is the first line): that is the
            // once-a-day moment to drop what is past retention.
            self.prune_now();
        }
        Ok(())
    }

    /// Remove files older than `keep_days`, counted from today's local date.
    pub fn prune_now(&self) -> usize {
        prune_dir(&self.dir, Local::now().date_naive(), self.keep_days)
    }
}

/// `YYYY-MM-DD.jsonl`.
pub fn file_name(date: &str) -> String {
    format!("{date}.jsonl")
}

/// Delete every `YYYY-MM-DD.jsonl` in `dir` whose date is more than
/// `keep_days` before `today`. Files with any other name are left alone.
/// Returns how many were removed. A missing directory removes nothing.
pub fn prune_dir(dir: &Path, today: NaiveDate, keep_days: u32) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(date) = NaiveDate::parse_from_str(stem, "%Y-%m-%d") else {
            continue;
        };
        if (today - date).num_days() > i64::from(keep_days) {
            match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(e) => tracing::debug!(path = %path.display(), "prune: {e}"),
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_is_named_by_local_date_and_lines_are_json_objects() {
        let dir = tempfile::tempdir().unwrap();
        let log = CallLog::new(dir.path(), 30);
        assert_eq!(log.dir(), dir.path().join("logs/calls"));
        let line = CallLine {
            time: CallLine::now(),
            store: "700c5bd4a969723107c1b92b83c0f1ec1497d9d4",
            id: "ygjjmvkw",
            stage: "caption",
            model: "qwen/qwen3.8-27b",
            url: "http://127.0.0.1:1215/percept/vlm",
            via: "primary",
            request_bytes: 812_344,
            status: Some(200),
            latency_ms: 9_412,
            response_bytes: 3_120,
            outcome: "recorded",
            error: None,
            finish_reason: Some("stop"),
            prompt_tokens: Some(1_402),
            completion_tokens: Some(388),
        };
        log.record(&line);
        log.record(&CallLine {
            status: None,
            outcome: "backend_down",
            error: Some("503 backend_down"),
            ..line
        });
        let today = Local::now().format("%Y-%m-%d").to_string();
        let text = fs::read_to_string(log.dir().join(file_name(&today))).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        // Column order on disk is declaration order (a parsed Value re-sorts
        // keys, so check the raw line): time first, error last.
        let raw = text.lines().next().unwrap();
        let order = [
            "\"time\"",
            "\"store\"",
            "\"id\"",
            "\"stage\"",
            "\"model\"",
            "\"url\"",
            "\"via\"",
            "\"request_bytes\"",
            "\"status\"",
            "\"latency_ms\"",
            "\"response_bytes\"",
            "\"outcome\"",
            "\"error\"",
            "\"finish_reason\"",
            "\"prompt_tokens\"",
            "\"completion_tokens\"",
        ];
        let positions: Vec<usize> = order
            .iter()
            .map(|k| raw.find(k).unwrap_or_else(|| panic!("missing key {k}")))
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "keys in declaration order: {raw}"
        );
        assert_eq!(
            lines[0].as_object().unwrap().len(),
            order.len(),
            "exactly these columns"
        );
        assert_eq!(lines[0]["status"], 200);
        assert_eq!(lines[0]["outcome"], "recorded");
        assert!(lines[0]["error"].is_null());
        assert_eq!(lines[0]["finish_reason"], "stop");
        assert_eq!(lines[0]["prompt_tokens"], 1_402);
        assert_eq!(lines[0]["completion_tokens"], 388);
        assert!(
            lines[1]["status"].is_null(),
            "no answer = null status, not 0"
        );
        assert_eq!(lines[1]["error"], "503 backend_down");
        assert!(lines[0]["time"].as_str().unwrap().contains('T'), "RFC3339");
    }

    #[test]
    fn prune_removes_only_dated_files_past_retention() {
        let dir = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        for name in [
            "2026-09-16.jsonl",
            "2026-08-17.jsonl",
            "2026-08-16.jsonl",
            "2026-01-01.jsonl",
            "notes.txt",
            "2026-08-01.log",
        ] {
            fs::write(dir.path().join(name), "x\n").unwrap();
        }
        let removed = prune_dir(dir.path(), today, 30);
        // 30 days before 2026-09-16 is 2026-08-17: kept (exactly 30 days is
        // not MORE than 30). 08-16 and 01-01 go. Non-matching names stay.
        assert_eq!(removed, 2);
        let mut left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "2026-08-01.log",
                "2026-08-17.jsonl",
                "2026-09-16.jsonl",
                "notes.txt"
            ]
        );
        assert_eq!(prune_dir(&dir.path().join("missing"), today, 30), 0);
    }

    #[test]
    fn meter_hands_measurements_across_once() {
        let m = Meter::new();
        assert!(m.take().is_none());
        m.set(CallMeta {
            url: "u".into(),
            status: Some(200),
            latency_ms: 5,
            request_bytes: 1,
            response_bytes: 2,
            ..Default::default()
        });
        assert_eq!(m.take().unwrap().status, Some(200));
        assert!(m.take().is_none());
    }
}
