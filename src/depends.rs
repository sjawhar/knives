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
/// states, under the fork's lock; a spelling of a renamed own repository
/// among them is resolved through the forge before the lock is taken.
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
    // A `--forget` naming a renamed own repository needs the forge's name of
    // it ([`Context::renamed_spelling`]): asked here, from the list the
    // pulled ledger states, so no forge round trip holds the fork's lock. A
    // list another writer changes before the lock is taken can only make a
    // forget refuse, never forget the wrong requirement.
    let target = BranchTarget::new(fork.upstream.clone(), branch.clone());
    let mut renamed = Vec::with_capacity(change.forget.len());
    if !change.forget.is_empty() {
        let read = Store::open(
            default_state_path(),
            &bound.registry.former_names(),
            &[&fork.upstream],
        )?;
        let required = read.dependencies(&target);
        drop(read);
        for text in change.forget {
            match context.renamed_spelling(&required, text) {
                Ok(name) => renamed.push(name),
                Err(exit) => return Ok(exit),
            }
        }
    }
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
    let mut forgotten: Vec<Requirement> = Vec::new();
    for (text, renamed) in change.forget.iter().zip(&renamed) {
        match context.forgotten(&required, text, renamed.as_deref()) {
            Ok(answering) => {
                for requirement in answering {
                    if !forgotten.contains(&requirement) {
                        forgotten.push(requirement);
                    }
                }
            }
            Err(exit) => return Ok(exit),
        }
    }
    let pr = store.tracked_pull(&target);
    let replaces = store.statement_stamp(&target, StatementKind::Depends);
    let subject = Some(branch.as_str());
    let scribe = scribe_for(fork, bound)?;
    if forgotten.is_empty() {
        let list = with_requirements(&required, &added);
        let _ = scribe.record(&Draft {
            statement: Some(depends(Some(list.join(",")), replaces)),
            ..Draft::event(subject, format!("requires {}", listed(&added)), pr)
        })?;
        drop(store);
        println!("{}/{branch} now requires {}", fork.name, listed(&added));
        return Ok(Exit::Ok);
    }
    let kept: Vec<Requirement> = required
        .into_iter()
        .filter(|requirement| !forgotten.contains(requirement))
        .collect();
    let list = with_requirements(&kept, &added);
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
    Ok(Exit::Ok)
}

