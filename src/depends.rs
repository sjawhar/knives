//! `knives depends`: what a branch cannot land before, stated, corrected and
//! taken back.
//!
//! A requirement is a pull request on a fork's upstream, a review pull request
//! on a fork's own repository, or a member branch of this fork
//! ([`Requirement`]). What a caller types is resolved to one of them before
//! anything is written. A pull request named by a fork's registry name alone
//! is asked of the forge: the upstream and the fork number their pull requests
//! apart, and the same number on both, read as the upstream's, recorded an
//! unrelated pull request where the fork's was meant. When both have it, the
//! command refuses and names both; it never guesses.
//!
//! A requirement is taken back by a `depends` forget followed by the list it
//! leaves: two entries that every machine running this knives folds into that
//! list ([`knives::statement::Statements::from_entries`]). A knives from before
//! fork requirements folds the same list only while the list left holds no
//! member branch: it cannot read a list that holds one, so the forget is the
//! newest statement it reads there, and the branch requires nothing at all.
//!
//! A forget restates the list this machine holds, and the machines still
//! agree on one list, but it can differ from what was meant, both ways. A
//! requirement another machine added that had not reached this one when it
//! wrote the forget is not in that list, and is lost. A list another machine
//! writes after the forget, from a ledger that had not yet received it (the
//! forget was not sent, or was sent after that machine's pull), still holds
//! the forgotten requirement, and brings it back; two forgets made that way
//! can undo each other. `docs/design.md`, "Dependencies".

use std::fmt::Display;

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::Bound;
use knives::config::{NonMember, Registry, RepoEntry};
use knives::forge::github::CliForge;
use knives::forge::{Forge, PullFacts, RepoIdentity};
use knives::ids::{BranchName, BranchTarget, Requirement};
use knives::jj::Repo;
use knives::ledger::{Draft, Stated};
use knives::remote_url::remote_slug;
use knives::statement::{Statement, StatementKind};
use knives::store::{Store, default_state_path};

use super::scribe_for;
use crate::branch_verbs::pulled_before_replacing;

/// The remote, and the role, of a fork's own repository.
const ORIGIN: &str = "origin";

/// What one `depends` asks for: requirements to add, as typed, and
/// requirements to take back, as typed.
pub(crate) struct Change<'a> {
    pub(crate) on: &'a [String],
    pub(crate) forget: &'a [String],
}

/// Record what a branch cannot land before, or take a requirement back.
///
/// Every requirement to add is resolved before the ledger is pulled, so a
/// refusal writes nothing and a forge round trip holds no lock. The
/// requirements to take back are matched against the list the pulled ledger
/// states, under the fork's lock; what the forge says of renamed spellings,
/// for a forget and for an add, is asked before the lock is taken.
pub(crate) fn run(
    fork: &Fork<'_>,
    branch: &BranchName,
    change: &Change<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let context = Context {
        fork,
        registry: bound.registry,
        branch,
        forge: &CliForge,
    };
    let mut added = Vec::new();
    for text in change.on {
        match context.requirement_on(text) {
            Ok(requirement) => added.push(requirement),
            Err(exit) => return Ok(exit),
        }
    }
    if !pulled_before_replacing(fork, bound, "the dependency") {
        return Ok(Exit::Incomplete);
    }
    let target = BranchTarget::new(fork.upstream.clone(), branch.clone());
    let forged = forged(&context, bound, change.forget, &added)?;
    // The fork's lock is held while the list is read and the next statement
    // appended, so two concurrent `depends` on this machine each build on the
    // other's list rather than racing; the pull above brought in what other
    // machines stated. Nothing here writes the state file.
    let store = Store::open_for_update(
        default_state_path(),
        &bound.registry.former_names(),
        &[&fork.upstream],
    )?;
    let required = store.dependencies(&target);
    let forgotten = match context.forgotten(&required, change.forget, &forged.ties) {
        Ok(forgotten) => forgotten,
        Err(exit) => return Ok(exit),
    };
    // A requirement to add is named as a record already held only when that
    // record stays: one this command takes back is not held.
    let staying: Vec<Requirement> = required
        .iter()
        .filter(|requirement| !forgotten.contains(requirement))
        .cloned()
        .collect();
    let own = context.own();
    let added = as_held(added, &staying, &forged.aliases, &own);
    let beside = context.spelled_beside(&staying, &added);
    let pr = store.tracked_pull(&target);
    let replaces = store.statement_stamp(&target, StatementKind::Depends);
    let subject = Some(branch.as_str());
    let scribe = scribe_for(fork, bound)?;
    if forgotten.is_empty() {
        let list = with_requirements(&required, &added, &own);
        let _ = scribe.record(&Draft {
            statement: Some(depends(Some(list.join(",")), replaces)),
            ..Draft::event(subject, format!("requires {}", listed(&added)), pr)
        })?;
        drop(store);
        println!("{}/{branch} now requires {}", fork.name, listed(&added));
        for line in &beside {
            println!("{line}");
        }
        return Ok(Exit::Ok);
    }
    let kept: Vec<Requirement> = required
        .into_iter()
        .filter(|requirement| !forgotten.contains(requirement))
        .collect();
    let list = with_requirements(&kept, &added, &own);
    let mut drafts = vec![Draft {
        statement: Some(depends(None, replaces)),
        ..Draft::event(
            subject,
            format!("no longer requires {}", listed(&forgotten)),
            pr,
        )
    }];
    if !list.is_empty() {
        drafts.push(Draft {
            statement: Some(depends(Some(list.join(",")), None)),
            ..Draft::event(subject, restated(&added, &kept), pr)
        });
    }
    let _ = scribe.record_in_order(&drafts)?;
    drop(store);
    let now = if list.is_empty() {
        "nothing".to_owned()
    } else {
        list.join(", ")
    };
    println!(
        "{}/{branch} no longer requires {}; it requires {now}",
        fork.name,
        listed(&forgotten)
    );
    for line in &beside {
        println!("{line}");
    }
    Ok(Exit::Ok)
}

/// Refuse `knives track --pr <number>` when `number` is not the upstream pull
/// request `branch` becomes. A fork and its upstream number their pull
/// requests apart, so the fork's review pull request #n read as the
/// upstream's #n names another pull request, or none; the upstream numbers
/// in the thousands, so most small fork numbers are also upstream numbers.
///
/// `Ok` when the upstream's #n is `branch`'s: its head is a branch named
/// `branch` of the fork's own repository's owner as the forge names it now,
/// asked first: a former owner a remote here still spells is not that
/// owner. `Ok` too when its head is another branch and the fork's #n is not
/// `branch`'s: a branch may carry someone else's upstream pull request. A
/// head is `branch`'s only by name and owner together: an outside
/// contributor's branch of the same name is not. Refused when the fork's #n
/// is `branch`'s and the upstream's is not (naming both), when only the fork
/// has #n, and when neither does. `Ok`, with no forge call, when the fork has
/// one forge repository at most, whose numbers are the only ones there are.
/// The fork's own repository is the one its checkout's `origin` remote
/// names when that is a forge repository, else its registry `origin`, as
/// `status` and `depends` read it ([`Fork::own_origin`]).
pub(crate) fn upstream_pull(
    fork: &Fork<'_>,
    registry: &Registry,
    branch: &BranchName,
    number: u64,
) -> Result<(), Exit> {
    let entry = fork.entry;
    let origin = fork.own_origin();
    let (Some(upstream), Some(spelled)) = (remote_slug(&entry.upstream), remote_slug(origin))
    else {
        return Ok(());
    };
    if upstream.eq_ignore_ascii_case(spelled) {
        return Ok(());
    }
    let context = Context {
        fork,
        registry,
        branch,
        forge: &CliForge,
    };
    let own = context.identity(origin)?;
    let own_name = own.name_with_owner.to_ascii_lowercase();
    let own_owner = own_name
        .split_once('/')
        .map_or(own_name.as_str(), |(owner, _)| owner);
    let branchs = |pull: &PullFacts| {
        pull.pull.head_ref_name == branch.as_str() && pull.pull.is_from(own_owner)
    };
    let theirs = RepoIdentity {
        name_with_owner: upstream.to_owned(),
        id: String::new(),
    };
    let on_upstream = context.pull(&theirs, number)?;
    if on_upstream.as_ref().is_some_and(branchs) {
        return Ok(());
    }
    let upstream_name = &fork.upstream;
    let suggested = context.accepted_spelling(&own_name, spelled);
    let waits = format!(
        "`track` states the upstream pull request {branch} becomes; a branch that waits for a \
         review pull request says so with `knives depends <branch> --on {suggested}#{number}`"
    );
    match (on_upstream, context.pull(&own, number)?) {
        (Some(theirs), Some(ours)) if branchs(&ours) => refused(
            Exit::Usage,
            format_args!(
                "#{number} names two pull requests: {upstream_name}#{number} ({}, head {}:{}) is \
                 not {branch}'s, and {own_name}#{number} ({}, head {own_owner}:{branch}) is \
                 {branch}'s review pull request on {}'s own repository. {waits}",
                theirs.pull.state.to_lowercase(),
                theirs
                    .pull
                    .head_repository_owner
                    .as_ref()
                    .map_or("unknown", |account| account.login.as_str()),
                theirs.pull.head_ref_name,
                ours.pull.state.to_lowercase(),
                fork.name,
            ),
        ),
        (Some(_), _) => Ok(()),
        (None, Some(ours)) => refused(
            Exit::Usage,
            format_args!(
                "{upstream_name} has no pull request #{number}; {own_name}#{number} is a review \
                 pull request on {}'s own repository (head {}). {waits}",
                fork.name, ours.pull.head_ref_name,
            ),
        ),
        (None, None) => refused(
            Exit::Usage,
            format_args!("neither {upstream_name} nor {own_name} has a pull request #{number}"),
        ),
    }
}

