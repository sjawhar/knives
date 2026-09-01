//! The command surface.
//!
//! Argument shapes are enforced by the parser rather than checked at runtime,
//! so a command cannot start work and then discover it lacks something it
//! needed. `--why` on a claim is required here for exactly that reason: the
//! description is what makes another agent's claim legible.

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
/// signal rather than a guess. OMP exposes no such variable to its bash shells; that tool's
/// stdout is not a terminal, so OMP lands on the non-terminal fallback below.
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
    /// Configure remote roles for a repo. Writes the registry.
    Init {
        /// Repo directory. Defaults to the current directory.
        repo: Option<PathBuf>,
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
        /// Skip the landed probe, which replays every branch onto the trunk.
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
    /// Take a branch and open a workspace on the fetched upstream trunk.
    ///
    /// Claiming a branch and opening a workspace for it were two commands for no
    /// reason: starting work on a branch is one act. `finish` is its inverse.
    Start {
        branch: String,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
        /// What you are doing and why. A claim nobody can read is noise.
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
    /// Removing the directory loses no work: jj snapshots a working copy into a commit,
    /// so every change made in that workspace is already in the repository and reachable
    /// by change id. What does not survive is anything jj never tracked — build output,
    /// an untracked `.env` — so `--no-cleanup` leaves the directory alone.
    Finish {
        branch: String,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
        /// Forget the workspace but leave its directory on disk.
        #[arg(long)]
        no_cleanup: bool,
        /// Release a claim held by another identity. Requires a durable reason.
        #[arg(long, requires = "why")]
        force: bool,
        /// Why this forced release is safe.
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
    /// Record that a branch cannot land before something else does.
    ///
    /// Dependencies cross forks: a branch here can require a pull request in another
    /// managed repo. Dropping the thing it needs from a release, without dropping
    /// this too, ships something that cannot work.
    Depends {
        branch: String,
        /// What it needs, as `<repo>#<number>`, repeatable.
        #[arg(long = "on", required = true)]
        on: Vec<String>,
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
pub enum ReleaseAction {
    /// Name a new cut of the composition in hand, verbatim. Never pushes.
    Cut {
        /// The dated release name. Omit it for a configured fixed release branch.
        name: Option<String>,
        /// Proceed even when the cut would drop something: commits reachable
        /// only from the previous release lineage, or members the previous
        /// cut's ledger event recorded. The refusal lists exactly what.
        #[arg(long)]
        allow_drop: bool,
    },
    /// Rebase the composition onto an upstream commit: `jj rebase -b <release> -d <target>`.
    ///
    /// Every member branch's commits move onto the target and the release
    /// merge moves with them, bookmarks and workspaces following; recorded
    /// conflict resolutions replay as ordinary rebase semantics. The base is
    /// never a release parent — this is how the members change theirs. Bare,
    /// it targets the first upstream trunk commit that contains every merged
    /// pull request, then drops the members whose landed branches carry
    /// nothing more; with nothing merged there is no default, and which commit
    /// to move onto is a decision — a cut does not make it for you.
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
    Include {
        /// A branch name, or any revision when no bookmark fits.
        branch: String,
        /// Why it belongs. Recorded on the release itself.
        #[arg(long)]
        why: Option<String>,
    },
    /// Remove a branch's parent from the release in hand.
    ///
    /// The branch and its bookmark are untouched — only the release changes. When
    /// the branch has advanced past its released parent, ancestry finds the
    /// parent; a commit id works when no bookmark does.
    Drop {
        /// A branch name, or the parent's commit id when no bookmark fits.
        branch: String,
        /// Why it was dropped. Required, and recorded on the release itself:
        /// dropping shipped content without a reason is how a release becomes
        /// unexplainable later.
        #[arg(long)]
        why: String,
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
    /// release's recorded conflict resolution to a `drop` + `include`.
    Advance {
        /// Branches to advance. Empty means every member that has advanced.
        branches: Vec<String>,
        /// The old parent commit a `jj duplicate`-rebuilt branch replaces.
        /// Requires exactly one branch; bypasses the ancestry search.
        #[arg(long)]
        from: Option<String>,
    },
    /// Whether a revision's content is actually carried by releases or the upstream trunk.
    ///
    /// The rigorous answer to "is this fix safe to delete": compare the
    /// revision's net content with each target. Matching content is carried; a
    /// remaining diff is not carried; grep answers a different question. Bare
    /// form checks live releases and the upstream trunk before consulting
    /// superseded releases. `--in` checks one arbitrary revset instead.
    Carries {
        /// A branch name, or any revision when no bookmark fits.
        #[arg(required_unless_present = "all")]
        revision: Option<String>,
        /// The one release ref or revision to check instead of every target.
        #[arg(long = "in")]
        target: Option<String>,
        /// Every maintained branch against the live releases and upstream trunk;
        /// superseded releases are checked only after a complete, everywhere-not-carried
        /// primary matrix. The census that answers "what would deleting this lose".
        #[arg(long, conflicts_with_all = ["revision", "target"])]
        all: bool,
        /// Skip pull request lookups; the orphan test then reports unknown.
        #[arg(long, requires = "all")]
        no_github: bool,
    },
    /// The release's parents: count, commits, and who still holds each.
    ///
    /// The count uses the repository's own parent list — the audit's worst
    /// instrument error was counting `^parent` lines in prose. `--verify`
    /// replays every member against the release and reports what it lacks.
    Members {
        /// The release ref to inspect. Defaults to the release in hand.
        reference: Option<String>,
        /// Replay each member's content against the release (heavier: one
        /// replay per member) and report drop-guard anchors.
        #[arg(long)]
        verify: bool,
    },
    /// Reap superseded dated cuts: forget their bookmarks everywhere, abandon their commits.
    /// The remote is never touched.
    ///
    /// Runs automatically after every cut; exists standalone for pre-knives repos carrying
    /// years of historical refs, and as the unlock when a rebase needs old-lineage commits
    /// mutable (superseded release refs are immutable heads, and they freeze every member
    /// commit in their ancestry). A later fetch re-materializes forgotten refs as untracked;
    /// re-run to clear them.
    ///
    /// Keeps every superseded cut while the live one still carries conflicts: the previous
    /// cut is the only record of how they were last resolved.
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
            vec!["knives", "init"],
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
            vec!["knives", "depends", "a-branch", "--on", "other#1"],
            vec!["knives", "track", "a-branch", "--pr", "7"],
            vec!["knives", "notch"],
            vec!["knives", "notch", "feat/alpha"],
            vec!["knives", "notch", "feat/alpha", "-m", "superseded"],
            vec!["knives", "notch", "--pr", "1157"],
            vec!["knives", "notch", "release/2026-08-15", "--repo", "a-repo"],
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
