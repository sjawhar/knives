//! Claims on a fork whose registry entry shares its ledger, as two machines
//! meet them through the real binary.
//!
//! A machine is a config home of its own, scanning a `HOME` of its own that
//! holds its own checkout of the lab fork, and sharing the fork's ledger
//! through a ledger root whose `origin` git reaches at one bare remote
//! (`url.<remote>.insteadOf`), as a machine set up by hand shares it.
//! Machine one's checkout is the lab's own; machine two clones the lab's
//! fork into a scan root of its own. Both run in this place (one host, one
//! process namespace), so only their names on the ledger tell them apart,
//! and every owner is named by `KNIVES_OWNER`, but in the tests of an Oh My
//! Pi session, named by `PI_SESSION_ID`, and of no harness session, named
//! by none. A config home that joins the ledger itself names its machine
//! for the session that joins it, or, in no harness session, with random
//! digits, so two config homes here that join it are two machines.
//!
//! Every test here drives the `knives` binary, raw git, `tests/common/lab.rs`
//! and the fake gh of `tests/common/forge_shim.rs` alone, so this file also
//! runs, unchanged, against the release before claims were shared.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/forge_shim.rs"]
mod forge_shim;
#[path = "common/lab.rs"]
mod lab;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use lab::Lab;
use serde_json::Value;
use tempfile::TempDir;

/// The forge repository the lab fork's upstream is reached by. A
/// filesystem upstream names no repository another machine could share,
/// so the registry refuses a `ledger` for it.
const UPSTREAM: &str = "https://forge.invalid/acme/demo";
/// [`UPSTREAM`]'s `<owner>/<name>`: what the ledger and the claim refs keep
/// the lab fork under.
const FORK: &str = "acme/demo";
/// The repository the registry says the fork's ledger belongs to.
const LEDGER: &str = "acme/ledger";
/// [`LEDGER`] as a ledger root's `origin` spells it.
const LEDGER_URL: &str = "https://forge.invalid/acme/ledger";
/// [`LEDGER`] as the `origin` of a git directory knives makes to join it:
/// the registry's `origin` of the fork, with the ledger's path in place of
/// the fork's.
const JOINED_URL: &str = "https://forge.invalid/acme/ledger.git";
/// The branch the tests claim.
const BRANCH: &str = "feat/gamma";
/// A `pre-receive` hook that refuses every push, saying so.
const REFUSE: &str = "#!/bin/sh\necho 'push refused by the test' >&2\nexit 1\n";
/// A claim record as a claim commit holds it.
const CLAIM_RECORD: &str = "repo = \"acme/demo\"\nbranch = \"feat/gamma\"\nowner = \"owner-one\"\n\
    kind = \"harness-session\"\nwhy = \"port it\"\nstarted = \"2026-11-16T00:00:00Z\"\n\
    place = \"box-one 4026531836\"\nmachine = \"machine-one\"\n\
    entry = \"20261116T000000.000000000Z-0000.md\"\n";

/// The registry every config home holds: the lab fork as `demo`, its
/// ledger shared with [`LEDGER`] when `shared`.
fn registry(shared: bool) -> String {
    let ledger = if shared {
        format!("ledger = \"{LEDGER}\"\n")
    } else {
        String::new()
    };
    format!(
        "[repos.demo]\nupstream = \"{UPSTREAM}\"\norigin = \"https://forge.invalid/acme/work.git\"\n\
         {ledger}"
    )
}

/// The claim ref of `branch` on the ledger's remote: one ref component per
/// branch, with `%` written `%25` and `/` written `%2F`.
fn claim_ref(branch: &str) -> String {
    format!(
        "refs/knives-claims/{FORK}/{}",
        branch.replace('%', "%25").replace('/', "%2F")
    )
}

/// A lab whose work checkout reaches its upstream at [`UPSTREAM`].
fn forge_lab() -> Lab {
    let lab = Lab::new();
    lab.upstream_at_forge_url(UPSTREAM);
    lab
}

/// `git --git-dir=<git_dir> <args>`'s trimmed stdout, or `None` when it fails.
fn git_in(git_dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .expect("run git");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `git <args>` in `directory`, whatever it comes to.
fn git_at(directory: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .expect("run git")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Commit everything in the git repository `root`, quietly.
fn commit_all(root: &Path, message: &str) {
    lab::git_output(root, ["add", "--all"]);
    lab::git_output(
        root,
        [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
}

/// Push a commit with no parent whose tree holds `contents` at `path` to
/// `reference` on `remote`, over whatever it held.
fn push_file(remote: &Path, reference: &str, path: &str, contents: &str) {
    let scratch = tempfile::tempdir().expect("scratch repository");
    let root = scratch.path().join("pusher");
    lab::git_repository(&root, &[]);
    std::fs::write(root.join(path), contents).expect("write the file");
    commit_all(&root, "pushed by the test");
    lab::git_output(
        &root,
        [
            "push",
            "--quiet",
            "--force",
            remote.to_str().expect("utf-8"),
            &format!("HEAD:{reference}"),
        ],
    );
}

/// A claim the remote holds: its claim commit, and the record in its
/// `claim.toml`.
struct Held {
    commit: String,
    record: toml::Table,
}

impl Held {
    fn get(&self, key: &str) -> &str {
        self.record
            .get(key)
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("the claim record has no {key}: {:?}", self.record))
    }
}

/// The claim `remote` holds of `branch`, if any.
fn claim_on(remote: &Path, branch: &str) -> Option<Held> {
    let commit = git_in(
        remote,
        &["rev-parse", "--verify", "--quiet", &claim_ref(branch)],
    )?;
    let record = git_in(
        remote,
        &["cat-file", "blob", &format!("{commit}:claim.toml")],
    )
    .expect("a claim commit holds claim.toml");
    Some(Held {
        commit,
        record: toml::from_str(&record).expect("the claim record reads"),
    })
}

/// Each entry file `machine`'s ref holds on `remote`, by its path under the
/// ledger root; none before it has the ref.
fn listed_on(remote: &Path, machine: &str) -> Vec<String> {
    let mut listed: Vec<String> = git_in(
        remote,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            &format!("refs/knives/{machine}"),
        ],
    )
    .map_or_else(Vec::new, |listing| {
        listing
            .lines()
            .filter(|path| lab::is_entry(path))
            .map(str::to_owned)
            .collect()
    });
    listed.sort();
    listed
}

/// A ledger entry as its file spells it: the frontmatter, and the text.
struct Recorded {
    front: toml::Table,
    text: String,
}

impl Recorded {
    fn parse(contents: &str) -> Self {
        let (front, text) = contents
            .strip_prefix("+++\n")
            .and_then(|rest| rest.split_once("+++\n"))
            .unwrap_or_else(|| panic!("not a ledger entry: {contents}"));
        Self {
            front: toml::from_str(front).unwrap_or_else(|error| panic!("{error}: {contents}")),
            text: text.strip_suffix('\n').unwrap_or(text).to_owned(),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.front.get(key).and_then(toml::Value::as_str)
    }
}

/// How a config home shares the lab fork's ledger.
#[derive(Clone, Copy)]
enum Ledger<'a> {
    /// Its registry names no ledger for the fork.
    Unnamed,
    /// Its registry names [`LEDGER`], and no git directory there reaches it.
    Unjoined,
    /// Its registry names [`LEDGER`], and its ledger root is a repository
    /// whose `origin` git reaches at this bare remote.
    At(&'a Path),
}

/// Make `home`'s ledger root a repository whose `origin` is [`LEDGER_URL`],
/// reached at `remote`, committing as `machine`: a machine set up to share
/// its ledger.
fn share(home: &Path, machine: &str, remote: &Path) {
    let root = home.join("ledger");
    lab::git_repository(&root, &[("origin", LEDGER_URL)]);
    lab::git_output(
        &root,
        [
            "config",
            &format!("url.{}.insteadOf", remote.display()),
            LEDGER_URL,
        ],
    );
    lab::git_output(&root, ["config", "knives.machine", machine]);
}

/// A config home for machine `name`, sharing the ledger as `ledger` says.
fn config_home(name: &str, ledger: Ledger<'_>) -> TempDir {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        registry(!matches!(ledger, Ledger::Unnamed)),
    )
    .expect("write the registry");
    if let Ledger::At(remote) = ledger {
        share(home.path(), name, remote);
    }
    home
}

/// Clone the lab's fork to `checkout` as another machine has it: from the
/// fork's own repository, with an `upstream` remote at [`UPSTREAM`].
fn clone_fork(lab: &Lab, checkout: &Path) {
    let origin = lab.temp_origin();
    let parent = checkout.parent().expect("a checkout has a parent");
    lab::jj(
        parent,
        [
            "git",
            "clone",
            origin.to_str().expect("utf-8"),
            checkout.to_str().expect("utf-8"),
        ],
    );
    for (key, value) in [
        ("user.name", "Knives Lab"),
        ("user.email", "knives-lab@example.test"),
    ] {
        lab::jj(checkout, ["config", "set", "--repo", key, value]);
    }
    lab::jj(checkout, ["git", "remote", "add", "upstream", UPSTREAM]);
    lab::git_output(
        checkout,
        [
            "config",
            &format!("url.{}.insteadOf", lab.upstream.display()),
            UPSTREAM,
        ],
    );
    lab::jj(checkout, ["git", "fetch", "--remote", "upstream"]);
}

/// One machine: its name on the ledger, its config home, the `HOME` it
/// scans for checkouts, and its checkout of the lab fork there.
struct Machine {
    name: &'static str,
    home: TempDir,
    scan: PathBuf,
    checkout: PathBuf,
    /// The scan root of a machine whose checkout is not the lab's own.
    _scan_root: Option<TempDir>,
}

impl Machine {
    /// A machine whose checkout is the lab's own work checkout.
    fn on_lab(lab: &Lab, name: &'static str, ledger: Ledger<'_>) -> Self {
        Self {
            name,
            home: config_home(name, ledger),
            scan: lab.temp_path().to_owned(),
            checkout: lab.work.clone(),
            _scan_root: None,
        }
    }

    /// A machine with a clone of the lab's fork under a scan root of its own.
    fn cloned(lab: &Lab, name: &'static str, ledger: Ledger<'_>) -> Self {
        let root = tempfile::tempdir().expect("scan root");
        let checkout = root.path().join("work");
        clone_fork(lab, &checkout);
        Self {
            name,
            home: config_home(name, ledger),
            scan: root.path().to_owned(),
            checkout,
            _scan_root: Some(root),
        }
    }

    /// `knives <args>` from `cwd` as `owner`, not yet run.
    fn command_in(&self, cwd: &Path, owner: &str, args: &[&str]) -> Command {
        let mut command = lab::knives_command(cwd, self.home.path(), &self.scan, args);
        command.env("KNIVES_OWNER", owner);
        command
    }

    /// `knives <args>` from this machine's checkout as `owner`, not yet run.
    fn command(&self, owner: &str, args: &[&str]) -> Command {
        self.command_in(&self.checkout, owner, args)
    }

    /// `knives <args>` from this machine's checkout as `owner`, run to
    /// completion, whatever it comes to.
    fn tried(&self, owner: &str, args: &[&str]) -> Output {
        self.command(owner, args).output().expect("run knives")
    }

    /// `knives <args>` from this machine's checkout in the Oh My Pi session
    /// `session`, which names itself by `PI_SESSION_ID` alone, as builds
    /// that set no `OMP_SESSION_ID` do; run to completion, whatever it
    /// comes to.
    fn tried_in_pi_session(&self, session: &str, args: &[&str]) -> Output {
        lab::knives_command(&self.checkout, self.home.path(), &self.scan, args)
            .env("PI_SESSION_ID", session)
            .output()
            .expect("run knives")
    }

    /// [`Self::tried`], its session handing git a URL rule that reaches
    /// [`JOINED_URL`] at `remote`, as a session reaches the ledger this
    /// machine joins.
    fn tried_joining(&self, remote: &Path, owner: &str, args: &[&str]) -> Output {
        lab::with_git_rule(
            &mut self.command(owner, args),
            &format!("url.{}.insteadOf", remote.display()),
            JOINED_URL,
        )
        .output()
        .expect("run knives")
    }

    /// `knives <args>` from this machine's checkout in no harness session,
    /// its git reaching [`JOINED_URL`] at `remote` as [`Self::tried_joining`]
    /// reaches it; run to completion, whatever it comes to.
    fn tried_joining_in_no_session(&self, remote: &Path, args: &[&str]) -> Output {
        lab::with_git_rule(
            &mut lab::knives_command(&self.checkout, self.home.path(), &self.scan, args),
            &format!("url.{}.insteadOf", remote.display()),
            JOINED_URL,
        )
        .output()
        .expect("run knives")
    }

    /// The machine the git directory joining [`LEDGER`] names.
    fn joined_as(&self) -> Option<String> {
        git_in(&self.joined(), &["config", "--get", "knives.machine"])
    }

    /// [`Self::tried`], which must succeed.
    fn ran(&self, owner: &str, args: &[&str]) -> Output {
        let output = self.tried(owner, args);
        assert!(
            output.status.success(),
            "`knives {}` as {owner} on {}: {}",
            args.join(" "),
            self.name,
            stderr_of(&output)
        );
        output
    }

    /// `owner`'s start of [`BRANCH`] here, which must succeed.
    fn start(&self, owner: &str) -> Output {
        self.ran(
            owner,
            &[
                "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
            ],
        )
    }

    /// [`Self::ran`], which must write exactly one ledger entry; that entry.
    fn wrote(&self, owner: &str, args: &[&str]) -> Recorded {
        let before = self.entries();
        self.ran(owner, args);
        let written = self.written_since(&before);
        assert_eq!(
            written.len(),
            1,
            "`knives {}` wrote {written:?}",
            args.join(" ")
        );
        self.recorded(&written[0])
    }

    /// This machine's claim of `branch` in its state file; null when none.
    fn claim(&self, branch: &str) -> Value {
        std::fs::read_to_string(self.home.path().join("state.json")).map_or(Value::Null, |text| {
            let state: Value = serde_json::from_str(&text).expect("parse the state file");
            state["claims"][format!("{FORK}/{branch}")].clone()
        })
    }

    /// Where this machine's workspace of `branch` is.
    fn workspace(&self, branch: &str) -> PathBuf {
        self.scan.join(branch.replace('/', "-"))
    }

    /// This machine's ledger root.
    fn root(&self) -> PathBuf {
        self.home.path().join("ledger")
    }

    /// The log of this machine's last ledger sweep or send, which exists
    /// only while that one failed.
    fn sweep_log(&self) -> PathBuf {
        self.home.path().join("ledger-sweep.log")
    }

    /// The git directory knives makes to join [`LEDGER`].
    fn joined(&self) -> PathBuf {
        self.home
            .path()
            .join("ledger-repositories")
            .join("acme--ledger.git")
    }

    /// The commit this machine's own ref holds in its ledger root.
    fn machine_ref(&self) -> Option<String> {
        git_in(
            &self.root().join(".git"),
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/knives/{}", self.name),
            ],
        )
    }

    /// Each entry file in this machine's ledger of the lab fork, by its path
    /// under the ledger root, sorted.
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.root().join(FORK))
            .map(|listing| {
                listing
                    .map(|dirent| {
                        format!(
                            "{FORK}/{}",
                            dirent.expect("an entry").file_name().to_string_lossy()
                        )
                    })
                    .filter(|path| lab::is_entry(path))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The entries here now that were not in `before`.
    fn written_since(&self, before: &[String]) -> Vec<String> {
        self.entries()
            .into_iter()
            .filter(|path| !before.contains(path))
            .collect()
    }

    /// The bytes of the entry at `path` under this machine's ledger root.
    fn entry(&self, path: &str) -> String {
        std::fs::read_to_string(self.root().join(path)).expect("read an entry")
    }

    fn recorded(&self, path: &str) -> Recorded {
        Recorded::parse(&self.entry(path))
    }
}

/// The lab fork, a bare remote standing in for the ledger's repository,
/// and machine one, whose checkout is the lab's own, sharing the ledger
/// through that remote.
struct Shared {
    lab: Lab,
    remote_root: TempDir,
    remote: PathBuf,
    one: Machine,
}

impl Shared {
    fn new() -> Self {
        let lab = forge_lab();
        let (remote_root, remote) = lab::bare_remote();
        let one = Machine::on_lab(&lab, "machine-one", Ledger::At(&remote));
        Self {
            lab,
            remote_root,
            remote,
            one,
        }
    }

    /// Machine two: a clone of the fork under a scan root of its own,
    /// sharing the same ledger.
    fn two(&self) -> Machine {
        Machine::cloned(&self.lab, "machine-two", Ledger::At(&self.remote))
    }

    /// A new config home on the lab's checkout under the machine name
    /// `name`, sharing the same ledger, whose `refs/knives/<name>` is first
    /// restored from the remote, as a new machine of that name restores it.
    fn fresh(&self, name: &'static str) -> Machine {
        let machine = Machine::on_lab(&self.lab, name, Ledger::At(&self.remote));
        lab::git_output(
            &machine.root(),
            [
                "fetch",
                "--quiet",
                "origin",
                &format!("refs/knives/{name}:refs/knives/{name}"),
            ],
        );
        machine
    }

    /// The claim the remote holds of `branch`, if any.
    fn claim(&self, branch: &str) -> Option<Held> {
        claim_on(&self.remote, branch)
    }

    /// Each ref under `prefix` on the remote.
    fn refs(&self, prefix: &str) -> Vec<String> {
        git_in(
            &self.remote,
            &["for-each-ref", "--format=%(refname)", prefix],
        )
        .map_or_else(Vec::new, |refs| refs.lines().map(str::to_owned).collect())
    }

    /// Each entry file `machine`'s ref holds on the remote.
    fn listed(&self, machine: &str) -> Vec<String> {
        listed_on(&self.remote, machine)
    }

    /// The entry at `path` in `machine`'s ref on the remote, trimmed.
    fn on_remote(&self, machine: &str, path: &str) -> String {
        git_in(
            &self.remote,
            &["cat-file", "blob", &format!("refs/knives/{machine}:{path}")],
        )
        .unwrap_or_else(|| panic!("{machine}'s ref on the remote holds no {path}"))
    }

    /// Every entry `machine`'s ref holds on the remote.
    fn recorded_on(&self, machine: &str) -> Vec<Recorded> {
        self.listed(machine)
            .iter()
            .map(|path| Recorded::parse(&self.on_remote(machine, path)))
            .collect()
    }

    /// Run `script` as the remote's hook `name`.
    fn hook(&self, name: &str, script: &str) {
        executable(&self.remote.join("hooks").join(name), script);
    }

    /// Take the remote's hook `name` away.
    fn unhook(&self, name: &str) {
        std::fs::remove_file(self.remote.join("hooks").join(name)).expect("remove the hook");
    }

    /// Run as the remote's hook `name`, once, a script that kills the
    /// remote's receive-pack, its parent, after the shell line `pass`
    /// (which ends a run that should not act); the file it writes when it
    /// acts.
    fn hang_up(&self, name: &str, pass: &str) -> PathBuf {
        let acted = self.remote_root.path().join(format!("{name}.acted"));
        self.hook(
            name,
            &format!(
                "#!/bin/sh\n{pass}rm -f \"$0\"\n: > '{}'\nkill -9 \"$PPID\"\n",
                acted.display()
            ),
        );
        acted
    }

    /// `command` with git's pushes to the forge sent to a directory that is
    /// no repository, while its fetches still reach the remote: a ledger
    /// that can be read and not written.
    fn push_nowhere<'c>(&self, command: &'c mut Command) -> &'c mut Command {
        let nowhere = format!(
            "url.{}/nowhere/.pushInsteadOf",
            self.remote_root.path().display()
        );
        lab::with_git_rule(command, &nowhere, "https://forge.invalid/")
    }
}

