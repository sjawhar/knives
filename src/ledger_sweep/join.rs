//! Joining a ledger the registry names and no git directory here reaches.
//!
//! A machine whose `repos.toml` shares a fork's ledger, and that nobody set
//! up to carry it, would otherwise have that fork's entries travel nowhere
//! and no claim of it held anywhere but here. So knives makes the git
//! directory itself, as a machine is set up by hand to share a ledger:
//! `<owner>--<name>.git` in [`ledger_repositories`], over the ledger root,
//! whose `origin` is the ledger's repository on the fork's own forge, and
//! which names this machine ([`this_machine`]). That name is never one
//! whose ref the ledger's repository already holds, unless the ref is the
//! session's own. A caller in a harness session names it for that session
//! on this host: two sessions on one host, each with its own config home,
//! never share a ref, and the same session joining again gets the same
//! name back. What the remote already holds under that name is that
//! session's own history, taken up before the directory is in place
//! ([`make`]), so a config home made again resumes the claims it took
//! rather than refuse its own ref. A caller in no harness session names it
//! for the host and eight random digits, written once to each git directory
//! the join makes: random, so that two config homes of one host that both
//! join before either pushes never share a name, which asking the remote,
//! holding neither's ref yet, could not prevent. A name the remote already
//! holds is another config home's, so the join draws other digits and
//! takes nothing up. A git directory made by hand or by an installer keeps
//! the `knives.machine` it was given; a join in another config home picks
//! digits of its own, so the two cannot share a name. A directory already
//! at that path is never touched, and a dry run joins nothing.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::REMOTE;
use super::destinations::{identity, ledger_repositories};
use crate::config::Registry;
use crate::ids::UpstreamName;

/// The git directory joining `ledger`, `<owner>/<name>`, makes beside
/// `root`: `<owner>--<name>.git` in [`ledger_repositories`].
pub(super) fn joined_git_dir(root: &Path, ledger: &str) -> PathBuf {
    ledger_repositories(root).join(format!("{}.git", ledger.replacen('/', "--", 1)))
}

/// Why a fork whose ledger is `ledger` is carried nowhere although a git
/// directory stands at [`joined_git_dir`]: its origin names another
/// repository, and knives changes no git directory it did not make.
pub(super) fn occupied(root: &Path, ledger: &str) -> String {
    let git_dir = joined_git_dir(root, ledger);
    format!(
        "{} is there already, and its origin is not {ledger}; knives changes no git directory \
         it did not make. Give it {ledger} as its origin, or move it away so that knives joins \
         the ledger itself",
        git_dir.display()
    )
}

/// Join each ledger `unreached` names, each fork no git directory reaches
/// with the `<owner>/<name>` of the ledger it belongs to, as
/// [`this_machine`], saying each join on `said`; why each one that could not
/// be joined was not, by ledger.
pub(super) fn join_unreached<'a>(
    root: &Path,
    registry: &Registry,
    unreached: impl IntoIterator<Item = (&'a UpstreamName, &'a String)>,
    said: &mut dyn Write,
) -> BTreeMap<String, String> {
    let mut by_ledger: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (fork, ledger) in unreached {
        by_ledger.entry(ledger).or_default().push(fork.as_str());
    }
    let machine = this_machine();
    let mut failed = BTreeMap::new();
    for (ledger, forks) in by_ledger {
        match join(root, registry, ledger, &forks, &machine) {
            Ok(Some(line)) => {
                let _ = writeln!(said, "{}", crate::ledger::inline_human_text(&line));
            }
            Ok(None) => {}
            Err(why) => {
                failed.insert(ledger.to_owned(), why);
            }
        }
    }
    failed
}

/// Make [`joined_git_dir`] for `ledger`, which `forks` name, as the machine
/// `name` gives; the line that says so, `None` when another command made it
/// first, or why it could not be made.
///
/// Its `origin` is the URL of a fork entry naming `ledger` ([`ledger_url`]),
/// its origin's else its upstream's, and it commits as `name`, or as another
/// name when the remote already holds that one and it is not this session's
/// own ([`make`]). Nothing is made where something already stands at its
/// path. A git directory there whose origin is `ledger` is another
/// command's join, as when [`make`] loses its rename: `None`. One whose
/// origin is another repository is why ([`occupied`]), and one whose config
/// cannot be read gives that read's error ([`identity`]). Nothing is made
/// either when the remote cannot be asked: the next command that routes
/// `ledger` tries again.
#[allow(
    clippy::too_many_arguments,
    reason = "the ledger root, the registry, the ledger, the forks naming it and the machine it \
              joins as are independent inputs"
)]
fn join(
    root: &Path,
    registry: &Registry,
    ledger: &str,
    forks: &[&str],
    name: &Name,
) -> Result<Option<String>, String> {
    let root =
        &std::path::absolute(root).map_err(|error| format!("{}: {error}", root.display()))?;
    let git_dir = joined_git_dir(root, ledger);
    if git_dir.symlink_metadata().is_ok() {
        let joined = identity(&git_dir)
            .map_err(|error| error.to_string())?
            .origin
            .as_deref()
            .and_then(crate::remote_url::remote_slug)
            .is_some_and(|slug| crate::config::repository_slug(slug) == ledger);
        return if joined {
            Ok(None)
        } else {
            Err(occupied(root, ledger))
        };
    }
    let url = registry
        .repos
        .values()
        .filter(|entry| entry.shares_ledger_with(ledger))
        .find_map(|entry| {
            ledger_url(&entry.origin, ledger).or_else(|| ledger_url(&entry.upstream, ledger))
        })
        .ok_or_else(|| {
            format!(
                "neither the origin nor the upstream repos.toml gives {} is a remote URL to \
                 take {ledger}'s URL from",
                forks.join(", ")
            )
        })?;
    Ok(make(root, &git_dir, &url, name)?.map(|machine| {
        format!(
            "joined the ledger {ledger}: made {} as machine {machine}, carrying {}",
            git_dir.display(),
            forks.join(", ")
        )
    }))
}

