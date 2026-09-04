use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::bind;
use crate::config::{Registry, expand_registry_path};
use crate::ids::RepoName;

/// A touched path inside a repository, and what the registry says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The nearest repository root containing the touched path (a jj workspace
    /// is its own root). Guidance walks this tree; session-state keys use it.
    pub root: PathBuf,
    /// The touched path, canonicalised.
    pub candidate: PathBuf,
    /// The registry name when the *checkout's* `upstream` matches an entry.
    pub managed: Option<RepoName>,
    /// Whether `[trust]` grants guidance for this checkout (any remote, or `roots`).
    pub trusted: bool,
}

impl Match {
    /// The registry name when managed, else `guidance_name(&self.root)`.
    pub fn name(&self) -> String {
        self.managed
            .as_ref()
            .map_or_else(|| guidance_name(&self.root), ToString::to_string)
    }

    pub const fn is_managed(&self) -> bool {
        self.managed.is_some()
    }
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

/// The nearest repository root above a path that may not exist yet: a file
/// about to be written has no canonical form, but its repository does.
pub(crate) fn nearest_root(path: &Path) -> Option<PathBuf> {
    let candidate = canonical_path(path)?;
    bind::nearest_root(existing_ancestor(&candidate)?)
}

fn existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| ancestor.exists())
}

