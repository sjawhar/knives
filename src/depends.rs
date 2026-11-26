//! `knives depends`: what a branch cannot land before, stated, corrected and
//! taken back.
//!
//! A requirement is a pull request on a fork's upstream or a member branch of
//! this fork ([`Requirement`]). What a caller types is resolved to one of
//! them before anything is written. A pull request on a fork's own repository
//! is refused: pull requests go upstream. A record of one that an older
//! knives wrote is still read, and taken back by its spelling.
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
use knives::forge::{Forge, RepoIdentity};
use knives::ids::{BranchName, BranchTarget, Requirement};
use knives::jj::Repo;
use knives::ledger::{Draft, Stated};
use knives::remote_url::{remote_slug, with_slug};
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
    let beside = context.spelled_beside(&staying, &added, &forged.unplaced);
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

/// Refuse `knives track --pr <number>` when the fork's upstream has no pull
/// request `number`: `track` states the upstream pull request a branch
/// becomes. `Ok`, with no forge call, when the upstream or the fork's own
/// repository names no forge repository, or both name one. The fork's own
/// repository is the one its checkout's `origin` remote names when that is
/// a forge repository, else its registry `origin` ([`Fork::own_origin`]).
pub(crate) fn upstream_pull(fork: &Fork<'_>, number: u64) -> Result<(), Exit> {
    let (Some(upstream), Some(own)) = (
        remote_slug(&fork.entry.upstream),
        remote_slug(fork.own_origin()),
    ) else {
        return Ok(());
    };
    if upstream.eq_ignore_ascii_case(own) {
        return Ok(());
    }
    let theirs = RepoIdentity {
        name_with_owner: upstream.to_owned(),
        id: String::new(),
    };
    match CliForge.pull_facts(&fork.checkout.path, &theirs, &[number]) {
        Ok(facts) if facts.contains_key(&number) => Ok(()),
        Ok(_) => refused(
            Exit::Usage,
            format_args!("{} has no pull request #{number}", fork.upstream),
        ),
        Err(error) => refused(
            Exit::Incomplete,
            format_args!(
                "nothing recorded: could not ask the forge about {upstream}#{number}: {error}"
            ),
        ),
    }
}