/// Make the git directory `git_dir` over the ledger root `root`, an
/// absolute path, whose `origin` is `url`, naming the machine `name` gives
/// or another; that machine when this call made it, `None` when another
/// command that joined the same ledger at the same moment made it first.
///
/// It is made whole in a scratch directory beside [`ledger_repositories`]
/// and only then renamed into it, because every command reads each
/// directory there as a git directory: half made, one with no origin yet
/// would carry nothing, and one with no machine would refuse every sweep.
/// Its `core.worktree` is the root, which is not made: a root that does not
/// exist yet holds no entries, and the first write or pull makes it. Its
/// `origin` is `url` as the registry spells it, credentials and all, since
/// git reaches the remote through it. Every text this gives shows `url`
/// with no userinfo ([`crate::remote_url::without_userinfo`]), so that no
/// password or token in it reaches a terminal or a log.
///
/// Before the rename, the remote is asked for `refs/knives/<machine>`. A
/// [`Name::Session`] it holds is this session's own, from a git directory
/// whose disk or config home is gone, and is taken up
/// ([`crate::ledger_git::take_up_own_ref`]): made without that history,
/// this one would refuse its own ref at the first write, and the claims
/// taken under it could be released only from another machine. A
/// [`Name::Host`] it holds is another config home's, which drew the same
/// digits or was given the name by hand, and may still be in use: taken
/// up, that ref would have two writers, and the first push of either would
/// leave the other refused at every write after. So nothing is taken up,
/// and the directory names this machine [`machine_name`] with other random
/// digits. A remote that cannot be asked, or a fetch that fails, makes
/// nothing, so the next command asks again rather than find a git
/// directory that refuses its own ref or shares a live one's.
fn make(root: &Path, git_dir: &Path, url: &str, name: &Name) -> Result<Option<String>, String> {
    let shown = crate::remote_url::without_userinfo(url);
    let failed = |cause: String| {
        format!(
            "could not make {}: {}",
            git_dir.display(),
            cause.replace(url, &shown)
        )
    };
    let repositories = ledger_repositories(root);
    let home = repositories.parent().unwrap_or(root);
    std::fs::create_dir_all(home)
        .map_err(|error| failed(format!("making {}: {error}", home.display())))?;
    let scratch = tempfile::Builder::new()
        .prefix(".knives-joining-")
        .tempdir_in(home)
        .map_err(|error| {
            failed(format!(
                "making a scratch directory in {}: {error}",
                home.display()
            ))
        })?;
    let made = scratch.path().join("ledger.git");
    let os = OsStr::new;
    let configure = |args: &[&OsStr]| {
        git(&[[os("--git-dir"), made.as_os_str()].as_slice(), args].concat()).map_err(&failed)
    };
    git(&[os("init"), os("--quiet"), os("--bare"), made.as_os_str()]).map_err(&failed)?;
    for args in [
        [os("config"), os("core.bare"), os("false")].as_slice(),
        &[os("config"), os("core.worktree"), root.as_os_str()],
        &[os("remote"), os("add"), os(REMOTE), os(url)],
    ] {
        configure(args)?;
    }
    let repository = crate::ledger_git::Repository::new(&made, root, std::iter::empty())
        .map_err(|error| failed(error.to_string()))?;
    let machine = match name {
        Name::Session(machine) => crate::ledger_git::take_up_own_ref(&repository, REMOTE, machine)
            .map(|()| machine.clone()),
        Name::Host(machine) => {
            crate::ledger_git::remote_has_machine_ref(&repository, REMOTE, machine).map(|held| {
                if held {
                    machine_name(&crate::liveness::host_name(), crate::lock::random_u64())
                } else {
                    machine.clone()
                }
            })
        }
    }
    .map_err(|error| {
        let (Name::Session(asked) | Name::Host(asked)) = name;
        failed(format!(
            "asking {shown} for this machine's ref, refs/knives/{asked}: {error}"
        ))
    })?;
    configure(&[os("config"), os("knives.machine"), os(machine.as_str())])?;
    std::fs::create_dir_all(&repositories)
        .map_err(|error| failed(format!("making {}: {error}", repositories.display())))?;
    match std::fs::rename(&made, git_dir) {
        Ok(()) => Ok(Some(machine)),
        // Another command joined first: what is there is its directory.
        Err(_) if git_dir.symlink_metadata().is_ok() => Ok(None),
        Err(error) => Err(failed(error.to_string())),
    }
}

/// Run `git <args>`; git's own words when it fails.
fn git(args: &[&OsStr]) -> Result<(), String> {
    crate::ledger_git::run(crate::bind::git_command().args(args))
        .map(drop)
        .map_err(|error| error.to_string())
}

/// `fork_url` with its repository path replaced by `ledger`, its
/// `<owner>/<name>`: the scheme, user, host and port kept as spelled, and a
/// `.git` suffix too when the path had one. `None` for a value that names no
/// repository on a host: a filesystem path, or a `file://` URL, whose
/// authority is empty. Read by the rule that finds which git directory has
/// the URL made as its origin ([`crate::remote_url::authority_and_path`]),
/// so that the directory a join makes is the one every later read finds.
///
/// A forge serves a fork and the ledger its owners share from one host, so
/// the fork's own URL says how this machine reaches the ledger, with the
/// credentials and the protocol it already uses there.
fn ledger_url(fork_url: &str, ledger: &str) -> Option<String> {
    let url = fork_url.trim().trim_end_matches('/');
    let (authority, path) = crate::remote_url::authority_and_path(url)?;
    let repository = path.trim_start_matches('/');
    if authority.is_empty() || repository.is_empty() {
        return None;
    }
    let kept = url.strip_suffix(repository)?;
    let suffix = repository
        .strip_prefix(crate::remote_url::without_git_suffix(repository))
        .unwrap_or_default();
    Some(format!("{kept}{ledger}{suffix}"))
}