/// A `git` first on `PATH` that, when `LAB_PUSH_GATE` names a file, writes
/// `$LAB_PUSH_GATE.waiting` before a push it holds and waits (up to a
/// minute) for that file to exist, then runs the real `git`: a command held
/// there has read all it reads before that push, and pushes once the gate
/// opens.
struct Gate {
    root: TempDir,
    path: OsString,
}

impl Gate {
    /// A gate that holds every push.
    fn new() -> Self {
        Self::holding("*\" push \"*")
    }

    /// A gate that holds only a push that deletes a claim ref, as a
    /// release does: a `start` held there has pushed its claim already.
    fn on_release() -> Self {
        Self::holding("*\" push \"*\" :refs/knives-claims/\"*")
    }

    /// A gate that holds each git command whose arguments, spelled ` $* `,
    /// match the shell pattern `pattern`.
    fn holding(pattern: &str) -> Self {
        let root = tempfile::tempdir().expect("gate directory");
        let path = std::env::var_os("PATH").expect("PATH");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).expect("gate bin directory");
        executable(
            &bin.join("git"),
            &format!(
                "#!/bin/sh\n\
                 case \" $* \" in\n\
                 {pattern})\n\
                 \x20 if [ -n \"$LAB_PUSH_GATE\" ]; then\n\
                 \x20   : > \"$LAB_PUSH_GATE.waiting\"\n\
                 \x20   i=0\n\
                 \x20   while [ ! -e \"$LAB_PUSH_GATE\" ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done\n\
                 \x20 fi;;\n\
                 esac\n\
                 exec '{}' \"$@\"\n",
                on_path("git").display()
            ),
        );
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path)))
            .expect("join PATH");
        Self { root, path }
    }

    /// This gate, with a `jj` first on its `PATH` that refuses `jj
    /// workspace add`, saying so, and runs the real `jj` for anything else:
    /// a `start` it holds takes its claim, and then cannot open the
    /// workspace.
    fn refusing_workspace_add(self) -> Self {
        executable(
            &self.root.path().join("bin").join("jj"),
            &format!(
                "#!/bin/sh\n\
                 case \" $* \" in\n\
                 *\" workspace add \"*)\n\
                 \x20 echo 'workspace add refused by the test' >&2\n\
                 \x20 exit 1;;\n\
                 esac\n\
                 exec '{}' \"$@\"\n",
                on_path("jj").display()
            ),
        );
        self
    }

    fn file(&self) -> PathBuf {
        self.root.path().join("gate")
    }

    fn waiting(&self) -> PathBuf {
        self.root.path().join("gate.waiting")
    }

    /// Spawn `command` with its pushes held at the gate; the child, and
    /// whether a push of it waits there, which is not so when the child
    /// exited, or a minute went by, first.
    fn hold(&self, command: &mut Command) -> (Child, bool) {
        let mut child = command
            .env("PATH", &self.path)
            .env("LAB_PUSH_GATE", self.file())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the held command");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self.waiting().exists() {
            if child.try_wait().expect("poll the held command").is_some()
                || Instant::now() >= deadline
            {
                return (child, false);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        (child, true)
    }

    /// Open the gate, and collect `child`.
    fn release(&self, child: Child) -> Output {
        std::fs::write(self.file(), "").expect("open the gate");
        child.wait_with_output().expect("collect the held command")
    }
}

/// Where `program` is on this process's `PATH`.
fn on_path(program: &str) -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{program} on PATH"))
}

/// Write the script `contents` to `path`, executable.
fn executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write the script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make the script executable");
}

/// Drop every claim from the state file in `home`, as a start killed
/// between its claim push and its save leaves it.
fn lose_claims(home: &Path) {
    let path = home.join("state.json");
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read the state file"))
            .expect("parse the state file");
    state["claims"] = Value::Object(serde_json::Map::new());
    std::fs::write(&path, state.to_string()).expect("write the state file");
}

/// Write `owner`'s claim of [`BRANCH`] into the state file in `home` as a
/// release before shared claims records it: no machine, no claim commit.
fn older_claim(home: &Path, owner: &str) {
    let state = serde_json::json!({
        "claims": {
            "acme/demo/feat/gamma": {
                "repo": FORK,
                "branch": BRANCH,
                "owner": owner,
                "kind": "harness-session",
                "why": "an older release took it",
                "started": "2026-11-01T12:00:00Z",
                "files": [],
            }
        }
    });
    std::fs::write(home.join("state.json"), state.to_string()).expect("write the state file");
}