/// What the forge says of a `--forget` member text's records: for a token on
/// a repository no spelling here gives, whether the forge names that
/// repository as this fork's own (`member_own`); and for each record of the
/// member on a repository that is neither a spelling here nor another fork's,
/// whether the forge names it as this fork's own (`placed`).
#[derive(Debug, Default)]
struct Tie {
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

/// What GitHub says of a repository it cannot resolve: one that is gone, or
/// that the token cannot see. It picks the words of a refusal only; it never
/// stands for an answer that the repository does not exist ([`Context::named`]).
const UNRESOLVED: &str = "Could not resolve to a Repository";

/// The records of `required` that `text` spells exactly, the repository's
/// letter case aside ([`Requirement::spelled_as`]).
fn exactly(required: &[Requirement], text: &str) -> Vec<Requirement> {
    required
        .iter()
        .filter(|held| held.spelled_as(text))
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

/// What a refusal for an unanswered forge advises, from `why`, the forge's
/// reasons: run it again when the forge answers; or, when the forge said it
/// could not resolve a repository ([`UNRESOLVED`]), that the repository
/// (`repository`, when the refusal knows which) is gone, the token cannot
/// see it, or the forge is failing, which nothing here tells apart
/// ([`Context::named`]).
fn rerun(why: &str, repository: Option<&str>) -> String {
    if !why.contains(UNRESOLVED) {
        return "Run it again when the forge answers".to_owned();
    }
    format!(
        "The forge could not resolve {}: it is gone, the token cannot see it, or the forge is \
         failing. Run it again later, or check the repository",
        repository.unwrap_or("a repository it was asked about")
    )
}

/// What the forge says, before the fork's lock, about the list the pulled
/// ledger states: per `--forget` text, its [`Tie`]; per requirement to add,
/// the record already held under another name of its repository
/// ([`Context::held_alias`]), and the member records held under a name
/// whose repository the forge could not be asked about (`unplaced`). Asked
/// before the lock, so no forge round trip holds it. A list another writer
/// changes before the lock is taken can only make a forget refuse or an add
/// write a second spelling, never forget the wrong requirement.
#[derive(Default)]
struct Forged {
    ties: Vec<Tie>,
    aliases: Vec<Option<Requirement>>,
    unplaced: Vec<Requirement>,
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
            aliases: vec![None; added.len()],
            ..Forged::default()
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
    let mut aliases = Vec::new();
    let mut unplaced = Vec::new();
    for wanted in added {
        let (alias, unasked) = context.held_alias(&required, wanted);
        aliases.push(alias);
        joined(&mut unplaced, unasked);
    }
    Ok(Forged {
        ties,
        aliases,
        unplaced,
    })
}

/// `added`, each as `required` already holds it when it holds it under
/// another spelling: a member under another spelling of the fork's own
/// repository here ([`same`]), or under a name the forge resolves to its own
/// (`aliases`, from [`forged`]).
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
                Some(Requirement::ForkPull { repository, .. }) => {
                    self.fork_pull_refused(text, &repository)
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
                        "cannot read {text} as a requirement: write `origin:<owner>/<name>:<branch>`"
                    ),
                ),
            };
        }
        match numbered(text) {
            Some((repo, number)) => self.pull_request(text, repo, number),
            None => self.member_branch(text),
        }
    }

    /// The upstream pull request `<repo>#<number>` names: by a registry name,
    /// that fork's upstream's; by an upstream's `<owner>/<name>`, that
    /// upstream's. No forge is asked. A pull request on a fork's own
    /// repository is refused ([`Self::fork_pull_refused`]): named by a
    /// spelling of this fork's own repository here, by another fork's own
    /// repository, or by the key of a fork whose upstream is this fork's own
    /// repository. A spelling of this fork's own repository is read so before
    /// it is read as an upstream: an `origin` that spells this fork's
    /// upstream is a configuration error. Another fork whose upstream is no
    /// forge repository, such as a path, is named by its registry key
    /// exactly: its key in another letter case is refused (exit 2), naming
    /// the key. This fork's own such upstream is its key in any letter case
    /// ([`Registry::resolve`]).
    fn pull_request(&self, text: &str, repo: &str, number: u64) -> Result<Requirement, Exit> {
        if let Some((key, entry)) = self.registry.repos.get_key_value(repo) {
            let upstream = entry.upstream_name(key);
            if upstream != self.fork.upstream
                && let Some(slug) = remote_slug(&entry.upstream)
                && self.is_own_repository(slug)
            {
                return self.fork_pull_refused(text, slug);
            }
            return Ok(Requirement::Upstream {
                repo: upstream,
                number,
            });
        }
        if self.own_origin_named(repo).is_some() {
            return self.fork_pull_refused(text, repo);
        }
        if let Some(upstream) = self.registry.resolve(repo) {
            if upstream != self.fork.upstream && self.others_upstream(repo).is_none() {
                let key = self
                    .registry
                    .repos
                    .iter()
                    .find(|(key, entry)| entry.upstream_name(key) == upstream)
                    .map_or(upstream.as_str(), |(key, _)| key.as_str());
                return refused(
                    Exit::Usage,
                    format_args!(
                        "{repo} in {text} is no registry key: {key}'s upstream is no forge \
                         repository, so it is named by its key exactly: `--on {key}#{number}`"
                    ),
                );
            }
            return Ok(Requirement::Upstream {
                repo: upstream,
                number,
            });
        }
        if self.other_fork(repo).is_some() {
            return self.fork_pull_refused(text, repo);
        }
        if self.has_branch(text) {
            return self.member_branch(text);
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

    /// Refuse `text`, a pull request on `repository`, a fork's own
    /// repository: pull requests go upstream, so none is a requirement.
    fn fork_pull_refused(&self, text: &str, repository: &str) -> Result<Requirement, Exit> {
        refused(
            Exit::Usage,
            format_args!(
                "{text} names a pull request on {repository}, a fork's own repository: pull \
                 requests go upstream, so no branch depends on one. Name the upstream pull \
                 request as `<repo>#<number>`, or the member branch by its name"
            ),
        )
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
        let why = reasons(&unasked, &also);
        refused(
            Exit::Incomplete,
            format_args!(
                "nothing recorded: whether {} also {} {} could not be asked of the forge: {why}. {}",
                texts.join(" or "),
                if texts.len() == 1 { "names" } else { "name" },
                listed(also.iter().copied()),
                self.recovery(required, &at_most, &why)
            ),
        )
    }

    /// What to do when the forge could not say what a forget takes, `why`
    /// being its reasons: what [`rerun`] advises, or run the printed command,
    /// which takes back every record of every requirement among `records`
    /// ([`Self::token_sets`]) whatever the forge answers. It can take a
    /// record the forge named as another repository's: with the forge unable
    /// to say what one text names, any record of the member on a repository
    /// that is no other fork's may be meant. To keep one, wait for the forge.
    fn recovery(&self, required: &[Requirement], records: &[Requirement], why: &str) -> String {
        let mut tokens: Vec<String> = Vec::new();
        joined(
            &mut tokens,
            self.token_sets(required, records).into_iter().flatten(),
        );
        format!(
            "{}, or take back every record it might name and every other record of the same \
             member, each as it is spelled, which no forge answer changes: {}",
            rerun(why, None),
            self.command(&tokens)
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
    /// one set per requirement: an upstream or fork pull request record
    /// alone; every record of a member in `required` that may be the same
    /// requirement, each as it is spelled, but by its branch name when every
    /// such record spells this fork's repository as a spelling here and the
    /// name holds no `#`. A record on another fork's own repository is never
    /// a record of this fork's, nor the reverse ([`same`]): it is taken
    /// alone. A text spelled as a record names that record directly, so no
    /// forge answer changes what a set takes.
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
                    .filter(|held| !away(held) && held.same_member(record))
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
    /// one requirement it names. A text spelled exactly as a held upstream
    /// record names that record alone; one spelled as a held fork pull
    /// request record, the letter case of its repository aside, names every
    /// record spelled so. A member text: [`Self::member_weighed`]. Any other
    /// text names the upstream records it answers to ([`Self::answers_to`]),
    /// and is refused (exit 2) when it names two requirements or nothing.
    fn weighed(&self, required: &[Requirement], text: &str, tie: &Tie) -> Result<Weighed, Exit> {
        if let Some(exact) = required
            .iter()
            .find(|held| matches!(held, Requirement::Upstream { .. }) && held.spelled_as(text))
        {
            return Ok(Weighed::settled(vec![exact.clone()]));
        }
        let fork_pulls: Vec<Requirement> = required
            .iter()
            .filter(|held| matches!(held, Requirement::ForkPull { .. }) && held.spelled_as(text))
            .cloned()
            .collect();
        if !fork_pulls.is_empty() {
            return Ok(Weighed::settled(fork_pulls));
        }
        if let Some(weighed) = self.member_weighed(required, text, tie) {
            return weighed;
        }
        match self.outcome(required, text) {
            Outcome::One(records) => Ok(Weighed::settled(records)),
            Outcome::Ambiguous(records) => refused(
                Exit::Usage,
                format_args!(
                    "{text} answers to {}; name the one to forget as its record spells it",
                    listed(&records)
                ),
            ),
            Outcome::Nothing => refused(
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
            let why = reasons(&unasked, &also);
            return Some(refused(
                Exit::Incomplete,
                format_args!(
                    "nothing recorded: whether {text} names {} could not be asked of the forge: \
                     {why}. {}",
                    listed(also.iter().copied()),
                    self.recovery(required, &at_most, &why)
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
                !matches!(held, Requirement::ForkBranch { .. }) && self.answers_to(held, text)
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

    /// The records of `required` that `text` answers to, and whether they are
    /// one requirement.
    fn outcome(&self, required: &[Requirement], text: &str) -> Outcome {
        let answering: Vec<Requirement> = required
            .iter()
            .filter(|requirement| self.answers_to(requirement, text))
            .cloned()
            .collect();
        let own = self.own();
        match answering.as_slice() {
            [] => Outcome::Nothing,
            [first, rest @ ..] if rest.iter().all(|other| same(first, other, &own)) => {
                Outcome::One(answering)
            }
            _ => Outcome::Ambiguous(answering),
        }
    }

    /// Whether `text` names `requirement`, an upstream or fork pull request
    /// record: spelled as its record spells it, or, for an upstream one, as
    /// `--on` names it. A member text is weighed apart
    /// ([`Self::member_weighed`]): no member record answers here.
    fn answers_to(&self, requirement: &Requirement, text: &str) -> bool {
        if Requirement::parse(text).as_ref() == Some(requirement) {
            return true;
        }
        match requirement {
            Requirement::Upstream { repo, number } => numbered(text).is_some_and(|named| {
                named.1 == *number
                    && (named.0.eq_ignore_ascii_case(repo.as_str())
                        || self.registry.resolve(named.0).as_ref() == Some(repo))
            }),
            Requirement::ForkPull { .. } => requirement.spelled_as(text),
            Requirement::ForkBranch { .. } => false,
        }
    }

    /// What the forge ties a member `text` to among `required`
    /// ([`Self::member_tie`]); nothing for any other text, which names its
    /// records by spelling alone.
    fn tied_by_forge(&self, required: &[Requirement], text: &str) -> Tie {
        self.member_named(required, text)
            .map_or_else(Tie::default, |named| self.member_tie(required, &named))
    }

    /// The `<owner>/<name>` of another registered fork's upstream that
    /// `repo` names, by that fork's registry key or by the upstream's own
    /// name in any letter case ([`Registry::resolve`]); `None` for this
    /// fork's upstream, for a repository no fork has as its upstream, and for
    /// an upstream that names no forge repository, such as a path: the forge
    /// is never asked about it.
    fn others_upstream(&self, repo: &str) -> Option<String> {
        let upstream = self.registry.resolve(repo)?;
        if upstream == self.fork.upstream {
            return None;
        }
        let entry = self.registry.kept_under(&upstream)?;
        remote_slug(&entry.upstream).map(str::to_owned)
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
                    && !held.spelled_as(named.text)
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
            let url = with_slug(own, on).ok_or_else(|| {
                format!(
                    "{}'s {ORIGIN} {own} names no forge repository",
                    self.fork.name
                )
            })?;
            self.named(&url).map(|name| name == own_name)
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

    /// The record of `required` the forge names as `wanted`, a member this
    /// command is about to add under its current name: a record of the same
    /// branch under another name the forge resolves to this fork's
    /// repository, as one written before a rename, or by a machine whose
    /// `origin` spells the repository so. `None` when there is none, or the
    /// forge cannot say: then `wanted` is written as it is. With it, the
    /// records of its branch on a repository the forge could not be asked
    /// about.
    fn held_alias(
        &self,
        required: &[Requirement],
        wanted: &Requirement,
    ) -> (Option<Requirement>, Vec<Requirement>) {
        let origin = self.fork.own_origin();
        if remote_slug(origin).is_none() {
            return (None, Vec::new());
        }
        let named_as = |other: &str| {
            with_slug(origin, other).map_or_else(
                || Err(format!("{origin} names no forge repository")),
                |url| self.named(&url),
            )
        };
        match wanted {
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
                    return (None, Vec::new());
                }
                let own = self.named(origin);
                let mut alias = None;
                let mut unplaced = Vec::new();
                for held in elsewhere {
                    let Some(other) = held.repository() else {
                        continue;
                    };
                    match own.as_ref().map_err(Clone::clone).and_then(|own| {
                        named_as(other).map(|name| name.eq_ignore_ascii_case(own))
                    }) {
                        Ok(true) if alias.is_none() => alias = Some(held.clone()),
                        Ok(_) => {}
                        Err(_) => unplaced.push(held.clone()),
                    }
                }
                (alias, unplaced)
            }
            Requirement::Upstream { .. } | Requirement::ForkPull { .. } => (None, Vec::new()),
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
    /// branch whose repository the forge could not be asked about
    /// (`unplaced`, from [`Self::held_alias`]): the two are written apart,
    /// and if that repository is another name of this fork's, the list now
    /// holds one requirement under two spellings. Nothing is said of a
    /// record the forge named as another repository's.
    fn spelled_beside(
        &self,
        staying: &[Requirement],
        added: &[Requirement],
        unplaced: &[Requirement],
    ) -> Vec<String> {
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
                    && unplaced.contains(held)
                {
                    lines.push(format!(
                        "{wanted} is recorded beside {held}: whether {repository} is another \
                         name of {}'s own repository could not be asked of the forge; if it is, \
                         they are one requirement under two spellings",
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
        let added = [parse("maintainer/demo#12")];
        let kept = [parse("origin:acme/demo:feat/base")];
        assert_eq!(
            restated(&added, &kept),
            "requires maintainer/demo#12; still requires origin:acme/demo:feat/base"
        );
        assert_eq!(restated(&added, &[]), "requires maintainer/demo#12");
        assert_eq!(
            restated(&[], &kept),
            "still requires origin:acme/demo:feat/base"
        );
    }
}