/// The name a join gives this machine ([`this_machine`]); its kind says
/// whose the ref the remote may already hold under it is ([`make`]).
#[derive(Debug, PartialEq, Eq)]
enum Name {
    /// [`session_machine_name`]'s, for a caller in a harness session. No
    /// other session has it, so the ref the remote holds under it is this
    /// session's own, from an earlier config home of it.
    Session(String),
    /// [`machine_name`]'s, for a caller in no harness session: random
    /// digits, so the ref the remote holds under it is never this config
    /// home's but another's, which drew the same digits or was given the
    /// name by hand, and may still be in use.
    Host(String),
}

/// This machine's name on a ledger it joins, before the remote is asked
/// ([`make`]): for a caller in a harness session
/// ([`crate::commands::claim::harness_session`]), [`session_machine_name`]
/// of its host name, its `/etc/machine-id` and that session; else
/// [`machine_name`] of its host name and a random number.
fn this_machine() -> Name {
    let host = crate::liveness::host_name();
    crate::commands::claim::harness_session().map_or_else(
        || Name::Host(machine_name(&host, crate::lock::random_u64())),
        |(_, owner)| {
            let id = std::fs::read_to_string("/etc/machine-id").ok();
            Name::Session(session_machine_name(&host, id.as_deref(), &owner))
        },
    )
}

/// `<short host name>-<8 hex digits>`, in the form machines set up by hand
/// to share a ledger are named: the host name as [`named`] keeps it, and
/// the low 32 bits of `random` as eight hex digits. The name of a machine
/// that joins in no harness session, and of one whose first name the
/// remote already holds ([`make`]).
///
/// The digits are random, not the machine id's: two config homes of one
/// host that join before either pushes would otherwise share a name, and
/// the second of them to push would be refused at every write.
fn machine_name(host: &str, random: u64) -> String {
    named(host, &format!("{:08x}", random & 0xffff_ffff))
}

/// `<short host name>-<8 hex digits>` for a machine that joins in the
/// harness session `owner`: the host name as [`named`] keeps it, and the
/// first eight hex digits of the SHA-256 of `machine_id` trimmed (empty when
/// there is none), a newline, and `owner`.
///
/// A host runs many sessions, and each that joins with a config home of its
/// own commits to a ref of its own: under one name, the second to push would
/// be refused as another writer of the first one's ref. The same session on
/// the same host gets the same name back, so a config home it makes again
/// takes up the ref it committed to ([`make`]); the machine id keeps one
/// session's name on two hosts of one host name apart.
fn session_machine_name(host: &str, machine_id: Option<&str>, owner: &str) -> String {
    use sha2::Digest as _;
    let [a, b, c, d, ..]: [u8; 32] = sha2::Sha256::new()
        .chain_update(machine_id.unwrap_or_default().trim())
        .chain_update("\n")
        .chain_update(owner)
        .finalize()
        .into();
    named(host, &format!("{:08x}", u32::from_be_bytes([a, b, c, d])))
}

