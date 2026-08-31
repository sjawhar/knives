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
    /// Checkouts that pin this repo's releases, so pinned-versus-newest can be answered
    /// without being asked.
    ///
    /// A list, because a fork can be consumed by several things at once and they can sit
    /// on different releases — which is the case the pin logic already reasoned about
    /// while the registry could only record one of them. Recorded here rather than passed
    /// as a flag every time, because the fact worth knowing is that a consumer is behind,
    /// and nobody runs a command to discover a question they have not thought of. Empty
    /// is normal: not every fork is consumed by something we also check out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumers: Vec<PathBuf>,
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

/// A repository whose agent instructions we trust, but which we do not maintain.
///
/// Deliberately its own section rather than a fork entry with optional remotes.
/// `RepoEntry` requires `upstream` and `origin` so that a malformed fork entry
/// fails at parse time; relaxing them to fit a repository that has no upstream
/// would trade that for a failure at the first query instead. A company repo
/// with nothing to contribute upstream is a different kind of thing, so it gets
/// a different shape: a path, and nothing else to get wrong.
///
/// No fork command reads these. They exist so the plugin can surface a
/// repository's own instructions when an agent reads its files, which is
/// useful well beyond the set of repositories we maintain forks of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedEntry {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustRules {
    /// Directory subtrees whose repositories are all trusted for guidance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<PathBuf>,
    /// Forge owners whose repositories are trusted for guidance, matched
    /// against remote URLs case-insensitively.
    ///
    /// SECURITY: matches SELF-DECLARED remote URLs read from the candidate
    /// checkout's own git config — not forge-authenticated; any cloned repo can
    /// claim any owner; grants guidance-as-data injection only (same grant as a
    /// `[trusted]` entry), never fork-command access; prefer roots when in doubt.
    /// The probe accepts only the checkout's own Git toplevel, so nested directories
    /// cannot inherit an enclosing checkout's identity. Owner rules read Git remote
    /// config, so jj-only checkouts match only through `roots`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<String>,
}

impl TrustRules {
    pub const fn is_empty(&self) -> bool {
        self.roots.is_empty() && self.owners.is_empty()
    }
}

/// Whether a guidance root comes from a maintained fork or trusted instructions.
///
/// The distinction survives resolution because callers may surface contribution
/// guidance for either kind without treating a trusted repository as a fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidanceRootKind {
    /// A maintained fork declared under `[repos.*]`.
    Managed,
    /// A repository whose instructions are trusted under `[trusted.*]`.
    Trusted,
}

/// A canonical repository root eligible to provide agent guidance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceRoot {
    pub name: String,
    pub root: PathBuf,
    pub kind: GuidanceRootKind,
}

impl TrustedEntry {
    /// Resolved exactly as `RepoEntry::resolved_path` resolves, and for the same
    /// reason: the CLI and the plugin must agree on which directory an entry names.
    pub fn resolved_path(&self, config_home: &Path) -> PathBuf {
        expand_registry_path(&self.path, config_home)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub repos: BTreeMap<String, RepoEntry>,
    /// Trusted but unmaintained. Present on this type, rather than ignored as an
    /// unknown section, because `save` rewrites the whole file: a section serde
    /// does not know about would be silently deleted the next time `init` runs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub trusted: BTreeMap<String, TrustedEntry>,
    /// Trust rules stay on this type because `save` rewrites the whole file;
    /// an unknown section would otherwise be silently deleted by `init`.
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

    /// Return the existing registry entries that may provide guidance.
    ///
    /// Resolve and skip each entry independently: a moved checkout must not disable
    /// guidance for the remaining registered repositories.
    pub fn guidance_roots(&self) -> Vec<GuidanceRoot> {
        let managed = self.repos.iter().filter_map(|(name, entry)| {
            entry.path.canonicalize().ok().map(|root| GuidanceRoot {
                name: name.clone(),
                root,
                kind: GuidanceRootKind::Managed,
            })
        });
        let trusted = self.trusted.iter().filter_map(|(name, entry)| {
            entry.path.canonicalize().ok().map(|root| GuidanceRoot {
                name: name.clone(),
                root,
                kind: GuidanceRootKind::Trusted,
            })
        });
        managed.chain(trusted).collect()
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

fn home_dir() -> PathBuf {
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
    for entry in registry.repos.values_mut() {
        entry.path = entry.resolved_path(&home);
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
        // Consumer paths get the same treatment as the repo path. Leaving `~` unexpanded
        // made a recorded consumer read as pinning nothing, which is the same wrong answer
        // as having no consumer at all.
        entry.consumers = entry
            .consumers
            .iter()
            .map(|consumer| expand_registry_path(consumer, &home))
            .collect();
    }
    for entry in registry.trusted.values_mut() {
        entry.path = entry.resolved_path(&home);
    }
    for root in &mut registry.trust.roots {
        *root = expand_registry_path(root, &home);
    }
    Ok(registry)
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
    fn consumer_paths_are_expanded_like_the_repo_path() {
        // An unexpanded `~` made a recorded consumer look like one pinning nothing,
        // which reads identically to having no consumer at all.
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["HOME"]);
        environment.set("HOME", "/home/someone");
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.demo]\npath = \"/tmp/demo\"\nupstream = \"u\"\norigin = \"o\"\n\
                    consumers = [\"~/one/default\", \"~/two/default\"]\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(
            registry.repos["demo"].consumers,
            vec![
                PathBuf::from("/home/someone/one/default"),
                PathBuf::from("/home/someone/two/default")
            ],
            "a fork can be consumed by several things at once"
        );
    }

