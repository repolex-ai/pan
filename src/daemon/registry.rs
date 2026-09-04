//! Store registry — turns each configured directory into (id, store root).
//!
//! Two kinds of entry, told apart by what is on disk, never by a flag:
//!
//! - A **soul repo** (a git repository): the store lives at `<repo>/.pan`
//!   (gitignored — media is never git history) and its id is the repo's
//!   genesis SHA, the same identity git-lex, Horae and Syrinx already use
//!   for that soul. Declared once by git; pand derives, never assigns.
//! - A **bare store directory**: the store IS the directory and its id comes
//!   from its own `pan.yml` (`storage_id`, defaulting to "default").
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

pub fn resolve_one(declared: &Path) -> Result<StoreEntry> {
    if declared.join(".git").exists() {
        let sha = genesis_sha(declared)
            .with_context(|| format!("{} is a git repository but its genesis SHA could not be read", declared.display()))?;
        return Ok(StoreEntry {
            id: sha,
            root: declared.join(".pan"),
            declared: declared.to_path_buf(),
            is_repo: true,
        });
    }
    let cfg = crate::config::PanConfig::load(declared)?;
    Ok(StoreEntry {
        id: cfg.storage_id,
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
    fn bare_dir_uses_its_pan_yml_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pan.yml"), "storage_id: scratch\n").unwrap();
        let e = resolve_one(dir.path()).unwrap();
        assert_eq!(e.id, "scratch");
        assert_eq!(e.root, dir.path());
        assert!(!e.is_repo);
    }

    #[test]
    fn repo_uses_declared_genesis_and_dot_pan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".lex")).unwrap();
        std::fs::write(dir.path().join(".lex/repo.yml"), "kit: soul\ngenesis_sha: abc123\n").unwrap();
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
