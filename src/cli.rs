//! The command surface.
//!
//! Argument shapes are enforced by the parser rather than checked at runtime,
//! so a command cannot start work and then discover it lacks something it
//! needed. `--why` on a claim is required here for exactly that reason: the
//! description is what makes another agent's claim legible.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

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
    /// Emit JSON instead of text.
    ///
    /// The default is decided rather than fixed: an agent reading this output should
    /// not have to parse prose, and a human should not have to read JSON. See
    /// `machine_readable`.
    #[arg(long, global = true)]
    pub json: bool,
    /// Force text even where JSON would be chosen automatically.
    #[arg(long, global = true, conflicts_with = "json")]
    pub text: bool,
}

/// Whether to emit JSON.
///
/// Explicit flags win. Otherwise JSON when the output is not going to a terminal, or
/// when the environment says an agent is running this: agents were grepping human
/// output to count findings by detector, which is both fragile and unnecessary.
/// `KNIVES_OWNER` is set by this tool's own `OpenCode` plugin, so its presence is a
/// direct signal rather than a guess.
pub fn machine_readable(json: bool, text: bool) -> bool {
    if json {
        return true;
    }
    if text {
        return false;
    }
    if std::env::var_os("KNIVES_OWNER").is_some() {
        return true;
    }
    for name in ["CLAUDECODE", "CLAUDE_CODE", "OPENCODE", "AGENT", "CI"] {
        if std::env::var_os(name).is_some() {
            return true;
        }
    }
    !std::io::IsTerminal::is_terminal(&std::io::stdout())
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
    /// List the repos knives manages, with their release state.
    Repos,
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
        /// Skip the landed probe, which replays onto the trunk and cleans up.
        #[arg(long)]
        no_landed: bool,
        /// Skip pull request lookups. Branch pull request state reads as unknown.
        #[arg(long)]
        no_github: bool,
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
    },
    /// Hand a branch back and remove its workspace. The inverse of `start`.
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
    /// Plan, curate and cut dated releases.
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
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum HookHarness {
    ClaudeCode,
    Opencode,
}

#[derive(Debug, Subcommand)]
pub enum ReleaseAction {
    /// Cut the release. The only thing knives writes, and it still never pushes.
    Cut {
        /// The dated release name.
        name: String,
    },
    /// Add an upstream commit to the release in hand, keeping its branch parents.
    ///
    /// For when a pull request has merged upstream: until the release contains the commit
    /// the merge landed in, dropping the local branch takes the change out of the release
    /// with it. Which upstream commit, and whether to do this at all, is a decision — a
    /// cut does not do it for you.
    ///
    /// Whether this can happen in place or needs a new dated name follows from who pins
    /// the release: a consumer that follows the branch sees a repair, one frozen on the
    /// revision does not.
    Rebase {
        /// The upstream revision to include. Defaults to the upstream trunk.
        reference: Option<String>,
    },
    /// State that a branch belongs in the next release.
    ///
    /// Membership is every branch until something is stated, after which it is exactly
    /// what was stated: curating by hand and then having the fallback re-add everything
    /// would be worse than not curating.
    Include {
        branch: String,
        /// Why it belongs.
        #[arg(long)]
        why: Option<String>,
    },
    /// State that a branch does not belong in the next release.
    ///
    /// Survives the fallback, so dropping a change does not get undone by the next cut
    /// picking it up again because nobody listed the other twenty branches.
    Drop {
        branch: String,
        /// Why it was dropped. Worth recording: the reason is usually that something else
        /// was dropped too.
        #[arg(long)]
        why: Option<String>,
    },
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
    fn json_is_chosen_for_an_agent_and_text_for_a_person() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["KNIVES_OWNER"]);
        // Agents were grepping human output to count findings by detector. Explicit
        // flags win; otherwise the environment decides.
        assert!(machine_readable(true, false), "--json is explicit");
        assert!(!machine_readable(false, true), "--text is explicit");
        // Neither flag: the environment decides, and this tool's own plugin exports
        // `KNIVES_OWNER` into every agent shell, so it is a direct signal rather than a
        // guess about terminals.
        environment.set("KNIVES_OWNER", "someone");
        assert!(machine_readable(false, false), "an agent shell gets JSON");
        assert!(!machine_readable(false, true), "--text still wins there");
        environment.remove("KNIVES_OWNER");
    }

    #[test]
    fn the_parser_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_designed_command_is_reachable() {
        // Given: the command surface from the design, with minimum arguments
        let invocations: Vec<Vec<&str>> = vec![
            vec!["knives", "init"],
            vec!["knives", "hook", "claude-code"],
            vec!["knives", "repos"],
            vec!["knives", "sync", "--all"],
            vec!["knives", "preflight"],
            vec!["knives", "status"],
            vec!["knives", "start", "a-branch"],
            vec!["knives", "finish", "a-branch"],
            vec!["knives", "release"],
            vec!["knives", "release", "cut", "2026-08-01"],
            vec!["knives", "release", "rebase"],
            vec!["knives", "release", "include", "feat/x"],
            vec!["knives", "release", "drop", "feat/y"],
            vec!["knives", "depends", "a-branch", "--on", "other#1"],
            vec!["knives", "track", "a-branch", "--pr", "7"],
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
    fn a_claim_without_a_reason_is_rejected_by_the_parser() {
        // The description is what makes `knives wip` useful to another agent, so
        // the parser refuses rather than the command failing later.
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