/// What the forge says of a `--forget` text's repository spelling: every
/// review pull request of its number the forge names as the repository the
/// text names (`same`), and those it could not be asked about (`unasked`).
/// For a member text: for a token on a repository no spelling here gives,
/// whether the forge names that repository as this fork's own
/// (`member_own`); and for each record of the member on a repository that is
/// neither a spelling here nor another fork's, whether the forge names it as
/// this fork's own (`placed`).
#[derive(Debug, Default)]
struct Tie {
    same: Vec<Requirement>,
    unasked: Vec<(Requirement, String)>,
    member_own: Option<Result<bool, String>>,
    placed: Vec<(Requirement, Result<bool, String>)>,
}

/// Which records of a list a `--forget` text answers to, and whether they
/// are one requirement.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Nothing,
    One(Vec<Requirement>),
    Ambiguous(Vec<Requirement>),
}

/// What one `--forget` text takes back: `known`, by what the forge
/// answered, and `at_most`, counting as named each record the forge could
/// not be asked about; `unasked` holds those records, each with why.
struct Weighed {
    known: Vec<Requirement>,
    at_most: Vec<Requirement>,
    unasked: Vec<(Requirement, String)>,
}

impl Weighed {
    /// A text whose records do not depend on what the forge says.
    fn settled(records: Vec<Requirement>) -> Self {
        Self {
            known: records.clone(),
            at_most: records,
            unasked: Vec::new(),
        }
    }
}

/// A `--forget` text that names a member ([`Context::member_named`]): the
/// repository a member token spells, none for a bare branch name.
struct MemberText<'t> {
    text: &'t str,
    repository: Option<String>,
    branch: BranchName,
}

/// The records of `required` spelled exactly `text`.
fn exactly(required: &[Requirement], text: &str) -> Vec<Requirement> {
    required
        .iter()
        .filter(|held| held.to_string() == text)
        .cloned()
        .collect()
}

/// Push each of `items` onto `into` that it does not hold yet.
fn joined<T: PartialEq>(into: &mut Vec<T>, items: impl IntoIterator<Item = T>) {
    for item in items {
        if !into.contains(&item) {
            into.push(item);
        }
    }
}

/// Why the forge could not be asked about any of `records`, each reason
/// once.
fn reasons(unasked: &[(Requirement, String)], records: &[&Requirement]) -> String {
    let mut why: Vec<&str> = Vec::new();
    joined(
        &mut why,
        unasked
            .iter()
            .filter(|(requirement, _)| records.contains(&requirement))
            .map(|(_, error)| error.as_str()),
    );
    why.join("; ")
}

/// What the forge says, before the fork's lock, about the list the pulled
/// ledger states: per `--forget` text, its [`Tie`]; per requirement to add,
/// the record already held under another name of its repository
/// ([`Context::held_alias`]). Asked before the lock, so no forge round trip
/// holds it. A list another writer changes before the lock is taken can
/// only make a forget refuse or an add write a second spelling, never
/// forget the wrong requirement.
#[derive(Default)]
struct Forged {
    ties: Vec<Tie>,
    aliases: Vec<Option<Requirement>>,
}

fn forged(
    context: &Context<'_, '_>,
    bound: Bound<'_>,
    forget: &[String],
    added: &[Requirement],
) -> anyhow::Result<Forged> {
    if forget.is_empty()
        && added
            .iter()
            .all(|wanted| matches!(wanted, Requirement::Upstream { .. }))
    {
        return Ok(Forged {
            ties: Vec::new(),
            aliases: vec![None; added.len()],
        });
    }
    let read = Store::open(
        default_state_path(),
        &bound.registry.former_names(),
        &[&context.fork.upstream],
    )?;
    let required = read.dependencies(&BranchTarget::new(
        context.fork.upstream.clone(),
        context.branch.clone(),
    ));
    drop(read);
    let ties = forget
        .iter()
        .map(|text| context.tied_by_forge(&required, text))
        .collect();
    let aliases = added
        .iter()
        .map(|wanted| context.held_alias(&required, wanted))
        .collect();
    Ok(Forged { ties, aliases })
}

/// `added`, each as `required` already holds it when it holds it under
/// another spelling: a member under another spelling of the fork's own
/// repository here ([`same`]), a review pull request or a member under a
/// name the forge resolves to its own (`aliases`, from [`forged`]).
fn as_held(
    added: Vec<Requirement>,
    required: &[Requirement],
    aliases: &[Option<Requirement>],
    own: &[String],
) -> Vec<Requirement> {
    added
        .into_iter()
        .zip(aliases.iter().chain(std::iter::repeat(&None)))
        .map(|(wanted, alias)| {
            required
                .iter()
                .find(|held| same(held, &wanted, own))
                .or_else(|| alias.as_ref().filter(|alias| required.contains(alias)))
                .cloned()
                .unwrap_or(wanted)
        })
        .collect()
}

/// A `depends` statement of `value`, a forget when it is `None`.
const fn depends(value: Option<String>, replaces: Option<jiff::Timestamp>) -> Stated {
    Stated {
        statement: Statement {
            kind: StatementKind::Depends,
            value,
        },
        replaces,
    }
}

/// The prose of the entry that restates a list after a forget: what was
/// `added`, and what of the list before was `kept`.
fn restated(added: &[Requirement], kept: &[Requirement]) -> String {
    let still: Vec<&Requirement> = kept
        .iter()
        .filter(|requirement| !added.contains(requirement))
        .collect();
    match (added.is_empty(), still.is_empty()) {
        (false, false) => format!(
            "requires {}; still requires {}",
            listed(added),
            listed(still)
        ),
        (false, true) => format!("requires {}", listed(added)),
        (true, _) => format!("still requires {}", listed(still)),
    }
}

