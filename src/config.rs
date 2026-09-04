//! The registry of managed repositories.
//!
//! Everything user-specific is configuration. No user, organisation, or
//! repository name appears in this crate; remotes are addressed by role.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ids::RepoName;

#[cfg(test)]
#[allow(
    clippy::redundant_pub_crate,
    reason = "crate-visible test helpers are consumed from sibling test modules"
)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

    /// Serializes every test that touches the process environment — in either
    /// direction. Tests that MUTATE variables (`PATH`, `HOME`, `KNIVES_*`)
    /// hold it while an [`EnvironmentGuard`] is live, and tests that SPAWN
    /// subprocesses hold it for their whole body: a spawn resolves its binary
    /// through `PATH` and inherits the environment at that instant, so an
    /// unlocked spawn racing a mutator fails in ways that look like the
    /// production code flaking (measured: `git`/`jj` fixtures in the hook and
    /// sync tests, 3 failures in 50 parallel suite runs).
    pub(crate) fn environment_lock() -> MutexGuard<'static, ()> {
        ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[derive(Debug)]
    pub(crate) struct EnvironmentGuard {
        values: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvironmentGuard {
        pub(crate) fn capture(names: &[&'static str]) -> Self {
            Self {
                values: names
                    .iter()
                    .map(|name| (*name, std::env::var_os(name)))
                    .collect(),
            }
        }

        fn assert_captures(&self, name: &str) {
            assert!(
                self.values.iter().any(|(captured, _)| *captured == name),
                "{name} was not captured"
            );
        }

        pub(crate) fn set(&self, name: &str, value: &str) {
            self.assert_captures(name);
            unsafe { std::env::set_var(name, value) };
        }

        pub(crate) fn remove(&self, name: &str) {
            self.assert_captures(name);
            unsafe { std::env::remove_var(name) };
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            for (name, value) in &self.values {
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }
}

/// What a remote is for, rather than what it is called.
///
/// An enum, so an unknown role cannot be requested and no runtime lookup error
/// is needed. `Release` is optional and falls back to `Origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// The repository we contribute to. Fetch only, including pull refs.
    Upstream,
    /// Where our branches get pushed.
    Origin,
    /// Where dated releases get pushed. Defaults to [`Role::Origin`].
    Release,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Upstream => "upstream",
            Self::Origin => "origin",
            Self::Release => "release",
        };
        f.write_str(text)
    }
}

/// One repository as it appears on disk in the registry.
///
/// `upstream` and `origin` are required by the type, so a registry missing
/// either fails to parse rather than failing later at the first query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub path: PathBuf,
    pub upstream: String,
    pub origin: String,
    /// Upstream's trunk: the branch we fork from, measure landed against, and
    /// target pull requests at. Defaults to "main". Configurable because not
    /// every upstream calls its default branch `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_branch: Option<String>,
    /// A command whose output ends in the number of tests, used to check that a
    /// release cut did not drop a branch's tests.
    ///
    /// Per repo because counting tests is repo-specific: there is no portable
    /// way to ask an arbitrary project how many tests it has. Absent means the
    /// check is reported as not configured, never as passed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_count_command: Option<String>,
    /// Forge slugs for repositories that pin this repo's releases, so
    /// pinned-versus-newest can be answered without local checkouts.
    ///
    /// A list, because a fork can be consumed by several things at once and they can sit
    /// on different releases. Local checkouts are deliberately not persisted here: pass
    /// them with `--consumer` for a one-off scan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumers: Vec<String>,
    /// Where this repository's branch workspaces live. Absent, they sit beside
    /// the checkout: the `<name>/default` layout, where each workspace is a
    /// sibling of `default`.
    ///
    /// Set it for a checkout at `~/<name>`: with no `default` leaf there is no
    /// room for siblings, and each branch would land in `~` itself. Resolved
    /// like `path`: `~` expands, and a relative value is taken from the config
    /// directory, not the checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<PathBuf>,
}

impl RepoEntry {
    /// The repository's location, resolved the same way the plugin resolves it.
    ///
    /// The two sides used to disagree: the plugin expanded `~` and resolved a
    /// relative path against the config home, while this side used the value
    /// verbatim, i.e. relative to the process's current directory. A
    /// `path = "~/repos/x"` entry was therefore a working allowlist entry and a
    /// broken CLI entry, so the trust set and the tool covered different
    /// directories. One rule now, and it is the plugin's.
    pub fn resolved_path(&self, config_home: &Path) -> PathBuf {
        expand_registry_path(&self.path, config_home)
    }

    /// The directory `knives start` opens this repository's workspaces under,
    /// and `finish` removes them from.
    pub fn workspace_root(&self) -> &Path {
        self.workspaces
            .as_deref()
            .unwrap_or_else(|| self.path.parent().unwrap_or(&self.path))
    }