    #[test]
    fn a_trusted_entry_needs_only_a_path() {
        // Given: a repository we do not maintain and have no upstream for
        let dir = tempfile::tempdir().unwrap();
        let text = "[trusted.workbench]\npath = \"/tmp/workbench\"\n";
        // When: the registry is loaded
        let registry = load(&write(dir.path(), text)).unwrap();
        // Then: it parses, with no remotes demanded of it
        assert_eq!(
            registry.trusted["workbench"].path,
            PathBuf::from("/tmp/workbench")
        );
    }

    #[test]
    fn a_trusted_entry_is_invisible_to_fork_commands() {
        // The parse-time guarantee that a fork entry carries its remotes only holds
        // if trusted entries never reach the code that assumes it.
        let dir = tempfile::tempdir().unwrap();
        let text = "[trusted.workbench]\npath = \"/tmp/workbench\"\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert!(registry.get(&RepoName::new("workbench")).is_none());
        assert!(registry.repos.is_empty());
    }

    #[test]
    fn guidance_roots_preserve_managed_and_trusted_kinds() {
        // Given: one managed fork and one trusted repository that both exist.
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        let trusted = dir.path().join("trusted");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::create_dir_all(&trusted).unwrap();
        let text = format!(
            "[repos.managed]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n\n\
             [trusted.workbench]\npath = \"{}\"\n",
            managed.display(),
            trusted.display()
        );
        let registry = load(&write(dir.path(), &text)).unwrap();

        // When: their guidance roots are collected.
        let roots = registry.guidance_roots();

        // Then: the roots retain their distinct registry roles.
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().any(|root| {
            root.name == "managed"
                && root.root == managed.canonicalize().unwrap()
                && root.kind == GuidanceRootKind::Managed
        }));
        assert!(roots.iter().any(|root| {
            root.name == "workbench"
                && root.root == trusted.canonicalize().unwrap()
                && root.kind == GuidanceRootKind::Trusted
        }));
    }

    #[test]
    fn guidance_roots_skip_an_unresolvable_entry_without_dropping_others() {
        // Given: one existing managed fork and one trusted checkout that was moved away.
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();
        let missing = dir.path().join("missing");
        let text = format!(
            "[repos.managed]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n\n\
             [trusted.moved]\npath = \"{}\"\n",
            managed.display(),
            missing.display()
        );
        let registry = load(&write(dir.path(), &text)).unwrap();

        // When: roots are resolved from the registry.
        let roots = registry.guidance_roots();

        // Then: the moved entry is skipped without disabling the existing one.
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "managed");
        assert_eq!(roots[0].kind, GuidanceRootKind::Managed);
    }

    #[test]
    fn a_trusted_tilde_path_resolves_like_a_fork_path() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["HOME"]);
        environment.set("HOME", "/home/someone");
        let dir = tempfile::tempdir().unwrap();
        let text = "[trusted.workbench]\npath = \"~/workbench/default\"\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(
            registry.trusted["workbench"].path,
            PathBuf::from("/home/someone/workbench/default")
        );
    }

    #[test]
    fn saving_preserves_trusted_entries() {
        // `init` rewrites the whole file. Before `trusted` existed on the type,
        // serde ignored the section on read and `save` then wrote it away.
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.demo]\npath = \"/tmp/demo\"\nupstream = \"u\"\norigin = \"o\"\n\n\
                    [trusted.workbench]\npath = \"/tmp/workbench\"\n";
        let path = write(dir.path(), text);
        let registry = load(&path).unwrap();

        save(&registry, &path).unwrap();

        let reloaded = load(&path).unwrap();
        assert_eq!(
            reloaded.trusted["workbench"].path,
            PathBuf::from("/tmp/workbench")
        );
        assert!(reloaded.repos.contains_key("demo"));
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
