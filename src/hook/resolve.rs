use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{GuidanceRoot, expand_registry_path};

/// A named path and the registered repository that contains it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub repo: GuidanceRoot,
    pub candidate: PathBuf,
}

/// Extract file paths directly named by a tool invocation.
///
/// A call's working directory does not name repository content; treating it as a
/// fallback spends guidance on pathless commands before a meaningful file access.
pub fn argument_paths(_tool: &str, args: &Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for field in ["path", "filePath", "file_path", "notebook_path"] {
        if let Some(path) = args.get(field).and_then(Value::as_str) {
            paths.push(expand_tilde(Path::new(path)));
        }
    }
    if let Some(command) = args.get("command").and_then(Value::as_str) {
        paths.extend(
            command
                .split(|character: char| {
                    character.is_whitespace() || matches!(character, '\'' | '"')
                })
                .filter(|token| {
                    token
                        .strip_prefix('/')
                        .is_some_and(|suffix| !suffix.is_empty())
                        || token
                            .strip_prefix("~/")
                            .is_some_and(|suffix| !suffix.is_empty())
                })
                .map(|token| expand_tilde(Path::new(token))),
        );
    }
    paths
}

/// Find the first named path inside a guidance root, preferring nested roots.
pub fn managed_repo_for(paths: &[PathBuf], roots: &[GuidanceRoot]) -> Option<Match> {
    for path in paths {
        let Some(candidate) = canonical_path(path) else {
            continue;
        };
        let repo = roots
            .iter()
            .filter(|root| candidate.strip_prefix(&root.root).is_ok())
            .max_by_key(|root| root.root.components().count());
        if let Some(repo) = repo {
            return Some(Match {
                repo: repo.clone(),
                candidate,
            });
        }
    }
    None
}

fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        return expand_registry_path(path, Path::new(""));
    }
    path.to_owned()
}

/// Canonicalize through the nearest existing parent so new leaves remain attributable.
fn canonical_path(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    canonical_existing_parent(&absolute)
}

fn canonical_existing_parent(path: &Path) -> Option<PathBuf> {
    match fs::canonicalize(path) {
        Ok(canonical) => Some(canonical),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let parent = path.parent()?;
            if parent == path {
                return None;
            }
            let file_name = path.file_name()?;
            canonical_existing_parent(parent).map(|canonical| canonical.join(file_name))
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::config::{GuidanceRoot, GuidanceRootKind};

    use super::{argument_paths, managed_repo_for};

    #[test]
    fn command_strings_yield_absolute_and_home_paths_only() {
        // Given: a command containing absolute, home-relative, and relative paths.
        let args = serde_json::json!({
            "command": "git -C /tmp/x/repo log && cat ~/notes.md && cat relative/file"
        });

        // When: its named paths are extracted.
        let paths = argument_paths("bash", &args);

        // Then: only paths whose locations need no assumed directory are returned.
        assert!(paths.iter().any(|path| path == Path::new("/tmp/x/repo")));
        assert!(paths.iter().any(|path| {
            path.is_absolute() && path.ends_with("notes.md") && !path.starts_with("~")
        }));
        assert_eq!(
            paths.len(),
            2,
            "a relative path cannot be resolved without assuming a directory"
        );
    }

    #[test]
    fn bare_root_and_home_directory_command_tokens_are_not_paths() {
        // Given: commands containing only root or home-directory tokens.
        for command in ["ls /", "cd ~/"] {
            let args = serde_json::json!({"command": command});

            // When: the command paths are extracted.
            let paths = argument_paths("bash", &args);

            // Then: a path requires content after its absolute or home prefix.
            assert!(paths.is_empty(), "{command}");
        }
    }

    #[test]
    fn a_working_directory_does_not_count_as_a_named_path() {
        // Given: a tool invocation with only execution-directory metadata.
        let args = serde_json::json!({"cwd": "/tmp/x", "workdir": "/tmp/y"});

        // When: argument paths are extracted.
        let paths = argument_paths("bash", &args);

        // Then: directory metadata cannot attribute a pathless invocation.
        assert_eq!(paths, Vec::<PathBuf>::new());
    }

    #[test]
    fn file_path_and_snake_case_variants_are_read() {
        // Given: every supported direct path field.
        for key in ["path", "filePath", "file_path", "notebook_path"] {
            let args = serde_json::json!({key: "/tmp/somewhere"});

            // When: its argument paths are read.
            let paths = argument_paths("read", &args);

            // Then: the named path is preserved.
            assert_eq!(paths, vec![PathBuf::from("/tmp/somewhere")], "{key}");
        }
    }

    #[test]
    fn a_sibling_directory_sharing_the_root_name_is_outside() {
        // Given: a root and a sibling with the root name as a string prefix.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let sibling = dir.path().join("repo-sibling/file");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        std::fs::write(&sibling, b"x").unwrap();
        let roots = vec![GuidanceRoot {
            name: "repo".into(),
            root: root.canonicalize().unwrap(),
            kind: GuidanceRootKind::Managed,
        }];

        // When: the sibling file is resolved.
        let match_ = managed_repo_for(&[sibling], &roots);

        // Then: component containment rejects it.
        assert!(match_.is_none());
    }

    #[test]
    fn the_longest_root_wins_for_nested_checkouts() {
        // Given: an inner checkout nested in an outer checkout.
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let roots = vec![
            GuidanceRoot {
                name: "outer".into(),
                root: outer.canonicalize().unwrap(),
                kind: GuidanceRootKind::Managed,
            },
            GuidanceRoot {
                name: "inner".into(),
                root: inner.canonicalize().unwrap(),
                kind: GuidanceRootKind::Managed,
            },
        ];

        // When: a nonexistent child of the inner checkout is resolved.
        let hit = managed_repo_for(&[inner.join("file.txt")], &roots).unwrap();

        // Then: the more specific root is selected.
        assert_eq!(hit.repo.name, "inner");
    }

    #[test]
    fn nonexistent_leaves_resolve_through_their_existing_parent() {
        // Given: an existing root and a not-yet-created descendant.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let roots = vec![GuidanceRoot {
            name: "r".into(),
            root: root.clone(),
            kind: GuidanceRootKind::Managed,
        }];

        // When: the nonexistent descendant is resolved.
        let hit = managed_repo_for(&[root.join("not/yet/created.txt")], &roots).unwrap();

        // Then: its existing parent identifies the root.
        assert_eq!(hit.repo.name, "r");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_path_that_escapes_the_root_is_outside() {
        // Given: a symlink under the root that points to an outside directory.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("file"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let roots = vec![GuidanceRoot {
            name: "root".into(),
            root: root.canonicalize().unwrap(),
            kind: GuidanceRootKind::Managed,
        }];

        // When: the file is named through the symlink.
        let match_ = managed_repo_for(&[root.join("escape/file")], &roots);

        // Then: canonicalization prevents the escape from being attributed to the root.
        assert!(match_.is_none());
    }
}
