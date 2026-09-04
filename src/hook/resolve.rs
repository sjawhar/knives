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
/// Both facts are decided from the remotes of the checkout `bind::checkout_root`
/// resolves to. `remotes_of` is the (cached) reader, keyed by that checkout path;
/// it returns `None` when remotes cannot be read — the caller has already
/// reported why. A `roots` rule still applies then.
pub fn match_checkout(
    paths: &[PathBuf],
    registry: &Registry,
    remotes_of: &mut dyn FnMut(&Path) -> Option<BTreeMap<String, String>>,
) -> Option<Match> {
    for path in paths {
        let Some(candidate) = canonical_path(path) else {
            continue;
        };
        let Some(existing) = existing_ancestor(&candidate) else {
            continue;
        };
        let Some(root) = bind::nearest_root(existing) else {
            continue;
        };
        let Some(checkout) = bind::checkout_root(existing) else {
            continue;
        };
        let remotes = remotes_of(&checkout).unwrap_or_default();
        let managed = remotes
            .get("upstream")
            .and_then(|upstream| bind::entry_for(registry, upstream))
            .map(|(name, _)| name);
        let trusted = registry.trust.grants(&root, &remotes);
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
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use crate::config::{Registry, RepoEntry, TrustRules};
    use crate::ids::RepoName;

    use super::{Match, argument_paths, match_checkout};

    fn remotes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, url)| ((*name).to_owned(), (*url).to_owned()))
            .collect()
    }

    fn registry(entries: &[(&str, &str)], trust: TrustRules) -> Registry {
        Registry {
            repos: entries
                .iter()
                .map(|(name, upstream)| {
                    (
                        (*name).to_owned(),
                        RepoEntry {
                            upstream: (*upstream).to_owned(),
                            origin: "https://forge.invalid/ours/fork".to_owned(),
                            base: None,
                            release: None,
                            release_branch: None,
                            test_count_command: None,
                            consumers: vec![],
                            workspaces: None,
                        },
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

    /// A reader that answers every checkout with the same remotes and records
    /// which checkouts it was asked about.
    fn reader(
        answer: Option<BTreeMap<String, String>>,
        asked: &mut Vec<PathBuf>,
    ) -> impl FnMut(&Path) -> Option<BTreeMap<String, String>> {
        move |checkout: &Path| {
            asked.push(checkout.to_owned());
            answer.clone()
        }
    }

    fn resolve(
        paths: &[PathBuf],
        registry: &Registry,
        answer: Option<BTreeMap<String, String>>,
    ) -> (Option<Match>, Vec<PathBuf>) {
        let mut asked = Vec::new();
        let matched = match_checkout(paths, registry, &mut reader(answer, &mut asked));
        (matched, asked)
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
        // Given: a repository and a sibling directory sharing its name as a prefix.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let sibling = dir.path().join("repo-sibling/file");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        std::fs::write(&sibling, b"x").unwrap();
        let registry = registry(&[], trusting_owner("ours"));

        // When: the sibling file is resolved.
        let (matched, asked) = resolve(
            &[sibling],
            &registry,
            Some(remotes(&[("origin", "https://forge.invalid/ours/repo")])),
        );

        // Then: no marker above it, so no remotes are read and nothing matches.
        assert!(matched.is_none());
        assert!(asked.is_empty());
    }

    #[test]
    fn the_nearest_repository_wins_for_nested_checkouts() {
        // Given: an inner checkout nested in an outer checkout.
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        let registry = registry(&[], trusting_owner("ours"));

        // When: a nonexistent child of the inner checkout is resolved.
        let (matched, asked) = resolve(
            &[inner.join("file.txt")],
            &registry,
            Some(remotes(&[("origin", "https://forge.invalid/ours/inner")])),
        );

        // Then: the inner root is the match and the only checkout consulted.
        let matched = matched.unwrap();
        assert_eq!(matched.root, inner.canonicalize().unwrap());
        assert_eq!(matched.name(), "inner");
        assert_eq!(asked, vec![inner.canonicalize().unwrap()]);
    }

    #[test]
    fn nonexistent_leaves_resolve_through_their_existing_parent() {
        // Given: an existing root and a not-yet-created descendant.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("r");
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        let registry = registry(&[], trusting_owner("ours"));

        // When: the nonexistent descendant is resolved.
        let (matched, _) = resolve(
            &[root.join("not/yet/created.txt")],
            &registry,
            Some(remotes(&[("origin", "https://forge.invalid/ours/r")])),
        );

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
        // Given: a symlink under the root that points to an outside directory.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("file"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let registry = registry(&[], trusting_owner("ours"));

        // When: the file is named through the symlink.
        let (matched, asked) = resolve(
            &[root.join("escape/file")],
            &registry,
            Some(remotes(&[("origin", "https://forge.invalid/ours/root")])),
        );

        // Then: canonicalisation lands outside every repository.
        assert!(matched.is_none());
        assert!(asked.is_empty());
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

        // When: a file inside it is resolved and the remote reader has nothing.
        let (matched, _) = resolve(&[root.join("AGENTS.md")], &registry, None);

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
        // Given: an unregistered checkout whose origin names a trusted owner.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("elsewhere/tool");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let registry = registry(&[], trusting_owner("someone"));
        let declared = remotes(&[("origin", "https://forge.invalid/Someone/tool.git")]);

        // When: the candidate is resolved.
        let (matched, asked) = resolve(&[root.join("src/lib.rs")], &registry, Some(declared));

        // Then: the owner rule trusts this one repository root, read once.
        let matched = matched.unwrap();
        assert!(matched.trusted);
        assert!(!matched.is_managed());
        assert_eq!(asked, vec![root.canonicalize().unwrap()]);

        let stranger = remotes(&[("origin", "https://forge.invalid/stranger/tool")]);
        let (denied, _) = resolve(&[root.join("src/lib.rs")], &registry, Some(stranger));
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
        let (matched, _) = resolve(&[outside.join("x")], &registry, None);

        // Then: component containment rejects the string-prefix sibling.
        assert!(matched.is_none());
    }

    #[test]
    fn managed_and_trusted_are_decided_independently() {
        // Given: a registry entry and a trust rule that cover different remotes.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("fork");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let registry = registry(
            &[("tool", "https://forge.invalid/maintainer/tool")],
            trusting_owner("ours"),
        );

        // When: the checkout forks the entry under a stranger's account.
        let (managed_only, _) = resolve(
            &[root.join("file")],
            &registry,
            Some(remotes(&[
                ("upstream", "git@forge.invalid:Maintainer/tool.git"),
                ("origin", "https://forge.invalid/stranger/tool"),
            ])),
        );
        // And when: the same fork is pushed to our own account.
        let (both, _) = resolve(
            &[root.join("file")],
            &registry,
            Some(remotes(&[
                ("upstream", "https://forge.invalid/maintainer/tool"),
                ("origin", "https://forge.invalid/ours/tool"),
            ])),
        );
        // And when: only `origin` points at the maintained repository.
        let (neither, _) = resolve(
            &[root.join("file")],
            &registry,
            Some(remotes(&[(
                "origin",
                "https://forge.invalid/maintainer/tool",
            )])),
        );

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
    fn a_workspace_is_its_own_root_but_reads_its_checkouts_remotes() {
        // Given: a checkout and a jj workspace whose `.jj/repo` points back at it.
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("tool/default");
        let workspace = dir.path().join("tool/feature");
        std::fs::create_dir_all(checkout.join(".jj/repo")).unwrap();
        std::fs::create_dir_all(workspace.join(".jj")).unwrap();
        std::fs::write(workspace.join(".jj/repo"), "../../default/.jj/repo").unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "rules\n").unwrap();
        let registry = registry(&[], trusting_owner("ours"));

        // When: a file in the workspace is resolved.
        let (matched, asked) = resolve(
            &[workspace.join("AGENTS.md")],
            &registry,
            Some(remotes(&[("origin", "https://forge.invalid/ours/tool")])),
        );

        // Then: guidance walks the workspace; identity comes from the checkout.
        let matched = matched.unwrap();
        assert_eq!(matched.root, workspace.canonicalize().unwrap());
        assert_eq!(matched.name(), "feature");
        assert_eq!(asked, vec![checkout.canonicalize().unwrap()]);
    }

    #[test]
    fn an_unrelated_path_does_not_hide_a_later_match() {
        // Given: an unresolved path before a repository under a trusted root.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("trusted/repo");
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        let registry = registry(&[], trusting_root(&dir.path().join("trusted")));

        // When: both the unrelated path and the trusted repository path are considered.
        let (matched, _) = resolve(
            &[dir.path().join("outside/missing"), root.join("AGENTS.md")],
            &registry,
            None,
        );

        // Then: resolution continues until it finds the trusted repository.
        assert_eq!(
            matched.map(|matched| matched.root),
            Some(root.canonicalize().unwrap())
        );
    }
}
