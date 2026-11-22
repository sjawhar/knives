//! The command surface.
//!
//! Argument shapes are enforced by the parser rather than checked at runtime,
//! so a command cannot start work and then discover it lacks something it
//! needed. `--force` on a claim requires `--why` here for exactly that reason:
//! a claim taken from another agent, or released under one, is legible to the
//! next reader only with the reason that explains it. A plain `start` takes
//! `--why` too, and falls back to a default reason.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

fn disposition_token(value: &str) -> Result<String, String> {
    match value {
        "merged-elsewhere" | "withdrawn" | "ruled-out" => Ok(value.to_owned()),
        _ => Err("a disposition must be one of: merged-elsewhere, withdrawn, ruled-out".to_owned()),
    }
}

fn evidence_token(value: &str) -> Result<String, String> {
    (!value.trim().is_empty())
        .then(|| value.to_owned())
        .ok_or_else(|| "evidence must not be blank".to_owned())
}

/// A `--why` that holds a reason. The record keeps it for the person who
/// reads it later, and an empty or whitespace-only one tells them nothing.
fn reason_text(value: &str) -> Result<String, String> {
    (!value.trim().is_empty())
        .then(|| value.to_owned())
        .ok_or_else(|| "a why must hold a reason, not be empty or only whitespace".to_owned())
}

/// Process exit codes, as a type so a command cannot invent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Ran, and found nothing the caller must act on.
    Ok,
    /// Ran, and found something worth acting on.
    Findings,
    /// Invoked wrongly, or asked about something unknown.
    Usage,
    /// Could not answer. Never conflated with `Ok`: a command that reports a
    /// problem in its text while returning success lets a script see green.
    Incomplete,
}

impl Exit {
    /// The more serious of two outcomes, so aggregating over several repos
    /// cannot drop the worse one. An earlier version kept only `Findings` and
    /// silently discarded `Incomplete`.
    #[must_use]
    pub const fn worst(self, other: Self) -> Self {
        match (self, other) {
            (Self::Incomplete, _) | (_, Self::Incomplete) => Self::Incomplete,
            (Self::Usage, _) | (_, Self::Usage) => Self::Usage,
            (Self::Findings, _) | (_, Self::Findings) => Self::Findings,
            (Self::Ok, Self::Ok) => Self::Ok,
        }
    }

    pub const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Findings => 1,
            Self::Usage => 2,
            Self::Incomplete => 3,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "knives",
    about = "Query and coordinate several forks worked by several agents.",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Emit JSON instead of the machine default (TOON).
    ///
    /// The default is decided rather than fixed: an agent reading this output should
    /// not have to parse prose, and a human should not have to read a machine
    /// format. See `output_format`.
    #[arg(long, global = true)]
    pub json: bool,
    /// Force text even where a machine format would be chosen automatically.
    #[arg(long, global = true, conflicts_with = "json")]
    pub text: bool,
}

/// What a report is rendered as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The machine default: the same structure as JSON at fewer tokens, for
    /// the agents that read almost all of this tool's output.
    Toon,
    /// On explicit request, for anything that wants JSON exactly.
    Json,
    /// Prose for a human at a terminal.
    Text,
}

/// Which format to emit.
///
/// Explicit flags win. Otherwise a machine format when the output is not going to a
/// terminal, or when the environment says an agent is running this: agents were
/// grepping human output to count findings by detector, which is both fragile and
/// unnecessary. The machine default is TOON — measured on real reports it carries
/// the same structure at fewer tokens — and `--json` stays as the stable opt-in.
/// `KNIVES_OWNER` is set by this tool's own `OpenCode` plugin, so it is a direct harness
/// signal rather than a guess; OMP sets `CLAUDECODE=1` in its bash shells for the same
/// reason, and either way that stdout is not a terminal.
pub fn output_format(json: bool, text: bool) -> Output {
    if json {
        return Output::Json;
    }
    if text {
        return Output::Text;
    }
    if agent_environment() || !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Output::Toon;
    }
    Output::Text
}

/// A report in its machine encoding, or `None` when prose was chosen.
pub fn machine_payload<T: serde::Serialize>(
    output: Output,
    report: &T,
) -> anyhow::Result<Option<String>> {
    Ok(match output {
        Output::Json => Some(serde_json::to_string_pretty(report)?),
        Output::Toon => Some(toon_format::encode_default(report)?),
        Output::Text => None,
    })
}

/// Print one or several reports as one document.
///
/// An array under `--all`, the lone object otherwise, or the prose `render`
/// makes of them all. One document per invocation is what lets a JSON reader
/// read the whole output.
pub fn emit_reports<T: serde::Serialize>(
    output: Output,
    all: bool,
    reports: &[T],
    render: impl Fn(&[T]) -> String,
) -> anyhow::Result<()> {
    let payload = match (all, reports) {
        (false, [only]) => machine_payload(output, only)?,
        _ => machine_payload(output, &reports)?,
    };
    match payload {
        Some(payload) => println!("{payload}"),
        None => println!("{}", render(reports)),
    }
    Ok(())
}

/// Each report's prose, one after another.
pub fn joined<T>(reports: &[T], separator: &str, render: impl Fn(&T) -> String) -> String {
    reports
        .iter()
        .map(render)
        .collect::<Vec<_>>()
        .join(separator)
}

