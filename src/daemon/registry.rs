//! Store registry — turns each configured directory into (id, store root).
//!
//! Two kinds of entry, told apart by what is on disk, never by a flag:
//!
//! - A **soul repo** (a git repository): the store lives at `<repo>/.pan`
//!   (gitignored — media is never git history) and its id is the FIRST SIX
//!   CHARACTERS of the repo's genesis SHA, the same identity git-lex, Horae
//!   and Syrinx already use for that soul. Declared once by git; pand derives,
//!   never assigns.
//! - A **bare store directory**: the store IS the directory and its id comes
//!   from its own `pan.yml` (`storage_id`, defaulting to "default"), cut to
//!   the same six characters.
//!
//! SIX CHARACTERS, EVERYWHERE (goodlux, 2026-09-18). The id in the routes, in
//! `<pan/Store/…>`, in the Instance's endpoint, in the log lines and in the
//! media folder name is one value: `700c5b`. A full forty-character hash in an
//! identifier is the fault Pan was rewritten to undo, and it is never the id
//! again. `STORE_ID_LEN` is the one place the length is written down.
//!
//! Ids must be unique across the machine — two entries resolving to one id is
//! a configuration error, reported at start.

use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreEntry {
    pub id: String,
    /// The store root (`<repo>/.pan` or the bare directory).
    pub root: PathBuf,
    /// The configured path as written (what `default:` is matched against).
    pub declared: PathBuf,
    pub is_repo: bool,
}

pub fn resolve_all(declared: &[PathBuf]) -> Result<Vec<StoreEntry>> {
    let mut out = Vec::with_capacity(declared.len());
    let mut seen: HashSet<String> = HashSet::new();
    for p in declared {
        let e = resolve_one(p)?;
        if !seen.insert(e.id.clone()) {
            return Err(anyhow!(
                "two configured stores share the id {}: check the stores list in the pand config",
                e.id
            ));
        }
        out.push(e);
    }
    Ok(out)
}

/// How many characters of the declared identity are the store id.
pub const STORE_ID_LEN: usize = 6;

/// The store id: the first [`STORE_ID_LEN`] characters of whatever declared it
/// — the genesis SHA of a soul repo, or a bare directory's `storage_id`.
/// Characters, not bytes, so a non-ASCII id cannot be cut mid-character.
pub fn store_id(declared_id: &str) -> Result<String> {
    let id: String = declared_id
        .trim()
        .chars()
        .take(STORE_ID_LEN)
        .collect::<String>()
        .to_lowercase();
    if id.is_empty() {
        return Err(anyhow!("a store id cannot be empty"));
    }
    Ok(id)
}

pub fn resolve_one(declared: &Path) -> Result<StoreEntry> {
    if declared.join(".git").exists() {
        let sha = genesis_sha(declared).with_context(|| {
            format!(
                "{} is a git repository but its genesis SHA could not be read",
                declared.display()
            )
        })?;
        return Ok(StoreEntry {
            id: store_id(&sha)?,
            root: declared.join(".pan"),
            declared: declared.to_path_buf(),
            is_repo: true,
        });
    }
    let cfg = crate::config::PanConfig::load(declared)?;
    Ok(StoreEntry {
        id: store_id(&cfg.storage_id)?,
        root: declared.to_path_buf(),
        declared: declared.to_path_buf(),
        is_repo: false,
    })
}

/// The repo's genesis SHA: `.lex/repo.yml` `genesis_sha:` is the declared
/// authority (git-lex writes it); git itself is the recompute of last resort.
pub fn genesis_sha(repo: &Path) -> Result<String> {
    let repo_yml = repo.join(".lex").join("repo.yml");
    if let Ok(raw) = std::fs::read_to_string(&repo_yml) {
        for line in raw.lines() {
            if let Some(v) = line.trim_start().strip_prefix("genesis_sha:") {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Ok(v.to_string());
                }
            }
        }
    }
    let out = Command::new("git")
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .current_dir(repo)
        .output()
        .context("run git rev-list")?;
    if !out.status.success() {
        return Err(anyhow!("git rev-list failed in {}", repo.display()));
    }
    let sha = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap_or("")
        .trim()
        .to_string();
    if sha.is_empty() {
        return Err(anyhow!("no commits in {}", repo.display()));
    }
    Ok(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_dir_uses_its_pan_yml_id_cut_to_six() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pan.yml"), "storage_id: scratchpad\n").unwrap();
        let e = resolve_one(dir.path()).unwrap();
        assert_eq!(e.id, "scratc");
        assert_eq!(e.root, dir.path());
        assert!(!e.is_repo);
    }

    #[test]
    fn the_all_zeros_bare_store_is_six_zeros() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pan.yml"),
            "storage_id: \"0000000000000000000000000000000000000000\"\n",
        )
        .unwrap();
        assert_eq!(resolve_one(dir.path()).unwrap().id, "000000");
    }

    /// The whole point: a forty-character genesis SHA becomes a six-character
    /// store id at the edge, so nothing downstream can put the long form in a
    /// route, an IRI or a log line (goodlux, 2026-09-18).
    #[test]
    fn a_genesis_sha_is_cut_to_six_characters() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".lex")).unwrap();
        std::fs::write(
            dir.path().join(".lex/repo.yml"),
            "kit: soul\ngenesis_sha: 700c5bd4a969723107c1b92b83c0f1ec1497d9d4\n",
        )
        .unwrap();
        let e = resolve_one(dir.path()).unwrap();
        assert_eq!(e.id, "700c5b");
        assert_eq!(e.id.chars().count(), STORE_ID_LEN);
    }

    #[test]
    fn an_upper_case_id_is_lowered() {
        assert_eq!(store_id("700C5BD4A9").unwrap(), "700c5b");
    }

    #[test]
    fn an_empty_id_is_refused() {
        assert!(store_id("   ").is_err());
    }

    #[test]
    fn repo_uses_declared_genesis_and_dot_pan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".lex")).unwrap();
        std::fs::write(
            dir.path().join(".lex/repo.yml"),
            "kit: soul\ngenesis_sha: abc123\n",
        )
        .unwrap();
        let e = resolve_one(dir.path()).unwrap();
        assert_eq!(e.id, "abc123");
        assert_eq!(e.root, dir.path().join(".pan"));
        assert!(e.is_repo);
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("pan.yml"), "storage_id: same\n").unwrap();
        std::fs::write(b.path().join("pan.yml"), "storage_id: same\n").unwrap();
        assert!(resolve_all(&[a.path().to_path_buf(), b.path().to_path_buf()]).is_err());
    }
}