/// Each output's exit and stderr, to show when a round goes wrong.
fn shown(outputs: &[Output]) -> String {
    outputs
        .iter()
        .map(|output| format!("{:?}: {}", output.status.code(), stderr_of(output)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn start_refuses_a_branch_another_machine_holds() {
    // Given: owner one's claim of a branch, taken on machine one, and
    // machine two sharing the same ledger, with nothing of its own.
    let shared = Shared::new();
    let two = shared.two();
    shared.one.start("owner-one");
    let taken = shared.claim(BRANCH);

    // When: owner two starts the branch on machine two.
    let refused = two.tried("owner-two", &["--text", "start", BRANCH, "--repo", "demo"]);

    // Then: it is refused, naming the holder, the machine the claim was
    // taken on and when, and saying that its activity cannot be read from
    // here; it says nothing of a sighting or of processes it cannot see.
    let stderr = stderr_of(&refused);
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    let taken = taken.expect("the remote holds machine one's claim");
    assert!(
        stderr.contains(&format!(
            "{FORK}/{BRANCH} is claimed by owner-one (harness-session) on machine machine-one, \
             taken {} (",
            taken.get("started")
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "its activity cannot be read from this machine (machine-two): its sightings and its \
             processes are on machine machine-one"
        ),
        "{stderr}"
    );
    for idle in [
        "no activity observed since claimed",
        "no process of this machine is in it",
    ] {
        assert!(!stderr.contains(idle), "{stderr}");
    }
    // And: machine two claimed nothing, and the claim ref still holds
    // machine one's claim commit.
    assert_eq!(two.claim(BRANCH), Value::Null);
    assert_eq!(
        shared.claim(BRANCH).map(|held| held.commit),
        Some(taken.commit.clone())
    );

    // When: owner one starts the branch on machine two.
    let resumed = two.ran("owner-one", &["--text", "start", BRANCH, "--repo", "demo"]);

    // Then: it resumes the claim, and machine two's state file holds a copy
    // of it as the ledger has it: taken on machine one, when and for what it
    // was, at the same claim commit, which the claim ref still holds.
    assert!(
        stdout_of(&resumed).starts_with("resumed\n"),
        "{}",
        stdout_of(&resumed)
    );
    let copy = two.claim(BRANCH);
    assert_eq!(copy["owner"], "owner-one", "{copy}");
    assert_eq!(copy["machine"], "machine-one", "{copy}");
    assert_eq!(copy["ledger_commit"], taken.commit.as_str(), "{copy}");
    assert_eq!(copy["started"], taken.get("started"), "{copy}");
    assert_eq!(copy["why"], "port it", "{copy}");
    assert_eq!(
        shared.claim(BRANCH).map(|held| held.commit),
        Some(taken.commit)
    );
}

#[test]
fn a_pi_session_holds_its_claim_against_another_and_resumes_it_from_a_new_config_home() {
    // Given: machine one and machine two sharing the ledger, each shell in
    // an Oh My Pi session named by `PI_SESSION_ID` alone.
    let shared = Shared::new();
    let two = shared.two();
    let start = ["--text", "start", BRANCH, "--repo", "demo"];

    // When: session one starts the branch on machine one.
    let taken = shared
        .one
        .tried_in_pi_session("session-one", &[&start[..], &["--why", "port it"]].concat());

    // Then: it takes the branch, and the ledger's claim is held by session
    // one, a harness session, on machine one.
    assert!(taken.status.success(), "{}", stderr_of(&taken));
    let held = shared.claim(BRANCH).expect("the claim landed");
    assert_eq!(held.get("owner"), "session-one");
    assert_eq!(held.get("kind"), "harness-session");
    assert_eq!(held.get("machine"), "machine-one");

    // When: session two starts the branch on machine one, and then on
    // machine two.
    let here = shared.one.tried_in_pi_session("session-two", &start);
    let there = two.tried_in_pi_session("session-two", &start);

    // Then: each is refused naming session one as the holder, a harness
    // session, on machine one, and the claim stands.
    for refused in [&here, &there] {
        let stderr = stderr_of(refused);
        assert_eq!(refused.status.code(), Some(2), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "{FORK}/{BRANCH} is claimed by session-one (harness-session) on machine \
                 machine-one, "
            )),
            "{stderr}"
        );
    }
    assert_eq!(two.claim(BRANCH), Value::Null);
    assert_eq!(
        shared.claim(BRANCH).map(|claim| claim.commit),
        Some(held.commit.clone())
    );

    // When: machine one's config home is gone, and session one, resumed in
    // a new config home under the same machine name, its ref restored from
    // the remote, starts the branch again without `--force`.
    std::fs::remove_dir_all(shared.one.home.path()).expect("remove machine one's config home");
    let fresh = shared.fresh("machine-one");
    let resumed = fresh.tried_in_pi_session("session-one", &start);

    // Then: it resumes session one's claim, and the new config home holds a
    // copy of it at the same claim commit.
    assert!(resumed.status.success(), "{}", stderr_of(&resumed));
    assert!(
        stdout_of(&resumed).starts_with("resumed\n"),
        "{}",
        stdout_of(&resumed)
    );
    let copy = fresh.claim(BRANCH);
    assert_eq!(copy["owner"], "session-one", "{copy}");
    assert_eq!(copy["kind"], "harness-session", "{copy}");
    assert_eq!(copy["ledger_commit"], held.commit.as_str(), "{copy}");
}

#[test]
fn a_start_that_read_no_claim_loses_to_a_machine_that_took_it_before_its_push() {
    // Given: machine two, whose own ref already carries an entry, and its
    // start of a branch nobody holds, held at its claim push: it read no
    // claim, and has pushed nothing.
    let shared = Shared::new();
    let two = shared.two();
    two.ran(
        "owner-two",
        &[
            "--text",
            "notch",
            "-m",
            "machine two was here",
            "--repo",
            "demo",
        ],
    );
    let (entries, machine_ref) = (two.entries(), two.machine_ref());
    let gate = Gate::new();
    let (held, reached) = gate.hold(&mut two.command(
        "owner-two",
        &[
            "--text",
            "start",
            BRANCH,
            "--repo",
            "demo",
            "--why",
            "port it too",
        ],
    ));

    // When: machine one takes the branch while that push waits, and then
    // the push goes.
    let took = shared.one.tried(
        "owner-one",
        &[
            "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
        ],
    );
    let lost = gate.release(held);

    // Then: machine one took it, and machine two's start, whose lease
    // expected no claim, is refused, saying who took the branch meanwhile
    // and then the refusal any start of that claim gets.
    assert!(
        reached,
        "machine two's start never reached its claim push: {}",
        stderr_of(&lost)
    );
    assert!(took.status.success(), "{}", stderr_of(&took));
    let stderr = stderr_of(&lost);
    assert_eq!(lost.status.code(), Some(2), "{stderr}");
    let claim = shared.claim(BRANCH).expect("machine one's claim");
    assert!(
        stderr.contains(&format!(
            "owner-one took {BRANCH} on machine machine-one while this start ran\n{FORK}/{BRANCH} \
             is claimed by owner-one (harness-session) on machine machine-one, taken {} (",
            claim.get("started")
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("use `knives start {BRANCH} --force --why")),
        "{stderr}"
    );
    // And: machine two left nothing behind: no workspace, no entry, no
    // claim, and its own ref where it was; the claim is machine one's.
    assert!(!two.workspace(BRANCH).exists());
    assert_eq!(two.entries(), entries);
    assert_eq!(two.claim(BRANCH), Value::Null);
    assert_eq!(two.machine_ref(), machine_ref);
    assert_eq!(claim.get("owner"), "owner-one");
    assert_eq!(claim.get("machine"), "machine-one");
}

#[test]
fn a_start_that_loses_to_its_own_owner_in_another_config_home_says_a_start_resumes_it() {
    // Given: machine two, whose own ref already carries an entry, and owner
    // one's start there of a branch nobody holds, held at its claim push:
    // it read no claim, and has pushed nothing.
    let shared = Shared::new();
    let two = shared.two();
    two.ran(
        "owner-one",
        &[
            "--text",
            "notch",
            "-m",
            "machine two was here",
            "--repo",
            "demo",
        ],
    );
    let (entries, machine_ref) = (two.entries(), two.machine_ref());
    let gate = Gate::new();
    let (held, reached) = gate.hold(&mut two.command(
        "owner-one",
        &[
            "--text",
            "start",
            BRANCH,
            "--repo",
            "demo",
            "--why",
            "port it too",
        ],
    ));

    // When: the same owner takes the branch on machine one, another config
    // home, while that push waits, and then the push goes.
    let took = shared.one.tried(
        "owner-one",
        &[
            "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
        ],
    );
    let lost = gate.release(held);

    // Then: machine two's start, whose lease expected no claim, claims
    // nothing and exits 3, saying that the claim is its own owner's and
    // that a plain start resumes it, and offers no seizure of it.
    assert!(
        reached,
        "machine two's start never reached its claim push: {}",
        stderr_of(&lost)
    );
    assert!(took.status.success(), "{}", stderr_of(&took));
    let stderr = stderr_of(&lost);
    assert_eq!(lost.status.code(), Some(3), "{stderr}");
    let claim = shared.claim(BRANCH).expect("owner one's claim");
    assert!(
        stderr.contains(&format!(
            "nothing was claimed: owner-one, the owner of this start, took {BRANCH} in another \
             config home, on machine machine-one, while this start ran; `knives start {BRANCH}` \
             resumes that claim\n{FORK}/{BRANCH} is claimed by owner-one (harness-session) on \
             machine machine-one, taken {} (",
            claim.get("started")
        )),
        "{stderr}"
    );
    assert!(!stderr.contains("--force"), "{stderr}");
    // And: machine two left nothing behind: no workspace, no entry, no
    // claim, and its own ref where it was.
    assert!(!two.workspace(BRANCH).exists());
    assert_eq!(two.entries(), entries);
    assert_eq!(two.claim(BRANCH), Value::Null);
    assert_eq!(two.machine_ref(), machine_ref);

    // When: owner one starts the branch on machine two again, as it was
    // told.
    let again = two.tried("owner-one", &["--text", "start", BRANCH, "--repo", "demo"]);

    // Then: it resumes the claim it took on machine one.
    assert!(again.status.success(), "{}", stderr_of(&again));
    assert!(
        stdout_of(&again).starts_with("resumed\n"),
        "{}",
        stdout_of(&again)
    );
    assert_eq!(two.claim(BRANCH)["ledger_commit"], claim.commit.as_str());
}

#[test]
fn a_start_whose_claim_another_owner_seized_before_its_release_names_the_holder() {
    // Given: owner two's start on machine two of a branch nobody holds,
    // which takes the claim on the ledger, cannot open the workspace, and
    // is held at the push that releases the claim again.
    let shared = Shared::new();
    let two = shared.two();
    let gate = Gate::on_release().refusing_workspace_add();
    let (held, reached) = gate.hold(&mut two.command(
        "owner-two",
        &[
            "--text",
            "start",
            BRANCH,
            "--repo",
            "demo",
            "--why",
            "port it too",
        ],
    ));
    let taken = shared
        .claim(BRANCH)
        .map(|claim| claim.get("owner").to_owned());

    // When: owner one seizes the branch on machine one while that release
    // waits, and then the release goes.
    let seized = shared.one.tried(
        "owner-one",
        &[
            "--text",
            "start",
            BRANCH,
            "--repo",
            "demo",
            "--force",
            "--why",
            "take it over",
        ],
    );
    let lost = gate.release(held);

    // Then: machine two's release, leased on the claim it took, finds owner
    // one's claim there and exits 3, naming owner one as the holder, with
    // the refusal any start of that claim gets, and offers no finish, which
    // that claim would refuse.
    assert!(
        reached,
        "machine two's start never reached its release: {}",
        stderr_of(&lost)
    );
    assert_eq!(taken.as_deref(), Some("owner-two"));
    assert!(seized.status.success(), "{}", stderr_of(&seized));
    let stderr = stderr_of(&lost);
    assert_eq!(lost.status.code(), Some(3), "{stderr}");
    let claim = shared.claim(BRANCH).expect("owner one's claim");
    assert!(
        stderr.contains(&format!(
            "knives start could not open the workspace of {BRANCH}: "
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "\nowner-one took {BRANCH} on machine machine-one while this start ran\n{FORK}/{BRANCH} \
             is claimed by owner-one (harness-session) on machine machine-one, taken {} (",
            claim.get("started")
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "): take it over\nits activity cannot be read from this machine (machine-two): its \
             sightings and its processes are on machine machine-one\nuse `knives start {BRANCH} \
             --force --why \"…\"` to seize the claim\n"
        )),
        "{stderr}"
    );
    assert!(!stderr.contains("knives finish"), "{stderr}");
    // And: the claim is owner one's, and machine two keeps no copy of it
    // and no workspace.
    assert_eq!(claim.get("owner"), "owner-one");
    assert_eq!(claim.get("machine"), "machine-one");
    assert_eq!(two.claim(BRANCH), Value::Null);
    assert!(!two.workspace(BRANCH).exists());
}

#[test]
fn two_machines_starting_one_branch_at_once_leave_one_holder() {
    // Given: two machines sharing one ledger.
    let shared = Shared::new();
    let two = shared.two();
    let racers = [(&shared.one, "owner-one"), (&two, "owner-two")];
    for round in 0..10 {
        let branch = format!("feat/race-{round}");
        let barrier = Barrier::new(2);

        // When: each starts the same new branch at once, behind a barrier.
        let outputs: [Output; 2] = std::thread::scope(|scope| {
            let starts = racers.map(|(machine, owner)| {
                let mut command = machine.command(
                    owner,
                    &[
                        "--text", "start", &branch, "--repo", "demo", "--why", "race",
                    ],
                );
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    command.output().expect("run knives start")
                })
            });
            starts.map(|start| start.join().expect("a start thread"))
        });

        // Then: exactly one exits 0, the other is refused or fails, never
        // taking it, and the claim on the ledger is the winner's alone.
        let won = outputs
            .iter()
            .filter(|output| output.status.success())
            .count();
        assert_eq!(won, 1, "round {round}: {}", shown(&outputs));
        let winner = outputs
            .iter()
            .position(|output| output.status.success())
            .expect("a winner");
        let loser = &outputs[1 - winner];
        assert!(
            matches!(loser.status.code(), Some(2 | 3)),
            "round {round}: {}",
            shown(&outputs)
        );
        let (machine, owner) = racers[winner];
        let held = shared.claim(&branch).expect("the winner's claim");
        assert_eq!(held.get("owner"), owner, "round {round}");
        assert_eq!(held.get("machine"), machine.name, "round {round}");
        assert_eq!(racers[1 - winner].0.claim(&branch), Value::Null);
    }
}

#[test]
fn a_push_that_does_not_compare_the_old_value_lets_two_machines_hold_one_branch() {
    // Given: a bare remote and two clones of it, each with a claim commit
    // of its own, both of which read the claim refs of two branches absent.
    let (_remote_root, remote) = lab::bare_remote();
    let scratch = tempfile::tempdir().expect("clones");
    let clones = ["owner-one", "owner-two"].map(|owner| {
        let root = scratch.path().join(owner);
        lab::git_repository(&root, &[("origin", remote.to_str().expect("utf-8"))]);
        std::fs::write(root.join("claim.toml"), format!("owner = \"{owner}\"\n"))
            .expect("write a claim");
        commit_all(&root, &format!("{owner}: claim {FORK} {BRANCH}"));
        root
    });
    let [one, two] = &clones;
    let (gamma, delta) = (claim_ref(BRANCH), claim_ref("feat/delta"));
    for clone in [one, two] {
        for claim in [&gamma, &delta] {
            assert_eq!(lab::git_output(clone, ["ls-remote", "origin", claim]), "");
        }
    }
    let head = |clone: &Path| lab::git_output(clone, ["rev-parse", "HEAD"]);

    // When: one creates the first ref, then two pushes its commit to it
    // with `--force` and no lease, as a push that compares nothing does.
    let created = git_at(one, &["push", "origin", &format!("HEAD:{gamma}")]);
    let forced = git_at(
        two,
        &["push", "--force", "origin", &format!("HEAD:{gamma}")],
    );

    // Then: both pushes succeed, and two's commit replaced one's: each
    // machine would take itself for the holder.
    assert!(created.status.success(), "{}", stderr_of(&created));
    assert!(forced.status.success(), "{}", stderr_of(&forced));
    assert_eq!(git_in(&remote, &["rev-parse", &gamma]), Some(head(two)));

    // When: the same on the second ref, with the lease a claim push carries,
    // which asks that the ref not exist.
    let created = git_at(one, &["push", "origin", &format!("HEAD:{delta}")]);
    let leased = git_at(
        two,
        &[
            "push",
            &format!("--force-with-lease={delta}:"),
            "origin",
            &format!("HEAD:{delta}"),
        ],
    );

    // Then: that push is refused, and the ref keeps the first commit.
    assert!(created.status.success(), "{}", stderr_of(&created));
    assert!(!leased.status.success(), "the leased push went through");
    assert!(
        stderr_of(&leased).contains("(stale info)"),
        "{}",
        stderr_of(&leased)
    );
    assert_eq!(git_in(&remote, &["rev-parse", &delta]), Some(head(one)));
}

#[test]
fn every_write_on_a_shared_fork_is_on_the_ledger_when_it_exits() {
    // Given: machine one sharing the lab fork's ledger, whose remote takes
    // two seconds over the next push each time: a push left running behind
    // a write could not have landed by the time the write exits.
    let shared = Shared::new();
    let one = &shared.one;
    // A forge whose upstream has #7, which `track --pr 7` asks it.
    let forge = tempfile::tempdir().expect("gh shim directory");
    forge_shim::install_repository_gh(
        forge.path(),
        &[
            forge_shim::Repository {
                named: FORK,
                canonical: FORK,
                pulls: &format!("[{}]", forge_shim::pull_record(7, "OPEN", BRANCH, None)),
            },
            forge_shim::Repository {
                named: "acme/work",
                canonical: "acme/work",
                pulls: "[]",
            },
        ],
        &forge.path().join("calls.log"),
    );
    let writes: [&[&str]; 5] = [
        &[
            "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
        ],
        &[
            "--text",
            "notch",
            "-m",
            "the port has begun",
            "--repo",
            "demo",
        ],
        &["--text", "track", BRANCH, "--pr", "7", "--repo", "demo"],
        // Named by the upstream's own `<owner>/<name>`, which asks no forge.
        &[
            "--text",
            "depends",
            BRANCH,
            "--on",
            "acme/demo#3",
            "--repo",
            "demo",
        ],
        &["--text", "finish", BRANCH, "--repo", "demo"],
    ];
    for args in writes {
        shared.hook("pre-receive", "#!/bin/sh\nrm -f \"$0\"\nsleep 2\n");
        let before = one.entries();

        // When: the write runs.
        let output = one
            .command("owner-one", args)
            .env("PATH", forge_shim::path_with_gh_shim(forge.path()))
            .output()
            .expect("run knives");
        assert!(
            output.status.success(),
            "`knives {}`: {}",
            args.join(" "),
            stderr_of(&output)
        );

        // Then: as it exits, machine one's ref on the remote holds the
        // entry it wrote, as it wrote it; and the write said nothing on
        // stderr and left no sweep log.
        let written = one.written_since(&before);
        assert!(!written.is_empty(), "`{}` wrote no entry", args.join(" "));
        let listed = shared.listed("machine-one");
        for path in &written {
            assert!(
                listed.contains(path),
                "`{}` exited before its entry {path} was on the remote: {listed:?}",
                args.join(" ")
            );
            assert_eq!(
                shared.on_remote("machine-one", path),
                one.entry(path).trim()
            );
        }
        assert!(
            output.stderr.is_empty(),
            "`{}`: {}",
            args.join(" "),
            stderr_of(&output)
        );
        assert!(
            !one.sweep_log().exists(),
            "`{}` left a sweep log",
            args.join(" ")
        );
    }
}

#[test]
fn a_write_the_ledger_refuses_exits_3_naming_it_and_a_later_sweep_sends_it() {
    // Given: two machines sharing the lab fork's ledger, whose remote
    // refuses every push for now.
    let shared = Shared::new();
    let (one, two) = (&shared.one, shared.two());
    shared.hook("pre-receive", REFUSE);

    // When: machine one writes a note.
    let wrote = one.tried(
        "owner-one",
        &["--text", "notch", "-m", "kept for later", "--repo", "demo"],
    );

    // Then: it exits 3, naming the fork, the ledger, the git directory and
    // the remote's refusal, and what sends the entry; the entry stays on
    // machine one, the remote has none, and machine one's sweep log says
    // why.
    let stderr = stderr_of(&wrote);
    assert_eq!(wrote.status.code(), Some(3), "{stderr}");
    let named = format!(
        "{FORK}: 1 ledger entry this command wrote did not reach the ledger {LEDGER} ({}): ",
        one.root().join(".git").display()
    );
    let line = stderr
        .lines()
        .find(|line| line.starts_with(&named))
        .unwrap_or_else(|| panic!("no line starting {named:?}: {stderr}"));
    assert!(
        line.contains("push refused by the test")
            && line.ends_with("; `knives ledger sweep` sends them"),
        "{line}"
    );
    let written = one.entries();
    assert_eq!(written.len(), 1, "{written:?}");
    assert_eq!(shared.listed("machine-one"), Vec::<String>::new());
    let log = std::fs::read_to_string(one.sweep_log()).expect("a sweep log");
    assert!(log.contains("push refused by the test"), "{log}");

    // When: the remote takes pushes again, machine one sweeps, and then
    // machine two.
    shared.unhook("pre-receive");
    let sent = one.tried("owner-one", &["--json", "ledger", "sweep"]);
    let pulled = two.tried("owner-two", &["--json", "ledger", "sweep"]);

    // Then: machine one's sweep sends the entry and leaves no sweep log,
    // and machine two's brings it to machine two as machine one wrote it.
    assert!(sent.status.success(), "{}", stdout_of(&sent));
    assert_eq!(shared.listed("machine-one"), written);
    assert!(!one.sweep_log().exists());
    assert!(pulled.status.success(), "{}", stdout_of(&pulled));
    assert_eq!(two.entries(), written);
    assert_eq!(two.entry(&written[0]), one.entry(&written[0]));
}

#[test]
fn a_claim_outlives_the_config_home_that_took_it() {
    // Given: owner one's claim of a branch, taken on machine one, whose
    // config home is then gone with all it held, as a machine's disk goes
    // with the machine.
    let shared = Shared::new();
    let two = shared.two();
    shared.one.start("owner-one");
    std::fs::remove_dir_all(shared.one.home.path()).expect("remove machine one's config home");

    // When: owner two starts the branch on machine two.
    let refused = two.tried("owner-two", &["--text", "start", BRANCH, "--repo", "demo"]);

    // Then: it is refused, naming the holder on machine one, and takes
    // nothing.
    let stderr = stderr_of(&refused);
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "{FORK}/{BRANCH} is claimed by owner-one (harness-session) on machine machine-one, \
             taken "
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains("its activity cannot be read from this machine (machine-two)"),
        "{stderr}"
    );
    assert_eq!(two.claim(BRANCH), Value::Null);
    assert!(!two.workspace(BRANCH).exists());
    assert_eq!(
        shared
            .claim(BRANCH)
            .map(|held| held.get("owner").to_owned()),
        Some("owner-one".to_owned())
    );
}

