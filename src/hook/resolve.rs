use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{GuidanceRoot, GuidanceRootKind, TrustRules, expand_registry_path};

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

/// Find the nearest checkout root at or above a path's canonical existing parent.
pub fn repo_root_above(path: &Path) -> Option<PathBuf> {
    let candidate = canonical_path(path)?;
    let mut directory = if candidate.is_dir() {
        candidate
    } else {
        candidate.parent()?.to_owned()
    };

    loop {
        if directory.join(".jj").exists() || directory.join(".git").exists() {
            return Some(directory);
        }
        if !directory.pop() {
            return None;
        }
    }
}

/// Find the first checkout whose configured trust rule grants guidance.
pub fn trust_rule_match(
    paths: &[PathBuf],
    trust: &TrustRules,
    probe: &mut dyn FnMut(&Path) -> Option<bool>,
) -> Option<Match> {
    if trust.is_empty() {
        return None;
    }
    // Trust roots are tilde-expanded at config load but can be symlinked; compare
    // canonical paths when possible so a real checkout under one is not missed.
    let trusted_roots = trust
        .roots
        .iter()
        .map(|configured_root| {
            std::fs::canonicalize(configured_root).unwrap_or_else(|_| configured_root.clone())
        })
        .collect::<Vec<_>>();
    for path in paths {
        let Some(candidate) = canonical_path(path) else {
            continue;
        };
        let Some(root) = repo_root_above(&candidate) else {
            continue;
        };
        let under_trusted_root = trusted_roots
            .iter()
            .any(|trusted_root| root.strip_prefix(trusted_root).is_ok());
        if under_trusted_root
            || (!trust.owners.is_empty() && probe(&root).is_some_and(|verdict| verdict))
        {
            return Some(Match {
                repo: GuidanceRoot {
                    name: guidance_name(&root),
                    root,
                    kind: GuidanceRootKind::Trusted,
                },
                candidate,
            });
        }
    }
    None
}

/// Name a checkout, using the parent directory for the conventional `<name>/default` layout.
pub(crate) fn guidance_name(root: &Path) -> String {
    let last = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    if last == "default" {
        return root
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or(last)
            .to_owned();
    }
    last.to_owned()
}

/// Extract the forge owner from an authority-delimited `<owner>/<repository>` remote path.
pub(crate) fn url_owner(url: &str) -> Option<&str> {
    let (_, path) = remote_authority_and_path(url)?;
    let (owner, repository) = path.split_once('/')?;
    (!owner.is_empty() && !repository.is_empty()).then_some(owner)
}