    /// The remote for a role. Total: every role resolves.
    pub fn remote(&self, role: Role) -> &str {
        match role {
            Role::Upstream => &self.upstream,
            Role::Origin => &self.origin,
            Role::Release => self.release.as_deref().unwrap_or(&self.origin),
        }
    }

    /// The branch upstream treats as its trunk: what we fork from, measure
    /// landed against, and target pull requests at. One field, not two, because
    /// no repo we manage has ever split them; `base` keeps its name for
    /// compatibility with existing registries.
    pub fn trunk(&self) -> &str {
        self.base.as_deref().unwrap_or("main")
    }

    /// How releases are named, derived from `release_branch`.
    pub fn release_scheme(&self) -> crate::ids::ReleaseScheme {
        self.release_branch
            .as_deref()
            .map_or(crate::ids::ReleaseScheme::Dated, |name| {
                crate::ids::ReleaseScheme::Fixed(crate::ids::BranchName::new(name))
            })
    }

    /// The upstream remote's view of the trunk, e.g. `dev@upstream`.
    ///
    /// Every landed probe and fork point measures against this, never the local
    /// trunk: our fork's trunk answers about the wrong repository.
    pub fn upstream_trunk(&self) -> String {
        format!("{}@{}", self.trunk(), Role::Upstream)
    }

    /// The `immutable_heads()` this fork runs under: jj's `trunk()`, tags, and the
    /// trunk by name on every remote knives knows — upstream, origin, and the
    /// release remote when one is configured.
    ///
    /// jj's default adds `untracked_remote_bookmarks()`. In a fork, a remote ref
    /// that is not trunk is ours or something we build on — a superseded release
    /// cut a fetch re-materialized, another fork's pull request head — and
    /// freezing its ancestors protects nothing (a local rewrite never reaches a
    /// remote; the next fetch restores whatever was dropped) while refusing every
    /// routine `jj rebase` of a member whose old tip sits under one. The trunks
    /// are named outright because `trunk()` need not resolve to them: `jj git
    /// clone` pins the alias to `<trunk>@origin`, and the default picks whichever
    /// trunk-named ref is newest. Each remote is named, never
    /// `remote_bookmarks(exact:"<trunk>")` alone, which also matches the `@git`
    /// export of whatever the local bookmark points at. `knives start` writes
    /// this into the repository's jj config; knives' own in-process rewrites keep
    /// jj's default pins (`jj::assert_mutable`).
    pub fn immutable_heads(&self) -> String {
        let trunk = self.trunk();
        let mut remotes = vec![Role::Upstream, Role::Origin];
        if self.has_split_release() {
            remotes.push(Role::Release);
        }
        let pinned: Vec<String> = remotes
            .iter()
            .map(|remote| format!("remote_bookmarks(exact:\"{trunk}\", exact:\"{remote}\")"))
            .collect();
        format!("trunk() | tags() | {}", pinned.join(" | "))
    }

    /// The branch a pull request from this repo should target.
    ///
    /// Kept for existing PR-base callers; trunk is the branch they target.
    pub fn default_base(&self) -> &str {
        self.trunk()
    }

    /// Whether release publishing has a remote distinct from the origin fork.
    ///
    /// A configured `release` URL equal to `origin` is an explicit spelling of
    /// the default topology, not a second role. Roles name ref ownership, so
    /// every downstream release decision must continue to use origin there.
    pub fn has_split_release(&self) -> bool {
        self.release
            .as_deref()
            .is_some_and(|release| release != self.origin.as_str())
    }