#[test]
fn standing_in_a_workspace_resumes_only_on_the_machine_that_took_the_claim() {
    // Given: owner one's claim of a branch, taken on machine one into a
    // workspace there, and a workspace of the same branch on machine two,
    // which machine two's checkout registered.
    let shared = Shared::new();
    let two = shared.two();
    shared.one.start("owner-one");
    let taken = shared.claim(BRANCH).map(|held| held.commit);
    let theirs = two.workspace(BRANCH);
    lab::jj_workspace_add(&two.checkout, "feat-gamma", &theirs);
    let start = ["--text", "start", BRANCH, "--repo", "demo"];

    // When: owner two starts the branch standing in machine two's workspace.
    let refused = two
        .command_in(&theirs, "owner-two", &start)
        .output()
        .expect("run knives start");

    // Then: it is refused as any start of another machine's claim is: a
    // directory of this machine says nothing of who works the claim taken
    // on machine one.
    let stderr = stderr_of(&refused);
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("is claimed by owner-one (harness-session) on machine machine-one"),
        "{stderr}"
    );
    assert_eq!(two.claim(BRANCH), Value::Null);

    // When: owner two starts the branch standing in machine one's workspace.
    let resumed = shared
        .one
        .command_in(&shared.one.workspace(BRANCH), "owner-two", &start)
        .output()
        .expect("run knives start");

    // Then: on the machine that took the claim the directory is evidence of
    // the work, and owner two resumes it by possession, the claim staying
    // owner one's.
    assert!(resumed.status.success(), "{}", stderr_of(&resumed));
    assert!(
        stdout_of(&resumed).starts_with("resumed via workspace possession\n"),
        "{}",
        stdout_of(&resumed)
    );
    assert_eq!(shared.claim(BRANCH).map(|held| held.commit), taken);
    assert_eq!(shared.one.claim(BRANCH)["owner"], "owner-one");
}