fn listed<'r>(requirements: impl IntoIterator<Item = &'r Requirement>) -> String {
    requirements
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether `a` and `b` require the same thing, by their spelling alone. Two
/// records of one member branch are one requirement when each spells this
/// fork's own repository as a spelling here, one of `own`: machines whose
/// `origin` remotes spell it differently record the same member under two
/// spellings. A record on any other repository, another fork's or one no
/// spelling here gives, is the same only as a record spelled so: only the
/// forge can say it names this fork's repository ([`Context::member_tie`]).
fn same(a: &Requirement, b: &Requirement, own: &[String]) -> bool {
    let ours = |repository: &str| own.contains(&repository.to_ascii_lowercase());
    match (a, b) {
        (
            Requirement::ForkBranch {
                repository: one_repository,
                branch: one,
            },
            Requirement::ForkBranch {
                repository: other_repository,
                branch: other,
            },
        ) => {
            one == other
                && (one_repository.eq_ignore_ascii_case(other_repository)
                    || (ours(one_repository) && ours(other_repository)))
        }
        _ => a == b,
    }
}

/// Everything a branch requires once `added` joins what it already `required`.
///
/// Additive and deduplicated ([`same`]), so declaring the same requirement
/// twice is not an error and re-running a script does not accumulate
/// duplicates. The result is the whole list, so each entry reads on its own
/// as everything the branch needed when it was written.
fn with_requirements(
    required: &[Requirement],
    added: &[Requirement],
    own: &[String],
) -> Vec<String> {
    let mut kept: Vec<&Requirement> = required.iter().collect();
    for requirement in added {
        if !kept.iter().any(|held| same(held, requirement, own)) {
            kept.push(requirement);
        }
    }
    let mut texts: Vec<String> = kept.iter().map(ToString::to_string).collect();
    texts.sort();
    texts.dedup();
    texts
}

/// Say `message` and refuse with `exit`.
fn refused<T>(exit: Exit, message: impl Display) -> Result<T, Exit> {
    eprintln!("{message}");
    Err(exit)
}

/// `<repo>` of `text` when `text` is `<repo>#<number>`, with the number.
fn numbered(text: &str) -> Option<(&str, u64)> {
    let (repo, number) = text.rsplit_once('#')?;
    if repo.is_empty() {
        return None;
    }
    Some((repo, number.parse().ok()?))
}

/// Where requirements are resolved: the fork the dependent branch is a branch
/// of, the registry, and the forge to ask.
struct Context<'a, 'fork> {
    fork: &'a Fork<'fork>,
    registry: &'a Registry,
    branch: &'a BranchName,
    forge: &'a dyn Forge,
}

impl Context<'_, '_> {
    /// The requirement `--on <text>` names, or the exit its refusal takes,
    /// said on stderr.
    fn requirement_on(&self, text: &str) -> Result<Requirement, Exit> {
        if text.starts_with("origin:") {
            return match Requirement::parse(text) {
                Some(Requirement::ForkPull { repository, number }) => {
                    let origin = self.own_origin(&repository, text)?;
                    self.fork_pull(&origin, number)
                }
                Some(Requirement::ForkBranch { repository, branch }) => {
                    if let Some(key) = self.other_fork(&repository) {
                        return refused(
                            Exit::Usage,
                            format_args!(
                                "{text} names a branch of {repository}, which is {key}'s own \
                                 repository, not {}'s: a requirement names a member of this fork",
                                self.fork.name
                            ),
                        );
                    }
                    if !self.is_own_repository(&repository)
                        && self.renamed_origin(&repository, text)?.is_none()
                    {
                        return refused(
                            Exit::Usage,
                            format_args!(
                                "{text} names a branch of {repository}, which is not {}'s own \
                                 repository",
                                self.fork.name
                            ),
                        );
                    }
                    self.member_branch(branch.as_str())
                }
                _ => refused(
                    Exit::Usage,
                    format_args!(
                        "cannot read {text} as a requirement: write `origin:<owner>/<name>#<number>` \
                         or `origin:<owner>/<name>:<branch>`"
                    ),
                ),
            };
        }
        match numbered(text) {
            Some((repo, number)) => self.pull_request(text, repo, number),
            None => self.member_branch(text),
        }
    }

    /// The pull request `<repo>#<number>` names: by a registry name, the one
    /// the forge finds; by this fork's own repository, that repository's,
    /// even when a fork of this fork in the registry has it as its upstream;
    /// by an upstream's `<owner>/<name>`, that upstream's.
    fn pull_request(&self, text: &str, repo: &str, number: u64) -> Result<Requirement, Exit> {
        if let Some(fork) = self.registry.repos.get_key_value(repo) {
            return self.asked_which(text, fork, number);
        }
        if let Some(origin) = self.own_origin_named(repo) {
            return self.fork_pull(origin, number);
        }
        if let Some(upstream) = self.registry.resolve(repo) {
            return Ok(Requirement::Upstream {
                repo: upstream,
                number,
            });
        }
        if let Some(key) = self.other_fork(repo) {
            return self.another_forks_review(text, key);
        }
        if self.has_branch(text) {
            return self.member_branch(text);
        }
        if let Some(origin) = self.renamed_origin(repo, text)? {
            return self.fork_pull(&origin, number);
        }
        let known: Vec<String> = self
            .registry
            .repos
            .iter()
            .flat_map(|(key, entry)| {
                std::iter::once(key.clone()).chain(remote_slug(&entry.origin).map(str::to_owned))
            })
            .collect();
        refused(
            Exit::Usage,
            format_args!(
                "unknown repo {repo} in {text}; known: {}, or a branch of {}",
                known.join(", "),
                self.fork.name
            ),
        )
    }

    /// Pull request `number` of the fork `key` names: on its upstream or on
    /// its own repository, whichever the forge says has it. A review pull
    /// request on the own repository of a fork other than this one is
    /// refused: its head is no member of this fork, so nothing here can say
    /// when it has landed ([`Self::another_forks_review`]).
    fn asked_which(
        &self,
        text: &str,
        (key, entry): (&String, &RepoEntry),
        number: u64,
    ) -> Result<Requirement, Exit> {
        let upstream_name = entry.upstream_name(key);
        let this_fork = upstream_name == self.fork.upstream;
        // This fork's own repository is read as `status` and `track` read it:
        // its checkout's `origin` when that names a forge repository.
        let origin = if this_fork {
            self.fork.own_origin()
        } else {
            entry.origin.as_str()
        };
        let upstream = remote_slug(&entry.upstream);
        let (Some(upstream), Some(spelled)) = (upstream, remote_slug(origin)) else {
            // At most one forge repository holds this fork's pull requests:
            // the number is that repository's, as it always was.
            return Ok(Requirement::Upstream {
                repo: upstream_name,
                number,
            });
        };
        let own = self.identity(origin)?;
        if own.name_with_owner.eq_ignore_ascii_case(upstream) {
            return Ok(Requirement::Upstream {
                repo: upstream_name,
                number,
            });
        }
        let theirs = RepoIdentity {
            name_with_owner: upstream.to_owned(),
            id: String::new(),
        };
        let on_upstream = self.pull(&theirs, number)?;
        let on_fork = self.pull(&own, number)?;
        let own = own.name_with_owner.to_ascii_lowercase();
        match (on_upstream, on_fork) {
            (Some(theirs), Some(ours)) if this_fork => refused(
                Exit::Usage,
                format_args!(
                    "{text} names two pull requests: {upstream_name}#{number} on {key}'s upstream \
                     ({}, head {}) and {own}#{number} on {key}'s own repository ({}, head {}); \
                     name the one meant: `--on {upstream_name}#{number}` or `--on {}#{number}`",
                    theirs.pull.state.to_lowercase(),
                    theirs.pull.head_ref_name,
                    ours.pull.state.to_lowercase(),
                    ours.pull.head_ref_name,
                    self.accepted_spelling(&own, spelled),
                ),
            ),
            (Some(theirs), Some(ours)) => refused(
                Exit::Usage,
                format_args!(
                    "{text} names two pull requests: {upstream_name}#{number} on {key}'s upstream \
                     ({}, head {}) and {own}#{number} on {key}'s own repository ({}, head {}). \
                     Only the upstream one can be required from {}: a review pull request on \
                     another fork's repository cannot be judged here. If it is meant, name it: \
                     `--on {upstream_name}#{number}`",
                    theirs.pull.state.to_lowercase(),
                    theirs.pull.head_ref_name,
                    ours.pull.state.to_lowercase(),
                    ours.pull.head_ref_name,
                    self.fork.name,
                ),
            ),
            (Some(_), None) => Ok(Requirement::Upstream {
                repo: upstream_name,
                number,
            }),
            (None, Some(ours)) if this_fork => {
                self.review_head(&own, number, &ours)?;
                Ok(Requirement::ForkPull {
                    repository: own,
                    number,
                })
            }
            (None, Some(_)) => self.another_forks_review(text, key),
            (None, None) => refused(
                Exit::Usage,
                format_args!(
                    "neither {upstream_name} nor {own} has a pull request #{number}, so {text} \
                     names nothing"
                ),
            ),
        }
    }

    /// Refuse a review pull request on the own repository of the fork `key`,
    /// not this one: its head is a branch of that fork, which `status` here
    /// cannot judge.
    fn another_forks_review(&self, text: &str, key: &str) -> Result<Requirement, Exit> {
        refused(
            Exit::Usage,
            format_args!(
                "{text} is a review pull request on {key}'s own repository, and {} cannot depend \
                 on one: its head is a branch of {key}, which nothing here can judge. Name the \
                 upstream pull request that branch becomes, as `<upstream-owner>/<name>#<number>`",
                self.fork.name
            ),
        )
    }

    /// Review pull request `number` on the own repository `origin` names, by
    /// the name the forge gives that repository now.
    fn fork_pull(&self, origin: &str, number: u64) -> Result<Requirement, Exit> {
        let own = self.identity(origin)?;
        // A spelling that matched the registry's `origin` while the checkout's
        // names another live repository is not this fork's own: `status`
        // judges review pull requests on the checkout's.
        let own_now = self.fork.own_origin();
        if origin != own_now {
            let current = self.identity(own_now)?;
            if !current
                .name_with_owner
                .eq_ignore_ascii_case(&own.name_with_owner)
            {
                return refused(
                    Exit::Usage,
                    format_args!(
                        "{} is not {}'s own repository: its checkout's {ORIGIN} names {}",
                        own.name_with_owner, self.fork.name, current.name_with_owner
                    ),
                );
            }
        }
        let Some(ours) = self.pull(&own, number)? else {
            return refused(
                Exit::Usage,
                format_args!("{} has no pull request #{number}", own.name_with_owner),
            );
        };
        let own = own.name_with_owner.to_ascii_lowercase();
        self.review_head(&own, number, &ours)?;
        Ok(Requirement::ForkPull {
            repository: own,
            number,
        })
    }

    /// Refuse a review pull request on this fork's own repository `own` whose
    /// head is no member branch another branch can wait for: a branch of
    /// another owner (an outside contributor's `main`), a base, a release, a
    /// fetched pull request head, or the dependent itself. `status` judges a
    /// review pull request by its head; any of these it could not judge, or
    /// would judge as another branch.
    fn review_head(&self, own: &str, number: u64, ours: &PullFacts) -> Result<(), Exit> {
        let head = BranchName::new(ours.pull.head_ref_name.as_str());
        let owner = own.split_once('/').map_or(own, |(owner, _)| owner);
        if !ours.pull.is_from(owner) {
            let whose = ours
                .pull
                .head_repository_owner
                .as_ref()
                .map_or("an unknown owner", |account| account.login.as_str());
            return refused(
                Exit::Usage,
                format_args!(
                    "{own}#{number}'s head {head} is a branch of {whose}, not of {owner}: no \
                     member of {}, so nothing here can say when it has landed",
                    self.fork.name
                ),
            );
        }
        if head == *self.branch {
            return refused(
                Exit::Usage,
                format_args!(
                    "{own}#{number} is {head}'s own review pull request (head {head}): a branch \
                     cannot require itself"
                ),
            );
        }
        if let Some(kind) = self.fork.entry.non_member(&head) {
            return refused(
                Exit::Usage,
                format_args!(
                    "{own}#{number}'s head {head} is {kind} of {}, not a member branch",
                    self.fork.name
                ),
            );
        }
        Ok(())
    }

    /// The spelling of a fork's own repository that `--on` takes: the name
    /// the forge gives it, `own`, when a remote here spells it so, else the
    /// registry's `spelled`.
    fn accepted_spelling<'s>(&self, own: &'s str, spelled: &'s str) -> &'s str {
        if self.own_origin_named(own).is_some() {
            own
        } else {
            spelled
        }
    }

    /// The member branch `name` of this fork, which must be a bookmark here
    /// or on its origin, a branch `status` judges (no base, release or
    /// fetched pull request head), and not the dependent itself.
    fn member_branch(&self, name: &str) -> Result<Requirement, Exit> {
        let branch = match BranchName::parse(name) {
            Ok(branch) => branch,
            Err(reason) => return refused(Exit::Usage, reason),
        };
        if branch == *self.branch {
            return refused(Exit::Usage, format_args!("{branch} cannot require itself"));
        }
        if name.contains(',') {
            return refused(
                Exit::Usage,
                format_args!(
                    "{name} cannot be recorded as a requirement: a record joins its requirements \
                     with `,`"
                ),
            );
        }
        let fork = &self.fork.name;
        if let Some(kind) = self.fork.entry.non_member(&branch) {
            let hint = if kind == NonMember::PullHead {
                "; name the pull request as `<repo>#<number>`"
            } else {
                ""
            };
            return refused(
                Exit::Usage,
                format_args!("{name} is {kind} of {fork}, not a member branch{hint}"),
            );
        }
        if !self.has_branch(name) {
            return refused(
                Exit::Usage,
                format_args!(
                    "this checkout has no branch {name}, here or on {ORIGIN}; a pull request is \
                     named as `<repo>#<number>`"
                ),
            );
        }
        let Some(repository) = self.own_repository() else {
            return refused(
                Exit::Usage,
                format_args!(
                    "{}'s {ORIGIN} {} names no forge repository, and a member branch is recorded \
                     with the repository it is pushed to",
                    self.fork.name, self.fork.entry.origin
                ),
            );
        };
        Ok(Requirement::ForkBranch { repository, branch })
    }

    /// What the `--forget` texts take back from `required`: every record one
    /// of them takes ([`Self::weighed`]), the texts weighed together. When
    /// the forge could not be asked about a record, and counting it as named
    /// would take back more than the forge's answers take, nothing is taken
    /// back and the refusal says so (exit 3): neither an outage nor a
    /// repository the token cannot see decides what a forget takes. Each such
    /// refusal prints a command no forge answer changes ([`Self::recovery`]).
    fn forgotten(
        &self,
        required: &[Requirement],
        texts: &[String],
        ties: &[Tie],
    ) -> Result<Vec<Requirement>, Exit> {
        let mut known: Vec<Requirement> = Vec::new();
        let mut at_most: Vec<Requirement> = Vec::new();
        let mut unasked: Vec<(Requirement, String)> = Vec::new();
        for (text, tie) in texts.iter().zip(ties) {
            let weighed = self.weighed(required, text, tie)?;
            joined(&mut known, weighed.known);
            joined(&mut at_most, weighed.at_most);
            joined(&mut unasked, weighed.unasked);
        }
        let also: Vec<&Requirement> = at_most
            .iter()
            .filter(|record| !known.contains(record))
            .collect();
        if also.is_empty() {
            return Ok(known);
        }
        refused(
            Exit::Incomplete,
            format_args!(
                "nothing recorded: whether {} also {} {} could not be asked of the forge: {}. {}",
                texts.join(" or "),
                if texts.len() == 1 { "names" } else { "name" },
                listed(also.iter().copied()),
                reasons(&unasked, &also),
                self.recovery(required, &at_most)
            ),
        )
    }

    /// What to do when the forge could not say what a forget takes: run it
    /// again when it answers, or run the printed command, which takes back
    /// every record of every requirement among `records` ([`Self::token_sets`])
    /// whatever the forge answers. It can take a record the forge named as
    /// another repository's: with the forge unable to say what one text
    /// names, any record of the number on a repository that is no other
    /// fork's may be meant. To keep one, wait for the forge.
    fn recovery(&self, required: &[Requirement], records: &[Requirement]) -> String {
        let mut tokens: Vec<String> = Vec::new();
        joined(
            &mut tokens,
            self.token_sets(required, records).into_iter().flatten(),
        );
        format!(
            "Run it again when the forge answers, or take back every record it might name and \
             every other record of the same number or member, each as it is spelled, which no \
             forge answer changes: {}",
            self.command(&tokens)
        )
    }

    /// What to do when a text may name two requirements and the forge could
    /// not say: run it again when it answers, or take back the one meant
    /// with every other record of it, by a command per requirement among
    /// `records` that no forge answer changes.
    fn choices(&self, required: &[Requirement], records: &[Requirement]) -> String {
        let commands: Vec<String> = self
            .token_sets(required, records)
            .iter()
            .map(|tokens| self.command(tokens))
            .collect();
        format!(
            "If it does, it names two requirements. Run it again when the forge answers, or take \
             back the one meant, with every other record of it, by a command no forge answer \
             changes: {}",
            commands.join(" or ")
        )
    }

    /// `knives depends <branch> --forget <token>…`, quoted as a refusal
    /// prints it.
    fn command(&self, tokens: &[String]) -> String {
        format!(
            "`knives depends {} --forget {}`",
            self.branch,
            tokens.join(" --forget ")
        )
    }

    /// The `--forget` texts that take back each requirement among `records`,
    /// one set per requirement: an upstream record alone; every record of a
    /// review pull request number or a member in `required` that may be the
    /// same requirement, each as it is spelled, but a member by its branch
    /// name when every such record spells this fork's repository as a
    /// spelling here and the name holds no `#`. A record on another fork's
    /// own repository is never a record of this fork's, nor the reverse
    /// ([`Self::tied_by_forge`], [`same`]): it is taken alone. A text spelled
    /// as a record names that record directly, so no forge answer changes
    /// what a set takes.
    fn token_sets(&self, required: &[Requirement], records: &[Requirement]) -> Vec<Vec<String>> {
        let foreign = self.foreign();
        let away = |record: &Requirement| {
            record
                .repository()
                .is_some_and(|repository| foreign.contains(&repository.to_ascii_lowercase()))
        };
        let mut sets: Vec<Vec<String>> = Vec::new();
        for record in records {
            let of_it: Vec<&Requirement> = if away(record) {
                vec![record]
            } else {
                required
                    .iter()
                    .filter(|held| !away(held) && held.same_number_or_member(record))
                    .chain([record])
                    .collect()
            };
            let mut tokens: Vec<String> = Vec::new();
            match record {
                Requirement::ForkBranch { branch, .. }
                    if !away(record)
                        && !branch.as_str().contains('#')
                        && of_it.iter().all(|held| {
                            held.repository()
                                .is_some_and(|repository| self.is_own_repository(repository))
                        }) =>
                {
                    tokens.push(branch.to_string());
                }
                _ => joined(&mut tokens, of_it.iter().map(ToString::to_string)),
            }
            if !sets.contains(&tokens) {
                sets.push(tokens);
            }
        }
        sets
    }

    /// What `--forget <text>` takes back from `required` ([`Weighed`]): the
    /// one requirement it names, every record of it whatever repository
    /// spelling each carries. A text spelled exactly as a held upstream
    /// record names that record alone. A member text: [`Self::member_weighed`].
    /// A review pull request's records by the forge naming each record's
    /// repository as the one `text` names (`tie`, asked before the fork's
    /// lock: [`Self::tied_by_forge`]). Refused when, by what the forge
    /// answered, it names two requirements or nothing (exit 2); and when
    /// counting a record the forge could not be asked about as named would
    /// make it name two, join two it names into one, or name what it
    /// otherwise names nothing of (exit 3).
    fn weighed(&self, required: &[Requirement], text: &str, tie: &Tie) -> Result<Weighed, Exit> {
        if let Some(exact) = required
            .iter()
            .find(|held| matches!(held, Requirement::Upstream { .. }) && held.to_string() == text)
        {
            return Ok(Weighed::settled(vec![exact.clone()]));
        }
        if let Some(weighed) = self.member_weighed(required, text, tie) {
            return weighed;
        }
        let mut tied = tie.same.clone();
        tied.extend(
            tie.unasked
                .iter()
                .map(|(requirement, _)| requirement.clone()),
        );
        let at_most = self.outcome(required, text, &tied);
        let two = matches!(at_most, Outcome::Ambiguous(_));
        match (self.outcome(required, text, &tie.same), at_most) {
            (Outcome::Ambiguous(records), Outcome::One(_)) => refused(
                Exit::Incomplete,
                format_args!(
                    "nothing recorded: {text} answers to {}, and whether they are one requirement \
                     could not be asked of the forge: {}. {}",
                    listed(&records),
                    reasons(&tie.unasked, &records.iter().collect::<Vec<_>>()),
                    self.recovery(required, &records)
                ),
            ),
            (Outcome::Ambiguous(records), _) => refused(
                Exit::Usage,
                format_args!(
                    "{text} answers to {}; name the one to forget as its record spells it",
                    listed(&records)
                ),
            ),
            (Outcome::Nothing, Outcome::Nothing) => refused(
                Exit::Usage,
                format_args!(
                    "{}/{} does not require {text}; it requires {}",
                    self.fork.name,
                    self.branch,
                    if required.is_empty() {
                        "nothing".to_owned()
                    } else {
                        listed(required)
                    }
                ),
            ),
            (Outcome::One(known), Outcome::One(at_most)) => Ok(Weighed {
                known,
                at_most,
                unasked: tie.unasked.clone(),
            }),
            (Outcome::One(known), Outcome::Nothing) => Ok(Weighed::settled(known)),
            (known, Outcome::One(at_most) | Outcome::Ambiguous(at_most)) => {
                let known = match known {
                    Outcome::One(records) => records,
                    _ => Vec::new(),
                };
                let also: Vec<&Requirement> = at_most
                    .iter()
                    .filter(|record| !known.contains(record))
                    .collect();
                refused(
                    Exit::Incomplete,
                    format_args!(
                        "nothing recorded: whether {text} {} {} could not be asked of the \
                         forge: {}. {}{}",
                        if known.is_empty() {
                            "names"
                        } else {
                            "also names"
                        },
                        listed(also.iter().copied()),
                        reasons(&tie.unasked, &also),
                        if two {
                            self.choices(required, &at_most)
                        } else {
                            self.recovery(required, &at_most)
                        },
                        ""
                    ),
                )
            }
        }
    }

    /// What `--forget` of a member text takes back: a member token, or a
    /// bare branch name the list holds a member of ([`Self::member_named`]).
    /// A token on another fork's own repository: the record spelled exactly
    /// so alone, or a refusal (exit 2). A text that names this fork's member
    /// (a bare name, a token on a spelling here, or on a repository the forge
    /// names as this fork's own): every record of the member that spells a
    /// spelling here or that the forge places as this fork's own, and at most
    /// those it could not be asked about too ([`Tie::placed`]). A token on a
    /// repository the forge names as another: the record spelled exactly so
    /// alone. One on a repository the forge could not be asked about: the
    /// record spelled exactly so, and at most every record of the member.
    /// Exit 2 when it names nothing; exit 3 when it names nothing but by
    /// what the forge could not be asked.
    fn member_weighed(
        &self,
        required: &[Requirement],
        text: &str,
        tie: &Tie,
    ) -> Option<Result<Weighed, Exit>> {
        let named = self.member_named(required, text)?;
        let fork = &self.fork.name;
        if let Some(repository) = named.repository.as_deref()
            && let Some(key) = self.other_fork(repository)
        {
            let exact = exactly(required, text);
            if !exact.is_empty() {
                return Some(Ok(Weighed::settled(exact)));
            }
            return Some(refused(
                Exit::Usage,
                format_args!(
                    "{text} names a branch of {repository}, which is {key}'s own repository, \
                     not {fork}'s, and {fork}/{} requires nothing spelled so",
                    self.branch
                ),
            ));
        }
        let (names_ours, weighed) = self.member_records(required, &named, tie);
        let Weighed {
            known,
            at_most,
            unasked,
        } = weighed;
        if at_most.is_empty() {
            return Some(match (named.repository.as_deref(), names_ours) {
                (Some(repository), Ok(false)) => refused(
                    Exit::Usage,
                    format_args!(
                        "{text} names a branch of {repository}, which is not {fork}'s own \
                         repository; name the requirement as its record spells it, or by its \
                         branch name"
                    ),
                ),
                _ => refused(
                    Exit::Usage,
                    format_args!(
                        "{fork}/{} does not require {text}; it requires {}",
                        self.branch,
                        listed(required)
                    ),
                ),
            });
        }
        if known.is_empty() {
            let also: Vec<&Requirement> = at_most.iter().collect();
            return Some(refused(
                Exit::Incomplete,
                format_args!(
                    "nothing recorded: whether {text} names {} could not be asked of the forge: \
                     {}. {}",
                    listed(also.iter().copied()),
                    reasons(&unasked, &also),
                    self.recovery(required, &at_most)
                ),
            ));
        }
        Some(Ok(Weighed {
            known,
            at_most,
            unasked,
        }))
    }

    /// The records a member text that is no other fork's token names
    /// ([`Self::member_weighed`]): whether it names this fork's member, and
    /// what it takes ([`Weighed`]).
    fn member_records(
        &self,
        required: &[Requirement],
        named: &MemberText<'_>,
        tie: &Tie,
    ) -> (Result<bool, String>, Weighed) {
        let exact = exactly(required, named.text);
        let ours: Vec<Requirement> = required
            .iter()
            .filter(|held| {
                matches!(held, Requirement::ForkBranch { repository: on, branch: other }
                    if *other == named.branch && self.is_own_repository(on))
            })
            .cloned()
            .collect();
        let mut placed_own: Vec<Requirement> = Vec::new();
        let mut unplaced: Vec<(Requirement, String)> = Vec::new();
        for (held, placed) in &tie.placed {
            match placed {
                Ok(true) => placed_own.push(held.clone()),
                Ok(false) => {}
                Err(error) => unplaced.push((held.clone(), error.clone())),
            }
        }
        let names_ours = match named.repository.as_deref() {
            None => Ok(true),
            Some(repository) if self.is_own_repository(repository) => Ok(true),
            Some(_) => tie.member_own.clone().unwrap_or(Ok(false)),
        };
        let mut known = exact.clone();
        let mut unasked: Vec<(Requirement, String)> = Vec::new();
        match &names_ours {
            Ok(true) => {
                joined(&mut known, ours.into_iter().chain(placed_own));
                unasked = unplaced;
            }
            Ok(false) => {}
            Err(error) => {
                for record in ours.into_iter().chain(placed_own) {
                    if !exact.contains(&record) {
                        unasked.push((record, error.clone()));
                    }
                }
                joined(&mut unasked, unplaced);
            }
        }
        let mut at_most = known.clone();
        joined(
            &mut at_most,
            unasked.iter().map(|(record, _)| record.clone()),
        );
        (
            names_ours,
            Weighed {
                known,
                at_most,
                unasked,
            },
        )
    }

    /// The member `text` names ([`MemberText`]): a member token, or a bare
    /// branch name the list holds a member of, which the registry does not
    /// read as a pull request and no other record answers to.
    fn member_named<'t>(&self, required: &[Requirement], text: &'t str) -> Option<MemberText<'t>> {
        if let Some(Requirement::ForkBranch { repository, branch }) = Requirement::parse(text) {
            return Some(MemberText {
                text,
                repository: Some(repository),
                branch,
            });
        }
        if self.names_a_pull_request(text)
            || required.iter().any(|held| {
                !matches!(held, Requirement::ForkBranch { .. }) && self.answers_to(held, text, &[])
            })
        {
            return None;
        }
        required.iter().find_map(|held| match held {
            Requirement::ForkBranch { branch, .. } if branch.as_str() == text => Some(MemberText {
                text,
                repository: None,
                branch: branch.clone(),
            }),
            _ => None,
        })
    }

    /// The records of `required` that `text` answers to, `tied` holding the
    /// review pull requests the forge ties to it, and whether they are one
    /// requirement.
    fn outcome(&self, required: &[Requirement], text: &str, tied: &[Requirement]) -> Outcome {
        let answering: Vec<Requirement> = required
            .iter()
            .filter(|requirement| self.answers_to(requirement, text, tied))
            .cloned()
            .collect();
        let own = self.own();
        let one_requirement = |first: &Requirement, other: &Requirement| {
            same(first, other, &own) || (tied.contains(first) && tied.contains(other))
        };
        match answering.as_slice() {
            [] => Outcome::Nothing,
            [first, rest @ ..] if rest.iter().all(|other| one_requirement(first, other)) => {
                Outcome::One(answering)
            }
            _ => Outcome::Ambiguous(answering),
        }
    }

    /// Whether `text` names `requirement`, an upstream or review pull request
    /// record: spelled as its record spells it, or as `--on` names it. A
    /// registry name with a number answers for that fork's upstream pull
    /// request and for its own repository's; `tied` holds the review pull
    /// requests `text` names only through the forge
    /// ([`Self::tied_by_forge`]). A member text is weighed apart
    /// ([`Self::member_weighed`]): no member record answers here.
    fn answers_to(&self, requirement: &Requirement, text: &str, tied: &[Requirement]) -> bool {
        if Requirement::parse(text).as_ref() == Some(requirement) {
            return true;
        }
        match requirement {
            Requirement::Upstream { repo, number } => numbered(text).is_some_and(|named| {
                named.1 == *number
                    && (named.0.eq_ignore_ascii_case(repo.as_str())
                        || self.registry.resolve(named.0).as_ref() == Some(repo))
            }),
            Requirement::ForkPull { repository, number } => {
                tied.contains(requirement)
                    || numbered(text.strip_prefix("origin:").unwrap_or(text)).is_some_and(|named| {
                        named.1 == *number
                            && (named.0.eq_ignore_ascii_case(repository)
                                || self.registry.repos.get_key_value(named.0).is_some_and(
                                    |(key, entry)| {
                                        self.repositories_of(key, entry)
                                            .iter()
                                            .any(|own| own.eq_ignore_ascii_case(repository))
                                    },
                                ))
                    })
            }
            Requirement::ForkBranch { .. } => false,
        }
    }

    /// What the forge ties `text` to among `required` ([`Tie`]). For a review
    /// pull request: every record of `text`'s number whose repository the
    /// forge names as the one `text` names, those `text` spells directly
    /// included, when two records are spelled apart or any is spelled
    /// otherwise; a repository renamed between two records, in either
    /// direction, ties them. A record on another fork's own repository is
    /// never one on this fork's, and the reverse: no forge is asked about it.
    /// Nothing, with no forge call, for a text that names an upstream by its
    /// `<owner>/<name>` without an `origin:` prefix: no review pull request
    /// is on it. A record whose repository the forge does not name, by an
    /// answer or a failure, is listed as unasked ([`Self::named`]); one it
    /// names as another repository, as apart. For a member text:
    /// [`Self::member_tie`].
    fn tied_by_forge(&self, required: &[Requirement], text: &str) -> Tie {
        if let Some(named) = self.member_named(required, text) {
            return self.member_tie(required, &named);
        }
        let explicit = text.starts_with("origin:");
        let Some((repo, number)) = numbered(text.strip_prefix("origin:").unwrap_or(text)) else {
            return Tie::default();
        };
        // A text spelled exactly as a held upstream record names that record
        // alone, as an exact member token does: no forge is asked.
        if !explicit
            && required.iter().any(|held| {
                matches!(held, Requirement::Upstream { .. }) && held.to_string() == text
            })
        {
            return Tie::default();
        }
        if !explicit
            && !self.registry.repos.contains_key(repo)
            && self.registry.resolve(repo).is_some()
        {
            return Tie::default();
        }
        let foreign = self.foreign();
        let theirs = |repository: &str| foreign.contains(&repository.to_ascii_lowercase());
        let text_theirs = match self.registry.repos.get_key_value(repo) {
            Some((key, entry)) => {
                entry.upstream_name(key) != self.fork.upstream
                    && remote_slug(&entry.origin).is_some_and(theirs)
            }
            None => theirs(repo),
        };
        let candidates: Vec<&Requirement> = required
            .iter()
            .filter(|requirement| {
                matches!(requirement, Requirement::ForkPull { number: wanted, repository }
                    if *wanted == number && theirs(repository) == text_theirs)
            })
            .collect();
        let spellings: std::collections::BTreeSet<String> = candidates
            .iter()
            .filter_map(|requirement| match requirement {
                Requirement::ForkPull { repository, .. } => Some(repository.to_ascii_lowercase()),
                _ => None,
            })
            .collect();
        if spellings.len() <= 1
            && candidates
                .iter()
                .all(|requirement| self.answers_to(requirement, text, &[]))
        {
            return Tie::default();
        }
        let origin = if let Some((key, entry)) = self.registry.repos.get_key_value(repo) {
            if entry.upstream_name(key) == self.fork.upstream {
                self.fork.own_origin().to_owned()
            } else {
                entry.origin.clone()
            }
        } else if let Some(origin) = self.own_origin_named(repo) {
            origin.to_owned()
        } else {
            // A name no remote here gives: the forge says which repository it
            // is, asked on the host of this fork's own `origin`.
            let own = self.fork.own_origin();
            let Some(slug) = remote_slug(own) else {
                return Tie::default();
            };
            own.replacen(slug, repo, 1)
        };
        let Some(slug) = remote_slug(&origin) else {
            return Tie::default();
        };
        let mut tie = Tie::default();
        let named = match self.named(&origin) {
            Ok(named) => named,
            Err(error) => {
                tie.unasked = candidates
                    .into_iter()
                    .map(|requirement| (requirement.clone(), error.clone()))
                    .collect();
                return tie;
            }
        };
        for requirement in candidates {
            let Requirement::ForkPull { repository, .. } = requirement else {
                continue;
            };
            let is_named = if repository.eq_ignore_ascii_case(&named)
                || repository.eq_ignore_ascii_case(slug)
            {
                Ok(true)
            } else {
                self.named(&origin.replacen(slug, repository, 1))
                    .map(|name| name.eq_ignore_ascii_case(&named))
            };
            match is_named {
                Ok(true) => tie.same.push(requirement.clone()),
                Ok(false) => {}
                Err(error) => tie.unasked.push((requirement.clone(), error)),
            }
        }
        tie
    }

    /// What the forge says of a member text's records ([`Tie`]), asked only
    /// when the list holds the member under another spelling than `text`:
    /// for a token on a repository that is neither a spelling here nor
    /// another fork's, whether that repository is this fork's own
    /// (`member_own`); and for each record of the member on such a
    /// repository, other than the token's, whether it is (`placed`).
    fn member_tie(&self, required: &[Requirement], named: &MemberText<'_>) -> Tie {
        let mut tie = Tie::default();
        let repository = named.repository.as_deref();
        let elsewhere = |on: &str| !self.is_own_repository(on) && self.other_fork(on).is_none();
        let named_by_text = |on: &str| repository.is_some_and(|text| text.eq_ignore_ascii_case(on));
        let others: Vec<&Requirement> = required
            .iter()
            .filter(|held| {
                matches!(held, Requirement::ForkBranch { branch: other, .. } if *other == named.branch)
                    && held.to_string() != named.text
            })
            .collect();
        let text_elsewhere = repository.filter(|on| elsewhere(on));
        if others.is_empty() || repository.is_some_and(|on| self.other_fork(on).is_some()) {
            return tie;
        }
        let to_place: Vec<&Requirement> = others
            .into_iter()
            .filter(|held| {
                matches!(held, Requirement::ForkBranch { repository: on, .. }
                    if elsewhere(on) && !named_by_text(on))
            })
            .collect();
        if text_elsewhere.is_none() && to_place.is_empty() {
            return tie;
        }
        let own = self.fork.own_origin();
        let own_name = self.named(own);
        let place = |on: &str| -> Result<bool, String> {
            let own_name = own_name.clone()?;
            let slug = remote_slug(own).ok_or_else(|| {
                format!(
                    "{}'s {ORIGIN} {own} names no forge repository",
                    self.fork.name
                )
            })?;
            self.named(&own.replacen(slug, on, 1))
                .map(|name| name == own_name)
        };
        if let Some(on) = text_elsewhere {
            tie.member_own = Some(place(on));
        }
        for held in to_place {
            if let Requirement::ForkBranch { repository: on, .. } = held {
                tie.placed.push((held.clone(), place(on)));
            }
        }
        tie
    }

    /// The record of `required` the forge names as `wanted`, a review pull
    /// request or a member this command is about to add under its current
    /// name: a record of the same number or branch under another name the
    /// forge resolves to this fork's repository, as one written before a
    /// rename, or by a machine whose `origin` spells the repository so.
    /// `None` when there is none, or the forge cannot say: then `wanted` is
    /// written as it is.
    fn held_alias(&self, required: &[Requirement], wanted: &Requirement) -> Option<Requirement> {
        let origin = self.fork.own_origin();
        let slug = remote_slug(origin)?;
        let names = |other: &str, repository: &str| {
            matches!(self.named(&origin.replacen(slug, other, 1)),
                Ok(name) if name.eq_ignore_ascii_case(repository))
        };
        match wanted {
            Requirement::ForkPull { repository, number } => required
                .iter()
                .find(|held| {
                    matches!(held, Requirement::ForkPull { repository: other, number: held_number }
                        if held_number == number
                            && !other.eq_ignore_ascii_case(repository)
                            && names(other, repository))
                })
                .cloned(),
            Requirement::ForkBranch { repository, branch } => {
                let elsewhere: Vec<&Requirement> = required
                    .iter()
                    .filter(|held| {
                        matches!(held, Requirement::ForkBranch { repository: other, branch: held_branch }
                            if held_branch == branch
                                && !other.eq_ignore_ascii_case(repository)
                                && !self.is_own_repository(other)
                                && self.other_fork(other).is_none())
                    })
                    .collect();
                if elsewhere.is_empty() {
                    return None;
                }
                let own = self.named(origin).ok()?;
                elsewhere
                    .into_iter()
                    .find(|held| held.repository().is_some_and(|other| names(other, &own)))
                    .cloned()
            }
            Requirement::Upstream { .. } => None,
        }
    }

    /// The name the forge gives the repository the remote `url` names, or
    /// why it could not be asked. The forge's `Could not resolve to a
    /// Repository` is such a reason, not an answer that no repository has the
    /// name: GitHub says it for one that does not exist, for one the token
    /// cannot see, and in some outages, and nothing tells those apart.
    fn named(&self, url: &str) -> Result<String, String> {
        self.forge
            .repo_identity(&self.fork.checkout.path, url, url)
            .map(|identity| identity.name_with_owner.to_ascii_lowercase())
            .map_err(|error| format!("{url}: {error}"))
    }

    /// This fork's own `origin` URL when the forge names that repository
    /// `repository`, a name no remote here spells: the repository was
    /// renamed. Asked only of a `repository` that reads as `<owner>/<name>`;
    /// `text` is what named it, for the refusal when the forge cannot say.
    fn renamed_origin(&self, repository: &str, text: &str) -> Result<Option<String>, Exit> {
        let shaped = repository.split_once('/').is_some_and(|(owner, name)| {
            !owner.is_empty() && !name.is_empty() && !name.contains('/')
        });
        let origin = self.fork.own_origin();
        if !shaped || remote_slug(origin).is_none() {
            return Ok(None);
        }
        match self
            .forge
            .repo_identity(&self.fork.checkout.path, origin, origin)
        {
            Ok(own) => Ok(own
                .name_with_owner
                .eq_ignore_ascii_case(repository)
                .then(|| origin.to_owned())),
            Err(error) => refused(
                Exit::Incomplete,
                format_args!(
                    "nothing recorded: {repository} in {text} is no repository the registry or \
                     this checkout names, and whether {}'s own repository ({origin}) was renamed \
                     to it could not be asked of the forge: {error}",
                    self.fork.name
                ),
            ),
        }
    }

    /// The forge's identity of the repository the remote `url` names, which
    /// names it as it is called now.
    fn identity(&self, url: &str) -> Result<RepoIdentity, Exit> {
        self.forge
            .repo_identity(&self.fork.checkout.path, url, url)
            .or_else(|error| {
                refused(
                    Exit::Incomplete,
                    format_args!("nothing recorded: could not ask the forge about {url}: {error}"),
                )
            })
    }

    /// Pull request `number` of `repository`, or `None` when it has none.
    fn pull(&self, repository: &RepoIdentity, number: u64) -> Result<Option<PullFacts>, Exit> {
        match self
            .forge
            .pull_facts(&self.fork.checkout.path, repository, &[number])
        {
            Ok(mut facts) => Ok(facts.remove(&number)),
            Err(error) => refused(
                Exit::Incomplete,
                format_args!(
                    "nothing recorded: could not ask the forge about {}#{number}: {error}",
                    repository.name_with_owner
                ),
            ),
        }
    }

    /// This fork's `origin` URL that spells its own repository as
    /// `repository`: its checkout's `origin` remote, or its registry
    /// `origin`.
    fn own_origin_named(&self, repository: &str) -> Option<&str> {
        let names =
            |url: &&str| remote_slug(url).is_some_and(|slug| slug.eq_ignore_ascii_case(repository));
        self.fork
            .checkout
            .remotes
            .get(ORIGIN)
            .map(String::as_str)
            .filter(names)
            .or_else(|| Some(self.fork.entry.origin.as_str()).filter(names))
    }

    /// The `origin` URL of this fork's own repository named `repository` by
    /// `text`: a spelling here, or the name the forge gives it now. Refused
    /// for another fork's own repository and for a repository no fork has.
    fn own_origin(&self, repository: &str, text: &str) -> Result<String, Exit> {
        if let Some(origin) = self.own_origin_named(repository) {
            return Ok(origin.to_owned());
        }
        if let Some(key) = self.other_fork(repository) {
            self.another_forks_review(text, key)?;
        }
        let Some(origin) = self.renamed_origin(repository, text)? else {
            return refused(
                Exit::Usage,
                format_args!(
                    "{repository} in {text} is not {}'s own repository, nor any fork's in the \
                     registry",
                    self.fork.name
                ),
            );
        };
        Ok(origin)
    }

    /// Every spelling of this fork's own repository here, lowercase: its
    /// checkout's `origin` and its registry `origin` ([`same`]).
    fn own(&self) -> Vec<String> {
        self.repositories_of(self.fork.name.as_str(), self.fork.entry)
            .into_iter()
            .map(|repository| repository.to_ascii_lowercase())
            .collect()
    }

    /// What to say of each member in `added` that this command writes under
    /// this checkout's spelling while `staying` holds a record of the same
    /// branch on a repository that is neither a spelling here nor another
    /// fork's: the forge did not place that record as this fork's (it was
    /// down, or names another repository), so the two are written apart. If
    /// that repository is a former name of this fork's, the list now holds
    /// one requirement under two spellings.
    fn spelled_beside(&self, staying: &[Requirement], added: &[Requirement]) -> Vec<String> {
        let mut lines = Vec::new();
        for wanted in added {
            let Requirement::ForkBranch { branch, .. } = wanted else {
                continue;
            };
            if staying.contains(wanted) {
                continue;
            }
            for held in staying {
                if let Requirement::ForkBranch {
                    repository,
                    branch: other,
                } = held
                    && other == branch
                    && !self.is_own_repository(repository)
                    && self.other_fork(repository).is_none()
                {
                    lines.push(format!(
                        "{wanted} is recorded beside {held}: the forge did not say {repository} \
                         is {}'s own repository; if it is a former name, they are one \
                         requirement under two spellings",
                        self.fork.name
                    ));
                }
            }
        }
        lines
    }

    /// The own repositories of the forks other than this one, lowercase, as
    /// the registry spells them: a member token on one names that fork's
    /// branch, never a member of this one.
    fn foreign(&self) -> Vec<String> {
        self.registry
            .repos
            .iter()
            .filter(|(key, entry)| entry.upstream_name(key) != self.fork.upstream)
            .filter_map(|(_, entry)| remote_slug(&entry.origin).map(str::to_ascii_lowercase))
            .collect()
    }

    /// Whether `text`, a `<repo>#<n>`, names a pull request as `--on` reads
    /// it: `<repo>` a registry name or an upstream's `<owner>/<name>`.
    fn names_a_pull_request(&self, text: &str) -> bool {
        numbered(text).is_some_and(|(repo, _)| {
            self.registry.repos.contains_key(repo) || self.registry.resolve(repo).is_some()
        })
    }

    /// The registry name of a fork other than this one whose own repository
    /// the registry spells `repository`.
    fn other_fork(&self, repository: &str) -> Option<&str> {
        self.registry
            .repos
            .iter()
            .find(|(key, entry)| {
                entry.upstream_name(key) != self.fork.upstream
                    && remote_slug(&entry.origin)
                        .is_some_and(|slug| slug.eq_ignore_ascii_case(repository))
            })
            .map(|(key, _)| key.as_str())
    }

    /// This fork's own repository: what its checkout's `origin` remote names,
    /// else the registry's `origin`.
    fn own_repository(&self) -> Option<String> {
        self.fork
            .checkout
            .remotes
            .get(ORIGIN)
            .and_then(|url| remote_slug(url))
            .or_else(|| remote_slug(&self.fork.entry.origin))
            .map(str::to_ascii_lowercase)
    }

    fn is_own_repository(&self, repository: &str) -> bool {
        self.repositories_of(self.fork.name.as_str(), self.fork.entry)
            .iter()
            .any(|own| own.eq_ignore_ascii_case(repository))
    }

    /// Every spelling of the own repository of the fork `key` names: its
    /// registry `origin`, and, for this fork, its checkout's `origin` remote.
    fn repositories_of(&self, key: &str, entry: &RepoEntry) -> Vec<String> {
        let mut names: Vec<String> = remote_slug(&entry.origin)
            .map(str::to_ascii_lowercase)
            .into_iter()
            .collect();
        if entry.upstream_name(key) == self.fork.upstream {
            names.extend(self.own_repository());
        }
        names
    }

    fn has_branch(&self, name: &str) -> bool {
        Repo::open(&self.fork.checkout.path).is_ok_and(|repo| repo.has_bookmark(name, ORIGIN))
    }
}