/// `<short host name>-<digits>`: the host name up to its first `.`, and
/// `digits`.
///
/// The name spells this machine's ref, `refs/knives/<machine>`, as one ref
/// component ([`crate::ledger_git::ref_component`]). So each character of
/// the host name no component can hold (a control character, a space, any
/// of `~^:?*[\/`) becomes `-`, as does the `{` of `@{`, and the name never
/// starts with `-`: a host name that would leave nothing gives the digits
/// alone.
fn named(host: &str, digits: &str) -> String {
    let short: String = host
        .split('.')
        .next()
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_control() || " ~^:?*[\\/".contains(character) {
                '-'
            } else {
                character
            }
        })
        .collect();
    let short = short.replace("@{", "@-");
    let short = short.trim_start_matches('-');
    if short.is_empty() {
        digits.to_owned()
    } else {
        format!("{short}-{digits}")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::ids::CommitId;
    use crate::ledger::{Draft, Ledger, Scribe};
    use crate::ledger_sweep::{
        Swept, destinations, lab, ledger_of, pull_at, shared_fork, unpullable,
    };

    /// A machine id, as `/etc/machine-id` holds one.
    const MACHINE_ID: &str = "0123abcd4567ef890123abcd4567ef89\n";

    /// A config home with a registry naming `forks`, each `(key, origin,
    /// ledger)`: the fork `acme/<key>`, whose upstream is on the forge, and
    /// no ledger root or git directory at all, as a new machine has.
    fn new_machine(forks: &[(&str, &str, Option<&str>)]) -> (tempfile::TempDir, PathBuf, Registry) {
        use std::fmt::Write as _;
        let home = tempfile::tempdir().unwrap();
        let mut text = String::new();
        for (key, origin, ledger) in forks {
            let _ = writeln!(
                text,
                "[repos.{key}]\nupstream = \"https://forge.invalid/acme/{key}\"\n\
                 origin = \"{origin}\"\n{}",
                ledger.map_or_else(String::new, |ledger| format!("ledger = \"{ledger}\"\n"))
            );
        }
        let path = home.path().join("repos.toml");
        std::fs::write(&path, text).unwrap();
        let registry = crate::config::load(&path).unwrap();
        let root = home.path().join("ledger");
        (home, root, registry)
    }

    /// `git --git-dir <git_dir> config <key>`'s value, or `None`.
    fn config(git_dir: &Path, key: &str) -> Option<String> {
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(["config", "--get", key])
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }

    /// Every name in `directory`, or none when it does not exist.
    fn listed(directory: &Path) -> BTreeSet<String> {
        std::fs::read_dir(directory).map_or_else(
            |_| BTreeSet::new(),
            |listing| {
                listing
                    .map(|dirent| dirent.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            },
        )
    }

    /// Write an entry of `acme/a-repo` under the ledger root `root`, as a
    /// write records one; its path.
    fn noted(root: &Path) -> PathBuf {
        let (_, path) = Scribe::unanchored(
            Ledger::at(root.join("acme/a-repo")),
            UpstreamName::new("acme/a-repo"),
            "owner-one".to_owned(),
        )
        .record_to(&Draft::event(None, "noted".into(), None))
        .unwrap();
        path
    }

    /// The ledger's remote: a bare repository git reaches at `url` through
    /// the URL rule a session hands it (`GIT_CONFIG_COUNT` and its numbered
    /// pair), with the caller in a harness session or in none; both are this
    /// process's environment until it drops.
    struct Remote {
        git_dir: PathBuf,
        dir: tempfile::TempDir,
        environment: crate::config::test_support::EnvironmentGuard,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Remote {
        /// [`Remote::reached_in`] the harness session `owner-one`.
        fn reached_at(url: &str) -> Self {
            Self::reached_in(url, Some("owner-one"))
        }

        /// The remote reached at `url`, with the caller in the harness
        /// session `owner`, or in none.
        fn reached_in(url: &str, owner: Option<&str>) -> Self {
            let lock = crate::config::test_support::environment_lock();
            let environment = crate::config::test_support::no_harness_session(&[
                "GIT_CONFIG_COUNT",
                "GIT_CONFIG_KEY_0",
                "GIT_CONFIG_VALUE_0",
                "GIT_TRACE",
            ]);
            if let Some(owner) = owner {
                environment.set("KNIVES_OWNER", owner);
            }
            let dir = tempfile::tempdir().unwrap();
            let git_dir = dir.path().join("ledger.git");
            lab::init(&git_dir, true);
            environment.set("GIT_CONFIG_COUNT", "1");
            environment.set(
                "GIT_CONFIG_KEY_0",
                &format!("url.{}.insteadOf", git_dir.display()),
            );
            environment.set("GIT_CONFIG_VALUE_0", url);
            Self {
                git_dir,
                dir,
                environment,
                _lock: lock,
            }
        }

        /// Have every git command that starts until this drops append what
        /// it runs, with its arguments, to the log [`Remote::traced`] reads.
        fn trace_git(&self) {
            self.environment
                .set("GIT_TRACE", self.git_log().to_str().unwrap());
        }

        /// Every git command [`Remote::trace_git`] logged so far.
        fn traced(&self) -> String {
            std::fs::read_to_string(self.git_log()).unwrap_or_default()
        }

        fn git_log(&self) -> PathBuf {
            self.dir.path().join("git.log")
        }

        /// Point `reference` here at a new commit of an empty tree; that commit.
        fn commit_at(&self, reference: &str) -> CommitId {
            let tree = lab::git(&self.git_dir, &["mktree"]);
            let commit = lab::git(
                &self.git_dir,
                &[
                    "commit-tree",
                    &tree,
                    "-m",
                    "committed by an earlier config home",
                ],
            );
            lab::git(&self.git_dir, &["update-ref", reference, &commit]);
            CommitId::new(commit)
        }
    }

    #[test]
    fn a_ledger_no_git_directory_reaches_is_joined_once_and_then_carries_its_forks() {
        // Given: a new machine whose registry shares two forks' ledger with
        // acme/ledger, and one fork's not at all, and that ledger's remote,
        // which holds nothing yet.
        let _remote = Remote::reached_at("https://forge.invalid/acme/ledger.git");
        let (home, root, registry) = new_machine(&[
            (
                "a-repo",
                "https://forge.invalid/ours/a-repo.git",
                Some("acme/ledger"),
            ),
            (
                "b-repo",
                "https://forge.invalid/ours/b-repo",
                Some("acme/ledger"),
            ),
            ("c-repo", "https://forge.invalid/ours/c-repo", None),
        ]);

        // When: where the ledger travels is read, as a sweep or a pull does.
        let mut said = Vec::new();
        let found =
            super::super::destinations::destinations_telling(&root, &registry, None, &mut said)
                .unwrap();

        // Then: the git directory is made, over the root, reaching the
        // ledger on the fork's forge, and naming this machine.
        let git_dir = ledger_repositories(&root).join("acme--ledger.git");
        assert_eq!(config(&git_dir, "core.bare").as_deref(), Some("false"));
        assert_eq!(
            config(&git_dir, "core.worktree"),
            Some(std::path::absolute(&root).unwrap().display().to_string())
        );
        assert_eq!(
            config(&git_dir, "remote.origin.url").as_deref(),
            Some("https://forge.invalid/acme/ledger.git")
        );
        let machine = config(&git_dir, "knives.machine").unwrap();
        let (_, digits) = machine.rsplit_once('-').unwrap();
        assert_eq!(digits.len(), 8, "{machine}");
        assert!(
            digits.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{machine}"
        );
        crate::ledger_git::ref_component("machine", &machine).unwrap();
        // And: it says so in one line, naming the forks it carries.
        assert_eq!(
            String::from_utf8(said).unwrap(),
            format!(
                "joined the ledger acme/ledger: made {} as machine {machine}, carrying \
                 acme/a-repo, acme/b-repo\n",
                std::path::absolute(&git_dir).unwrap().display()
            )
        );
        // And: the directory is one of the destinations, carrying both forks,
        // and no fork is unreached; nothing else was made.
        assert!(found.unreached.is_empty(), "{:?}", found.unreached);
        let [destination] = found.found.as_slice() else {
            panic!("{found:?}");
        };
        assert_eq!(destination.machine, machine);
        assert_eq!(
            destination.repository.forks(),
            &BTreeSet::from([
                UpstreamName::new("acme/a-repo"),
                UpstreamName::new("acme/b-repo")
            ])
        );
        assert!(!root.exists(), "the ledger root was made");
        assert_eq!(
            listed(home.path()),
            BTreeSet::from(["ledger-repositories".to_owned(), "repos.toml".to_owned()])
        );

        // When: it is read again.
        let mut again = Vec::new();
        let second =
            super::super::destinations::destinations_telling(&root, &registry, None, &mut again)
                .unwrap();

        // Then: it finds the directory, makes nothing and says nothing.
        assert!(again.is_empty(), "{}", String::from_utf8_lossy(&again));
        assert_eq!(
            listed(&ledger_repositories(&root)),
            BTreeSet::from(["acme--ledger.git".to_owned()])
        );
        assert_eq!(second.found.len(), 1);
        assert_eq!(config(&git_dir, "knives.machine"), Some(machine));
    }

    #[test]
    fn a_dry_run_joins_nothing_and_says_the_run_would_join_the_ledger() {
        // Given: a new machine whose registry shares a fork's ledger.
        let (home, root, registry) = new_machine(&[(
            "a-repo",
            "https://forge.invalid/ours/a-repo",
            Some("acme/ledger"),
        )]);
        let fork = UpstreamName::new("acme/a-repo");

        // When: a dry run asks what a pull would hold back.
        let problems = unpullable(&root, &registry, &[&fork], &[]);

        // Then: nothing is made, and the fork's problem says the run that is
        // not a dry run joins its ledger first.
        assert_eq!(
            listed(home.path()),
            BTreeSet::from(["repos.toml".to_owned()])
        );
        let said = problems.get(&fork).unwrap().join("\n");
        assert!(
            said.contains("has acme/ledger as its origin yet; a run that is not a dry run joins"),
            "{said}"
        );
        assert!(
            said.contains(&joined_git_dir(&root, "acme/ledger").display().to_string()),
            "{said}"
        );
    }

    #[test]
    fn a_fork_with_no_forge_origin_takes_the_ledger_url_from_its_upstream() {
        // Given: a fork whose origin is a directory on this machine, and the
        // ledger's remote on the upstream's forge.
        let _remote = Remote::reached_at("https://forge.invalid/acme/ledger");
        let (_home, root, registry) =
            new_machine(&[("a-repo", "/srv/ours/a-repo", Some("acme/ledger"))]);

        // When: its ledger is joined.
        destinations(&root, &registry).unwrap();

        // Then: the ledger's URL is on the upstream's forge.
        assert_eq!(
            config(&joined_git_dir(&root, "acme/ledger"), "remote.origin.url").as_deref(),
            Some("https://forge.invalid/acme/ledger")
        );
    }

    #[test]
    fn a_join_takes_up_the_ref_the_remote_holds_under_this_machines_name() {
        // Given: a caller in a harness session, a new machine whose registry
        // shares a fork's ledger, and a remote that already holds a commit
        // under the name this session joins as, as a config home of it that
        // wrote there and then was deleted leaves it.
        let remote = Remote::reached_at("https://forge.invalid/acme/ledger");
        let (_home, root, registry) = new_machine(&[(
            "a-repo",
            "https://forge.invalid/ours/a-repo",
            Some("acme/ledger"),
        )]);
        let Name::Session(machine) = this_machine() else {
            panic!("a caller in a harness session joins under the host's name");
        };
        let own = format!("refs/knives/{machine}");
        let commit = remote.commit_at(&own);

        // When: where the ledger travels is read.
        let found = destinations(&root, &registry).unwrap();

        // Then: the git directory it made names that machine, and holds the
        // remote's commit as this machine's own ref and as the remote's copy
        // of it.
        let git_dir = joined_git_dir(&root, "acme/ledger");
        assert_eq!(config(&git_dir, "knives.machine"), Some(machine.clone()));
        for reference in [&own, &format!("refs/knives-remotes/origin/{machine}")] {
            assert_eq!(
                lab::at(&git_dir, reference).as_ref(),
                Some(&commit),
                "{reference}"
            );
        }
        // And: a write as that machine is not refused as another's.
        let [destination] = found.found.as_slice() else {
            panic!("{found:?}");
        };
        assert!(!crate::ledger_git::has_new_entries(&destination.repository, &machine).unwrap());
        // And: the join sent nothing: the remote's ref is where it was.
        assert_eq!(lab::at(&remote.git_dir, &own), Some(commit));
    }

    #[test]
    fn a_join_in_no_session_whose_name_the_remote_holds_takes_another_and_takes_up_nothing() {
        // Given: a caller in no harness session, a new machine whose registry
        // shares a fork's ledger, and a remote that already holds a commit
        // under the name this join is to take, as another config home of
        // this host, still in use, leaves it.
        let remote = Remote::reached_in("https://forge.invalid/acme/ledger", None);
        let (_home, root, registry) = new_machine(&[(
            "a-repo",
            "https://forge.invalid/ours/a-repo",
            Some("acme/ledger"),
        )]);
        assert!(matches!(this_machine(), Name::Host(_)));
        let host = machine_name(&crate::liveness::host_name(), 0x0123_abcd);
        let theirs = format!("refs/knives/{host}");
        let commit = remote.commit_at(&theirs);

        // When: the ledger is joined under that name.
        let said = join(
            &root,
            &registry,
            "acme/ledger",
            &["acme/a-repo"],
            &Name::Host(host.clone()),
        )
        .unwrap()
        .unwrap();

        // Then: the git directory names another machine of this host, with
        // other random digits, and says so.
        let git_dir = joined_git_dir(&root, "acme/ledger");
        let machine = config(&git_dir, "knives.machine").unwrap();
        assert_ne!(machine, host);
        let short = host.strip_suffix("0123abcd").unwrap();
        let digits = machine.strip_prefix(short).unwrap();
        assert!(
            digits.len() == 8 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{machine}"
        );
        assert_eq!(
            said,
            format!(
                "joined the ledger acme/ledger: made {} as machine {machine}, carrying \
                 acme/a-repo",
                git_dir.display()
            )
        );
        // And: it took up nothing the remote holds.
        assert_eq!(
            lab::git(
                &git_dir,
                &["for-each-ref", "refs/knives/", "refs/knives-remotes/"]
            ),
            ""
        );

        // When: an entry this config home writes is sent.
        noted(&root);
        let swept = crate::ledger_sweep::run(&root, Ok(registry)).unwrap();

        // Then: it reached the remote under the new name, and the ref of the
        // first name is where it was.
        let Swept::Ran { problems, .. } = swept else {
            panic!("{swept:?}");
        };
        assert_eq!(problems, Vec::<String>::new());
        assert!(lab::at(&remote.git_dir, &format!("refs/knives/{machine}")).is_some());
        assert_eq!(lab::at(&remote.git_dir, &theirs), Some(commit));
    }

    #[test]
    fn two_joins_in_no_session_on_one_host_take_two_random_names_neither_the_machine_ids() {
        // Given: a caller in no harness session, two new config homes on
        // this host whose registries share a fork's ledger, and a remote
        // that holds nothing.
        let remote = Remote::reached_in("https://forge.invalid/acme/ledger", None);
        let homes = [(); 2].map(|()| {
            new_machine(&[(
                "a-repo",
                "https://forge.invalid/ours/a-repo",
                Some("acme/ledger"),
            )])
        });

        // When: where the ledger travels is read in each, as a sweep or a
        // pull does, before either has sent anything.
        let names = homes.each_ref().map(|(_, root, registry)| {
            destinations(root, registry).unwrap();
            config(&joined_git_dir(root, "acme/ledger"), "knives.machine").unwrap()
        });

        // Then: each git directory names a machine of this host with eight
        // hex digits, none of them the first eight of its machine id.
        let host = crate::liveness::host_name();
        let id = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
        for machine in &names {
            let digits = machine
                .rsplit_once('-')
                .map_or(machine.as_str(), |(_, digits)| digits);
            assert!(
                digits.len() == 8 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "{machine}"
            );
            assert_eq!(machine, &named(&host, digits));
            assert!(!id.starts_with(digits), "{machine}: {id}");
        }
        // And: the two names differ, and neither join sent anything.
        let [one, two] = &names;
        assert_ne!(one, two);
        assert_eq!(lab::git(&remote.git_dir, &["for-each-ref"]), "");
    }

    #[test]
    fn a_ledger_whose_remote_cannot_be_asked_is_not_joined_and_the_next_read_tries_again() {
        // Given: a new machine whose registry shares a fork's ledger on a
        // forge whose host does not resolve.
        let (home, root, registry) = new_machine(&[(
            "a-repo",
            "https://forge.invalid/ours/a-repo",
            Some("acme/ledger"),
        )]);
        let fork = UpstreamName::new("acme/a-repo");
        {
            // The lock keeps the remote unreachable: another test's `Remote`
            // reaches the same URL while it holds it. It is released before
            // this test's own `Remote` takes it.
            let _lock = crate::config::test_support::environment_lock();

            // When: where the ledger travels is read.
            let mut said = Vec::new();
            let found =
                super::super::destinations::destinations_telling(&root, &registry, None, &mut said)
                    .unwrap();

            // Then: nothing is made or said, and the fork is unreached, with
            // why: the remote could not be asked for this machine's ref.
            assert_eq!(
                listed(home.path()),
                BTreeSet::from(["repos.toml".to_owned()])
            );
            assert!(said.is_empty(), "{}", String::from_utf8_lossy(&said));
            let why = found
                .unreached
                .get(&fork)
                .and_then(|unreached| unreached.join_failed.as_deref())
                .unwrap();
            assert!(
                why.starts_with(&format!(
                    "could not make {}: asking https://forge.invalid/acme/ledger for this \
                     machine's ref, refs/knives/",
                    std::path::absolute(joined_git_dir(&root, "acme/ledger"))
                        .unwrap()
                        .display()
                )),
                "{why}"
            );
        }

        // When: the remote can be reached, and it is read again.
        let _remote = Remote::reached_at("https://forge.invalid/acme/ledger");
        let again = destinations(&root, &registry).unwrap();

        // Then: the ledger is joined.
        assert!(again.unreached.is_empty(), "{:?}", again.unreached);
        assert_eq!(again.found.len(), 1);
    }

    #[test]
    fn a_join_whose_remote_cannot_be_asked_shows_its_url_with_no_password() {
        // Given: a new machine whose registry shares a fork's ledger through
        // an origin URL holding a user and a password, on a forge whose host
        // does not resolve.
        let (_home, root, registry) = new_machine(&[(
            "a-repo",
            "https://someone:secret@forge.invalid/ours/a-repo",
            Some("acme/ledger"),
        )]);
        let fork = UpstreamName::new("acme/a-repo");
        // The lock keeps the remote unreachable: no other test's `Remote`
        // reaches forge.invalid while it is held.
        let _lock = crate::config::test_support::environment_lock();

        // When: the fork is routed to its ledger, as `start` does, and a
        // sweep runs.
        let refused = shared_fork(
            &root,
            &registry,
            &fork,
            ledger_of(&registry, &fork).unwrap(),
        )
        .unwrap_err()
        .to_string();
        let swept = crate::ledger_sweep::run(&root, Ok(registry)).unwrap();

        // Then: the refusal, the sweep's problems and its log each say the
        // ledger could not be joined, naming the URL it asked with no
        // userinfo, and none of them holds the password.
        let Swept::Ran { problems, .. } = swept else {
            panic!("{swept:?}");
        };
        let log = std::fs::read_to_string(crate::ledger_sweep::sweep_log(&root)).unwrap();
        for said in [refused, problems.join("\n"), log] {
            assert!(
                said.contains("asking https://forge.invalid/acme/ledger for this machine's ref"),
                "{said}"
            );
            assert!(!said.contains("secret"), "{said}");
        }
    }

    #[test]
    fn routing_a_fork_joins_its_own_ledger_alone_and_a_sweep_joins_every_one() {
        // Given: a new machine whose registry shares one fork's ledger with
        // acme/ledger, whose remote answers, and another fork's with
        // acme/elsewhere, on a forge whose host does not resolve; and a log
        // of every git command run from here on.
        let remote = Remote::reached_at("https://forge.invalid/acme/ledger");
        let (_home, root, registry) = new_machine(&[
            (
                "a-repo",
                "https://forge.invalid/ours/a-repo",
                Some("acme/ledger"),
            ),
            (
                "b-repo",
                "https://forge.invalid/ours/b-repo",
                Some("acme/elsewhere"),
            ),
        ]);
        remote.trace_git();
        let a_repo = UpstreamName::new("acme/a-repo");

        // When: the first fork is routed to its ledger, as `start` and
        // `finish` do.
        let ledger = ledger_of(&registry, &a_repo).unwrap();
        let shared = shared_fork(&root, &registry, &a_repo, ledger).unwrap();

        // Then: its ledger is joined, and carries it.
        assert_eq!(
            shared.destination.repository.git_dir(),
            joined_git_dir(&root, "acme/ledger")
        );
        // And: the other ledger was neither joined nor asked: no git
        // directory is made for it, and no git command named it.
        assert_eq!(
            listed(&ledger_repositories(&root)),
            BTreeSet::from(["acme--ledger.git".to_owned()])
        );
        let traced = remote.traced();
        assert!(!traced.contains("acme/elsewhere"), "{traced}");

        // When: the first fork is pulled, as a report about it does.
        let pulled = pull_at(&root, &registry, &[&a_repo]);

        // Then: the pull found no problem, and nothing asked the other
        // ledger either.
        assert_eq!(pulled.problems_for(&a_repo), Vec::<String>::new());
        let traced = remote.traced();
        assert!(!traced.contains("acme/elsewhere"), "{traced}");

        // When: a sweep runs.
        let swept = crate::ledger_sweep::run(&root, Ok(registry)).unwrap();

        // Then: it tries to join every ledger the registry names, and says
        // why the other could not be joined.
        let Swept::Ran { problems, .. } = swept else {
            panic!("{swept:?}");
        };
        assert!(
            problems
                .iter()
                .any(|problem| problem.starts_with("acme/b-repo: ")
                    && problem.contains("and joining it failed")),
            "{problems:?}"
        );
        assert!(
            remote.traced().contains("acme/elsewhere"),
            "{}",
            remote.traced()
        );
    }

    #[test]
    fn a_routed_read_lists_as_unreached_only_the_forks_it_routes() {
        // Given: a new machine whose registry shares one fork's ledger with
        // acme/ledger and another fork's with acme/elsewhere, both on a forge
        // whose host does not resolve.
        let (_home, root, registry) = new_machine(&[
            (
                "a-repo",
                "https://forge.invalid/ours/a-repo",
                Some("acme/ledger"),
            ),
            (
                "b-repo",
                "https://forge.invalid/ours/b-repo",
                Some("acme/elsewhere"),
            ),
        ]);
        let a_repo = UpstreamName::new("acme/a-repo");
        // The lock keeps both remotes unreachable: no other test's `Remote`
        // reaches forge.invalid while it is held.
        let _lock = crate::config::test_support::environment_lock();

        // When: where the ledger travels is read for the first fork alone,
        // as `start`, a pull of that fork and a send of its entries do.
        let found = super::super::destinations::destinations_telling(
            &root,
            &registry,
            Some(&[&a_repo]),
            &mut std::io::sink(),
        )
        .unwrap();

        // Then: the first fork is unreached, with why its ledger could not be
        // joined; the other fork is not listed at all, since nothing tried to
        // join its ledger and this is no dry run.
        assert_eq!(
            found.unreached.keys().collect::<Vec<_>>(),
            vec![&a_repo],
            "{:?}",
            found.unreached
        );
        let why = found
            .unreached
            .get(&a_repo)
            .and_then(|unreached| unreached.join_failed.as_deref())
            .unwrap();
        assert!(why.starts_with("could not make "), "{why}");
    }

    #[test]
    fn a_write_sends_through_its_own_ledger_alone_and_asks_no_other() {
        // Given: a new machine whose registry shares one fork's ledger with
        // acme/ledger, whose remote answers, and another fork's with
        // acme/elsewhere, on a forge whose host does not resolve; an entry a
        // write recorded about the first fork; and a log of every git
        // command run from here on.
        let remote = Remote::reached_at("https://forge.invalid/acme/ledger");
        let (_home, root, registry) = new_machine(&[
            (
                "a-repo",
                "https://forge.invalid/ours/a-repo",
                Some("acme/ledger"),
            ),
            (
                "b-repo",
                "https://forge.invalid/ours/b-repo",
                Some("acme/elsewhere"),
            ),
        ]);
        let written = noted(&root);
        let appended: Vec<crate::ledger::Appended> = crate::ledger::appended()
            .into_iter()
            .filter(|appended| appended.path.starts_with(&root))
            .collect();
        remote.trace_git();

        // When: the write's entry is sent, as the command exits.
        let sent = crate::ledger_sweep::send_now(&root, &registry, &appended);

        // Then: it reached the first fork's ledger, joined for it, under
        // this machine's ref.
        assert_eq!(sent.unsent, Vec::new());
        assert_eq!(sent.unlogged, None);
        let machine = config(&joined_git_dir(&root, "acme/ledger"), "knives.machine").unwrap();
        let carried = lab::git(
            &remote.git_dir,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                &format!("refs/knives/{machine}"),
            ],
        );
        let relative = written.strip_prefix(&root).unwrap();
        assert!(
            carried.lines().any(|path| Path::new(path) == relative),
            "{carried}"
        );
        // And: the other ledger was neither joined nor asked, and the sweep
        // log says nothing of its fork.
        assert_eq!(
            listed(&ledger_repositories(&root)),
            BTreeSet::from(["acme--ledger.git".to_owned()])
        );
        let log =
            std::fs::read_to_string(crate::ledger_sweep::sweep_log(&root)).unwrap_or_default();
        assert!(!log.contains("acme/b-repo"), "{log}");
        assert!(
            !remote.traced().contains("acme/elsewhere"),
            "{}",
            remote.traced()
        );
    }

    #[test]
    fn the_ledger_url_is_the_fork_url_with_the_ledger_as_its_repository() {
        for (fork, ledger) in [
            (
                "https://forge.invalid/ours/a-repo",
                "https://forge.invalid/acme/ledger",
            ),
            (
                "https://forge.invalid/ours/a-repo.git",
                "https://forge.invalid/acme/ledger.git",
            ),
            (
                "https://forge.invalid/ours/a-repo.GIT/",
                "https://forge.invalid/acme/ledger.GIT",
            ),
            (
                "https://someone@forge.invalid:8443/ours/a-repo",
                "https://someone@forge.invalid:8443/acme/ledger",
            ),
            (
                "git@forge.invalid:ours/a-repo.git",
                "git@forge.invalid:acme/ledger.git",
            ),
            (
                "git@forge.invalid:/ours/a-repo",
                "git@forge.invalid:/acme/ledger",
            ),
            (
                "ssh://git@forge.invalid/ours/a-repo.git",
                "ssh://git@forge.invalid/acme/ledger.git",
            ),
            (
                "ssh://git@forge.invalid:2222/ours/a-repo",
                "ssh://git@forge.invalid:2222/acme/ledger",
            ),
            (
                "forge.invalid:ours/a-repo.git",
                "forge.invalid:acme/ledger.git",
            ),
        ] {
            assert_eq!(
                ledger_url(fork, "acme/ledger").as_deref(),
                Some(ledger),
                "{fork}"
            );
            // The rule that finds which git directory has it as its origin
            // reads it as the ledger's.
            assert_eq!(
                crate::remote_url::remote_slug(ledger),
                Some("acme/ledger"),
                "{ledger}"
            );
        }
        for not_a_forge in [
            "/srv/ours/a-repo",
            "file:///srv/ours/a-repo.git",
            "https://forge.invalid",
            "ssh://git@forge.invalid:2222",
        ] {
            assert_eq!(
                ledger_url(not_a_forge, "acme/ledger"),
                None,
                "{not_a_forge}"
            );
        }
    }

    #[test]
    fn a_machine_is_named_for_its_short_host_name_and_eight_random_digits() {
        // The digits are the low 32 bits of the random number, as eight
        // hex digits, zero-padded.
        assert_eq!(
            machine_name("box-one.example.invalid", 0x1234_5678_9abc_def0),
            "box-one-9abcdef0"
        );
        assert_eq!(machine_name("box-one", 7), "box-one-00000007");
        // A host name character no ref component holds becomes `-`, and the
        // name never starts with one.
        for (host, name) in [
            (
                "box one:two~3^4?5*6[7\\8\u{7}9",
                "box-one-two-3-4-5-6-7-8-9-0123abcd",
            ),
            ("box@{1}", "box@-1}-0123abcd"),
            ("-box", "box-0123abcd"),
            (".hidden", "0123abcd"),
            ("", "0123abcd"),
        ] {
            let named = machine_name(host, 0x0123_abcd);
            assert_eq!(named, name, "{host:?}");
            crate::ledger_git::ref_component("machine", &named).unwrap();
        }
    }

    #[test]
    fn a_machine_joined_in_a_session_is_named_for_its_host_machine_id_and_session() {
        // One host, machine id and session give one name: the first eight
        // hex digits of the SHA-256 of the trimmed id, a newline and the
        // session.
        let id = Some(MACHINE_ID);
        let host = "box-one.example.invalid";
        assert_eq!(
            session_machine_name(host, id, "owner-one"),
            "box-one-e6fa3479"
        );
        // Another session on the same host gets another name.
        assert_eq!(
            session_machine_name(host, id, "owner-two"),
            "box-one-46e08708"
        );
        // A missing machine id, or one that is empty once trimmed, still
        // gives one name per session.
        for missing in [None, Some(""), Some(" \n")] {
            assert_eq!(
                session_machine_name(host, missing, "owner-one"),
                "box-one-04164234",
                "{missing:?}"
            );
            assert_eq!(
                session_machine_name(host, missing, "owner-two"),
                "box-one-627a5184",
                "{missing:?}"
            );
        }
        // The host name is kept as outside a session: each character no ref
        // component holds becomes `-`, and the name never starts with one.
        for (host, name) in [
            (
                "box one:two~3^4?5*6[7\\8\u{7}9",
                "box-one-two-3-4-5-6-7-8-9-38c73318",
            ),
            ("box@{1}", "box@-1}-38c73318"),
            ("-box", "box-38c73318"),
            (".hidden", "38c73318"),
            ("", "38c73318"),
        ] {
            let named = session_machine_name(host, Some("0123abcd"), "owner-one");
            assert_eq!(named, name, "{host:?}");
            crate::ledger_git::ref_component("machine", &named).unwrap();
        }
    }

    #[test]
    fn this_machine_is_named_for_its_session_in_a_harness_session_and_its_host_in_none() {
        let _lock = crate::config::test_support::environment_lock();
        let environment = crate::config::test_support::no_harness_session(&[]);
        let host = crate::liveness::host_name();
        let id = std::fs::read_to_string("/etc/machine-id").ok();

        // In no harness session, the name is the host's: its host name and
        // eight random digits.
        let Name::Host(named) = this_machine() else {
            panic!("a caller in no harness session joins under a session's name");
        };
        crate::ledger_git::ref_component("machine", &named).unwrap();
        let digits = named
            .rsplit_once('-')
            .map_or(named.as_str(), |(_, digits)| digits);
        assert_eq!(
            named,
            machine_name(&host, u64::from_str_radix(digits, 16).unwrap())
        );

        // In one, it is that session's on this host.
        environment.set("KNIVES_OWNER", "owner-one");
        assert_eq!(
            this_machine(),
            Name::Session(session_machine_name(&host, id.as_deref(), "owner-one"))
        );
    }
}