#[test]
fn a_claim_taken_on_another_machine_is_finished_only_with_force_and_why() {
    // Given: owner one's claim of a branch, taken on machine one.
    let shared = Shared::new();
    let two = shared.two();
    shared.one.start("owner-one");
    let taken = shared.claim(BRANCH).map(|held| held.commit);

    // When: owner two finishes the branch on machine two, plainly and then
    // with `--force` alone.
    let plain = two.tried("owner-two", &["--text", "finish", BRANCH, "--repo", "demo"]);
    let unexplained = two.tried(
        "owner-two",
        &["--text", "finish", BRANCH, "--repo", "demo", "--force"],
    );

    // Then: each is refused, the first naming the holder, its machine and
    // how to release the claim, and the claim stands.
    let stderr = stderr_of(&plain);
    assert_eq!(plain.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "{FORK}/{BRANCH} is claimed by owner-one (harness-session) on machine machine-one, \
             taken "
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("use `knives finish {BRANCH} --force --why")),
        "{stderr}"
    );
    assert_eq!(
        unexplained.status.code(),
        Some(2),
        "{}",
        stderr_of(&unexplained)
    );
    assert_eq!(shared.claim(BRANCH).map(|held| held.commit), taken);

    // When: owner two finishes it with `--force --why`.
    let forced = two.ran(
        "owner-two",
        &[
            "--text",
            "finish",
            BRANCH,
            "--repo",
            "demo",
            "--force",
            "--why",
            "abandoned on machine one",
        ],
    );

    // Then: the claim ref is gone from the remote, and machine two's ref
    // there carries the release, naming the claim it ended and why.
    assert!(
        stdout_of(&forced).contains(&format!("demo/{BRANCH}: claim released")),
        "{}",
        stdout_of(&forced)
    );
    assert!(shared.claim(BRANCH).is_none());
    let released = shared
        .recorded_on("machine-two")
        .into_iter()
        .find(|entry| entry.get("claim_change") == Some("released"))
        .expect("a released entry on machine two's ref");
    assert_eq!(
        released.get("claim_change_why"),
        Some("abandoned on machine one")
    );
    assert_eq!(released.get("previous_owner"), Some("owner-one"));
}

#[test]
fn a_claim_taken_on_another_machine_is_seized_with_force_and_why() {
    // Given: owner one's claim of a branch, taken on machine one.
    let shared = Shared::new();
    let two = shared.two();
    shared.one.start("owner-one");
    let before = shared.claim(BRANCH);

    // When: owner two seizes it on machine two.
    let seized = two.ran(
        "owner-two",
        &[
            "--text",
            "start",
            BRANCH,
            "--repo",
            "demo",
            "--force",
            "--why",
            "rescue it",
        ],
    );

    // Then: it names the claim it seized and the holder it took it from, as
    // taken when that claim's record says, on a line of its own after
    // whatever it said of the checkout first; the claim ref holds owner
    // two's claim, taken on machine two, and machine two holds its copy.
    let stdout = stdout_of(&seized);
    let seized_line = format!(
        "seized {FORK}/{BRANCH} from owner-one (harness-session) on machine machine-one, \
         taken {} (",
        before.as_ref().map_or("", |held| held.get("started"))
    );
    assert!(
        stdout.lines().any(|line| line.starts_with(&seized_line)),
        "{stdout}"
    );
    let held = shared.claim(BRANCH).expect("owner two's claim");
    assert_ne!(
        Some(&held.commit),
        before.as_ref().map(|before| &before.commit)
    );
    assert_eq!(held.get("owner"), "owner-two");
    assert_eq!(held.get("machine"), "machine-two");
    assert_eq!(held.get("why"), "rescue it");
    let copy = two.claim(BRANCH);
    assert_eq!(copy["owner"], "owner-two", "{copy}");
    assert_eq!(copy["ledger_commit"], held.commit.as_str(), "{copy}");
    // And: the `taken` entry on machine two's ref names the claim it ended
    // and how its owner was named, with no sighting, which machine two
    // cannot read.
    let taken = shared
        .recorded_on("machine-two")
        .into_iter()
        .find(|entry| entry.get("claim_change") == Some("taken"))
        .expect("a taken entry on machine two's ref");
    assert_eq!(taken.get("previous_owner"), Some("owner-one"));
    assert_eq!(taken.get("previous_kind"), Some("harness-session"));
    assert_eq!(taken.get("previous_last_seen"), None);
    assert_eq!(taken.get("claim_change_why"), Some("rescue it"));

    // When: owner one starts the branch again on machine one.
    let refused = shared
        .one
        .tried("owner-one", &["--text", "start", BRANCH, "--repo", "demo"]);

    // Then: it is refused by owner two's claim, taken on machine two.
    let stderr = stderr_of(&refused);
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "{FORK}/{BRANCH} is claimed by owner-two (harness-session) on machine machine-two, \
             taken "
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains("its activity cannot be read from this machine (machine-one)"),
        "{stderr}"
    );
}

#[test]
fn a_new_config_home_with_the_same_machine_name_resumes_and_finishes_its_claim() {
    // Given: owner one's claim of a branch on the ledger, taken on machine
    // one.
    let shared = Shared::new();
    let one = &shared.one;
    one.start("owner-one");
    let held = shared.claim(BRANCH).expect("the claim landed");
    let start = ["--text", "start", BRANCH, "--repo", "demo"];

    // When: machine one's state file loses the claim, as a start killed
    // between its claim push and its save leaves it, and owner one starts
    // the branch again.
    lose_claims(one.home.path());
    let resumed = one.ran("owner-one", &start);

    // Then: it resumes the ledger's claim and writes its copy back.
    assert!(
        stdout_of(&resumed).starts_with("resumed\n"),
        "{}",
        stdout_of(&resumed)
    );
    assert_eq!(one.claim(BRANCH)["ledger_commit"], held.commit.as_str());

    // When: machine one's config home is gone, and a new one under the same
    // machine name, its ref restored from the remote, starts the branch
    // for owner one.
    std::fs::remove_dir_all(one.home.path()).expect("remove machine one's config home");
    let fresh = shared.fresh("machine-one");
    let resumed = fresh.ran("owner-one", &start);

    // Then: it resumes the claim by owner name, and its state file holds a
    // copy of the ledger's claim as it was taken: by owner one, on machine
    // one, when and for what it was.
    assert!(
        stdout_of(&resumed).starts_with("resumed\n"),
        "{}",
        stdout_of(&resumed)
    );
    let copy = fresh.claim(BRANCH);
    assert_eq!(copy["owner"], "owner-one", "{copy}");
    assert_eq!(copy["machine"], "machine-one", "{copy}");
    assert_eq!(copy["ledger_commit"], held.commit.as_str(), "{copy}");
    assert_eq!(copy["started"], held.get("started"), "{copy}");
    assert_eq!(copy["why"], "port it", "{copy}");

    // When: owner one finishes it there, in the place it was taken in.
    let finished = fresh.ran("owner-one", &["--text", "finish", BRANCH, "--repo", "demo"]);

    // Then: the claim is released on the ledger, and the copy is gone.
    assert!(
        stdout_of(&finished).contains(&format!("demo/{BRANCH}: claim released")),
        "{}",
        stdout_of(&finished)
    );
    assert!(shared.claim(BRANCH).is_none());
    assert_eq!(fresh.claim(BRANCH), Value::Null);
}

#[test]
fn a_new_config_home_finishes_a_claim_taken_in_another_place_only_with_force_and_why() {
    // Given: owner one's claim of a branch on the ledger under machine
    // one's name, as taken in another place (another host, or another
    // process namespace), and a new config home under that machine name,
    // its ref restored from the remote, that resumed it.
    let shared = Shared::new();
    shared.one.start("owner-one");
    let mut record = shared.claim(BRANCH).expect("the claim landed").record;
    record.insert(
        "place".to_owned(),
        toml::Value::String("box-two 4026531836".to_owned()),
    );
    push_file(
        &shared.remote,
        &claim_ref(BRANCH),
        "claim.toml",
        &toml::to_string(&record).expect("write a claim record"),
    );
    let elsewhere = shared.claim(BRANCH).map(|held| held.commit);
    let fresh = shared.fresh("machine-one");
    fresh.ran("owner-one", &["--text", "start", BRANCH, "--repo", "demo"]);

    // When: owner one finishes it from here.
    let finish = ["--text", "finish", BRANCH, "--repo", "demo"];
    let refused = fresh.tried("owner-one", &finish);

    // Then: it is refused, naming the place the claim was taken in, whose
    // processes this place cannot see, and the claim stands.
    let stderr = stderr_of(&refused);
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "{FORK}/{BRANCH} was claimed by owner-one in box-two 4026531836; this finish runs in "
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("use `knives finish {BRANCH} --force --why")),
        "{stderr}"
    );
    assert_eq!(shared.claim(BRANCH).map(|held| held.commit), elsewhere);

    // When: it finishes with `--force --why`.
    fresh.ran(
        "owner-one",
        &[
            &finish[..],
            &["--force", "--why", "the box that took it is gone"],
        ]
        .concat(),
    );

    // Then: the claim is released on the ledger.
    assert!(shared.claim(BRANCH).is_none());
    assert_eq!(fresh.claim(BRANCH), Value::Null);
}

#[test]
fn a_ledger_with_no_git_directory_is_joined_and_the_claim_reaches_it() {
    // Given: a machine whose registry shares the lab fork's ledger, with no
    // ledger git directory at all, whose git reaches the ledger's
    // repository only through the URL rule its session hands it.
    let lab = forge_lab();
    let (_remote_root, remote) = lab::bare_remote();
    let one = Machine::on_lab(&lab, "machine-one", Ledger::Unjoined);

    // When: owner one starts a branch.
    let started = one.tried_joining(
        &remote,
        "owner-one",
        &[
            "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
        ],
    );

    // Then: it joined the ledger, naming the ledger, the git directory it
    // made and the machine that directory names, and took the branch.
    let stderr = stderr_of(&started);
    assert!(started.status.success(), "{stderr}");
    let joined = one.joined();
    assert!(
        joined.is_dir(),
        "no git directory joined the ledger: {stderr}"
    );
    let machine = one
        .joined_as()
        .expect("the joined git directory names a machine");
    assert!(
        stderr.contains(&format!(
            "joined the ledger {LEDGER}: made {} as machine {machine}, carrying {FORK}\n",
            joined.display()
        )),
        "{stderr}"
    );
    // And: the claim ref is on the remote, taken on that machine, and the
    // entry recording it is in that machine's ref there.
    let held = claim_on(&remote, BRANCH).expect("the claim reached the remote");
    assert_eq!(held.get("owner"), "owner-one");
    assert_eq!(held.get("machine"), machine);
    assert_eq!(
        listed_on(&remote, &machine),
        [format!("{FORK}/{}", held.get("entry"))]
    );
    // And: it said so; the claim records the reason given, machine one's
    // state file holds a copy of it as taken when the record says, the
    // entry the record names is the one entry here, and the workspace is
    // there.
    assert!(
        stdout_of(&started).contains(&format!("claimed demo/{BRANCH} for owner-one")),
        "{}",
        stdout_of(&started)
    );
    assert_eq!(held.get("why"), "port it");
    let copy = one.claim(BRANCH);
    assert_eq!(copy["owner"], "owner-one", "{copy}");
    assert_eq!(copy["machine"], machine.as_str(), "{copy}");
    assert_eq!(copy["ledger_commit"], held.commit.as_str(), "{copy}");
    assert_eq!(copy["started"], held.get("started"), "{copy}");
    assert_eq!(one.entries(), [format!("{FORK}/{}", held.get("entry"))]);
    assert!(one.workspace(BRANCH).join(".jj").is_dir());
}