#[cfg(test)]
mod tests {
    use super::{restated, same, with_requirements};
    use knives::ids::Requirement;

    fn parse(text: &str) -> Requirement {
        Requirement::parse(text).expect("a requirement")
    }

    #[test]
    fn requirements_join_what_was_already_required_without_duplicates() {
        assert_eq!(
            with_requirements(
                &[parse("sibling#49"), parse("other#7")],
                &[parse("sibling#49"), parse("another#3")],
                &[],
            ),
            ["another#3", "other#7", "sibling#49"]
        );
    }

    #[test]
    fn one_member_under_two_spellings_of_the_own_repository_is_one_requirement() {
        let own = ["acme-old/demo".to_owned(), "acme/demo".to_owned()];
        let old = parse("origin:acme-old/demo:feat/a");
        let new = parse("origin:acme/demo:feat/a");
        assert!(same(&old, &new, &own));
        assert!(!same(&new, &parse("origin:acme/demo:feat/b"), &own));
        assert!(!same(
            &parse("origin:acme/demo#1"),
            &parse("origin:acme-old/demo#1"),
            &own
        ));
        // A record on a repository no spelling here gives, another fork's or
        // any other, is the same only as a record spelled so.
        let sibling = parse("origin:acme/sibling:feat/a");
        assert!(!same(&new, &sibling, &own));
        assert!(same(&sibling, &sibling, &own));
        assert!(!same(&old, &new, &["acme/demo".to_owned()]));
        assert_eq!(
            with_requirements(&[old], &[new], &own),
            ["origin:acme-old/demo:feat/a"]
        );
    }

    #[test]
    fn a_restated_list_says_what_was_added_and_what_stayed() {
        let added = [parse("origin:acme/demo#12")];
        let kept = [parse("origin:acme/demo:feat/base")];
        assert_eq!(
            restated(&added, &kept),
            "requires origin:acme/demo#12; still requires origin:acme/demo:feat/base"
        );
        assert_eq!(restated(&added, &[]), "requires origin:acme/demo#12");
        assert_eq!(
            restated(&[], &kept),
            "still requires origin:acme/demo:feat/base"
        );
    }
}