/// The first touched path inside a repository, and what the registry says about it.
///
/// Both facts are decided from the remotes [`bind::remotes`] reads for the
/// nearest root: its own git configuration, or its jj store's git backend; a
/// `.jj/repo` pointer to another checkout counts only when that checkout
/// vouches for the root as its workspace, and a `.jj` an enclosing git
/// repository tracks is content, since a clone can carry any pointer or store
/// it likes. Remotes that cannot be read are reported on stderr and contribute
/// no facts; a `roots` rule is decided from the path alone, so it holds with
/// no readable repository at all.
pub fn match_checkout(paths: &[PathBuf], registry: &Registry) -> Option<Match> {
    for path in paths {
        let Some(candidate) = canonical_path(path) else {
            continue;
        };
        let Some(root) = existing_ancestor(&candidate).and_then(bind::nearest_root) else {
            continue;
        };
        let under_root = registry.trust.contains_root(&root);
        let remotes = bind::remotes(&root).unwrap_or_else(|error| {
            eprintln!("knives hook: {error}");
            BTreeMap::new()
        });
        let managed = remotes
            .get("upstream")
            .and_then(|upstream| bind::entry_for(registry, upstream))
            .map(|(name, _)| name);
        let trusted = under_root || registry.trust.grants_by_remotes(&remotes);
        if managed.is_some() || trusted {
            return Some(Match {
                root,
                candidate,
                managed,
                trusted,
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

    use crate::config::{Registry, RepoEntry, TrustRules};
    use crate::ids::RepoName;

    use super::{argument_paths, match_checkout};

    fn registry(entries: &[(&str, &str)], trust: TrustRules) -> Registry {
        Registry {
            repos: entries
                .iter()
                .map(|(name, upstream)| {
                    (
                        (*name).to_owned(),
                        RepoEntry::new(*upstream, "https://forge.invalid/ours/fork"),
                    )
                })
                .collect(),
            trust,
        }
    }

    fn trusting_owner(owner: &str) -> TrustRules {
        TrustRules {
            owners: vec![owner.to_owned()],
            ..TrustRules::default()
        }
    }

    fn trusting_root(root: &Path) -> TrustRules {
        TrustRules {
            roots: vec![root.to_owned()],
            ..TrustRules::default()
        }
    }

    /// A git repository at `root` declaring `remotes`: what the hook reads.
    fn git_repository(root: &Path, remotes: &[(&str, &str)]) {
        std::fs::create_dir_all(root).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "--quiet"]);
        for (name, url) in remotes {
            git(&["remote", "add", name, url]);
        }
    }

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
    fn a_path_outside_every_repository_does_not_match() {
        // Given: a trusted repository and a sibling directory sharing its name as a prefix.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let sibling = dir.path().join("repo-sibling/file");
        git_repository(&root, &[("origin", "https://forge.invalid/ours/repo")]);
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        std::fs::write(&sibling, b"x").unwrap();
        let registry = registry(&[], trusting_owner("ours"));

        // When: the sibling file is resolved.
        let matched = match_checkout(&[sibling], &registry);

        // Then: no marker above it, so nothing matches.
        assert!(matched.is_none());
    }

    #[test]
    fn the_nearest_repository_wins_for_nested_checkouts() {
        // Given: an inner checkout nested in an outer checkout, only the inner
        // one under a trusted owner.
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("outer");
        let inner = outer.join("inner");
        git_repository(
            &outer,
            &[("origin", "https://forge.invalid/stranger/outer")],
        );
        git_repository(&inner, &[("origin", "https://forge.invalid/ours/inner")]);
        let registry = registry(&[], trusting_owner("ours"));

        // When: a nonexistent child of the inner checkout is resolved.
        let matched = match_checkout(&[inner.join("file.txt")], &registry);

        // Then: the inner root is the match, judged by its own remotes.
        let matched = matched.unwrap();
        assert_eq!(matched.root, inner.canonicalize().unwrap());
        assert_eq!(matched.name(), "inner");
        assert!(matched.trusted);
    }

    #[test]
    fn nonexistent_leaves_resolve_through_their_existing_parent() {
        // Given: an existing root and a not-yet-created descendant.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("r");
        git_repository(&root, &[("origin", "https://forge.invalid/ours/r")]);
        let registry = registry(&[], trusting_owner("ours"));

        // When: the nonexistent descendant is resolved.
        let matched = match_checkout(&[root.join("not/yet/created.txt")], &registry);

        // Then: its existing parent identifies the root, and the leaf is kept.
        let matched = matched.unwrap();
        assert_eq!(matched.root, root.canonicalize().unwrap());
        assert_eq!(
            matched.candidate,
            root.canonicalize().unwrap().join("not/yet/created.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_path_that_escapes_the_root_is_outside() {
        // Given: a symlink under a trusted root that points to an outside directory.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        git_repository(&root, &[("origin", "https://forge.invalid/ours/root")]);
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("file"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let registry = registry(&[], trusting_owner("ours"));

        // When: the file is named through the symlink.
        let matched = match_checkout(&[root.join("escape/file")], &registry);

        // Then: canonicalisation lands outside every repository.
        assert!(matched.is_none());
    }

    #[test]
    fn a_repo_under_a_trust_root_is_trusted_without_readable_remotes() {
        // Given: a workspace-shaped checkout under a trusted subtree whose remotes
        // cannot be read, never registered.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agent-c/platform/default");
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "rules\n").unwrap();
        let registry = registry(&[], trusting_root(&dir.path().join("agent-c")));

        // When: a file inside it is resolved.
        let matched = match_checkout(&[root.join("AGENTS.md")], &registry);

        // Then: it is trusted, unmanaged, and named for its parent because the
        // leaf is `default`.
        let matched = matched.expect("a root rule needs no remotes");
        assert!(matched.trusted);
        assert!(!matched.is_managed());
        assert_eq!(matched.name(), "platform");
        assert_eq!(matched.root, root.canonicalize().unwrap());
    }

    #[test]
    fn a_repo_declaring_a_trusted_owner_is_trusted_through_its_remotes() {
        // Given: two unregistered clones, one whose origin names a trusted owner.
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("elsewhere/tool");
        let theirs = dir.path().join("elsewhere/other");
        git_repository(
            &ours,
            &[("origin", "https://forge.invalid/Someone/tool.git")],
        );
        git_repository(
            &theirs,
            &[("origin", "https://forge.invalid/stranger/tool")],
        );
        let registry = registry(&[], trusting_owner("someone"));

        // When: a file in each is resolved.
        let matched = match_checkout(&[ours.join("src/lib.rs")], &registry);
        let denied = match_checkout(&[theirs.join("src/lib.rs")], &registry);

        // Then: the owner rule trusts the one repository root that declares it.
        let matched = matched.unwrap();
        assert!(matched.trusted);
        assert!(!matched.is_managed());
        assert_eq!(matched.root, ours.canonicalize().unwrap());
        assert!(denied.is_none());
    }

    #[test]
    fn a_sibling_of_a_trust_root_sharing_its_name_prefix_is_outside() {
        // Given: `agent-c-2`, a sibling whose string prefix matches a trusted root.
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("agent-c-2/repo");
        std::fs::create_dir_all(outside.join(".jj")).unwrap();
        let registry = registry(&[], trusting_root(&dir.path().join("agent-c")));

        // When: a file under the sibling is resolved.
        let matched = match_checkout(&[outside.join("x")], &registry);

        // Then: component containment rejects the string-prefix sibling.
        assert!(matched.is_none());
    }

    #[test]
    fn managed_and_trusted_are_decided_independently() {
        // Given: a registry entry and a trust rule that cover different remotes,
        // and three clones: a fork under a stranger's account, the same fork
        // pushed to our own account, and a clone of the maintained repository.
        let dir = tempfile::tempdir().unwrap();
        let strangers = dir.path().join("strangers");
        let ours = dir.path().join("ours");
        let plain = dir.path().join("plain");
        git_repository(
            &strangers,
            &[
                ("upstream", "git@forge.invalid:Maintainer/tool.git"),
                ("origin", "https://forge.invalid/stranger/tool"),
            ],
        );
        git_repository(
            &ours,
            &[
                ("upstream", "https://forge.invalid/maintainer/tool"),
                ("origin", "https://forge.invalid/ours/tool"),
            ],
        );
        git_repository(
            &plain,
            &[("origin", "https://forge.invalid/maintainer/tool")],
        );
        let registry = registry(
            &[("tool", "https://forge.invalid/maintainer/tool")],
            trusting_owner("ours"),
        );

        // When: a file in each is resolved.
        let managed_only = match_checkout(&[strangers.join("file")], &registry);
        let both = match_checkout(&[ours.join("file")], &registry);
        let neither = match_checkout(&[plain.join("file")], &registry);

        // Then: identity is the upstream remote, trust is any remote, and
        // neither implies the other.
        let managed_only = managed_only.unwrap();
        assert_eq!(managed_only.managed, Some(RepoName::new("tool")));
        assert_eq!(managed_only.name(), "tool");
        assert!(!managed_only.trusted);
        let both = both.unwrap();
        assert!(both.is_managed());
        assert!(both.trusted);
        assert!(neither.is_none());
    }

    #[test]
    fn a_forged_pointer_does_not_borrow_the_checkouts_remotes() {
        // Given: a trusted clone, and a tree beside it whose `.jj/repo` file
        // names that clone's store — what a `git clone` of an attacker's
        // repository would materialise — with no `.git` of its own.
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("tool/default");
        let forged = dir.path().join("tool/forged");
        git_repository(&checkout, &[("origin", "https://forge.invalid/ours/tool")]);
        std::fs::create_dir_all(checkout.join(".jj/repo")).unwrap();
        std::fs::create_dir_all(forged.join(".jj")).unwrap();
        std::fs::write(forged.join(".jj/repo"), "../../default/.jj/repo").unwrap();
        std::fs::write(forged.join("AGENTS.md"), "rules\n").unwrap();
        let registry = registry(&[], trusting_owner("ours"));

        // When: a file in the forged tree is resolved.
        let matched = match_checkout(&[forged.join("AGENTS.md")], &registry);

        // Then: the pointer is not followed; the tree has no remotes of its own
        // and earns nothing.
        assert!(matched.is_none(), "{matched:?}");
    }

    #[test]
    fn an_unrelated_path_does_not_hide_a_later_match() {
        // Given: an unresolved path before a repository under a trusted root.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("trusted/repo");
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        let registry = registry(&[], trusting_root(&dir.path().join("trusted")));

        // When: both the unrelated path and the trusted repository path are considered.
        let matched = match_checkout(
            &[dir.path().join("outside/missing"), root.join("AGENTS.md")],
            &registry,
        );

        // Then: resolution continues until it finds the trusted repository.
        assert_eq!(
            matched.map(|matched| matched.root),
            Some(root.canonicalize().unwrap())
        );
    }
}