#[test]
fn start_takes_no_claim_when_the_ledger_cannot_be_written() {
    for refusing in [true, false] {
        // Given: machine one sharing the lab fork's ledger, its own ref
        // already carrying an entry, whose remote then refuses every push,
        // or whose pushes alone go nowhere for one command.
        let shared = Shared::new();
        let one = &shared.one;
        one.ran(
            "owner-one",
            &[
                "--text",
                "notch",
                "-m",
                "before the start",
                "--repo",
                "demo",
            ],
        );
        let before = (one.machine_ref(), one.entries());
        let mut start = one.command(
            "owner-one",
            &[
                "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
            ],
        );
        let cause = if refusing {
            shared.hook("pre-receive", REFUSE);
            "push refused by the test"
        } else {
            shared.push_nowhere(&mut start);
            "does not appear to be a git repository"
        };

        // When: owner one starts a branch.
        let refused = start.output().expect("run knives start");

        // Then: it exits 3, naming the ledger, its git directory and why.
        let stderr = stderr_of(&refused);
        assert_eq!(refused.status.code(), Some(3), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "nothing was claimed: the ledger {LEDGER} could not be written ({}): ",
                one.root().join(".git").display()
            )) && stderr.contains(cause),
            "{stderr}"
        );
        // And: nothing was claimed anywhere: no claim here, no claim ref,
        // no entry of the claim, machine one's ref where it was, and no
        // workspace.
        assert_eq!(one.claim(BRANCH), Value::Null);
        assert!(shared.claim(BRANCH).is_none());
        assert_eq!((one.machine_ref(), one.entries()), before);
        assert!(!one.workspace(BRANCH).exists());
    }
}

#[test]
fn a_ledger_that_cannot_be_reached_refuses_start_by_name() {
    // Given: a machine whose registry shares the lab fork's ledger, with no
    // ledger git directory, and no route to the ledger's repository: its
    // forge's host does not resolve.
    let lab = forge_lab();
    let one = Machine::on_lab(&lab, "machine-one", Ledger::Unjoined);

    // When: owner one starts a branch.
    let refused = one.tried(
        "owner-one",
        &[
            "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
        ],
    );

    // Then: it exits 3, joining nothing, since the ledger's repository
    // could not be asked for this machine's ref: no git directory is made
    // that would refuse that ref later, so the next command tries again.
    let stderr = stderr_of(&refused);
    assert_eq!(refused.status.code(), Some(3), "{stderr}");
    assert!(!stderr.contains("joined the ledger"), "{stderr}");
    assert!(
        !one.home.path().join("ledger-repositories").exists(),
        "{stderr}"
    );
    // And: it says nothing was claimed, naming the fork, the ledger, the git
    // directory a join would make, and git's cause.
    let unjoined = format!(
        "nothing was claimed: {FORK}: repos.toml says its ledger belongs to {LEDGER}, but no \
         git directory knives reads"
    );
    let line = stderr
        .lines()
        .find(|line| line.starts_with(&unjoined))
        .unwrap_or_else(|| panic!("no line starting {unjoined:?}: {stderr}"));
    assert!(
        line.contains(&format!(
            "and joining it failed: could not make {}: ",
            one.joined().display()
        )),
        "{line}"
    );
    assert!(line.contains("forge.invalid/acme/ledger"), "{line}");
    // And: nothing was claimed: no claim, no entry, no workspace.
    assert_eq!(one.claim(BRANCH), Value::Null);
    assert_eq!(one.entries(), Vec::<String>::new());
    assert!(!one.workspace(BRANCH).exists());
}

#[test]
fn a_config_home_made_again_joins_as_the_same_machine_and_resumes_and_finishes_its_claim() {
    // Given: owner one's claim of a branch, taken through a ledger machine
    // one's config home joined.
    let lab = forge_lab();
    let (_remote_root, remote) = lab::bare_remote();
    let one = Machine::on_lab(&lab, "machine-one", Ledger::Unjoined);
    let start = ["--text", "start", BRANCH, "--repo", "demo"];
    let taken = one.tried_joining(
        &remote,
        "owner-one",
        &[&start[..], &["--why", "port it"]].concat(),
    );
    assert!(taken.status.success(), "{}", stderr_of(&taken));
    let machine = one
        .joined_as()
        .expect("the joined git directory names a machine");
    let held = claim_on(&remote, BRANCH).expect("the claim landed");
    assert_eq!(held.get("machine"), machine);

    // When: the config home is gone with its git directory and its ledger
    // root, as a machine's disk goes with it, and is made again with the
    // same registry; and owner one starts the branch again, without
    // `--force`.
    std::fs::remove_dir_all(one.home.path()).expect("remove machine one's config home");
    std::fs::create_dir(one.home.path()).expect("make machine one's config home again");
    std::fs::write(one.home.path().join("repos.toml"), registry(true)).expect("write the registry");
    let resumed = one.tried_joining(&remote, "owner-one", &start);

    // Then: it joins the ledger again as the same machine and resumes owner
    // one's claim, and the config home holds a copy of it at the same claim
    // commit.
    let stderr = stderr_of(&resumed);
    assert_eq!(resumed.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "joined the ledger {LEDGER}: made {} as machine {machine}, carrying {FORK}\n",
            one.joined().display()
        )),
        "{stderr}"
    );
    assert!(
        stdout_of(&resumed).starts_with("resumed\n"),
        "{}",
        stdout_of(&resumed)
    );
    let copy = one.claim(BRANCH);
    assert_eq!(copy["owner"], "owner-one", "{copy}");
    assert_eq!(copy["machine"], machine.as_str(), "{copy}");
    assert_eq!(copy["ledger_commit"], held.commit.as_str(), "{copy}");

    // When: owner one finishes it.
    let finished = one.tried_joining(
        &remote,
        "owner-one",
        &["--text", "finish", BRANCH, "--repo", "demo"],
    );

    // Then: it exits 0, and the claim is released on the ledger and here.
    assert_eq!(finished.status.code(), Some(0), "{}", stderr_of(&finished));
    assert!(
        stdout_of(&finished).contains(&format!("demo/{BRANCH}: claim released")),
        "{}",
        stdout_of(&finished)
    );
    assert!(claim_on(&remote, BRANCH).is_none());
    assert_eq!(one.claim(BRANCH), Value::Null);
}

#[test]
fn two_sessions_on_one_host_join_as_two_machines_and_each_takes_its_branch() {
    // Given: two config homes on this host, each the shell of a session of
    // its own and with no ledger git directory yet.
    let lab = forge_lab();
    let (_remote_root, remote) = lab::bare_remote();
    let one = Machine::on_lab(&lab, "machine-one", Ledger::Unjoined);
    let two = Machine::cloned(&lab, "machine-two", Ledger::Unjoined);
    let other = "feat/delta";

    // When: session one starts a branch, and session two another.
    let started = [(&one, "session-one", BRANCH), (&two, "session-two", other)].map(
        |(machine, session, branch)| {
            machine.tried_joining(
                &remote,
                session,
                &[
                    "--text", "start", branch, "--repo", "demo", "--why", "port it",
                ],
            )
        },
    );

    // Then: both take their branch, and neither is told to rename its
    // machine.
    for output in &started {
        let stderr = stderr_of(output);
        assert_eq!(output.status.code(), Some(0), "{}", shown(&started));
        assert!(!stderr.contains("knives.machine"), "{stderr}");
    }
    // And: each joined under a name of its own, and each claim, with the
    // entry recording it, is on the remote under its own machine.
    let names = [&one, &two].map(|machine| {
        machine
            .joined_as()
            .expect("the joined git directory names a machine")
    });
    assert_ne!(names[0], names[1]);
    for ((session, branch), machine) in [("session-one", BRANCH), ("session-two", other)]
        .into_iter()
        .zip(&names)
    {
        let held = claim_on(&remote, branch).expect("the claim landed");
        assert_eq!(held.get("owner"), session);
        assert_eq!(held.get("machine"), machine.as_str());
        assert_eq!(
            listed_on(&remote, machine),
            [format!("{FORK}/{}", held.get("entry"))]
        );
    }
}

#[test]
fn two_config_homes_on_one_host_in_no_session_join_before_either_pushes_and_both_write() {
    // Given: two config homes on this host, each in no harness session and
    // with no ledger git directory yet.
    let lab = forge_lab();
    let (_remote_root, remote) = lab::bare_remote();
    let one = Machine::on_lab(&lab, "machine-one", Ledger::Unjoined);
    let two = Machine::cloned(&lab, "machine-two", Ledger::Unjoined);
    let machines = [&one, &two];

    // When: each sweeps, with nothing to send, before either has pushed
    // anything.
    let swept = machines.map(|machine| {
        machine.tried_joining_in_no_session(&remote, &["--json", "ledger", "sweep"])
    });

    // Then: each sweep joined the ledger, and the remote holds no machine's
    // ref yet.
    for (machine, output) in machines.into_iter().zip(&swept) {
        assert_eq!(output.status.code(), Some(0), "{}", shown(&swept));
        assert!(machine.joined_as().is_some(), "{}", shown(&swept));
    }
    assert_eq!(
        git_in(&remote, &["for-each-ref", "refs/knives/"]).as_deref(),
        Some("")
    );

    // When: each writes a note, home one first.
    let notes = ["a note from home one", "a note from home two"];
    let wrote = [(&one, notes[0]), (&two, notes[1])].map(|(machine, note)| {
        machine.tried_joining_in_no_session(
            &remote,
            &["--text", "notch", "-m", note, "--repo", "demo"],
        )
    });

    // Then: both writes exit 0, and neither is told to rename its machine.
    for output in &wrote {
        assert_eq!(output.status.code(), Some(0), "{}", shown(&wrote));
        assert!(
            !stderr_of(output).contains("knives.machine"),
            "{}",
            shown(&wrote)
        );
    }
    // And: the two joined under two names, and each note is on the remote
    // under its own machine's ref, the one entry there.
    let names = machines.map(|machine| {
        machine
            .joined_as()
            .expect("the joined git directory names a machine")
    });
    assert_ne!(names[0], names[1]);
    for ((machine, note), name) in machines.into_iter().zip(notes).zip(&names) {
        let own: Vec<String> = machine
            .entries()
            .into_iter()
            .filter(|path| machine.recorded(path).text.contains(note))
            .collect();
        assert_eq!(own.len(), 1, "{own:?}");
        assert_eq!(listed_on(&remote, name), own, "{name}");
    }
}