/// Refuse `knives track --pr <number>` when `number` is not the upstream pull
/// request `branch` becomes. A fork and its upstream number their pull
/// requests apart, so the fork's review pull request #n read as the
/// upstream's #n names another pull request, or none; the upstream numbers
/// in the thousands, so most small fork numbers are also upstream numbers.
///
/// `Ok` when the upstream's #n has `branch` as its head, and when it has
/// another head and the fork's #n is not `branch`'s: a branch may carry
/// someone else's upstream pull request. Refused when the fork's #n has
/// `branch` as its head (naming both), when only the fork has #n, and when
/// neither does. `Ok`, with no forge call, when the fork has one forge
/// repository at most, whose numbers are the only ones there are.
pub(crate) fn upstream_pull(
    fork: &Fork<'_>,
    registry: &Registry,
    branch: &BranchName,
    number: u64,
) -> Result<(), Exit> {
    let entry = fork.entry;
    let (Some(upstream), Some(spelled)) =
        (remote_slug(&entry.upstream), remote_slug(&entry.origin))
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
    let theirs = RepoIdentity {
        name_with_owner: upstream.to_owned(),
        id: String::new(),
    };
    let on_upstream = context.pull(&theirs, number)?;
    if on_upstream
        .as_ref()
        .is_some_and(|theirs| theirs.pull.head_ref_name == branch.as_str())
    {
        return Ok(());
    }
    let own = context.identity(&entry.origin)?;
    let upstream_name = &fork.upstream;
    let own_name = own.name_with_owner.to_ascii_lowercase();
    let suggested = context.accepted_spelling(&own_name, spelled);
    let waits = format!(
        "`track` states the upstream pull request {branch} becomes; a branch that waits for a \
         review pull request says so with `knives depends <branch> --on {suggested}#{number}`"
    );
    match (on_upstream, context.pull(&own, number)?) {
        (Some(theirs), Some(ours)) if ours.pull.head_ref_name == branch.as_str() => refused(
            Exit::Usage,
            format_args!(
                "#{number} names two pull requests: {upstream_name}#{number} ({}, head {}) is not \
                 {branch}'s, and {own_name}#{number} ({}, head {branch}) is {branch}'s review pull \
                 request on {}'s own repository. {waits}",
                theirs.pull.state.to_lowercase(),
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

/// Whether `a` and `b` require the same thing. Two records of one member
/// branch are one requirement whatever spelling of the fork's repository
/// each carries: machines whose `origin` remotes spell it differently record
/// the same member under two spellings.
fn same(a: &Requirement, b: &Requirement) -> bool {
    match (a, b) {
        (
            Requirement::ForkBranch { branch: one, .. },
            Requirement::ForkBranch { branch: other, .. },
        ) => one == other,
        _ => a == b,
    }
}

/// Everything a branch requires once `added` joins what it already `required`.
///
/// Additive and deduplicated ([`same`]), so declaring the same requirement
/// twice is not an error and re-running a script does not accumulate
/// duplicates. The result is the whole list, so each entry reads on its own
/// as everything the branch needed when it was written.
fn with_requirements(required: &[Requirement], added: &[Requirement]) -> Vec<String> {
    let mut kept: Vec<&Requirement> = required.iter().collect();
    for requirement in added {
        if !kept.iter().any(|held| same(held, requirement)) {
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
    /// the forge finds; by an upstream's `<owner>/<name>`, that upstream's;
    /// by a fork's own repository, that repository's.
    fn pull_request(&self, text: &str, repo: &str, number: u64) -> Result<Requirement, Exit> {
        if let Some(fork) = self.registry.repos.get_key_value(repo) {
            return self.asked_which(text, fork, number);
        }
        if let Some(upstream) = self.registry.resolve(repo) {
            return Ok(Requirement::Upstream {
                repo: upstream,
                number,
            });
        }
        if let Some(origin) = self.own_origin_named(repo) {
            return self.fork_pull(origin, number);
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
        let upstream = remote_slug(&entry.upstream);
        let (Some(upstream), Some(spelled)) = (upstream, remote_slug(&entry.origin)) else {
            // At most one forge repository holds this fork's pull requests:
            // the number is that repository's, as it always was.
            return Ok(Requirement::Upstream {
                repo: upstream_name,
                number,
            });
        };
        let own = self.identity(&entry.origin)?;
        if own.name_with_owner.eq_ignore_ascii_case(upstream) {
            return Ok(Requirement::Upstream {
                repo: upstream_name,
                number,
            });
        }
        let this_fork = upstream_name == self.fork.upstream;
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

    /// What `--forget <text>` takes back from `required`: the one requirement
    /// it names, or every record of the one member branch it names, whatever
    /// repository spelling each carries ([`same`]). `renamed` is the forge's
    /// name of the own repository `text` names, when that repository was
    /// renamed ([`Self::renamed_spelling`], asked before the fork's lock).
    fn forgotten(
        &self,
        required: &[Requirement],
        text: &str,
        renamed: Option<&str>,
    ) -> Result<Vec<Requirement>, Exit> {
        let answering: Vec<&Requirement> = required
            .iter()
            .filter(|requirement| self.answers_to(requirement, text, renamed))
            .collect();
        match answering.as_slice() {
            [] => refused(
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
            [first, rest @ ..] if rest.iter().all(|other| same(first, other)) => {
                Ok(answering.into_iter().cloned().collect())
            }
            many => refused(
                Exit::Usage,
                format_args!(
                    "{text} answers to {}; name the one to forget as its record spells it",
                    listed(many.iter().copied())
                ),
            ),
        }
    }

    /// Whether `text` names `requirement`: spelled as its record spells it,
    /// or as `--on` names it, without asking the forge. A registry name with a
    /// number answers for that fork's upstream pull request and for its own
    /// repository's; `renamed` is the forge's name of the own repository
    /// `text` names, when that repository was renamed
    /// ([`Self::renamed_spelling`]).
    fn answers_to(&self, requirement: &Requirement, text: &str, renamed: Option<&str>) -> bool {
        if Requirement::parse(text).as_ref() == Some(requirement) {
            return true;
        }
        match requirement {
            Requirement::Upstream { repo, number } => numbered(text).is_some_and(|named| {
                named.1 == *number
                    && (named.0.eq_ignore_ascii_case(repo.as_str())
                        || self.registry.resolve(named.0).as_ref() == Some(repo))
            }),
            Requirement::ForkPull { repository, number } => numbered(text).is_some_and(|named| {
                named.1 == *number
                    && (named.0.eq_ignore_ascii_case(repository)
                        || renamed.is_some_and(|own| own.eq_ignore_ascii_case(repository))
                        || self.registry.repos.get_key_value(named.0).is_some_and(
                            |(key, entry)| {
                                self.repositories_of(key, entry)
                                    .iter()
                                    .any(|own| own.eq_ignore_ascii_case(repository))
                            },
                        ))
            }),
            Requirement::ForkBranch { branch, .. } => branch.as_str() == text,
        }
    }

    /// The name the forge gives the own repository `text` names, by a
    /// registry name or an `origin` spelling, when `required` holds a review
    /// pull request of that number on a repository no spelling here names:
    /// the repository was renamed, and only the forge ties the two names
    /// together. `None`, with no forge call, otherwise.
    fn renamed_spelling(
        &self,
        required: &[Requirement],
        text: &str,
    ) -> Result<Option<String>, Exit> {
        let Some((repo, number)) = numbered(text) else {
            return Ok(None);
        };
        let (origin, known) = if let Some((key, entry)) = self.registry.repos.get_key_value(repo) {
            (entry.origin.as_str(), self.repositories_of(key, entry))
        } else if let Some(origin) = self.own_origin_named(repo) {
            (origin, vec![repo.to_ascii_lowercase()])
        } else {
            return Ok(None);
        };
        let unnamed = required.iter().any(|requirement| {
            matches!(requirement, Requirement::ForkPull { repository, number: wanted }
                if *wanted == number
                    && !known.iter().any(|name| name.eq_ignore_ascii_case(repository)))
        });
        if !unnamed {
            return Ok(None);
        }
        self.identity(origin)
            .map(|own| Some(own.name_with_owner.to_ascii_lowercase()))
    }

    /// This fork's own `origin` URL when the forge names that repository
    /// `repository`, a name no remote here spells: the repository was
    /// renamed. Asked only of a `repository` that reads as `<owner>/<name>`;
    /// `text` is what named it, for the refusal when the forge cannot say.
    fn renamed_origin(&self, repository: &str, text: &str) -> Result<Option<String>, Exit> {
        let shaped = repository.split_once('/').is_some_and(|(owner, name)| {
            !owner.is_empty() && !name.is_empty() && !name.contains('/')
        });
        let origin = self
            .fork
            .checkout
            .remotes
            .get(ORIGIN)
            .map_or(self.fork.entry.origin.as_str(), String::as_str);
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
            ),
            ["another#3", "other#7", "sibling#49"]
        );
    }

    #[test]
    fn one_member_under_two_repository_spellings_is_one_requirement() {
        let old = parse("origin:acme-old/demo:feat/a");
        let new = parse("origin:acme/demo:feat/a");
        assert!(same(&old, &new));
        assert!(!same(&new, &parse("origin:acme/demo:feat/b")));
        assert!(!same(
            &parse("origin:acme/demo#1"),
            &parse("origin:acme-old/demo#1")
        ));
        assert_eq!(
            with_requirements(&[old], &[new]),
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
