use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::store::StoreLock;

const SESSIONS_DIRECTORY: &str = "hook-sessions";
const PRUNE_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFlags {
    pub noticed: bool,
    pub guided: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DiskState {
    #[serde(default)]
    repos: HashMap<PathBuf, RepoFlags>,
}

#[derive(Debug, Default)]
pub struct SessionState {
    repos: HashMap<PathBuf, RepoFlags>,
}

impl SessionState {
    pub fn load(home: &Path, harness: &str, session_id: &str) -> Self {
        Self::load_path(&state_path(home, harness, session_id))
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
        let path = state_path(home, harness, session_id);
        let _lock = StoreLock::acquire(&path)?;
        let mut state = Self::load_path(&path);
        apply(&mut state);
        state.persist(&path)?;
        prune_stale_siblings(path.parent().unwrap_or(home))?;
        Ok(state)
    }

    pub fn mark(&mut self, root: &Path, noticed: bool, guided: bool) {
        let flags = self.repos.entry(root.to_owned()).or_default();
        flags.noticed |= noticed;
        flags.guided |= guided;
    }

    pub fn clear(&mut self) {
        self.repos.clear();
    }

    pub fn delete(home: &Path, harness: &str, session_id: &str) {
        let _ = std::fs::remove_file(state_path(home, harness, session_id));
    }

    fn load_path(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<DiskState>(&text).ok())
            .map_or_else(Self::default, |disk| Self { repos: disk.repos })
    }

    fn persist(&self, path: &Path) -> anyhow::Result<()> {
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(
            &mut temporary,
            &DiskState {
                repos: self.repos.clone(),
            },
        )?;
        temporary.write_all(b"\n")?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

fn state_path(home: &Path, harness: &str, session_id: &str) -> PathBuf {
    home.join(SESSIONS_DIRECTORY).join(format!(
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

fn prune_stale_siblings(directory: &Path) -> anyhow::Result<()> {
    let stale_before = SystemTime::now() - PRUNE_AGE;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() && metadata.modified()? < stale_before {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::SessionState;

    #[test]
    fn a_fresh_session_has_no_flags() {
        let home = tempfile::tempdir().unwrap();
        let state = SessionState::load(home.path(), "claude-code", "s1");
        let flags = state.repo(Path::new("/some/repo"));
        assert!(!flags.noticed);
        assert!(!flags.guided);
    }

    #[test]
    fn marks_survive_update_and_reload() {
        let home = tempfile::tempdir().unwrap();
        SessionState::update(home.path(), "claude-code", "s1", |s| {
            s.mark(Path::new("/some/repo"), true, false);
        })
        .unwrap();
        let reloaded = SessionState::load(home.path(), "claude-code", "s1");
        assert!(reloaded.repo(Path::new("/some/repo")).noticed);
        assert!(!reloaded.repo(Path::new("/some/repo")).guided);
    }

    #[test]
    fn mark_merges_rather_than_overwrites() {
        let home = tempfile::tempdir().unwrap();
        SessionState::update(home.path(), "claude-code", "s1", |s| {
            s.mark(Path::new("/r"), true, false);
        })
        .unwrap();
        let state = SessionState::update(home.path(), "claude-code", "s1", |s| {
            s.mark(Path::new("/r"), false, true);
        })
        .unwrap();
        let flags = state.repo(Path::new("/r"));
        assert!(flags.noticed && flags.guided);
    }

    #[test]
    fn update_rereads_the_latest_disk_state_under_the_lock() {
        // A stale in-memory copy must not clobber flags another process wrote.
        // The closure API makes this structural: each update re-reads before applying.
        let home = tempfile::tempdir().unwrap();
        SessionState::update(home.path(), "claude-code", "s1", |s| {
            s.mark(Path::new("/a"), true, true);
        })
        .unwrap();
        SessionState::update(home.path(), "claude-code", "s1", |s| {
            s.mark(Path::new("/b"), true, true);
        })
        .unwrap();
        let state = SessionState::load(home.path(), "claude-code", "s1");
        assert!(
            state.repo(Path::new("/a")).noticed,
            "first write survives the second"
        );
        assert!(state.repo(Path::new("/b")).noticed);
    }

    #[test]
    fn sessions_do_not_share_state() {
        let home = tempfile::tempdir().unwrap();
        SessionState::update(home.path(), "claude-code", "s1", |s| {
            s.mark(Path::new("/r"), true, true);
        })
        .unwrap();
        let other = SessionState::load(home.path(), "claude-code", "s2");
        assert!(!other.repo(Path::new("/r")).noticed);
    }

    #[test]
    fn clear_forgets_everything() {
        let home = tempfile::tempdir().unwrap();
        SessionState::update(home.path(), "claude-code", "s1", |s| {
            s.mark(Path::new("/r"), true, true);
        })
        .unwrap();
        SessionState::update(home.path(), "claude-code", "s1", SessionState::clear).unwrap();
        assert!(
            !SessionState::load(home.path(), "claude-code", "s1")
                .repo(Path::new("/r"))
                .noticed
        );
    }

    #[test]
    fn a_corrupt_state_file_loads_as_empty() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("hook-sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("claude-code-s1.json"), b"{not json").unwrap();
        let state = SessionState::load(home.path(), "claude-code", "s1");
        assert!(!state.repo(Path::new("/r")).noticed);
    }

    #[test]
    fn session_ids_cannot_escape_the_state_directory() {
        let home = tempfile::tempdir().unwrap();
        SessionState::update(home.path(), "claude-code", "../../evil", |s| {
            s.mark(Path::new("/r"), true, true);
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
        filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(old)).unwrap();
        SessionState::update(home.path(), "claude-code", "s1", |s| {
            s.mark(Path::new("/r"), true, true);
        })
        .unwrap();
        assert!(!stale.exists());
    }
}