#[test]
fn a_machine_named_by_hand_keeps_its_name_in_a_harness_session() {
    // Given: a machine whose ledger git directory was made by hand, as the
    // installer makes it, naming the machine `machine-one`.
    let lab = forge_lab();
    let (_remote_root, remote) = lab::bare_remote();
    let one = Machine::on_lab(&lab, "machine-one", Ledger::Unjoined);
    lab::join_ledger(
        &one.joined(),
        &one.root(),
        JOINED_URL,
        &remote,
        "machine-one",
    );

    // When: an Oh My Pi session starts a branch there.
    let started = one.tried_in_pi_session(
        "session-one",
        &[
            "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
        ],
    );

    // Then: it takes the branch, joining nothing, and the git directory
    // still names `machine-one`.
    let stderr = stderr_of(&started);
    assert!(started.status.success(), "{stderr}");
    assert!(!stderr.contains("joined the ledger"), "{stderr}");
    assert_eq!(one.joined_as().as_deref(), Some("machine-one"));
    // And: the claim and the entry recording it are on the remote under
    // `machine-one`, the one machine ref there.
    let held = claim_on(&remote, BRANCH).expect("the claim landed");
    assert_eq!(held.get("owner"), "session-one");
    assert_eq!(held.get("machine"), "machine-one");
    assert_eq!(
        git_in(
            &remote,
            &["for-each-ref", "--format=%(refname)", "refs/knives/"]
        )
        .as_deref(),
        Some("refs/knives/machine-one")
    );
    assert_eq!(
        listed_on(&remote, "machine-one"),
        [format!("{FORK}/{}", held.get("entry"))]
    );
}

#[test]
fn a_branch_no_claim_ref_can_name_is_refused_before_the_ledger_is_joined() {
    // Given: a machine whose registry shares the lab fork's ledger, with no
    // ledger git directory, and a branch, `feat/` and 249 bytes more, whose
    // whole claim ref would be 285 bytes, more than the 255 a ref may be on
    // the ledger's remote.
    let lab = forge_lab();
    let one = Machine::on_lab(&lab, "machine-one", Ledger::Unjoined);
    let branch = format!("feat/{}", "a".repeat(249));
    assert_eq!(claim_ref(&branch).len(), 285);

    // When: owner one starts it.
    let refused = one.tried(
        "owner-one",
        &[
            "--text", "start", &branch, "--repo", "demo", "--why", "port it",
        ],
    );

    // Then: it is refused with exit 2, saying why, and wrote nothing: no
    // git directory joining the ledger, no claim, no entry, no workspace.
    let stderr = stderr_of(&refused);
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("its claim ref would be 285 bytes, more than the 255"),
        "{stderr}"
    );
    assert!(
        !one.home.path().join("ledger-repositories").exists(),
        "{stderr}"
    );
    assert_eq!(one.claim(&branch), Value::Null);
    assert_eq!(one.entries(), Vec::<String>::new());
    assert!(!one.workspace(&branch).exists());
}

#[test]
fn a_branch_whose_whole_claim_ref_passes_255_bytes_is_refused_before_the_ledger_is_joined() {
    // Given: a machine whose registry shares the lab fork's ledger, with no
    // ledger git directory, whose git reaches the ledger's repository
    // through the URL rule its session hands it, and a branch whose claim
    // ref's last component, `feat%2F` and 220 bytes more, is 227 bytes:
    // within the 255 a ref component may hold, while the whole claim ref is
    // 256 bytes. GitHub refuses every ref longer than 255 bytes ("GH005:
    // Sorry, refs longer than 255 bytes are not allowed"), so no claim of
    // this branch could ever be taken there, and a push of it fails on
    // every try.
    let lab = forge_lab();
    let (_remote_root, remote) = lab::bare_remote();
    let one = Machine::on_lab(&lab, "machine-one", Ledger::Unjoined);
    let branch = format!("feat/{}", "a".repeat(220));
    assert_eq!(claim_ref(&branch).len(), 256);

    // When: owner one starts it.
    let refused = one.tried_joining(
        &remote,
        "owner-one",
        &[
            "--text", "start", &branch, "--repo", "demo", "--why", "port it",
        ],
    );

    // Then: it is refused with exit 2, as a branch no claim ref can name,
    // and wrote nothing: no git directory joining the ledger, no claim here
    // or on the remote, no entry, no workspace.
    let stderr = stderr_of(&refused);
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    assert!(
        !one.home.path().join("ledger-repositories").exists(),
        "{stderr}"
    );
    assert_eq!(one.claim(&branch), Value::Null);
    assert!(claim_on(&remote, &branch).is_none(), "{stderr}");
    assert_eq!(one.entries(), Vec::<String>::new());
    assert!(!one.workspace(&branch).exists());

    // And: one byte shorter, the whole claim ref is 255 bytes, which GitHub
    // holds, so that branch is taken and its claim reaches the remote.
    let longest = format!("feat/{}", "a".repeat(219));
    assert_eq!(claim_ref(&longest).len(), 255);
    let taken = one.tried_joining(
        &remote,
        "owner-one",
        &[
            "--text", "start", &longest, "--repo", "demo", "--why", "port it",
        ],
    );
    assert_eq!(taken.status.code(), Some(0), "{}", stderr_of(&taken));
    let held = claim_on(&remote, &longest).expect("the claim reached the remote");
    assert_eq!(held.get("owner"), "owner-one");
}

#[test]
fn claim_refs_stay_outside_the_machine_refs_and_another_machine_reads_the_ledger() {
    // Given: owner one's claim of a branch, taken on machine one.
    let shared = Shared::new();
    let two = shared.two();
    shared.one.start("owner-one");

    // Then: the remote holds the claim under `refs/knives-claims/`, and
    // under `refs/knives/`, where every release reads its peers, machine
    // one's ref alone, whose tree holds entries of the fork and nothing else.
    assert_eq!(shared.refs("refs/knives-claims/"), [claim_ref(BRANCH)]);
    assert_eq!(shared.refs("refs/knives/"), ["refs/knives/machine-one"]);
    let tree = git_in(
        &shared.remote,
        &["ls-tree", "-r", "--name-only", "refs/knives/machine-one"],
    )
    .expect("machine one's tree");
    assert!(
        tree.lines()
            .all(|path| lab::is_entry(path) && path.starts_with(&format!("{FORK}/"))),
        "{tree}"
    );

    // When: machine two sweeps.
    let swept = two.tried("owner-two", &["--json", "ledger", "sweep"]);

    // Then: it swept with no problem, and holds machine one's entries.
    let report: Value = serde_json::from_slice(&swept.stdout).expect("a sweep report");
    assert_eq!(swept.status.code(), Some(0), "{report}");
    assert_eq!(report["outcome"], "swept", "{report}");
    assert!(report.get("problems").is_none(), "{report}");
    assert_eq!(two.entries(), shared.listed("machine-one"));
}

#[test]
fn a_shell_that_exported_another_object_store_still_writes_the_ledger_into_its_own() {
    // Given: machine one sharing the lab fork's ledger, run from a shell
    // that exported another repository's git directory, an object store of
    // its own, and that repository's objects as alternates, for some other
    // work: a shell keeps such exports from one command to the next.
    let shared = Shared::new();
    let elsewhere = tempfile::tempdir().expect("another repository");
    let other = elsewhere.path().join("other");
    lab::git_repository(&other, &[]);
    let store = elsewhere.path().join("objects");
    std::fs::create_dir(&store).expect("an empty object store");

    // When: it takes a branch, notes something, and finishes the branch,
    // each from that shell.
    for args in [
        &[
            "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
        ][..],
        &["--text", "notch", "-m", "noted from that shell"],
        &[
            "--text", "finish", BRANCH, "--repo", "demo", "--why", "ported",
        ],
    ] {
        let output = shared
            .one
            .command("owner-one", args)
            .env("GIT_DIR", other.join(".git"))
            .env("GIT_OBJECT_DIRECTORY", &store)
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                other.join(".git/objects"),
            )
            .output()
            .expect("run knives");

        // Then: each succeeds.
        assert!(
            output.status.success(),
            "`knives {}`: {}",
            args.join(" "),
            stderr_of(&output)
        );
    }

    // And: every ref in machine one's ledger git directory names objects
    // that git directory holds, as a git run without those exports reads
    // it: no other machine's read stops on one of them.
    let git_dir = shared.one.root().join(".git");
    assert!(
        git_in(&git_dir, &["rev-list", "--objects", "--all"]).is_some(),
        "a ref in {} names an object it does not hold: {}",
        git_dir.display(),
        git_in(&git_dir, &["for-each-ref"]).unwrap_or_default()
    );
    // And: nothing went into the exported object store.
    let written: Vec<PathBuf> = std::fs::read_dir(&store)
        .expect("read the exported object store")
        .map(|entry| entry.expect("an entry").path())
        .collect();
    assert!(written.is_empty(), "{written:?}");
    // And: the remote holds every entry machine one wrote, and no claim.
    assert_eq!(shared.listed("machine-one"), shared.one.entries());
    assert!(shared.claim(BRANCH).is_none());
}

#[test]
fn a_branch_whose_name_jj_would_quote_is_claimed_on_the_ledger_and_released_by_finish() {
    // Given: machine one sharing the lab fork's ledger, and branches nobody
    // holds whose workspace names jj quotes in a revset (`"feat-a%2Fb"`).
    let shared = Shared::new();
    for (branch, reference) in [
        ("feat/a%2Fb", "refs/knives-claims/acme/demo/feat%2Fa%252Fb"),
        ("feat/é", "refs/knives-claims/acme/demo/feat%2Fé"),
    ] {
        assert_eq!(claim_ref(branch), reference);
        let workspace = shared.one.workspace(branch);

        // When: owner one starts the branch.
        let started = shared.one.tried(
            "owner-one",
            &[
                "--text", "start", branch, "--repo", "demo", "--why", "port it",
            ],
        );

        // Then: it exits 0, its claim reached the remote under the branch's
        // claim ref, and its workspace is there.
        assert_eq!(
            started.status.code(),
            Some(0),
            "{branch}: {}",
            stderr_of(&started)
        );
        assert_eq!(shared.refs("refs/knives-claims/"), [reference]);
        let held = shared.claim(branch).expect("the claim reached the remote");
        assert_eq!(held.get("branch"), branch);
        assert_eq!(held.get("owner"), "owner-one");
        assert!(
            workspace.join(".jj").is_dir(),
            "no workspace at {}",
            workspace.display()
        );

        // When: owner one finishes it.
        let finished = shared
            .one
            .tried("owner-one", &["--text", "finish", branch, "--repo", "demo"]);

        // Then: it exits 0, the claim ref is gone from the remote, and the
        // workspace is gone.
        assert_eq!(
            finished.status.code(),
            Some(0),
            "{branch}: {}",
            stderr_of(&finished)
        );
        assert_eq!(shared.refs("refs/knives-claims/"), Vec::<String>::new());
        assert_eq!(shared.one.claim(branch), Value::Null);
        assert!(!workspace.exists(), "{} survived", workspace.display());
    }
}

#[test]
fn a_claim_shaped_ref_under_refs_knives_fails_the_whole_sweep_naming_its_path() {
    // Given: a ref under `refs/knives/` whose commit holds a claim record at
    // its root, as a claim kept beside the machine refs would be, and
    // machine two sharing the ledger.
    let shared = Shared::new();
    let two = shared.two();
    push_file(
        &shared.remote,
        "refs/knives/claims-probe",
        "claim.toml",
        CLAIM_RECORD,
    );

    // When: machine two sweeps.
    let swept = two.tried("owner-two", &["--json", "ledger", "sweep"]);

    // Then: the sweep fails, naming `claim.toml` as no ledger entry, and
    // writes nothing of that tree.
    let report: Value = serde_json::from_slice(&swept.stdout).expect("a sweep report");
    assert_eq!(swept.status.code(), Some(3), "{report}");
    let problems = report["problems"]
        .as_array()
        .map(|problems| {
            problems
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    assert!(
        problems.contains("\"claim.toml\", which is not a ledger entry"),
        "{report}"
    );
    assert!(!two.root().join("claim.toml").exists());
}

#[test]
fn a_claim_an_older_release_recorded_binds_only_its_machine() {
    // Given: owner one's claim of a branch in machine one's state file, as a
    // release before shared claims records it: no machine, no claim
    // commit, nothing on the ledger.
    let shared = Shared::new();
    let two = shared.two();
    older_claim(shared.one.home.path(), "owner-one");

    // When: owner two starts the branch on machine one.
    let refused = shared
        .one
        .tried("owner-two", &["--text", "start", BRANCH, "--repo", "demo"]);

    // Then: it is refused there, naming owner one, as it always was.
    let stderr = stderr_of(&refused);
    assert_eq!(refused.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "{FORK}/{BRANCH} is claimed by owner-one (harness-session)"
        )),
        "{stderr}"
    );

    // When: owner two starts the branch on machine two.
    let started = two.ran(
        "owner-two",
        &[
            "--text",
            "start",
            BRANCH,
            "--repo",
            "demo",
            "--why",
            "machine two works it",
        ],
    );

    // Then: machine two takes it: that claim was never shared, and binds
    // machine one alone, where it still is.
    assert!(
        stdout_of(&started).contains(&format!("claimed demo/{BRANCH} for owner-two")),
        "{}",
        stdout_of(&started)
    );
    assert_eq!(two.claim(BRANCH)["owner"], "owner-two");
    assert_eq!(shared.one.claim(BRANCH)["owner"], "owner-one");
}