pub(crate) fn remote_authority_and_path(url: &str) -> Option<(&str, &str)> {
    let url = url.trim_end_matches('/');
    if let Some((_, authority_and_path)) = url.split_once("://") {
        return authority_and_path.split_once('/');
    }
    let (authority, path) = url.split_once(':')?;
    authority.contains('@').then_some((authority, path))
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

    use crate::config::{GuidanceRoot, GuidanceRootKind, TrustRules};

    use super::{argument_paths, managed_repo_for, repo_root_above, trust_rule_match};

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

    #[test]
    fn a_repo_under_a_trust_root_is_a_trusted_guidance_root() {
        // Given: a workspace-shaped checkout under a trusted subtree, never registered.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agent-c/platform/default");
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "rules\n").unwrap();
        let trust = TrustRules {
            roots: vec![dir.path().join("agent-c")],
            owners: vec![],
        };
        let mut probe = |_: &Path| None;

        // When: a file inside it is resolved with no owner probe available.
        let hit = trust_rule_match(&[root.join("AGENTS.md")], &trust, &mut probe)
            .expect("a root rule needs no probe");

        // Then: it is trusted, named for its parent because the leaf is `default`.
        assert_eq!(hit.repo.kind, GuidanceRootKind::Trusted);
        assert_eq!(hit.repo.name, "platform");
        assert_eq!(hit.repo.root, root.canonicalize().unwrap());
    }

    #[test]
    fn a_trust_root_skips_owner_probing_even_when_owners_are_configured() -> anyhow::Result<()> {
        // Given: a checkout matches a trusted root while owner rules are also configured.
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("trusted/repo");
        std::fs::create_dir_all(root.join(".jj"))?;
        let trust = TrustRules {
            roots: vec![directory.path().join("trusted")],
            owners: vec!["also-trusted".to_owned()],
        };
        let mut probes = 0;
        let mut probe = |_: &Path| {
            probes += 1;
            Some(true)
        };

        // When: a file under the trusted root is resolved.
        let hit = trust_rule_match(&[root.join("src/lib.rs")], &trust, &mut probe);

        // Then: root trust succeeds without starting an owner probe.
        assert!(hit.is_some());
        assert_eq!(probes, 0);
        Ok(())
    }

    #[test]
    fn a_repo_matching_a_trusted_owner_is_found_through_the_probe() {
        // Given: an unregistered checkout whose owner probe accepts its root.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("elsewhere/tool");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let trust = TrustRules {
            roots: vec![],
            owners: vec!["someone".to_owned()],
        };
        let mut asked = Vec::new();
        let mut probe = |path: &Path| {
            asked.push(path.to_owned());
            Some(true)
        };

        // When: the candidate is resolved through the owner probe.
        let hit = trust_rule_match(&[root.join("src/lib.rs")], &trust, &mut probe);

        // Then: the owner rule trusts this one repository root once.
        assert!(hit.is_some());
        assert_eq!(asked.len(), 1, "the probe is asked once per root");

        let mut deny = |_: &Path| Some(false);
        assert!(trust_rule_match(&[root.join("src/lib.rs")], &trust, &mut deny).is_none());
    }

    #[test]
    fn a_sibling_of_a_trust_root_sharing_its_name_prefix_is_outside() {
        // Given: `agent-c-2`, a sibling whose string prefix matches a trusted root.
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("agent-c-2/repo");
        std::fs::create_dir_all(outside.join(".jj")).unwrap();
        let trust = TrustRules {
            roots: vec![dir.path().join("agent-c")],
            owners: vec![],
        };
        let mut probe = |_: &Path| None;

        // When: a file under the sibling is resolved.
        let hit = trust_rule_match(&[outside.join("x")], &trust, &mut probe);

        // Then: component containment rejects the string-prefix sibling.
        assert!(hit.is_none());
    }

    #[test]
    fn a_repo_self_declaring_a_trusted_owner_is_a_trusted_guidance_root() {
        // Given: a checkout outside every trusted root that claims a trusted owner.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("untrusted/repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let trust = TrustRules {
            roots: vec![dir.path().join("trusted")],
            owners: vec!["trusted-owner".to_owned()],
        };
        // This is the accepted, documented trade-off: remotes are self-declared,
        // and the grant is guidance-as-data only, never fork-command access.
        let mut probe = |_: &Path| Some(true);

        // When: the owner-based rule is evaluated.
        let hit = trust_rule_match(&[root.join("AGENTS.md")], &trust, &mut probe)
            .expect("the self-declared owner is accepted for guidance");

        // Then: it gets only the trusted guidance classification.
        assert_eq!(hit.repo.kind, GuidanceRootKind::Trusted);
    }

    #[test]
    fn an_unrelated_path_does_not_hide_a_later_trust_match() {
        // Given: an unresolved path before a repository under a trusted root.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("trusted/repo");
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        let trust = TrustRules {
            roots: vec![dir.path().join("trusted")],
            owners: vec![],
        };
        let mut probe = |_: &Path| None;

        // When: both the unrelated path and trusted repository path are considered.
        let hit = trust_rule_match(
            &[dir.path().join("outside/missing"), root.join("AGENTS.md")],
            &trust,
            &mut probe,
        );

        // Then: resolution continues until it finds the trusted repository.
        assert_eq!(
            hit.map(|matched| matched.repo.root),
            Some(root.canonicalize().unwrap())
        );
    }

    #[test]
    fn a_repo_root_is_found_above_a_missing_descendant() {
        // Given: an existing repository root and a missing descendant path.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join(".jj")).unwrap();

        // When: its repository root is located.
        let found = repo_root_above(&root.join("src/new/file.rs"));

        // Then: the canonical checkout root is returned.
        assert_eq!(found, Some(root.canonicalize().unwrap()));
    }
}
