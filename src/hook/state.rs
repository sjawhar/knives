use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::store::StoreLock;

const SESSIONS_DIRECTORY: &str = "hook-sessions";
const PRUNE_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFlags {
    #[serde(default)]
    pub guided: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DiskState {
    #[serde(default)]
    repos: HashMap<PathBuf, RepoFlags>,
    #[serde(default)]
    seen_notices: HashMap<PathBuf, BTreeSet<String>>,
}

#[derive(Debug, Default)]
pub struct SessionState {
    repos: HashMap<PathBuf, RepoFlags>,
    seen_notices: HashMap<PathBuf, BTreeSet<String>>,
}

impl SessionState {
    pub fn load(home: &Path, harness: &str, session_id: &str) -> Self {
        let directory = session_directory(home);
        Self::load_path(&state_path(&directory, harness, session_id))
    }

    pub fn repo(&self, root: &Path) -> RepoFlags {
        self.repos.get(root).copied().unwrap_or_default()
    }

    pub fn update(
        home: &Path,
        harness: &str,
        session_id: &str,
        apply: impl FnOnce(&mut Self),
    ) -> anyhow::Result<Self> {
        let directory = session_directory(home);
        let path = state_path(&directory, harness, session_id);
        let _lock = StoreLock::acquire(&path)?;
        let mut state = Self::load_path(&path);
        apply(&mut state);
        state.persist(&directory, &path)?;
        prune_stale_siblings(&directory);
        Ok(state)
    }

    pub fn mark_guided(&mut self, root: &Path) {
        self.repos.entry(root.to_owned()).or_default().guided = true;
    }

    pub fn record_notice(&mut self, root: &Path, digest: String) {
        self.seen_notices
            .entry(root.to_owned())
            .or_default()
            .insert(digest);
    }

    pub fn notice_seen(&self, root: &Path, digest: &str) -> bool {
        self.seen_notices
            .get(root)
            .is_some_and(|notices| notices.contains(digest))
    }

    pub fn clear(&mut self) {
        self.repos.clear();
        self.seen_notices.clear();
    }

    pub fn delete(home: &Path, harness: &str, session_id: &str) {
        let directory = session_directory(home);
        let _ = std::fs::remove_file(state_path(&directory, harness, session_id));
    }

    fn load_path(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<DiskState>(&text).ok())
            .map_or_else(Self::default, |disk| Self {
                repos: disk.repos,
                seen_notices: disk.seen_notices,
            })
    }

    fn persist(&self, directory: &Path, path: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(
            &mut temporary,
            &DiskState {
                repos: self.repos.clone(),
                seen_notices: self.seen_notices.clone(),
            },
        )?;
        temporary.write_all(b"\n")?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

fn session_directory(home: &Path) -> PathBuf {
    home.join(SESSIONS_DIRECTORY)
}

fn state_path(directory: &Path, harness: &str, session_id: &str) -> PathBuf {
    directory.join(format!(
        "{harness}-{}.json",
        sanitize_session_id(session_id)
    ))
}

fn sanitize_session_id(session_id: &str) -> String {
    session_id
        .chars()
        .map(|character| match character {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => character,
            _ => '-',
        })
        .collect()
}

fn prune_stale_siblings(directory: &Path) {
    let stale_before = SystemTime::now() - PRUNE_AGE;
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = std::fs::metadata(entry.path()) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if metadata.is_file() && modified < stale_before {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::SessionState;

    #[test]
    fn a_fresh_session_has_no_guidance_or_notices() {
        let home = tempfile::tempdir().unwrap();
        let root = Path::new("/some/repo");
        let state = SessionState::load(home.path(), "claude-code", "s1");
        assert!(!state.repo(root).guided);
        assert!(!state.notice_seen(root, "digest"));
    }

    #[test]
    fn notices_and_guidance_survive_update_and_reload() {
        let home = tempfile::tempdir().unwrap();
        let root = Path::new("/some/repo");
        SessionState::update(home.path(), "claude-code", "s1", |state| {
            state.record_notice(root, "digest".to_owned());
            state.mark_guided(root);
        })
        .unwrap();
        let reloaded = SessionState::load(home.path(), "claude-code", "s1");
        assert!(reloaded.notice_seen(root, "digest"));
        assert!(reloaded.repo(root).guided);
    }

    #[test]
    fn notice_digests_accumulate_for_one_root() {
        let home = tempfile::tempdir().unwrap();
        let root = Path::new("/r");
        SessionState::update(home.path(), "claude-code", "s1", |state| {
            state.record_notice(root, "first".to_owned());
        })
        .unwrap();
        let state = SessionState::update(home.path(), "claude-code", "s1", |state| {
            state.record_notice(root, "second".to_owned());
        })
        .unwrap();
        assert!(state.notice_seen(root, "first"));
        assert!(state.notice_seen(root, "second"));
    }

    #[test]
    fn update_rereads_the_latest_disk_state_under_the_lock() {
        // A stale in-memory copy must not clobber notices another process wrote.
        // The closure API makes this structural: each update re-reads before applying.
        let home = tempfile::tempdir().unwrap();
        SessionState::update(home.path(), "claude-code", "s1", |state| {
            state.record_notice(Path::new("/a"), "first".to_owned());
        })
        .unwrap();
        SessionState::update(home.path(), "claude-code", "s1", |state| {
            state.record_notice(Path::new("/b"), "second".to_owned());
        })
        .unwrap();
        let state = SessionState::load(home.path(), "claude-code", "s1");
        assert!(
            state.notice_seen(Path::new("/a"), "first"),
            "first write survives the second"
        );
        assert!(state.notice_seen(Path::new("/b"), "second"));
    }

    #[test]
    fn sessions_do_not_share_notices() {
        let home = tempfile::tempdir().unwrap();
        let root = Path::new("/r");
        SessionState::update(home.path(), "claude-code", "s1", |state| {
            state.record_notice(root, "digest".to_owned());
        })
        .unwrap();
        let other = SessionState::load(home.path(), "claude-code", "s2");
        assert!(!other.notice_seen(root, "digest"));
    }

    #[test]
    fn clear_forgets_everything() {
        let home = tempfile::tempdir().unwrap();
        let root = Path::new("/r");
        SessionState::update(home.path(), "claude-code", "s1", |state| {
            state.record_notice(root, "digest".to_owned());
            state.mark_guided(root);
        })
        .unwrap();
        SessionState::update(home.path(), "claude-code", "s1", SessionState::clear).unwrap();
        let cleared = SessionState::load(home.path(), "claude-code", "s1");
        assert!(!cleared.notice_seen(root, "digest"));
        assert!(!cleared.repo(root).guided);
    }

    #[test]
    fn a_corrupt_state_file_loads_as_empty() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("hook-sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("claude-code-s1.json"), b"{not json").unwrap();
        let state = SessionState::load(home.path(), "claude-code", "s1");
        assert!(!state.notice_seen(Path::new("/r"), "digest"));
    }

    #[test]
    fn session_ids_cannot_escape_the_state_directory() {
        let home = tempfile::tempdir().unwrap();
        SessionState::update(home.path(), "claude-code", "../../evil", |state| {
            state.mark_guided(Path::new("/r"));
        })
        .unwrap();
        // Whatever the name became, it is inside hook-sessions/ (lock files may sit beside it).
        assert!(!home.path().join("../..").join("evil.json").exists());
        assert!(
            std::fs::read_dir(home.path().join("hook-sessions"))
                .unwrap()
                .count()
                >= 1
        );
    }

    #[test]
    fn stale_sibling_files_are_pruned_on_update() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("hook-sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("claude-code-old.json");
        std::fs::write(&stale, b"{}").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();
        SessionState::update(home.path(), "claude-code", "s1", |state| {
            state.mark_guided(Path::new("/r"));
        })
        .unwrap();
        assert!(!stale.exists());
    }
}

#[cfg(test)]
#[path = "state_regression_tests.rs"]
mod regression_tests;