#[test]
fn a_fork_without_a_ledger_keeps_its_claims_on_the_machine() {
    // Given: two machines whose registry names no ledger for the lab fork.
    let lab = forge_lab();
    let one = Machine::on_lab(&lab, "machine-one", Ledger::Unnamed);
    let two = Machine::cloned(&lab, "machine-two", Ledger::Unnamed);

    // When: owner one starts a branch on machine one, and owner two the
    // same branch on machine two.
    one.start("owner-one");
    two.start("owner-two");

    // Then: each machine holds its own claim, as before: neither knows of
    // the other's, and neither joined a ledger to learn of it.
    assert_eq!(one.claim(BRANCH)["owner"], "owner-one");
    assert_eq!(two.claim(BRANCH)["owner"], "owner-two");
    for machine in [&one, &two] {
        assert!(!machine.home.path().join("ledger-repositories").exists());
        assert!(!machine.root().join(".git").exists());
    }
}

#[test]
fn finish_records_its_why_without_force() {
    for shares in [true, false] {
        // Given: owner one's claim of a branch, on a fork that shares its
        // ledger and on one that does not.
        let lab = forge_lab();
        let (_remote_root, remote) = lab::bare_remote();
        let ledger = if shares {
            Ledger::At(&remote)
        } else {
            Ledger::Unnamed
        };
        let one = Machine::on_lab(&lab, "machine-one", ledger);
        one.start("owner-one");
        let finish = ["--text", "finish", BRANCH, "--repo", "demo", "--why"];

        // When: owner one finishes it with a reason, and no `--force`.
        let released = one.wrote("owner-one", &[&finish[..], &["the port landed"]].concat());

        // Then: the release records that reason as why the claim ended.
        assert_eq!(released.get("claim_change"), Some("released"));
        assert_eq!(released.get("claim_change_why"), Some("the port landed"));
        // And: the claim is gone from this machine, with its workspace, and
        // on a fork that shares its ledger from the ledger too, machine
        // one's ref there carrying every entry this machine wrote.
        assert_eq!(one.claim(BRANCH), Value::Null);
        assert!(!one.workspace(BRANCH).exists());
        if shares {
            assert!(claim_on(&remote, BRANCH).is_none());
            assert_eq!(listed_on(&remote, "machine-one"), one.entries());
        }

        // When: with no claim held, it is finished as superseded, with a
        // reason.
        let superseded = one.wrote(
            "owner-one",
            &[
                &finish[..],
                &["delta carries it", "--superseded-by", "feat/delta"],
            ]
            .concat(),
        );

        // Then: the entry's text ends with the reason, and changes no claim.
        assert!(
            superseded.text.starts_with("superseded by feat/delta")
                && superseded.text.ends_with(": delta carries it"),
            "{}",
            superseded.text
        );
        assert_eq!(superseded.get("claim_change"), None);

        // When: with no claim held, it is finished with a reason alone.
        let plain = one.wrote("owner-one", &[&finish[..], &["nothing was left"]].concat());

        // Then: the entry says that no claim was held, and why it finished.
        assert_eq!(plain.text, "finished with no claim held: nothing was left");
        assert_eq!(plain.get("claim_change"), None);
    }
}

#[test]
fn a_finish_whose_release_cannot_be_sent_releases_nothing() {
    for refusing in [true, false] {
        // Given: owner one's claim of a branch on the ledger, taken on
        // machine one, whose remote then refuses every push, or whose
        // pushes alone go nowhere while its reads still reach the remote.
        let shared = Shared::new();
        let one = &shared.one;
        one.start("owner-one");
        let before = (
            shared.claim(BRANCH).map(|held| held.commit),
            one.claim(BRANCH),
            one.entries(),
            one.machine_ref(),
        );
        let finish = [
            "--text",
            "finish",
            BRANCH,
            "--repo",
            "demo",
            "--why",
            "the port landed",
        ];
        let mut command = one.command("owner-one", &finish);
        let cause = if refusing {
            shared.hook("pre-receive", REFUSE);
            "push refused by the test"
        } else {
            shared.push_nowhere(&mut command);
            "does not appear to be a git repository"
        };

        // When: owner one finishes it.
        let refused = command.output().expect("run knives finish");

        // Then: it exits 3, naming the ledger, its git directory and why,
        // and says how to try again.
        let stderr = stderr_of(&refused);
        assert_eq!(refused.status.code(), Some(3), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "nothing was released: the ledger {LEDGER} could not be written ({}): ",
                one.root().join(".git").display()
            )) && stderr.contains(cause)
                && stderr.contains(&format!("; run `knives finish {BRANCH}` again")),
            "{stderr}"
        );
        // And: nothing was released: the claim ref, this machine's copy,
        // its entries and its ref are as they were, and so is the
        // workspace.
        let after = (
            shared.claim(BRANCH).map(|held| held.commit),
            one.claim(BRANCH),
            one.entries(),
            one.machine_ref(),
        );
        assert_eq!(after, before);
        assert!(one.workspace(BRANCH).join(".jj").is_dir());

        // When: the pushes reach the remote again, and the finish runs
        // again.
        if refusing {
            shared.unhook("pre-receive");
        }
        one.ran("owner-one", &finish);

        // Then: the claim is released on the ledger.
        assert!(shared.claim(BRANCH).is_none());
        assert_eq!(one.claim(BRANCH), Value::Null);
    }
}

#[test]
fn a_seize_that_loses_to_a_finish_takes_nothing() {
    // Given: owner one's claim of a branch, taken on machine one, and owner
    // two's seizure of it on machine two, whose own ref already carries an
    // entry, held at its claim push: it read owner one's claim.
    let shared = Shared::new();
    let two = shared.two();
    shared.one.start("owner-one");
    two.ran(
        "owner-two",
        &[
            "--text",
            "notch",
            "-m",
            "machine two was here",
            "--repo",
            "demo",
        ],
    );
    let (entries, machine_ref) = (two.entries(), two.machine_ref());
    let gate = Gate::new();
    let (held, reached) = gate.hold(&mut two.command(
        "owner-two",
        &[
            "--text",
            "start",
            BRANCH,
            "--repo",
            "demo",
            "--force",
            "--why",
            "rescue it",
        ],
    ));

    // When: owner one finishes the branch while that push waits, and then
    // the push goes.
    let finished = shared
        .one
        .tried("owner-one", &["--text", "finish", BRANCH, "--repo", "demo"]);
    let lost = gate.release(held);

    // Then: the finish released the claim, and the seizure, leased on the
    // claim it read, finds it gone and exits 3, saying so and how to take
    // the branch now.
    assert!(
        reached,
        "machine two's seizure never reached its claim push: {}",
        stderr_of(&lost)
    );
    assert!(finished.status.success(), "{}", stderr_of(&finished));
    let stderr = stderr_of(&lost);
    assert_eq!(lost.status.code(), Some(3), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "nothing was claimed: the ledger {LEDGER} holds no claim of {BRANCH} now; it was \
             released while this start ran, and `knives start {BRANCH}` takes it"
        )),
        "{stderr}"
    );
    // And: it took nothing: no claim on the ledger or on machine two, no
    // workspace, no entry, and machine two's ref where it was.
    assert!(shared.claim(BRANCH).is_none());
    assert_eq!(two.claim(BRANCH), Value::Null);
    assert!(!two.workspace(BRANCH).exists());
    assert_eq!(two.entries(), entries);
    assert_eq!(two.machine_ref(), machine_ref);
}

#[test]
fn a_claim_push_the_remote_applied_before_it_hung_up_is_held_and_resumed() {
    // Given: a remote whose receive-pack is killed, once, right after it
    // commits a push's ref updates: the push lands, and git on machine one
    // sees the remote hang up.
    let shared = Shared::new();
    let one = &shared.one;
    let hung_up = shared.hang_up(
        "reference-transaction",
        "[ \"$1\" = committed ] || exit 0\n",
    );

    // When: owner one starts a branch.
    let started = one.tried(
        "owner-one",
        &[
            "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
        ],
    );

    // Then: the remote hung up on that push, and the start, finding that
    // both refs landed, took the claim: the claim ref names owner one, and
    // machine one holds its copy and the workspace.
    assert!(hung_up.exists(), "the remote never hung up");
    assert!(started.status.success(), "{}", stderr_of(&started));
    let held = shared.claim(BRANCH).expect("the claim landed");
    assert_eq!(held.get("owner"), "owner-one");
    assert_eq!(one.claim(BRANCH)["ledger_commit"], held.commit.as_str());
    assert!(one.workspace(BRANCH).join(".jj").is_dir());

    // When: owner one starts the branch again.
    let resumed = one.ran("owner-one", &["--text", "start", BRANCH, "--repo", "demo"]);

    // Then: it resumes that claim.
    assert!(
        stdout_of(&resumed).starts_with("resumed\n"),
        "{}",
        stdout_of(&resumed)
    );
    assert_eq!(
        shared.claim(BRANCH).map(|now| now.commit),
        Some(held.commit)
    );
}

#[test]
fn a_claim_push_the_remote_hung_up_on_before_applying_it_leaves_the_branch_to_the_next_start() {
    // Given: a remote whose receive-pack is killed, once, before it applies
    // a push.
    let shared = Shared::new();
    let one = &shared.one;
    let hung_up = shared.hang_up("pre-receive", "");
    let start = [
        "--text", "start", BRANCH, "--repo", "demo", "--why", "port it",
    ];

    // When: owner one starts a branch.
    let unknown = one.tried("owner-one", &start);

    // Then: it exits 3, saying that whether the claim landed is unknown and
    // that a start reads it again; nothing is claimed here, no workspace is
    // made, and the remote holds no claim.
    assert!(hung_up.exists(), "the remote never hung up");
    let stderr = stderr_of(&unknown);
    assert_eq!(unknown.status.code(), Some(3), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "the claim of {BRANCH} was pushed to the ledger {LEDGER} ({}), and whether it landed \
             is unknown: ",
            one.root().join(".git").display()
        )) && stderr.contains(&format!(
            "nothing is recorded on this machine, and `knives start {BRANCH}` reads the claim again"
        )),
        "{stderr}"
    );
    assert_eq!(one.claim(BRANCH), Value::Null);
    assert!(!one.workspace(BRANCH).exists());
    assert!(shared.claim(BRANCH).is_none());
    // And: nothing was rolled back here: the entry of the claim stays, in
    // machine one's own ref, for a later send.
    let kept = one.entries();
    assert_eq!(kept.len(), 1, "{kept:?}");
    let local = git_in(
        &one.root().join(".git"),
        &["ls-tree", "-r", "--name-only", "refs/knives/machine-one"],
    )
    .unwrap_or_default();
    assert!(local.lines().any(|path| path == kept[0]), "{local}");

    // When: owner one starts the branch again.
    let taken = one.ran("owner-one", &start);

    // Then: it takes the branch, and the remote holds owner one's claim and
    // both entries.
    assert!(
        stdout_of(&taken).contains(&format!("claimed demo/{BRANCH} for owner-one")),
        "{}",
        stdout_of(&taken)
    );
    let held = shared.claim(BRANCH).expect("the claim landed");
    assert_eq!(held.get("owner"), "owner-one");
    assert_eq!(shared.listed("machine-one"), one.entries());
    assert_eq!(one.entries().len(), 2);
}