    /// The remote role that publishes releases for this repository.
    pub fn publish_remote(&self) -> &str {
        if self.has_split_release() {
            "release"
        } else {
            "origin"
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustRules {
    /// Repositories trusted for guidance by identity: `owner/repo`, matched
    /// against any remote of a checkout, case-insensitively, `.git` stripped
    /// from both sides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    /// Directory subtrees whose repositories are all trusted for guidance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<PathBuf>,
    /// Forge owners whose repositories are trusted for guidance, matched
    /// against every remote URL of a checkout case-insensitively.
    ///
    /// SECURITY: matches SELF-DECLARED remote URLs read from the candidate
    /// checkout's own jj or git configuration — not forge-authenticated; any
    /// cloned repo can claim any owner; grants guidance-as-data injection only,
    /// never fork-command access; prefer roots when in doubt. Remotes are read
    /// from the nearest repository root only, so a directory nested inside a
    /// checkout cannot inherit the enclosing checkout's identity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<String>,
}

impl TrustRules {
    pub const fn is_empty(&self) -> bool {
        self.repos.is_empty() && self.roots.is_empty() && self.owners.is_empty()
    }

    /// Whether these rules trust a checkout at `root` declaring `remotes`.
    ///
    /// `roots` contains the root (canonicalised, component-wise); `owners`
    /// matches any remote's owner segment; `repos` matches any remote's
    /// `owner/repo` slug. Any rule true is enough.
    pub fn grants(&self, root: &Path, remotes: &BTreeMap<String, String>) -> bool {
        // Trust roots are tilde-expanded at load but can be symlinked; compare
        // canonical paths when possible so a real checkout under one is not missed.
        let under_root = self.roots.iter().any(|configured| {
            let trusted = configured
                .canonicalize()
                .unwrap_or_else(|_| configured.clone());
            root.strip_prefix(&trusted).is_ok()
        });
        if under_root {
            return true;
        }
        remotes.values().any(|url| {
            let owned = crate::bind::url_owner(url).is_some_and(|owner| {
                self.owners
                    .iter()
                    .any(|trusted| trusted.eq_ignore_ascii_case(owner))
            });
            let listed = crate::bind::remote_slug(url).is_some_and(|slug| {
                self.repos.iter().any(|trusted| {
                    trusted
                        .strip_suffix(".git")
                        .unwrap_or(trusted)
                        .eq_ignore_ascii_case(slug)
                })
            });
            owned || listed
        })
    }
}

/// A canonical repository root eligible to provide agent guidance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceRoot {
    pub name: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    #[serde(default)]
    pub repos: BTreeMap<String, RepoEntry>,
    /// Which repositories' instructions the hook injects as guidance. Decided
    /// by remote identity or by subtree, never by a fork entry.
    #[serde(default, skip_serializing_if = "TrustRules::is_empty")]
    pub trust: TrustRules,
}

impl Registry {
    pub fn get(&self, name: &RepoName) -> Option<&RepoEntry> {
        self.repos.get(name.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }

    /// The managed repo that contains `path`, if any.
    ///
    /// Longest root wins, so a repo checked out inside another repo resolves to the
    /// inner one. When `path` is a jj workspace beside its registered checkout,
    /// `.jj/repo` points back to that checkout's repository store; following that
    /// pointer retains the same component-based containment rule.
    pub fn containing(&self, path: &Path) -> Option<(RepoName, &RepoEntry)> {
        self.containing_direct(path).or_else(|| {
            workspace_checkout(path).and_then(|checkout| self.containing_direct(&checkout))
        })
    }

    fn containing_direct(&self, path: &Path) -> Option<(RepoName, &RepoEntry)> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        self.repos
            .iter()
            .filter(|(_, entry)| {
                let root = entry
                    .path
                    .canonicalize()
                    .unwrap_or_else(|_| entry.path.clone());
                canonical == root || canonical.strip_prefix(&root).is_ok()
            })
            .max_by_key(|(_, entry)| entry.path.components().count())
            .map(|(name, entry)| (RepoName::new(name.clone()), entry))
    }

    pub fn names(&self) -> impl Iterator<Item = RepoName> + '_ {
        self.repos.keys().map(|name| RepoName::new(name.clone()))
    }
}

/// The registered checkout behind a jj workspace, when `path` is inside one.
fn workspace_checkout(path: &Path) -> Option<PathBuf> {
    for directory in path.ancestors() {
        let pointer = directory.join(".jj").join("repo");
        if !pointer.is_file() {
            continue;
        }
        let store = PathBuf::from(std::fs::read_to_string(&pointer).ok()?.trim());
        let store = if store.is_absolute() {
            store
        } else {
            pointer.parent()?.join(store)
        };
        let checkout = store.parent()?.parent()?;
        return checkout
            .canonicalize()
            .ok()
            .or_else(|| Some(checkout.to_owned()));
    }
    None
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a valid registry: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("{path} is not a valid registry: {detail}")]
    Invalid { path: PathBuf, detail: String },
    #[error("serialising the registry: {source}")]
    Serialise {
        #[from]
        source: toml::ser::Error,
    },
}

/// Where the registry lives.
///
/// `KNIVES_CONFIG_HOME` wins over `XDG_CONFIG_HOME` so this tool can be pointed
/// elsewhere without moving every other tool's config too. Redirecting
/// `XDG_CONFIG_HOME` to isolate this tool also hides the forge CLI's
/// credentials, which turns a working setup into an authentication failure.
/// Expand `~` and resolve a relative registry path against the config home.
pub fn expand_registry_path(path: &Path, config_home: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    if path.is_absolute() {
        return path.to_owned();
    }
    config_home.join(path)
}

/// `$HOME`, the scan root for finding checkouts; `/` when unset.
pub fn home_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("/"), PathBuf::from)
}