fn agent_environment() -> bool {
    for name in [
        "KNIVES_OWNER",
        "CLAUDECODE",
        "CLAUDE_CODE",
        "OPENCODE",
        "AGENT",
        "CI",
    ] {
        if std::env::var_os(name).is_some() {
            return true;
        }
    }
    false
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Receive a hook event from an agent harness. Always exits successfully.
    Hook {
        #[arg(value_enum)]
        harness: HookHarness,
    },
    /// Print a registry snippet for this repo. Writes nothing: registration is a trust grant, so a human pastes it.
    /// Output is TOML regardless of `--json`.
    Register {
        /// Repo directory. Defaults to the current directory.
        repo: Option<PathBuf>,
    },
    /// List the repos knives manages, with their release state.
    Repos,
    /// Which registered forge consumers and ad-hoc local scans resolve this fork
    /// at which commit against the newest published release. Reports; never edits a consumer.
    Consumers {
        /// Registry name. Defaults to the repo you are standing in.
        fork: Option<String>,
        /// Extra ad-hoc local consumer scans, beyond the registry's forge slugs.
        #[arg(long)]
        consumer: Vec<PathBuf>,
    },
    /// Reconcile local branches against the live remote refs that own them.
    Pushed {
        /// Branches to verify. Defaults to every local bookmark.
        branches: Vec<String>,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Reconcile remote refs, open pull heads, recorded cuts, and anonymous heads.
    Audit {
        /// Registry name. Defaults to the repo you are standing in.
        repo: Option<String>,
        /// Every managed repository.
        #[arg(long)]
        all: bool,
        /// Skip the bounded live pull-request-head check.
        #[arg(long)]
        no_github: bool,
    },
    /// Fetch every remote and tracked pull ref, and classify each pull request.
    Sync {
        /// Registry name. Defaults to the repo you are standing in.
        repo: Option<String>,
        /// Every managed repo, whichever one you are standing in.
        #[arg(long)]
        all: bool,
        /// Skip pull request lookups. Pull request state reads as unknown.
        #[arg(long)]
        no_github: bool,
    },
    /// Pre-contribution facts for a repo. Reports; does not judge.
    Preflight {
        /// Registry name. Defaults to the repo you are standing in.
        repo: Option<String>,
    },
    /// Per-branch state and the detectors.
    Status {
        /// Registry name. Defaults to the repo you are standing in.
        repo: Option<String>,
        /// Every managed repo.
        #[arg(long)]
        all: bool,
        /// One line per finding rather than one line per kind.
        #[arg(long)]
        verbose: bool,
        /// Skip the landed probe, which replays every branch onto the trunk. A
        /// required member branch not shown to have landed then stays an unmet
        /// dependency: a merge alone does not show it.
        #[arg(long)]
        no_landed: bool,
        /// Skip pull request lookups. Branch pull request state reads as unknown.
        #[arg(long)]
        no_github: bool,
    },
    /// One pull request's present state, any number in any state.
    ///
    /// Exists because half of the audit's wrong premises were stale dispatch
    /// context: a closed pull request cited as open, a bare number nobody could
    /// map to a branch. One line answers both, from a live read.
    Pr {
        /// The pull request number.
        number: u64,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
        /// Also read the head-ref event log: force pushes with before/after
        /// commit and tree ids, deletes, restores, closes, reopens, and merges.
        #[arg(long)]
        timeline: bool,
    },
    /// Take a branch and open your own workspace for it.
    ///
    /// Claiming a branch and opening a workspace for it were two commands for no
    /// reason: starting work on a branch is one act. `finish` is its inverse.
    ///
    /// The start of a release name takes the release claim, the write lease
    /// for the release's refs. Its new workspace rests on a new empty commit
    /// off the root commit, not on a release commit, and its line says so.
    ///
    /// A `--why` from the claim's own holder replaces the reason on the claim,
    /// from inside the workspace or outside it. The claim keeps the time and the
    /// place it was taken, and a `start` with no `--why` keeps the reason it
    /// already carries.
    ///
    /// On a fork whose `repos.toml` entry names a `ledger`, the claim is a ref on
    /// that ledger's remote, so it holds on every machine that shares the ledger.
    /// `start` reads it before it decides, and pushes the claim before it opens the
    /// workspace. When it cannot read the claim or push it, it records no claim,
    /// opens no workspace and exits 3.
    ///
    /// Also states the fork's `immutable_heads()` — jj's trunk, the trunk by name
    /// on every knives remote, and every tag fetched from upstream — in the
    /// repository's jj config when that config states none, and says so on
    /// stdout. jj's default pins every commit beneath an untracked remote ref or
    /// any tag, which in a fork is a superseded release cut, another fork's pull
    /// request head, or the fork's own tagged release walling `jj rebase`; a
    /// stated rule is left alone.
    Start {
        branch: String,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
        /// What you are doing and why. A claim nobody can read is noise.
        /// Defaults to `started work`, and replaces the reason on a claim you
        /// hold.
        #[arg(long)]
        why: Option<String>,
        /// Replace an existing claim. Requires a new reason for the ledger.
        #[arg(long, requires = "why")]
        force: bool,
    },
    /// Hand a branch back and remove its workspace. The inverse of `start`.
    ///
    /// Only the claim is released: the branch, its bookmark, and any open pull request
    /// all survive, so hand a branch back as soon as your active work on it stops.
    ///
    /// Removing the directory loses no work: `finish` first snapshots the working copy
    /// into its commit and names it, so every change made in that workspace is in the
    /// repository and reachable by change id. When jj cannot snapshot all of it (a stale
    /// working copy, a file over `snapshot.max-new-file-size`), the claim comes back all
    /// the same, the workspace is kept exactly as it is, and the next `start` of this
    /// branch adopts that directory; what jj could snapshot stays in its commit. What
    /// does not survive is anything jj never tracks — build output, an ignored `.env` —
    /// so `--no-cleanup` leaves the directory alone.
    ///
    /// `finish` also refuses while another process on this machine has its working
    /// directory or an open file in the workspace, when the claim was taken on
    /// another host or agent box, whose processes it cannot see, and when another
    /// identity holds the claim. A caller whose own claim another owner took or
    /// released is told who did it and sent to `start`, never to `finish --force`,
    /// which would release the live claim of whoever holds the branch now. A forced
    /// release over another owner's claim leaves that workspace as it is.
    ///
    /// On a fork whose `repos.toml` entry names a `ledger`, the claim `finish` hands
    /// back is the one on that ledger's remote, read before `finish` decides. When
    /// it cannot read that claim or push its release, it leaves the state file and
    /// the workspace as they were, and exits 3.
    ///
    /// The finish of a release name releases the release claim. Once jj holds
    /// no workspace of the claim (it forgot it, or there was none), `finish`
    /// abandons the earlier commits of the release that nothing holds, in one
    /// jj operation. It names each earlier commit that something other than
    /// the release holds, with what holds it. While the release holds
    /// conflicts, it abandons none and says so. The sweep does not change the
    /// exit.
    Finish {
        branch: String,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
        /// Forget the workspace but leave its directory on disk.
        #[arg(long)]
        no_cleanup: bool,
        /// Release a claim held by another identity, finish a workspace another
        /// process still uses, or finish a claim taken on another host or agent
        /// box. Requires a durable reason.
        #[arg(long, requires = "why")]
        force: bool,
        /// Why you are finishing. The ledger records it every time: as the
        /// released claim's reason, or, where no claim was held, in the text
        /// of the entry this finish writes. `--force` requires it.
        #[arg(long)]
        why: Option<String>,
        /// Record that this branch was superseded by another.
        #[arg(long)]
        superseded_by: Option<String>,
    },
    /// State which pull request a branch belongs to, overriding inference.
    ///
    /// Inference finds an open pull request from our own copy of the repository, which
    /// is a sensible default and a bad rule. This accepts any number in any state from
    /// any author: one opened before this tool existed, one the maintainer closed
    /// because they wanted a different approach, or somebody else's that we are
    /// carrying because ours was superseded.
    Track {
        branch: String,
        /// The pull request number.
        #[arg(long = "pr")]
        pr: Option<u64>,
        /// This branch deliberately has no upstream pull request. Recorded so it does
        /// not read as an unanswered question in every report, forever.
        #[arg(long, conflicts_with = "pr")]
        fork_only: bool,
        /// Forget the stated pull request and go back to inference.
        #[arg(long, conflicts_with = "pr")]
        forget: bool,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Record that a branch cannot land before something else does, or take a
    /// requirement back.
    ///
    /// Dependencies cross forks: a branch here can require a pull request in another
    /// managed repo. Dropping the thing it needs from a release, without dropping
    /// this too, ships something that cannot work.
    ///
    /// A requirement is a pull request or a member branch, and the record says
    /// which: `<upstream>#<n>` is a pull request on the fork's upstream,
    /// `origin:<owner>/<name>#<n>` a review pull request on the fork's own
    /// repository, `origin:<owner>/<name>:<branch>` a member branch of this fork.
    ///
    /// `--forget` takes a requirement back; with `--on` it corrects one. The
    /// removal is a ledger entry, so every machine running this knives that
    /// shares the ledger reads the same list. A knives from before fork
    /// requirements cannot read a list holding a member branch, and reads a
    /// forget whose list holds one as requiring nothing.
    Depends {
        branch: String,
        /// What it needs, repeatable. `<repo>#<number>`, `<repo>` the registry
        /// name: another fork's key is first asked about as below, and names
        /// this fork's own repository when its upstream is that. Otherwise,
        /// when that fork's upstream and own repository are both forge
        /// repositories, knives asks the forge which has that pull request, and
        /// refuses when both or neither do (when the forge names that fork's
        /// own repository as its upstream, the upstream's, with no lookup of
        /// the number); when either is no forge repository, there is no such
        /// lookup, and it is the upstream's. `<owner>/<name>#<number>`: the
        /// upstream's pull request, or the fork's when `<owner>/<name>` is its
        /// own repository. Another fork's upstream, by either spelling, may be
        /// this fork's repository under another name, so the forge is asked,
        /// and nothing is recorded (exit 3) when it cannot say. The question is
        /// skipped when this fork's own repository names no forge repository
        /// (neither its checkout's nor its registry's `origin` is a forge URL).
        /// Another fork whose upstream is a path is named by its registry key
        /// exactly, with no forge call, and its key in another letter case is
        /// refused (exit 2); this fork's own path upstream is taken in any
        /// letter case. With the forge down, a pull request is recorded only
        /// when naming it needs no forge question: this fork's upstream's, by
        /// `<owner>/<name>`, unless this fork's checkout or registry `origin`
        /// spells that upstream; another fork's upstream's, by `<owner>/<name>`,
        /// when this fork's own repository names no forge repository; and, by a
        /// registry key, one of a fork whose upstream or own repository names no
        /// forge repository, when that fork is this one, its upstream is a
        /// path, or this fork's own repository names no forge repository. An
        /// `origin` that spells this fork's upstream is a configuration error.
        /// With the forge up, when only the registry's `origin` spells it,
        /// that upstream's `<owner>/<name>` is refused (exit 2: not this fork's
        /// own repository), while this fork's key is read as usual against the
        /// checkout's repository, and a number both have is refused without
        /// offering the upstream spelling; when this fork's own repository
        /// spells it, this fork's key takes the upstream's number with no
        /// lookup. A review pull request on another fork's repository is
        /// refused, and so is one whose head is not a member branch of the
        /// fork's owner. A branch name: a member branch of this fork.
        #[arg(long = "on", required_unless_present = "forget")]
        on: Vec<String>,
        /// A requirement to take back, repeatable: as its record spells it, or
        /// as `--on` would. Every record of it is taken back, whatever spelling
        /// of the fork's repository each carries; a record on another fork's
        /// repository is never one of this fork's. The texts are weighed
        /// together: when the forge cannot say whether a record is named (it
        /// is down, or cannot see a repository), and counting it would take
        /// back more, or tell two requirements from one, nothing is taken
        /// back (exit 3), and the refusal prints a command no forge answer
        /// changes: one that names every record of that number or member on a
        /// repository that is not another fork's, or, when the text may name
        /// two requirements, one such command for each.
        #[arg(long)]
        forget: Vec<String>,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Read what agents did and decided here, or add to it.
    ///
    /// One command, two moods: bare it reads, `-m` writes. Reading is
    /// intentional — nothing injects notches into a session — so the bare form
    /// answers the question an agent actually has, which is what happened here
    /// lately. A subject is a ref name: a branch, or a release, which is a
    /// subject like any other.
    Notch {
        /// The branch or release ref this is about. Omit it to read the whole
        /// repository, or to write an entry about the repository itself.
        subject: Option<String>,
        /// Record this text as a note. Without it, `notch` reads.
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
        /// A commit id, `file:line`, `<repo>#<number>` or URL backing the note.
        /// Repeatable, and it may name another repo.
        #[arg(long, requires = "message", value_parser = evidence_token)]
        evidence: Vec<String>,
        /// Record a terminal ruling with this entry: merged-elsewhere,
        /// withdrawn, ruled-out — one lowercase token. Requires -m and at
        /// least one --evidence, because a ruling without provenance is an
        /// unsupported claim.
        #[arg(long, requires = "message", requires = "evidence", value_parser = disposition_token)]
        disposition: Option<String>,
        /// Read only entries that carry a disposition.
        #[arg(long, conflicts_with_all = ["message", "disposition"])]
        dispositions: bool,
        #[arg(long, conflicts_with_all = ["message", "disposition", "dispositions"])]
        events: bool,
        /// Re-check selected commit evidence and anchors against the repository
        /// as it stands. A vanished SHA is a finding.
        #[arg(long, conflicts_with = "message")]
        verify: bool,
        /// Read only entries stamped with this pull request number, or stamp a
        /// written entry with it.
        #[arg(long = "pr")]
        pr: Option<u64>,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Plan, curate and cut releases.
    ///
    /// A release is a flat octopus merge of member branches.
    ///
    /// With no subcommand, plans: what a cut would contain, whether every parent is
    /// still its branch tip, and who pins the current release.
    Release {
        #[command(subcommand)]
        action: Option<ReleaseAction>,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
        /// An extra checkout that pins this repo's releases, scanned alongside the
        /// consumers recorded in the registry. Repeatable, because a fork can be consumed
        /// by several things sitting on different releases.
        #[arg(long)]
        consumer: Vec<PathBuf>,
        /// The release series to act on, one of the entry's `[repos.<name>.series]`.
        /// Omitted, the default series: the entry's own `base` and its
        /// `release/YYYY-MM-DD[.N|-<label>]` cuts. Every subcommand sees only
        /// that series' cuts and the branches that fork from its base. Each
        /// series has one order: by date, then by the rest of the name in
        /// version order, so a later name on a date supersedes an earlier one.
        #[arg(long, global = true)]
        series: Option<String>,
    },
    /// Carry this machine's ledger to the machines that share it, or move what
    /// an older knives kept in `state.json` onto it.
    Ledger {
        #[command(subcommand)]
        action: LedgerAction,
    },
    /// Run the real gh with fork-aware fixes applied. Plumbing for the gh shim.
    ///
    /// Three fixes, in order: export a GitHub-App token for the repo the
    /// invocation targets (when git's credential config routes it to
    /// `gh-app-token`), compensate for jj's detached HEAD on `gh pr`
    /// subcommands that need a current branch, then exec the real gh. Output
    /// and exit code are gh's own, untouched. The `--` is required so gh's
    /// flags (its own --json among them) are never parsed as ours.
    Gh {
        /// Everything after `--`, passed to gh verbatim.
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum HookHarness {
    ClaudeCode,
    Opencode,
}

#[derive(Debug, Subcommand)]
pub enum LedgerAction {
    /// Commit new entries, pull every peer's, and push this machine's.
    ///
    /// Every command that writes an entry sends it before it exits, and exits
    /// 3 naming the ledger when it could not, so this is typed to send what a
    /// write could not, or to pull. One sweep or send runs at a time: a sweep
    /// that finds another running waits for it, up to a minute, then runs its
    /// own pass and reports that, with the command it waited for.
    /// A fork's ledger travels when its `repos.toml` entry names, as
    /// `ledger`, the repository its ledger belongs to. It goes through a git
    /// directory over the ledger root whose `origin` is that repository and
    /// whose `knives.machine` is this machine's name: the root's own `.git`,
    /// or one in `ledger-repositories` beside it. Where no git directory has
    /// that `origin`, knives makes one there and says so on stderr. With no
    /// fork's `ledger` set, the ledger is not shared and this does nothing.
    Sweep,
    /// Move what an older knives left onto what this one reads: each fork
    /// kept under its registry key moves to its upstream's lowercase
    /// `<owner>/<name>`, and the branch statements kept in `state.json` move
    /// onto the ledger, one statement entry each, then leave `state.json`.
    ///
    /// A fork's ledger directory moves file by file, contents untouched, and
    /// every `state.json` key naming it is renamed; each old path and key is
    /// reported. Existing entries are left as they are: an event's prose
    /// states nothing, so nothing is read back out of it. A statement some
    /// entry about that branch already makes, kind and value alike, is
    /// skipped, so a second run writes nothing. `state.json` keeps every
    /// statement until all of them reach the ledger.
    ///
    /// Each fork's ledger is pulled first, and a fork whose pull fails has
    /// none of its statements written. A statement the ledger already makes
    /// otherwise for that branch is reported and not written: the ledger's is
    /// what every machine reads.
    Migrate {
        /// Report what it would move, rename and write, and change nothing:
        /// no lock, no pull, no write.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ReleaseAction {
    /// Name a new cut of the composition in hand, verbatim. Never pushes the release.
    ///
    /// On a fork that shares its ledger, the cut's ledger event is pushed
    /// there before the command exits.
    ///
    /// With `--at`, record a release cut outside knives instead: the commit
    /// REV becomes NAME's recorded cut, and no commit is made. NAME must sort
    /// after every other release of the series, so record the older name
    /// first. REV must have no conflict and be no workspace's working copy,
    /// every ref of NAME here or on the publish remote must name it, and a
    /// published NAME must be one the publish remote holds: the refusal
    /// gives the push that records it. A NAME no ref names gets a local
    /// bookmark at REV. The commit is audited as a cut is, and it must carry
    /// each member of the previous cut and of the newest write of the newest
    /// release the ledger records in the series, and each fix by hand an
    /// `adopt` of any release of the series recorded, file by file: at each
    /// file where the file's reference, the newest recorded write that holds
    /// it with no conflict, still holds the adopt's content, REV's own tree
    /// must carry the adopt's change there, by a replay where this
    /// repository can measure the adopt, else by holding what the reference
    /// holds there, nothing included. A write that leaves the file
    /// conflicted is no reference and does not end the hold. A deletion is
    /// content: a REV that holds a file the fix deleted lacks the fix. For an
    /// adopt this repository cannot measure, as where it does not hold the
    /// adopt, each named file is held to what its reference holds, and a
    /// reference that holds nothing there holds the fix as an absence. An
    /// adopt event that an older knives wrote names its files in its text
    /// alone; each path that its text may name is held where this
    /// repository cannot measure the adopt, and where it can, each such path
    /// that the adopt's own tree holds. Where this repository cannot measure
    /// the adopt and that text does not read whole (a list of more than 16
    /// pieces, or a list of files resolved by hand that another part
    /// follows, which may end there or name a path that holds that part),
    /// REV is refused, naming the adopt, and the fetch of its commit where
    /// this repository does not hold it and origin or the publish remote has
    /// a branch or tag at it, else `--allow-drop` alone. `--allow-drop` is
    /// the last resort there: it also waives the checks of each recorded
    /// member, of the newest recorded write and of each other fix by hand. A
    /// repository that does not hold the newest write is refused and told to
    /// fetch it. No reap runs, and the release is not signed or pushed; its
    /// ledger event is pushed as a cut's is.
    Cut {
        /// The dated release name, `release/[<series>/]YYYY-MM-DD[.N|-<label>]`.
        /// It must sort after the newest release of the series. Omit it for a
        /// configured fixed release branch.
        name: Option<String>,
        /// Proceed even when the cut would drop something: commits reachable
        /// only from the previous release lineage, members that the
        /// previous cut's ledger event or the newest recorded write of the
        /// newest release the ledger records in the series holds, a file of
        /// a fix by hand that an `adopt` of any release of the series
        /// recorded and the file's reference still holds, a fix by hand
        /// whose files this repository cannot read whole, or the newest write
        /// itself where this repository does not hold it. The refusal lists
        /// exactly what, each file of a fix by name.
        #[arg(long)]
        allow_drop: bool,
        /// Record a release cut outside knives: the commit becomes NAME's
        /// recorded cut; no commit is made. A dated or series NAME only: a
        /// fixed release branch records its published composition with a
        /// plain `cut`.
        #[arg(long, value_name = "REV")]
        at: Option<String>,
    },
    /// Rebase the composition onto an upstream commit: `jj rebase -b <release> -d <target>`.
    ///
    /// Each member branch's commits that the target's history does not hold
    /// move onto the target, and the release merge moves with them, member
    /// bookmarks and workspaces following; recorded conflict resolutions
    /// replay as ordinary rebase semantics. A member that the target's
    /// history holds does not move. A published release in hand (a tag, or a
    /// `record: published` note, names its commit) is refused: cut a new
    /// dated release first. Every other release name that is pushed or
    /// published stays on its commit, and one never pushed follows its
    /// rewrite. Pushed or published earlier releases keep their names and
    /// commit ids, and every tag stays; the copies the rebase makes of them
    /// are abandoned, except one something rests on, which is kept and named.
    /// A stale rule an earlier `knives start` wrote refuses first, naming
    /// `start` as the refresh. The base is never a release parent — this is
    /// how the members change theirs. Bare, it targets the first upstream
    /// trunk commit that contains every merged pull request, then drops the
    /// members whose landed branches carry nothing more. A flat release has
    /// no base parent, so the target reaches it only through a parent on the
    /// target: a member, or a carrier (an earlier write of the release). The
    /// drop does not keep the carriers, so a carrier on the target does not
    /// count. Where no member that stays is on the target, the drop keeps the
    /// landed members that are, and says why. Where none is, it drops nothing
    /// and exits 3. With nothing merged there is no default, and which commit
    /// to move onto is a decision — a cut does not make it for you.
    ///
    /// When the move leaves the rebased tip of a member conflicted, the
    /// report names that member, by its branch or by its short commit id,
    /// with its conflicted files, and the command exits 1. The member stays
    /// in the release: resolve it on its branch. The report says "no
    /// conflicts in this release" only when neither the release nor a moved
    /// member holds a conflict. A conflicted release with no conflicted
    /// member still exits 0. A landed drop that refuses exits 3, also when a
    /// moved member is conflicted.
    Rebase {
        /// The rebase target. Bare, the first upstream trunk commit containing
        /// every merged pull request; required when nothing has merged.
        reference: Option<String>,
        /// Keep members whose pull requests landed instead of dropping them.
        #[arg(long)]
        no_drop: bool,
    },
    /// Add a branch (or commit) to the release in hand as one new parent.
    ///
    /// Nothing else changes: every other parent stays at the commit the release
    /// already has. A member whose branch has advanced is not moved — that is a
    /// content change beyond including it, and `advance` is how it is asked for.
    /// Like `drop` and `advance`, the write is checked before it is made. An
    /// edit that would lose a line of a remaining member, away from any
    /// conflict that it leaves, writes nothing, names each such line with
    /// its path and member, and exits 3. A line at a conflict that the edit
    /// leaves is not counted. The edit's conflict report names the file of
    /// that conflict.
    Include {
        /// A branch name, or any revision when no bookmark fits.
        branch: String,
        /// Why it belongs. Recorded on the release itself.
        #[arg(long)]
        why: Option<String>,
        /// Write the edit even though it loses the named lines of a
        /// remaining member. The edit names the lost lines. Its ledger event
        /// records the loss as `; lost: <path> (<member>: <n> line(s))` for
        /// each path and member, then `; why lost: <reason>`. Put the lines
        /// back by hand, then record that with `knives release adopt <name>
        /// --why <reason>`. The reason must not be empty or only whitespace.
        /// Where the edit loses no line, the flag changes nothing.
        #[arg(long, value_name = "REASON", value_parser = reason_text)]
        allow_loss: Option<String>,
    },
    /// Remove a branch's parent from the release in hand.
    ///
    /// The branch and its bookmark are untouched — only the release changes. When
    /// the branch has advanced past its released parent, ancestry finds the
    /// parent; a commit id works when no bookmark does. The write is checked
    /// as `include`'s is.
    Drop {
        /// A branch name, or the parent's commit id when no bookmark fits.
        branch: String,
        /// Why it was dropped. Required, and recorded on the release itself:
        /// dropping shipped content without a reason is how a release becomes
        /// unexplainable later. It must hold a reason: an empty or
        /// whitespace-only value is refused.
        #[arg(long, value_parser = reason_text)]
        why: String,
        /// Write the drop even though it loses the named lines of a
        /// remaining member, as `include --allow-loss` does.
        #[arg(long, value_name = "REASON", value_parser = reason_text)]
        allow_loss: Option<String>,
    },
    /// Advance member parents to their branches' current tips.
    ///
    /// Moving a member is a content change, so it happens only when asked for:
    /// named branches move, and a bare `advance` moves every member whose branch
    /// has advanced. The trunk parent is `rebase`'s job.
    ///
    /// Matching a branch to its released parent is ancestry-based, which a `jj
    /// duplicate` rebuild breaks: the rebuilt tip shares no history with the
    /// commit it replaces. `--from` names that old commit directly, for exactly
    /// one branch, so the caller asserts the mapping instead of losing the
    /// release's recorded conflict resolution to a `drop` + `include`. The
    /// write is checked as `include`'s is.
    Advance {
        /// Branches to advance. Empty means every member that has advanced.
        branches: Vec<String>,
        /// The old parent commit a `jj duplicate`-rebuilt branch replaces.
        /// Requires exactly one branch; bypasses the ancestry search.
        #[arg(long)]
        from: Option<String>,
        /// Write the advance even though it loses the named lines of a
        /// remaining member, as `include --allow-loss` does.
        #[arg(long, value_name = "REASON", value_parser = reason_text)]
        allow_loss: Option<String>,
    },
    /// Record the commit a release name holds now as its newest knives write.
    ///
    /// For a release finished by hand: its conflicts resolved, a release log
    /// renumbered, a repair made on the release commit itself. Until it is
    /// recorded, audit reads that commit as drift from the write knives
    /// recorded, and `cut`, `include`, `drop`, `advance` and `rebase` refuse
    /// to write over a hand change it carries. The commit must have the
    /// parents of the write it replaces, plus any earlier write of the same
    /// name a fast-forward push added. A commit finished by hand must also
    /// have no conflicts. It is measured against that write rebuilt on the
    /// commit's member parents (the write itself when the commit is an older
    /// recorded write), or, where this repository lacks that write or the
    /// write's own record is marked unchecked, against a fresh merge of its
    /// member parents with the release-log union: the files it resolved are
    /// reported, with the lines each member added that it no longer holds,
    /// and a file changed outside the conflicts needs `--why`. Writes no
    /// commit and moves no bookmark: the record is a ledger event, and
    /// earlier writes of the name that nothing rests on are abandoned. A name
    /// the ledger records no cut of, a release cut outside knives, is
    /// recorded first with `knives release cut NAME --at <commit>`.
    ///
    /// A knives write that `jj describe`, `jj sign`, `jj metaedit` or a push
    /// that signs rewrote has a new commit id, with the same change, parents
    /// and tree. Run `adopt` on the machine that rewrote it, while the write
    /// is unpublished, so that another machine can read the release. It
    /// records the new commit in one event that holds no fix by hand. A
    /// conflict that the write left does not refuse it. The write is
    /// published when the publish remote holds its old commit, a tag is on
    /// that commit, or a `record: published` note names that commit. A copy
    /// of a published write is nothing to adopt. Where the write's record is
    /// marked unchecked, `adopt` measures the new commit as it measures any
    /// release that knives could not check.
    Adopt {
        /// The release name. Defaults to the release in hand.
        reference: Option<String>,
        /// Why the commit changes files the conflicts did not touch. Recorded
        /// in adopt's ledger event. It must hold a reason: an empty or
        /// whitespace-only value is refused.
        #[arg(long, value_parser = reason_text)]
        why: Option<String>,
    },
    /// The release's parents, or whether a revision's content is carried.
    ///
    /// Bare: count, commits, and who still holds each parent — the repository's
    /// own parent list, never `^parent` lines in prose. `--verify` reads each
    /// member's own lines in the release and names each line the release
    /// lacks, with its path. A member's own lines are the lines that its
    /// change adds and removes, from its fork point with the upstream trunk
    /// to its tip. A revision that the upstream trunk contains is read from
    /// where the trunk took it in. Where a merge commit took it in, that is
    /// its fork point with the first parent of that merge. Where it is on
    /// the trunk's first-parent line, that is the newest commit of that line
    /// below it that the release holds. A release that reaches the revision
    /// carries it. A member reads inconclusive only where the release, or the
    /// member's tip, holds an unresolved conflict in one of its paths and the
    /// release lacks nothing else of it. `--carries REV` asks the other
    /// direction: is REV's content carried — by REF when one is given,
    /// otherwise by every live release and the upstream trunk before any
    /// superseded cut (the rigorous "is this safe to delete"; grep answers a
    /// different question). Against a release it reads REV's own lines in
    /// the same way, and it reads `conflicted` only for such a conflict.
    /// Against the upstream trunk, ancestry and a replay of REV decide.
    /// `--census` asks that of every maintained branch at once.
    Members {
        /// The release ref to inspect. Defaults to the release in hand. With
        /// `--carries`, the one target to check instead of every target.
        reference: Option<String>,
        /// Read each member's own lines in the release, from the member's fork
        /// point with the upstream trunk. Name each line the release lacks and
        /// each line it holds only by its words.
        #[arg(long, conflicts_with = "carries")]
        verify: bool,
        /// A branch name, or any revision when no bookmark fits.
        #[arg(long, value_name = "REV")]
        carries: Option<String>,
        /// Every maintained branch against the live releases and upstream trunk;
        /// superseded releases only after a complete, everywhere-not-carried
        /// primary matrix. Answers "what would deleting this lose".
        #[arg(long, conflicts_with_all = ["reference", "verify", "carries"])]
        census: bool,
        /// Skip pull request lookups during a census; the orphan test then reports unknown.
        // `requires` alone does not keep this beside `--census`: clap waives a
        // required argument when a present one conflicts with it, so
        // `--carries x --no-github` would parse. The conflicts state the rule
        // in the direction clap enforces.
        #[arg(long, requires = "census", conflicts_with_all = ["reference", "verify", "carries"])]
        no_github: bool,
    },
    /// Reap superseded dated cuts: forget their bookmarks everywhere, abandon their commits
    /// unless a rule below keeps them. The remote is never touched.
    ///
    /// Runs automatically after every cut; exists standalone for pre-knives repos carrying
    /// years of historical refs. A later fetch re-materializes forgotten refs as untracked;
    /// re-run to clear them. Under the fork's `immutable_heads()` (the trunk, named on every
    /// knives remote, and upstream's tags, which `knives start` writes) those refs pin nothing
    /// for `jj rebase`, so reaping is tidiness, never an unlock.
    ///
    /// Keeps every superseded cut while the live one still carries conflicts: the previous
    /// cut is the only record of how they were last resolved.
    ///
    /// Keeps a superseded cut, refs and all, while something rests on it: a workspace's
    /// working copy on it or above it, empty or not, or any other descendant, on the cut, on
    /// a commit an edit wrote under its name, or on a rewrite of either. Exits 1 and names
    /// what rests on it; the next reap after that moves on retires it. A cut a tag, an
    /// untracked remote bookmark or a local bookmark of another name (`keep/…`) still names
    /// has its refs forgotten and its commit kept, exit 0. `--json` prints one document.
    ///
    /// Keeps a superseded name that a live release claim holds (`knives start <name>`),
    /// exit 1. Its line names the holder and `knives finish <name>`, which releases the
    /// claim. When it cannot read the claims, it keeps every superseded name and says so.
    Reap,
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use crate::config::test_support::{EnvironmentGuard, environment_lock};
    use clap::CommandFactory as _;

    #[test]
    fn a_machine_format_is_chosen_for_an_agent_and_text_for_a_person() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["KNIVES_OWNER"]);
        // Agents were grepping human output to count findings by detector. Explicit
        // flags win; otherwise the environment decides.
        assert_eq!(
            output_format(true, false),
            Output::Json,
            "--json is explicit"
        );
        assert_eq!(
            output_format(false, true),
            Output::Text,
            "--text is explicit"
        );
        // Neither flag: the environment decides, and this tool's own plugin exports
        // `KNIVES_OWNER` into every agent shell, so it is a direct signal rather than a
        // guess about terminals.
        environment.set("KNIVES_OWNER", "someone");
        assert_eq!(
            output_format(false, false),
            Output::Toon,
            "an agent shell gets the machine default"
        );
        assert_eq!(
            output_format(false, true),
            Output::Text,
            "--text still wins there"
        );
    }

    #[test]
    fn the_parser_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn gh_passthrough_keeps_ghs_own_flags_after_the_separator() {
        // Given: a gh invocation whose flags collide with our globals (--json).
        let cli = Cli::try_parse_from([
            "knives",
            "gh",
            "--",
            "pr",
            "list",
            "--json",
            "state",
            "-R",
            "acme/work",
        ])
        .expect("parse");
        // Then: everything after -- arrives verbatim, and OUR json flag is unset.
        let Command::Gh { args } = cli.command else {
            panic!("parsed into the wrong command");
        };
        assert_eq!(
            args,
            vec!["pr", "list", "--json", "state", "-R", "acme/work"]
        );
        assert!(!cli.json, "gh's --json leaked into the global flag");
    }

    #[test]
    fn gh_without_the_separator_takes_no_arguments() {
        // Given: a gh invocation without its required separator.
        // When / Then: clap refuses to guess where its arguments begin.
        assert!(Cli::try_parse_from(["knives", "gh", "pr", "list"]).is_err());
    }

    #[test]
    fn every_designed_command_is_reachable() {
        // Given: the command surface from the design, with minimum arguments
        let invocations: Vec<Vec<&str>> = vec![
            vec!["knives", "register"],
            vec!["knives", "hook", "claude-code"],
            vec!["knives", "repos"],
            vec!["knives", "sync", "--all"],
            vec!["knives", "preflight"],
            vec!["knives", "status"],
            vec!["knives", "pushed"],
            vec!["knives", "pushed", "feat/alpha", "--repo", "demo"],
            vec!["knives", "audit"],
            vec!["knives", "audit", "demo", "--no-github"],
            vec!["knives", "audit", "--all"],
            vec!["knives", "start", "a-branch"],
            vec!["knives", "finish", "a-branch"],
            vec!["knives", "release"],
            vec!["knives", "release", "cut"],
            vec!["knives", "release", "cut", "2026-08-01"],
            vec!["knives", "release", "rebase"],
            vec!["knives", "release", "reap"],
            vec!["knives", "release", "include", "feat/x"],
            vec!["knives", "release", "drop", "feat/y", "--why", "because"],
            vec!["knives", "release", "advance"],
            vec!["knives", "release", "advance", "feat/x"],
            vec!["knives", "release", "adopt"],
            vec![
                "knives",
                "release",
                "adopt",
                "release/2026-08-01",
                "--why",
                "renumbered",
            ],
            vec!["knives", "release", "members"],
            vec!["knives", "release", "members", "--carries", "x"],
            vec!["knives", "release", "members", "r", "--carries", "x"],
            vec!["knives", "release", "members", "--census"],
            vec!["knives", "depends", "a-branch", "--on", "other#1"],
            vec!["knives", "depends", "a-branch", "--forget", "other#1"],
            vec![
                "knives", "depends", "a-branch", "--forget", "other#1", "--on", "o/r#1",
            ],
            vec!["knives", "track", "a-branch", "--pr", "7"],
            vec!["knives", "notch"],
            vec!["knives", "notch", "feat/alpha"],
            vec!["knives", "notch", "feat/alpha", "-m", "superseded"],
            vec!["knives", "notch", "--pr", "1157"],
            vec!["knives", "notch", "release/2026-08-15", "--repo", "a-repo"],
            vec!["knives", "ledger", "sweep"],
            vec!["knives", "ledger", "migrate"],
            vec!["knives", "gh", "--", "pr", "list"],
        ];
        // When / Then: each parses
        for argv in invocations {
            assert!(
                Cli::try_parse_from(&argv).is_ok(),
                "failed to parse {argv:?}"
            );
        }
    }

    #[test]
    fn members_keeps_the_census_apart_from_a_target_a_replay_and_a_revision() {
        // Given: the parser is asked for two of the `members` questions at once,
        // or for `--no-github` beside anything but `--census`.
        let rejected: Vec<Vec<&str>> = vec![
            vec!["knives", "release", "members", "--verify", "--carries", "x"],
            vec!["knives", "release", "members", "--verify", "--census"],
            vec!["knives", "release", "members", "r", "--census"],
            vec!["knives", "release", "members", "--carries", "x", "--census"],
            vec!["knives", "release", "members", "--no-github"],
            vec![
                "knives",
                "release",
                "members",
                "--carries",
                "x",
                "--no-github",
            ],
            vec!["knives", "release", "members", "--verify", "--no-github"],
            vec!["knives", "release", "members", "r", "--no-github"],
        ];
        for argv in rejected {
            // Then: clap refuses rather than letting the dispatcher pick one.
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "parsed an incompatible members invocation: {argv:?}"
            );
        }
        assert!(
            Cli::try_parse_from(["knives", "release", "members", "--census", "--no-github"])
                .is_ok()
        );
    }

    #[test]
    fn sync_can_skip_forge_lookups() {
        assert!(Cli::try_parse_from(["knives", "sync", "--no-github"]).is_ok());
    }

    #[test]
    fn notch_rejects_evidence_without_a_message_or_nonblank_content() {
        assert!(
            Cli::try_parse_from(["knives", "notch", "feat/a", "-m", "x", "--pr", "7"]).is_ok(),
            "a write with an explicit pull-request stamp did not parse"
        );
        assert!(
            Cli::try_parse_from(["knives", "notch", "feat/a", "--evidence", "06d778b9"]).is_err(),
            "evidence with nothing to attach it to parsed"
        );
        assert!(
            Cli::try_parse_from([
                "knives",
                "notch",
                "feat/a",
                "-m",
                "x",
                "--disposition",
                "ruled-out",
                "--evidence",
                "   ",
            ])
            .is_err(),
            "blank disposition evidence parsed"
        );
        // And: a repo-level note needs no subject, so the model's absent subject
        // is reachable.
        assert!(Cli::try_parse_from(["knives", "notch", "-m", "the fork needs a cut"]).is_ok());
    }

    #[test]
    fn adopt_and_drop_refuse_a_why_that_holds_no_reason() {
        // Given: each verb with an empty and a whitespace-only `--why`.
        for blank in ["", "   ", " \t\n"] {
            for argv in [
                vec!["knives", "release", "adopt", "--why", blank],
                vec!["knives", "release", "drop", "feat/y", "--why", blank],
            ] {
                // When / Then: the parser refuses it, naming the flag.
                let error = Cli::try_parse_from(&argv)
                    .map(|_| ())
                    .expect_err("a blank why parsed");
                assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
                assert!(error.to_string().contains("--why"), "{argv:?}: {error}");
            }
        }
        // And: a real reason parses, padded or not.
        assert!(
            Cli::try_parse_from(["knives", "release", "adopt", "--why", " renumbered "]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["knives", "release", "drop", "feat/y", "--why", "not ready"])
                .is_ok()
        );
    }

    #[test]
    fn notch_accepts_only_the_terminal_disposition_vocabulary() {
        for disposition in ["merged-elsewhere", "withdrawn", "ruled-out"] {
            assert!(
                Cli::try_parse_from([
                    "knives",
                    "notch",
                    "feat/a",
                    "-m",
                    "x",
                    "--evidence",
                    "aabbccddeeff",
                    "--disposition",
                    disposition,
                ])
                .is_ok(),
                "{disposition} did not parse"
            );
        }
        assert!(
            Cli::try_parse_from([
                "knives",
                "notch",
                "feat/a",
                "-m",
                "x",
                "--evidence",
                "aabbccddeeff",
                "--disposition",
                "needs-review",
            ])
            .is_err(),
            "an unrecognized disposition parsed"
        );
    }

    #[test]
    fn notch_events_and_dispositions_are_mutually_exclusive_read_modes() {
        assert!(
            Cli::try_parse_from(["knives", "notch", "--events", "--dispositions"]).is_err(),
            "the parser accepted incompatible chronology selectors"
        );
    }

    #[test]
    fn track_fork_only_parses_without_a_pull_request_number() {
        // Marking a branch as deliberately having no upstream pull request must
        // not demand one.
        assert!(Cli::try_parse_from(["knives", "track", "a-branch", "--fork-only"]).is_ok());
    }

    #[test]
    fn an_unknown_command_is_rejected() {
        assert!(Cli::try_parse_from(["knives", "definitely-not-a-command"]).is_err());
    }

    #[test]
    fn exit_codes_are_distinct_and_success_is_zero() {
        assert_eq!(Exit::Ok.code(), 0);
        let codes = [Exit::Ok, Exit::Findings, Exit::Usage, Exit::Incomplete].map(Exit::code);
        let mut unique = codes.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 4);
    }
}
