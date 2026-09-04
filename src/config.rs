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

/// One repository as the registry describes it: identity, remotes by role,
/// policy. Where its checkout lives on this machine is not registry content;
/// [`crate::bind`] finds it by its remotes.
///
/// `upstream` and `origin` are required by the type, so a registry missing
/// either fails to parse rather than failing later at the first query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
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
    /// the found checkout: the `<name>/default` layout, where each workspace is
    /// a sibling of `default`.
    ///
    /// Set it for a checkout at `~/<name>`: with no `default` leaf there is no
    /// room for siblings, and each branch would land in `~` itself. A
    /// preference, valid on every machine: `~` expands, and a relative value is
    /// taken from the config directory, not the checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<PathBuf>,
}

impl RepoEntry {
    /// An entry with the two required remotes and every option unset.
    pub fn new(upstream: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            upstream: upstream.into(),
            origin: origin.into(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
            workspaces: None,
        }
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

    /// Whether `roots` contains `root` (canonicalised, component-wise). Decided
    /// from the path alone: it needs no remotes.
    pub fn contains_root(&self, root: &Path) -> bool {
        // Trust roots are tilde-expanded at load but can be symlinked; compare
        // canonical paths when possible so a real checkout under one is not missed.
        self.roots.iter().any(|configured| {
            let trusted = configured
                .canonicalize()
                .unwrap_or_else(|_| configured.clone());
            root.strip_prefix(&trusted).is_ok()
        })
    }

    /// Whether `owners` matches any remote's owner segment, or `repos` any
    /// remote's `owner/repo` slug.
    pub fn grants_by_remotes(&self, remotes: &BTreeMap<String, String>) -> bool {
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

    pub fn names(&self) -> impl Iterator<Item = RepoName> + '_ {
        self.repos.keys().map(|name| RepoName::new(name.clone()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
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
}

/// Expand `~` and resolve a relative registry path against the config home.
///
/// With no home directory `~` stays as written; the caller that needs it
/// reports the missing home.
pub fn expand_registry_path(path: &Path, config_home: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir().unwrap_or_else(|| path.to_owned());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir().map_or_else(|| path.to_owned(), |home| home.join(rest));
    }
    if path.is_absolute() {
        return path.to_owned();
    }
    config_home.join(path)
}

/// `$HOME`: the scan root for finding checkouts, and what `~` expands to.
///
/// `None` when unset or empty — the scan callers refuse rather than scanning
/// `/`. No password-database fallback: the refusal says `HOME is not set`, and
/// a fallback would make it unreachable on any machine with a passwd entry.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// What the scan callers print when [`home_dir`] is `None`.
pub const NO_HOME: &str = "HOME is not set; knives scans $HOME for checkouts";

/// Where the registry lives.
///
/// `KNIVES_CONFIG_HOME` wins over `XDG_CONFIG_HOME` so this tool can be pointed
/// elsewhere without moving every other tool's config too. Redirecting
/// `XDG_CONFIG_HOME` to isolate this tool also hides the forge CLI's
/// credentials, which turns a working setup into an authentication failure.
pub fn default_config_path() -> PathBuf {
    if let Ok(home) = std::env::var("KNIVES_CONFIG_HOME") {
        return PathBuf::from(home).join("repos.toml");
    }
    let base = std::env::var("XDG_CONFIG_HOME").map_or_else(
        |_| {
            // Never the current directory: that silently relocates the trust
            // set, and the registry decides which repositories the plugin will
            // inject guidance from.
            home_dir()
                .unwrap_or_else(|| PathBuf::from("/nonexistent"))
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
    let mut registry: Registry = match toml::from_str(&text) {
        Ok(registry) => registry,
        // A deleted field or table gets a message naming its replacement,
        // rather than serde's "unknown field"; both are named when both remain.
        Err(source) => {
            let raw = raw_table(&text, path)?;
            let rejections: Vec<String> = deleted_trusted_table(&raw)
                .into_iter()
                .chain(deleted_path_field(&raw))
                .collect();
            if rejections.is_empty() {
                return Err(ConfigError::Parse {
                    path: path.to_owned(),
                    source: Box::new(source),
                });
            }
            return Err(ConfigError::Invalid {
                path: path.to_owned(),
                detail: rejections.join("; "),
            });
        }
    };
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
    reject_shared_upstreams(&registry, path)?;
    reject_tilde_paths_without_a_home(&registry, path)?;
    let config_home = path.parent().unwrap_or_else(|| Path::new(".")).to_owned();
    for (name, entry) in &mut registry.repos {
        entry.workspaces =
            checked_workspaces(name, entry.workspaces.as_deref(), &config_home, path)?;
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
        checked_release_branch(entry, path)?;
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
        *root = expand_registry_path(root, &config_home);
    }
    Ok(registry)
}

/// A `~` in `workspaces` or `[trust] roots` expands through [`home_dir`]. With
/// no home it would stay as written — a *relative* path `~/…` — and `knives
/// start` would open workspaces under `<cwd>/~/…`. Refused by name instead.
fn reject_tilde_paths_without_a_home(registry: &Registry, path: &Path) -> Result<(), ConfigError> {
    if home_dir().is_some() {
        return Ok(());
    }
    let is_tilde = |value: &Path| {
        let text = value.to_string_lossy();
        text == "~" || text.starts_with("~/")
    };
    let offending = registry
        .repos
        .iter()
        .filter_map(|(name, entry)| {
            let workspaces = entry
                .workspaces
                .as_deref()
                .filter(|value| is_tilde(value))?;
            Some(format!(
                "[repos.{name}] workspaces = \"{}\" needs HOME",
                workspaces.display()
            ))
        })
        .chain(
            registry
                .trust
                .roots
                .iter()
                .filter(|root| is_tilde(root))
                .map(|root| format!("[trust] roots = \"{}\" needs HOME", root.display())),
        )
        .collect::<Vec<_>>();
    if offending.is_empty() {
        return Ok(());
    }
    Err(ConfigError::Invalid {
        path: path.to_owned(),
        detail: format!("{NO_HOME}: {}", offending.join("; ")),
    })
}

/// A release branch named for the trunk would make every trunk exclusion also
/// exclude the release, and one under the dated prefix would collide with the
/// dated scheme's namespace. Both corrupt every downstream check.
fn checked_release_branch(entry: &RepoEntry, path: &Path) -> Result<(), ConfigError> {
    let Some(name) = entry.release_branch.as_deref() else {
        return Ok(());
    };
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
                "release_branch {name:?} names the trunk; a release branch shadowing the trunk \
                 corrupts every trunk exclusion"
            ),
        });
    }
    if name.starts_with(crate::ids::RELEASE_PREFIX) {
        return Err(ConfigError::Invalid {
            path: path.to_owned(),
            detail: format!(
                "release_branch {name:?} sits in the dated {} namespace; the two schemes must \
                 not collide",
                crate::ids::RELEASE_PREFIX
            ),
        });
    }
    Ok(())
}

/// The registry as a plain TOML table, for the checks that name a deleted field
/// or table where serde only says "unknown field".
fn raw_table(text: &str, path: &Path) -> Result<toml::Table, ConfigError> {
    toml::from_str(text).map_err(|source| ConfigError::Parse {
        path: path.to_owned(),
        source: Box::new(source),
    })
}

/// `[trusted.*]` was deleted with the registry's paths. A message that names the
/// replacement beats serde's "unknown field" for the one section people had.
fn deleted_trusted_table(raw: &toml::Table) -> Option<String> {
    raw.get("trusted")
        .and_then(toml::Value::as_table)
        .and_then(|trusted| trusted.keys().next())
        .map(|name| {
            format!(
                "[trusted.{name}] is no longer a registry table; move it to [trust] repos = \
                 [\"<owner>/<repo>\"]"
            )
        })
}

/// `path` left the registry: checkouts are found by their remotes. Every
/// offending entry is named, by name, with what replaced the field.
fn deleted_path_field(raw: &toml::Table) -> Option<String> {
    let repos = raw.get("repos").and_then(toml::Value::as_table)?;
    let mut names: Vec<&String> = repos
        .iter()
        .filter(|(_, entry)| {
            entry
                .as_table()
                .is_some_and(|table| table.contains_key("path"))
        })
        .map(|(name, _)| name)
        .collect();
    names.sort();
    (!names.is_empty()).then(|| {
        let listed = names
            .iter()
            .map(|name| format!("[repos.{name}]"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{listed} path is no longer a registry field; delete it — knives finds checkouts \
             by their remotes"
        )
    })
}

/// Identity is the `upstream` remote, so two entries naming one repository
/// could never both bind. The first pair by name is reported in `a`'s spelling.
fn reject_shared_upstreams(registry: &Registry, path: &Path) -> Result<(), ConfigError> {
    let entries: Vec<(&String, &RepoEntry)> = registry.repos.iter().collect();
    for (index, (a, first)) in entries.iter().enumerate() {
        for (b, second) in entries.iter().skip(index + 1) {
            if crate::bind::same_remote(&first.upstream, &second.upstream) {
                return Err(ConfigError::Invalid {
                    path: path.to_owned(),
                    detail: format!(
                        "[repos.{a}] and [repos.{b}] share upstream {}; identity must be unique",
                        first.upstream
                    ),
                });
            }
        }
    }
    Ok(())
}

/// An entry's `workspaces`, expanded like every registry path and checked.
///
/// Empty would resolve to the config home itself, putting every branch workspace
/// beside the state file and the ledger. Whether it sits inside the checkout is
/// checked where the checkout is known: `knives start` and `finish`.
fn checked_workspaces(
    name: &str,
    raw: Option<&Path>,
    config_home: &Path,
    path: &Path,
) -> Result<Option<PathBuf>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.as_os_str().is_empty() {
        return Err(ConfigError::Invalid {
            path: path.to_owned(),
            detail: format!("[repos.{name}] workspaces is empty; name a directory or omit it"),
        });
    }
    Ok(Some(expand_registry_path(raw, config_home)))
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
upstream = "https://example.invalid/upstream.git"
origin = "https://example.invalid/origin.git"

[repos.split]
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
            registry.repos["example"].upstream,
            "https://example.invalid/upstream.git"
        );
    }

    #[test]
    fn a_repo_name_that_escapes_the_ledger_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.\"../escape\"]\nupstream = \"u\"\norigin = \"o\"\n";

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
        let plain = "[repos.demo]\nupstream = \"u\"\norigin = \"o\"\n";
        let registry = load(&write(dir.path(), plain)).unwrap();
        assert_eq!(
            registry.repos["demo"].release_scheme(),
            crate::ids::ReleaseScheme::Dated
        );

        let fixed = "[repos.demo]\nupstream = \"u\"\norigin = \"o\"\n\
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
                "[repos.d]\nupstream = \"u\"\norigin = \"o\"\n\
                 release_branch = \"\"\n",
                "empty",
            ),
            (
                "[repos.d]\nupstream = \"u\"\norigin = \"o\"\n\
                 release_branch = \"main\"\n",
                "trunk",
            ),
            (
                "[repos.d]\nupstream = \"u\"\norigin = \"o\"\n\
                 base = \"dev\"\nrelease_branch = \"dev\"\n",
                "trunk",
            ),
            (
                "[repos.d]\nupstream = \"u\"\norigin = \"o\"\n\
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
        let plain = "[repos.demo]\nupstream = \"u\"\norigin = \"o\"\n";
        let registry = load(&write(dir.path(), plain)).unwrap();
        assert_eq!(registry.repos["demo"].default_base(), "main");

        let stated = "[repos.demo]\nupstream = \"u\"\norigin = \"o\"\n\
                      base = \"develop\"\n";
        let registry = load(&write(dir.path(), stated)).unwrap();
        assert_eq!(registry.repos["demo"].default_base(), "develop");
    }

    #[test]
    fn the_trunk_is_the_base_field_and_defaults_to_main() {
        // The trunk we fork from, measure landed against, and target PRs at are the
        // same branch in every repo we know of, so one field serves both meanings.
        let dir = tempfile::tempdir().unwrap();
        let plain = "[repos.demo]\nupstream = \"u\"\norigin = \"o\"\n";
        let registry = load(&write(dir.path(), plain)).unwrap();
        assert_eq!(registry.repos["demo"].trunk(), "main");
        assert_eq!(registry.repos["demo"].upstream_trunk(), "main@upstream");

        let stated = "[repos.demo]\nupstream = \"u\"\norigin = \"o\"\n\
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
        let text = "[repos.broken]\norigin = \"o\"\n";
        // When: it is loaded
        let result = load(&write(dir.path(), text));
        // Then: it fails at parse time, naming the field, not later at query time
        let message = result.unwrap_err().to_string();
        assert!(message.contains("upstream"), "message was: {message}");
    }

    #[test]
    fn a_remote_url_that_looks_like_an_option_is_rejected_at_config_load() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.demo]\nupstream = \"u\"\norigin = \"--upload-pack=x\"\n";

        let error = load(&write(dir.path(), text)).unwrap_err().to_string();
        assert!(error.contains("origin remote"), "was: {error}");
        assert!(error.contains("must not start with `-`"), "was: {error}");
    }

    #[test]
    fn consumer_slugs_load_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.demo]\nupstream = \"u\"\norigin = \"o\"\n\
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
        let text = "[repos.demo]\nupstream = \"u\"\norigin = \"o\"\n\
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
        assert!(registry.trust.grants_by_remotes(&by_repo));
        let by_repo_with_git_suffix_configured = BTreeMap::from([(
            "origin".to_owned(),
            "https://forge.example/company/other".to_owned(),
        )]);
        assert!(
            registry
                .trust
                .grants_by_remotes(&by_repo_with_git_suffix_configured)
        );
        let other = BTreeMap::from([(
            "origin".to_owned(),
            "https://forge.example/company/third".to_owned(),
        )]);
        assert!(!registry.trust.grants_by_remotes(&other));
        let by_owner = BTreeMap::from([(
            "upstream".to_owned(),
            "https://forge.example/someone/anything".to_owned(),
        )]);
        assert!(registry.trust.grants_by_remotes(&by_owner));
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
        std::fs::write(&path, "[trusted.work]\n").unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(
            error.contains(
                "[trusted.work] is no longer a registry table; move it to [trust] repos = [\"<owner>/<repo>\"]"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_path_field_names_its_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.toml");
        std::fs::write(
            &path,
            "[repos.tool]\npath = \"~/tool\"\nupstream = \"https://forge.example/org/tool\"\n\
             origin = \"https://forge.example/ours/tool\"\n",
        )
        .unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(
            error.contains(
                "[repos.tool] path is no longer a registry field; delete it — knives finds checkouts by their remotes"
            ),
            "{error}"
        );
    }

    #[test]
    fn every_entry_carrying_a_path_field_is_named_at_once() {
        // Naming the first would take one load per entry to migrate a registry.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.toml");
        std::fs::write(
            &path,
            "[repos.zeta]\npath = \"~/z\"\nupstream = \"u1\"\norigin = \"o\"\n\n\
             [repos.alpha]\npath = \"~/a\"\nupstream = \"u2\"\norigin = \"o\"\n\n\
             [repos.clean]\nupstream = \"u3\"\norigin = \"o\"\n",
        )
        .unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(
            error.contains(
                "[repos.alpha], [repos.zeta] path is no longer a registry field; delete it — knives finds checkouts by their remotes"
            ),
            "{error}"
        );
        assert!(!error.contains("[repos.clean]"), "{error}");
    }

    #[test]
    fn a_registry_with_both_a_path_field_and_a_trusted_table_names_both_at_once() {
        // The registry that predates this shape has both; naming one per load
        // would take two rounds to migrate it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.toml");
        std::fs::write(
            &path,
            "[repos.tool]\npath = \"~/tool\"\nupstream = \"u\"\norigin = \"o\"\n\n[trusted.work]\n",
        )
        .unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(
            error.contains("[trusted.work] is no longer a registry table"),
            "{error}"
        );
        assert!(
            error.contains("[repos.tool] path is no longer a registry field"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_field_or_table_is_refused_by_name() {
        // `deny_unknown_fields` on every table: a misspelt key would otherwise
        // silently mean its default.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.toml");
        std::fs::write(
            &path,
            "[repos.x]\nupstream = \"u\"\norigin = \"o\"\nreleas = \"r\"\n",
        )
        .unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(error.contains("unknown field"), "{error}");
        assert!(error.contains("releas"), "{error}");

        std::fs::write(&path, "[trust]\nrepo = [\"a/b\"]\n").unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(error.contains("unknown field"), "{error}");
        assert!(error.contains("repo"), "{error}");

        std::fs::write(&path, "[bogus]\nkey = 1\n").unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(error.contains("unknown field"), "{error}");
        assert!(error.contains("bogus"), "{error}");
    }

    #[test]
    fn two_entries_sharing_an_upstream_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.toml");
        std::fs::write(
            &path,
            "[repos.a]\nupstream = \"https://forge.example/org/tool\"\n\
             origin = \"https://forge.example/ours/tool\"\n\
             [repos.b]\nupstream = \"git@forge.example:org/tool.git\"\n\
             origin = \"https://forge.example/theirs/tool\"\n",
        )
        .unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(
            error.contains(
                "[repos.a] and [repos.b] share upstream https://forge.example/org/tool; identity must be unique"
            ),
            "{error}"
        );
    }

    #[test]
    fn an_entry_without_path_loads_and_workspaces_is_a_preference() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["HOME"]);
        let dir = tempfile::tempdir().unwrap();
        environment.set("HOME", dir.path().to_str().unwrap());
        let path = dir.path().join("repos.toml");
        std::fs::write(
            &path,
            "[repos.tool]\nupstream = \"https://forge.example/org/tool\"\n\
             origin = \"https://forge.example/ours/tool\"\nworkspaces = \"~/.worktrees/tool\"\n",
        )
        .unwrap();
        let registry = load(&path).unwrap();
        let entry = &registry.repos["tool"];
        assert_eq!(
            entry.workspaces.as_deref(),
            Some(dir.path().join(".worktrees/tool").as_path())
        );
    }

    #[test]
    fn a_relative_workspaces_directory_is_resolved_against_the_config_home() {
        // One resolution for every registry path, so a value that works for a
        // trust root cannot silently mean something else for `workspaces`.
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.tool]\nupstream = \"u\"\norigin = \"o\"\n\
                    workspaces = \"worktrees/tool\"\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(
            registry.repos["tool"].workspaces.as_deref(),
            Some(dir.path().join("worktrees").join("tool").as_path())
        );
    }

    #[test]
    fn an_empty_workspaces_directory_is_a_config_error() {
        // Empty resolves to the config home itself, which would put every branch
        // workspace beside the state file and the ledger.
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.tool]\nupstream = \"u\"\norigin = \"o\"\n\
                    workspaces = \"\"\n";
        let error = load(&write(dir.path(), text)).unwrap_err().to_string();
        assert!(error.contains("workspaces"), "was: {error}");
        assert!(error.contains("empty"), "was: {error}");
    }

    #[test]
    fn trust_rules_parse_and_expand() {
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
    }

    #[test]
    fn a_tilde_path_is_refused_by_name_when_there_is_no_home_to_expand_it() {
        // Left unexpanded, `~/ws` is a relative path and `knives start` would
        // open workspaces under `<cwd>/~/ws`. Every offending key is named.
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["HOME"]);
        environment.remove("HOME");
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.demo]\nupstream = \"u\"\norigin = \"o\"\nworkspaces = \"~/ws/demo\"\n\n\
                    [repos.plain]\nupstream = \"u2\"\norigin = \"o\"\nworkspaces = \"/abs/plain\"\n\n\
                    [trust]\nroots = [\"~\", \"/abs/root\"]\n";
        let error = load(&write(dir.path(), text)).unwrap_err().to_string();
        assert!(error.contains(NO_HOME), "{error}");
        assert!(
            error.contains("[repos.demo] workspaces = \"~/ws/demo\" needs HOME"),
            "{error}"
        );
        assert!(
            error.contains("[trust] roots = \"~\" needs HOME"),
            "{error}"
        );
        assert!(!error.contains("plain"), "{error}");
        assert!(!error.contains("/abs/root"), "{error}");

        // Without a tilde anywhere, a missing home is not the registry's problem.
        let absolute = "[repos.plain]\nupstream = \"u\"\norigin = \"o\"\nworkspaces = \"/abs/plain\"\n\
                        [trust]\nroots = [\"/abs/root\"]\n";
        let registry = load(&write(dir.path(), absolute)).unwrap();
        assert_eq!(
            registry.repos["plain"].workspaces,
            Some(PathBuf::from("/abs/plain"))
        );
    }

    #[test]
    fn a_tilde_path_resolves_the_same_way_the_plugin_resolves_it() {
        // `workspaces` and `[trust] roots` go through this expansion, and the
        // plugin's tool-argument matcher expands `~` the same way. The two sides
        // disagreeing once meant a `~/...` entry was a working allowlist entry
        // and a broken CLI entry, so the trust set and the tool covered
        // different directories.
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
    fn a_missing_file_is_an_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("absent.toml")).unwrap().is_empty());
    }
}