pub fn default_config_path() -> PathBuf {
    if let Ok(home) = std::env::var("KNIVES_CONFIG_HOME") {
        return PathBuf::from(home).join("repos.toml");
    }
    let base = std::env::var("XDG_CONFIG_HOME").map_or_else(
        |_| {
            // Never the current directory: that silently relocates the trust
            // set, and the registry decides which repositories the plugin will
            // inject guidance from.
            std::env::var("HOME")
                .map_or_else(|_| PathBuf::from("/nonexistent"), PathBuf::from)
                .join(".config")
        },
        PathBuf::from,
    );
    base.join("knives").join("repos.toml")
}

/// Read the registry. A missing file is an empty registry, which the commands
/// surface explicitly rather than treating as "nothing to do".
pub fn load(path: &Path) -> Result<Registry, ConfigError> {
    if !path.exists() {
        return Ok(Registry::default());
    }
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    reject_deleted_trusted_table(&text, path)?;
    let mut registry: Registry = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_owned(),
        source: Box::new(source),
    })?;
    for name in registry.repos.keys() {
        if name.is_empty()
            || name == "."
            || name == ".."
            || Path::new(name).is_absolute()
            || name.contains('/')
            || name.contains('\\')
        {
            return Err(ConfigError::Invalid {
                path: path.to_owned(),
                detail: format!("repository key {name:?} is not a safe path component"),
            });
        }
    }
    // Resolve once, here, so no caller can accidentally use the raw value and
    // end up pointed somewhere other than the plugin's allowlist.
    let home = path.parent().unwrap_or_else(|| Path::new(".")).to_owned();
    for (name, entry) in &mut registry.repos {
        entry.path = entry.resolved_path(&home);
        entry.workspaces = checked_workspaces(name, entry, &home, path)?;
        for (role, remote) in [
            ("upstream", entry.upstream.as_str()),
            ("origin", entry.origin.as_str()),
        ]
        .into_iter()
        .chain(entry.release.as_deref().map(|remote| ("release", remote)))
        {
            if remote.starts_with('-') {
                return Err(ConfigError::Invalid {
                    path: path.to_owned(),
                    detail: format!("{role} remote {remote:?} must not start with `-`"),
                });
            }
        }
        if let Some(name) = entry.release_branch.as_deref() {
            if name.is_empty() {
                return Err(ConfigError::Invalid {
                    path: path.to_owned(),
                    detail: "release_branch is empty; a fixed release scheme needs a branch name"
                        .to_owned(),
                });
            }
            if name == entry.trunk() {
                return Err(ConfigError::Invalid {
                    path: path.to_owned(),
                    detail: format!(
                        "release_branch {name:?} names the trunk; a release branch shadowing the \
                         trunk corrupts every trunk exclusion"
                    ),
                });
            }
            if name.starts_with(crate::ids::RELEASE_PREFIX) {
                return Err(ConfigError::Invalid {
                    path: path.to_owned(),
                    detail: format!(
                        "release_branch {name:?} sits in the dated {} namespace; the two schemes \
                         must not collide",
                        crate::ids::RELEASE_PREFIX
                    ),
                });
            }
        }
        for consumer in &entry.consumers {
            if !is_forge_slug(consumer) {
                return Err(ConfigError::Invalid {
                    path: path.to_owned(),
                    detail: format!(
                        "repos.toml: [repos.{name}] consumers now takes forge slugs \
                         (\"<owner>/<repo>\"); found \"{consumer}\". Scan a local checkout \
                         with --consumer <path> instead."
                    ),
                });
            }
        }
    }
    for slug in &registry.trust.repos {
        if !is_forge_slug(slug) {
            return Err(ConfigError::Invalid {
                path: path.to_owned(),
                detail: format!(
                    "[trust] repos takes forge slugs (\"<owner>/<repo>\"); found \"{slug}\""
                ),
            });
        }
    }
    for root in &mut registry.trust.roots {
        *root = expand_registry_path(root, &home);
    }
    Ok(registry)
}

/// `[trusted.*]` was deleted with the registry's paths. A message that names the
/// replacement beats serde's "unknown field" for the one section people had.
fn reject_deleted_trusted_table(text: &str, path: &Path) -> Result<(), ConfigError> {
    let raw: toml::Table = toml::from_str(text).map_err(|source| ConfigError::Parse {
        path: path.to_owned(),
        source: Box::new(source),
    })?;
    raw.get("trusted")
        .and_then(toml::Value::as_table)
        .and_then(|trusted| trusted.keys().next())
        .map_or(Ok(()), |name| {
            Err(ConfigError::Invalid {
                path: path.to_owned(),
                detail: format!(
                    "[trusted.{name}] is no longer a registry table; move it to [trust] repos = \
                     [\"<owner>/<repo>\"]"
                ),
            })
        })
}

/// An entry's `workspaces`, resolved like `path` and checked. `entry.path` must
/// already be resolved.
///
/// Empty would resolve to the config home itself, putting every branch workspace
/// beside the state file and the ledger; a directory inside the checkout puts
/// them in the working copy they belong to. Neither is what anyone meant.
fn checked_workspaces(
    name: &str,
    entry: &RepoEntry,
    home: &Path,
    path: &Path,
) -> Result<Option<PathBuf>, ConfigError> {
    let Some(raw) = entry.workspaces.as_deref() else {
        return Ok(None);
    };
    if raw.as_os_str().is_empty() {
        return Err(ConfigError::Invalid {
            path: path.to_owned(),
            detail: format!("[repos.{name}] workspaces is empty; name a directory or omit it"),
        });
    }
    let workspaces = expand_registry_path(raw, home);
    if workspaces.starts_with(&entry.path) {
        return Err(ConfigError::Invalid {
            path: path.to_owned(),
            detail: format!(
                "[repos.{name}] workspaces {} is inside the checkout {}; branch workspaces cannot \
                 live in the working copy they belong to",
                workspaces.display(),
                entry.path.display()
            ),
        });
    }
    Ok(Some(workspaces))
}

fn is_forge_slug(value: &str) -> bool {
    let Some((owner, repository)) = value.split_once('/') else {
        return false;
    };
    let has_path_syntax =
        |segment: &str| segment.starts_with(['/', '~', '.']) || segment.contains('\\');
    !owner.is_empty()
        && !repository.is_empty()
        && !repository.contains('/')
        && !has_path_syntax(owner)
        && !has_path_syntax(repository)
}

pub fn save(registry: &Registry, path: &Path) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(registry)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.to_owned(),
            source,
        })?;
    }
    std::fs::write(path, text).map_err(|source| ConfigError::Write {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::test_support::{EnvironmentGuard, environment_lock};
    use super::*;

    const SAMPLE: &str = r#"
[repos.example]
path = "/tmp/example"
upstream = "https://example.invalid/upstream.git"
origin = "https://example.invalid/origin.git"

[repos.split]
path = "/tmp/split"
upstream = "https://example.invalid/upstream2.git"
origin = "https://example.invalid/branches.git"
release = "https://example.invalid/releases.git"
"#;

    fn write(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("repos.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_registry_parses_one_entry_per_repo() {
        let dir = tempfile::tempdir().unwrap();
        let registry = load(&write(dir.path(), SAMPLE)).unwrap();
        assert_eq!(registry.repos.len(), 2);
        assert_eq!(
            registry.repos["example"].path,
            PathBuf::from("/tmp/example")
        );
    }

    #[test]
    fn a_repo_name_that_escapes_the_ledger_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let text =
            "[repos.\"../escape\"]\npath = \"/tmp/escape\"\nupstream = \"u\"\norigin = \"o\"\n";

        let error = load(&write(dir.path(), text)).unwrap_err().to_string();
        assert!(error.contains("../escape"), "was: {error}");
    }

    #[test]
    fn the_release_role_falls_back_to_origin() {
        let dir = tempfile::tempdir().unwrap();
        let registry = load(&write(dir.path(), SAMPLE)).unwrap();
        let entry = &registry.repos["example"];
        assert_eq!(entry.remote(Role::Release), entry.remote(Role::Origin));
        assert_eq!(entry.publish_remote(), "origin");
        assert!(!entry.has_split_release());
    }

    #[test]
    fn the_release_scheme_is_dated_unless_a_fixed_branch_is_stated() {
        let dir = tempfile::tempdir().unwrap();
        let plain = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n";
        let registry = load(&write(dir.path(), plain)).unwrap();
        assert_eq!(
            registry.repos["demo"].release_scheme(),
            crate::ids::ReleaseScheme::Dated
        );

        let fixed = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n\
                     release_branch = \"integration\"\n";
        let registry = load(&write(dir.path(), fixed)).unwrap();
        assert_eq!(
            registry.repos["demo"].release_scheme(),
            crate::ids::ReleaseScheme::Fixed(crate::ids::BranchName::new("integration"))
        );
    }

    #[test]
    fn an_invalid_release_branch_fails_to_parse() {
        // A release branch named for the trunk would make every trunk exclusion
        // also exclude the release, and one under release/ would collide with the
        // dated scheme's namespace. Both corrupt every downstream check, so the
        // registry refuses at parse time, the same place a missing role fails.
        let dir = tempfile::tempdir().unwrap();
        for (text, needle) in [
            (
                "[repos.d]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n\
                 release_branch = \"\"\n",
                "empty",
            ),
            (
                "[repos.d]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n\
                 release_branch = \"main\"\n",
                "trunk",
            ),
            (
                "[repos.d]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n\
                 base = \"dev\"\nrelease_branch = \"dev\"\n",
                "trunk",
            ),
            (
                "[repos.d]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n\
                 release_branch = \"release/2026-01-01\"\n",
                "release/",
            ),
        ] {
            let message = load(&write(dir.path(), text)).unwrap_err().to_string();
            assert!(message.contains(needle), "for {text}: was {message}");
        }
    }

    #[test]
    fn the_expected_base_is_the_trunk_unless_stated() {
        // A pull request against our own fork's main never reaches the maintainer, so the
        // expected base has to be knowable. Configurable because not every upstream calls
        // its default branch main.
        let dir = tempfile::tempdir().unwrap();
        let plain = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n";
        let registry = load(&write(dir.path(), plain)).unwrap();
        assert_eq!(registry.repos["demo"].default_base(), "main");

        let stated = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n\
                      base = \"develop\"\n";
        let registry = load(&write(dir.path(), stated)).unwrap();
        assert_eq!(registry.repos["demo"].default_base(), "develop");
    }

    #[test]
    fn the_trunk_is_the_base_field_and_defaults_to_main() {
        // The trunk we fork from, measure landed against, and target PRs at are the
        // same branch in every repo we know of, so one field serves both meanings.
        let dir = tempfile::tempdir().unwrap();
        let plain = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n";
        let registry = load(&write(dir.path(), plain)).unwrap();
        assert_eq!(registry.repos["demo"].trunk(), "main");
        assert_eq!(registry.repos["demo"].upstream_trunk(), "main@upstream");

        let stated = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n\
                      base = \"dev\"\n";
        let registry = load(&write(dir.path(), stated)).unwrap();
        assert_eq!(registry.repos["demo"].trunk(), "dev");
        assert_eq!(registry.repos["demo"].upstream_trunk(), "dev@upstream");
        assert_eq!(registry.repos["demo"].default_base(), "dev");
    }

    #[test]
    fn a_split_release_remote_is_used_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let registry = load(&write(dir.path(), SAMPLE)).unwrap();
        let entry = &registry.repos["split"];
        assert_eq!(
            entry.remote(Role::Release),
            "https://example.invalid/releases.git"
        );
        assert_eq!(entry.publish_remote(), "release");
        assert!(entry.has_split_release());
    }

    #[test]
    fn an_equal_release_url_is_not_a_split_publish_role() {
        let dir = tempfile::tempdir().unwrap();
        let text = "\
            [repos.demo]\n\
            path = \"/tmp/d\"\n\
            upstream = \"https://forge.invalid/up/demo.git\"\n\
            origin = \"https://forge.invalid/ours/demo.git\"\n\
            release = \"https://forge.invalid/ours/demo.git\"\n";

        let registry = load(&write(dir.path(), text)).expect("equal release URL parses");
        let entry = &registry.repos["demo"];

        assert_eq!(entry.publish_remote(), "origin");
        assert!(!entry.has_split_release());
    }

    #[test]
    fn a_registry_missing_a_required_role_fails_to_parse() {
        // Given: an entry with no upstream
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.broken]\npath = \"/tmp/b\"\norigin = \"o\"\n";
        // When: it is loaded
        let result = load(&write(dir.path(), text));
        // Then: it fails at parse time, naming the field, not later at query time
        let message = result.unwrap_err().to_string();
        assert!(message.contains("upstream"), "message was: {message}");
    }

    #[test]
    fn a_remote_url_that_looks_like_an_option_is_rejected_at_config_load() {
        let dir = tempfile::tempdir().unwrap();
        let text =
            "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"--upload-pack=x\"\n";

        let error = load(&write(dir.path(), text)).unwrap_err().to_string();
        assert!(error.contains("origin remote"), "was: {error}");
        assert!(error.contains("must not start with `-`"), "was: {error}");
    }

    #[test]
    fn the_repo_containing_a_directory_is_found_and_a_sibling_is_not() {
        // Requiring the repo name on every command is absurd when you are standing in
        // the repository. Prefix matching would be worse than nothing: `<root>-2` is a
        // different repository that shares the prefix.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("managed");
        let sibling = dir.path().join("managed-2");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let text = format!(
            "[repos.managed]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n",
            root.display()
        );
        let registry = load(&write(dir.path(), &text)).unwrap();

        assert_eq!(
            registry.containing(&root.join("src")).map(|(n, _)| n),
            Some(RepoName::new("managed"))
        );
        assert_eq!(
            registry.containing(&root).map(|(n, _)| n),
            Some(RepoName::new("managed"))
        );
        assert!(
            registry.containing(&sibling).is_none(),
            "a sibling is not inside"
        );
        assert!(registry.containing(dir.path()).is_none());
    }

    #[test]
    fn consumer_slugs_load_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.demo]\npath = \"/tmp/demo\"\nupstream = \"u\"\norigin = \"o\"\n\
                    consumers = [\"an-org/a-consumer\"]\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(
            registry.repos["demo"].consumers,
            vec!["an-org/a-consumer".to_owned()]
        );
    }

    #[test]
    fn a_path_in_consumers_is_a_loud_config_error_naming_the_new_form() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.demo]\npath = \"/tmp/demo\"\nupstream = \"u\"\norigin = \"o\"\n\
                    consumers = [\"~/one/default\"]\n";
        let error = load(&write(dir.path(), text)).unwrap_err().to_string();
        assert!(error.contains("forge slugs"), "was: {error}");
        assert!(error.contains("--consumer"), "was: {error}");
    }

    #[test]
    fn trust_repos_are_forge_slugs_and_grant_by_any_remote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.toml");
        std::fs::write(
            &path,
            "[trust]\nrepos = [\"Company/Tool\", \"company/other.git\"]\nowners = [\"someone\"]\n",
        )
        .unwrap();
        let registry = load(&path).unwrap();
        let by_repo = BTreeMap::from([(
            "origin".to_owned(),
            "git@forge.example:company/tool.git".to_owned(),
        )]);
        assert!(registry.trust.grants(Path::new("/anywhere"), &by_repo));
        let by_repo_with_git_suffix_configured = BTreeMap::from([(
            "origin".to_owned(),
            "https://forge.example/company/other".to_owned(),
        )]);
        assert!(
            registry
                .trust
                .grants(Path::new("/anywhere"), &by_repo_with_git_suffix_configured)
        );
        let other = BTreeMap::from([(
            "origin".to_owned(),
            "https://forge.example/company/third".to_owned(),
        )]);
        assert!(!registry.trust.grants(Path::new("/anywhere"), &other));
        let by_owner = BTreeMap::from([(
            "upstream".to_owned(),
            "https://forge.example/someone/anything".to_owned(),
        )]);
        assert!(registry.trust.grants(Path::new("/anywhere"), &by_owner));
    }

    #[test]
    fn a_trust_repo_that_is_not_a_slug_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.toml");
        std::fs::write(&path, "[trust]\nrepos = [\"~/somewhere\"]\n").unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(
            error.contains(
                "[trust] repos takes forge slugs (\"<owner>/<repo>\"); found \"~/somewhere\""
            ),
            "{error}"
        );
    }

    #[test]
    fn a_trusted_table_names_its_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.toml");
        std::fs::write(&path, "[trusted.work]\npath = \"~/work\"\n").unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(
            error.contains(
                "[trusted.work] is no longer a registry table; move it to [trust] repos = [\"<owner>/<repo>\"]"
            ),
            "{error}"
        );
    }

    #[test]
    fn workspaces_sit_beside_the_checkout_unless_the_entry_says_where() {
        // The `<name>/default` layout: each branch's workspace is a sibling of
        // `default`. Absent configuration keeps it, so no registered repository's
        // existing workspaces move.
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.tool]\npath = \"/home/someone/forks/tool/default\"\nupstream = \"u\"\n\
                    origin = \"o\"\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(
            registry.repos["tool"].workspace_root(),
            PathBuf::from("/home/someone/forks/tool")
        );
    }

    #[test]
    fn a_configured_workspaces_directory_is_where_workspaces_go() {
        // A checkout at `~/<name>` has no room for siblings: they would land in `~`
        // itself, one directory per branch across every repository.
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["HOME"]);
        environment.set("HOME", "/home/someone");
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.tool]\npath = \"~/tool\"\nupstream = \"u\"\norigin = \"o\"\n\
                    workspaces = \"~/.worktrees/tool\"\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(
            registry.repos["tool"].workspace_root(),
            PathBuf::from("/home/someone/.worktrees/tool")
        );
    }

    #[test]
    fn saving_preserves_a_configured_workspaces_directory() {
        // `init` rewrites the whole file; a field serde does not know about would
        // silently move every workspace back beside the checkout.
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.tool]\npath = \"/tmp/tool\"\nupstream = \"u\"\norigin = \"o\"\n\
                    workspaces = \"/tmp/worktrees/tool\"\n";
        let path = write(dir.path(), text);
        let registry = load(&path).unwrap();

        save(&registry, &path).unwrap();

        let reloaded = load(&path).unwrap();
        assert_eq!(
            reloaded.repos["tool"].workspace_root(),
            PathBuf::from("/tmp/worktrees/tool")
        );
    }

    #[test]
    fn a_relative_workspaces_directory_is_resolved_against_the_config_home() {
        // The same rule as `path`: one resolution for every registry path, so a
        // value that works for one field cannot silently mean something else for
        // the other.
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.tool]\npath = \"/tmp/tool\"\nupstream = \"u\"\norigin = \"o\"\n\
                    workspaces = \"worktrees/tool\"\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(
            registry.repos["tool"].workspace_root(),
            dir.path().join("worktrees").join("tool")
        );
    }

    #[test]
    fn an_empty_workspaces_directory_is_a_config_error() {
        // Empty resolves to the config home itself, which would put every branch
        // workspace beside the state file and the ledger.
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.tool]\npath = \"/tmp/tool\"\nupstream = \"u\"\norigin = \"o\"\n\
                    workspaces = \"\"\n";
        let error = load(&write(dir.path(), text)).unwrap_err().to_string();
        assert!(error.contains("workspaces"), "was: {error}");
        assert!(error.contains("empty"), "was: {error}");
    }

    #[test]
    fn a_workspaces_directory_inside_the_checkout_is_a_config_error() {
        // A workspace inside the checkout's working copy is never what anyone
        // meant; the checkout path itself is the most likely slip.
        let dir = tempfile::tempdir().unwrap();
        for inside in ["/tmp/tool", "/tmp/tool/.worktrees"] {
            let text = format!(
                "[repos.tool]\npath = \"/tmp/tool\"\nupstream = \"u\"\norigin = \"o\"\n\
                 workspaces = \"{inside}\"\n"
            );
            let error = load(&write(dir.path(), &text)).unwrap_err().to_string();
            assert!(error.contains("workspaces"), "was: {error}");
            assert!(error.contains("inside the checkout"), "was: {error}");
        }
        // Containment is by component: a sibling sharing the checkout's name as a
        // string prefix is outside it.
        let beside = "[repos.tool]\npath = \"/tmp/tool\"\nupstream = \"u\"\norigin = \"o\"\n\
                      workspaces = \"/tmp/tool-worktrees\"\n";
        assert_eq!(
            load(&write(dir.path(), beside)).unwrap().repos["tool"].workspace_root(),
            Path::new("/tmp/tool-worktrees")
        );
    }

    #[test]
    fn trust_rules_parse_expand_and_survive_a_save() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["HOME"]);
        environment.set("HOME", "/home/someone");
        let dir = tempfile::tempdir().unwrap();
        let text = "[trust]\nroots = [\"~/agent-c\"]\nowners = [\"some-owner\", \"some-org\"]\n";
        let path = write(dir.path(), text);
        let registry = load(&path).unwrap();
        assert_eq!(
            registry.trust.roots,
            vec![PathBuf::from("/home/someone/agent-c")]
        );
        assert_eq!(
            registry.trust.owners,
            vec!["some-owner".to_owned(), "some-org".to_owned()]
        );
        // `init` rewrites the whole file; a section serde does not know about
        // would be silently deleted the next time it runs.
        save(&registry, &path).unwrap();
        let reloaded = load(&path).unwrap();
        assert_eq!(reloaded.trust.owners.len(), 2);
    }

    #[test]
    fn a_tilde_path_resolves_the_same_way_the_plugin_resolves_it() {
        // The two sides disagreeing meant `path = "~/repos/x"` was a working
        // allowlist entry and a broken CLI entry, so the trust set and the tool
        // covered different directories.
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["HOME"]);
        environment.set("HOME", "/home/someone");
        assert_eq!(
            expand_registry_path(Path::new("~/repos/x"), Path::new("/cfg")),
            PathBuf::from("/home/someone/repos/x")
        );
        assert_eq!(
            expand_registry_path(Path::new("~"), Path::new("/cfg")),
            PathBuf::from("/home/someone")
        );
    }

    #[test]
    fn a_relative_path_resolves_against_the_config_home_not_the_cwd() {
        assert_eq!(
            expand_registry_path(Path::new("repos/x"), Path::new("/cfg")),
            PathBuf::from("/cfg/repos/x")
        );
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        assert_eq!(
            expand_registry_path(Path::new("/abs/x"), Path::new("/cfg")),
            PathBuf::from("/abs/x")
        );
    }

    #[test]
    fn load_resolves_every_entry_so_no_caller_sees_a_raw_value() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.rel]\npath = \"sub/repo\"\nupstream = \"u\"\norigin = \"o\"\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(registry.repos["rel"].path, dir.path().join("sub/repo"));
    }

    #[test]
    fn a_missing_file_is_an_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("absent.toml")).unwrap().is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let original = load(&write(dir.path(), SAMPLE)).unwrap();
        let target = dir.path().join("out").join("repos.toml");
        save(&original, &target).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(
            !text.contains("release_branch"),
            "saved registry unexpectedly names release_branch: {text}"
        );
        assert!(
            !text.contains("[trust]"),
            "saved registry unexpectedly names [trust]: {text}"
        );
        assert_eq!(load(&target).unwrap(), original);
    }
}
