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
//! A forget restates the list this machine holds. A requirement another
//! machine added before the forget, that this machine had not pulled when it
//! wrote the forget, is not in that list, and is lost on every machine; the
//! machines still agree on one list (`docs/design.md`, "Dependencies").

use std::fmt::Display;

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::Bound;
use knives::config::{Registry, RepoEntry};
use knives::forge::github::CliForge;
use knives::forge::{Forge, PullFacts, RepoIdentity};
use knives::ids::{
    BranchName, BranchTarget, Requirement, is_release_name, pull_number_from_bookmark,
};
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
/// states, under the fork's lock.
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
    // The fork's lock is held while the list is read and the next statement
    // appended, so two concurrent `depends` on this machine each build on the
    // other's list rather than racing; the pull above brought in what other
    // machines stated. Nothing here writes the state file.
    let store = Store::open_for_update(
        default_state_path(),
        &bound.registry.former_names(),
        &[&fork.upstream],
    )?;
    let target = BranchTarget::new(fork.upstream.clone(), branch.clone());
    let required = store.dependencies(&target);
    let mut forgotten: Vec<Requirement> = Vec::new();
    for text in change.forget {
        match context.forgotten(&required, text) {
            Ok(requirement) if forgotten.contains(&requirement) => {}
            Ok(requirement) => forgotten.push(requirement),
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

/// Refuse `knives track --pr <number>` when `number` is no pull request on
/// the fork's upstream, the one `track` states. A fork and its upstream number
/// their pull requests apart, and the fork's review pull request #n read as
/// the upstream's #n names another pull request, or none. `Ok` when the
/// upstream has it, and when the fork has one forge repository at most, whose
/// numbers are the only ones there are. The refusal names the fork's pull
/// request when the fork has one.
pub(crate) fn upstream_pull(
    fork: &Fork<'_>,
    registry: &Registry,
    branch: &BranchName,
    number: u64,
) -> Result<(), Exit> {
    let entry = fork.entry;
    let (Some(upstream), Some(origin)) = (remote_slug(&entry.upstream), remote_slug(&entry.origin))
    else {
        return Ok(());
    };
    if upstream.eq_ignore_ascii_case(origin) {
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
    if context.pull(&theirs, number)?.is_some() {
        return Ok(());
    }
    let own = context.identity(&entry.origin)?;
    let upstream_name = &fork.upstream;
    let own_name = own.name_with_owner.to_ascii_lowercase();
    match context.pull(&own, number)? {
        Some(ours) => refused(
            Exit::Usage,
            format_args!(
                "{upstream_name} has no pull request #{number}; {own_name}#{number} is {}'s own \
                 review pull request (head {}). `track` states the upstream pull request \
                 {branch} becomes; a branch that waits for a review pull request says so with \
                 `knives depends <branch> --on {own_name}#{number}`",
                fork.name, ours.pull.head_ref_name,
            ),
        ),
        None => refused(
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

/// Everything a branch requires once `added` joins what it already `required`.
///
/// Additive and deduplicated, so declaring the same requirement twice is not an
/// error and re-running a script does not accumulate duplicates. The result is
/// the whole list, so each entry reads on its own as everything the branch
/// needed when it was written.
fn with_requirements(required: &[Requirement], added: &[Requirement]) -> Vec<String> {
    let mut texts: Vec<String> = required.iter().map(ToString::to_string).collect();
    for requirement in added {
        let text = requirement.to_string();
        if !texts.contains(&text) {
            texts.push(text);
        }
    }
    texts.sort();
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
                    let origin = match self.origin_named(&repository) {
                        Some(origin) => origin.to_owned(),
                        None => match self.renamed_origin(&repository)? {
                            Some(origin) => origin,
                            None => {
                                return refused(
                                    Exit::Usage,
                                    format_args!(
                                        "{repository} in {text} is not the own repository of any \
                                         fork in the registry"
                                    ),
                                );
                            }
                        },
                    };
                    self.fork_pull(&origin, number)
                }
                Some(Requirement::ForkBranch { repository, branch }) => {
                    if !self.is_own_repository(&repository)
                        && self.renamed_origin(&repository)?.is_none()
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
        if let Some(origin) = self.origin_named(repo) {
            return self.fork_pull(origin, number);
        }
        if self.has_branch(text) {
            return self.member_branch(text);
        }
        if let Some(origin) = self.renamed_origin(repo)? {
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
    /// its own repository, whichever the forge says has it.
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
        let theirs = RepoIdentity {
            name_with_owner: upstream.to_owned(),
            id: String::new(),
        };
        let on_upstream = self.pull(&theirs, number)?;
        let on_fork = self.pull(&own, number)?;
        let own = own.name_with_owner.to_ascii_lowercase();
        match (on_upstream, on_fork) {
            (Some(theirs), Some(ours)) => refused(
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
            (Some(_), None) => Ok(Requirement::Upstream {
                repo: upstream_name,
                number,
            }),
            (None, Some(ours)) => {
                self.not_own_review(&own, number, &ours)?;
                Ok(Requirement::ForkPull {
                    repository: own,
                    number,
                })
            }
            (None, None) => refused(
                Exit::Usage,
                format_args!(
                    "neither {upstream_name} nor {own} has a pull request #{number}, so {text} \
                     names nothing"
                ),
            ),
        }
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
        self.not_own_review(&own, number, &ours)?;
        Ok(Requirement::ForkPull {
            repository: own,
            number,
        })
    }

    /// Refuse the dependent's own review pull request: its head is the
    /// dependent, which cannot require itself.
    fn not_own_review(&self, own: &str, number: u64, ours: &PullFacts) -> Result<(), Exit> {
        if ours.pull.head_ref_name == self.branch.as_str() {
            return refused(
                Exit::Usage,
                format_args!(
                    "{own}#{number} is {}'s own review pull request (head {}): a branch cannot \
                     require itself",
                    self.branch, self.branch
                ),
            );
        }
        Ok(())
    }

    /// The spelling of a fork's own repository that `--on` takes: the name
    /// the forge gives it, `own`, when a remote here spells it so, else the
    /// registry's `spelled`.
    fn accepted_spelling<'s>(&self, own: &'s str, spelled: &'s str) -> &'s str {
        if self.origin_named(own).is_some() {
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
        let entry = self.fork.entry;
        let fork = &self.fork.name;
        if entry.bases().contains(&name) {
            return refused(
                Exit::Usage,
                format_args!("{name} is a base {fork}'s members fork from, not a member branch"),
            );
        }
        if is_release_name(&branch, &entry.release_scheme())
            || is_release_name(&branch, &entry.dated_scheme())
        {
            return refused(
                Exit::Usage,
                format_args!("{name} is a release of {fork}, not a member branch"),
            );
        }
        if pull_number_from_bookmark(name).is_some() {
            return refused(
                Exit::Usage,
                format_args!(
                    "{name} is a fetched pull request head, not a member branch; name the pull \
                     request as `<repo>#<number>`"
                ),
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

    /// The one requirement of `required` that `--forget <text>` names, or the
    /// exit its refusal takes, said on stderr.
    fn forgotten(&self, required: &[Requirement], text: &str) -> Result<Requirement, Exit> {
        let renamed = self.renamed_spelling(required, text)?;
        let answering: Vec<&Requirement> = required
            .iter()
            .filter(|requirement| self.answers_to(requirement, text, renamed.as_deref()))
            .collect();
        match answering.as_slice() {
            [one] => Ok((*one).clone()),
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
        } else if let Some(origin) = self.origin_named(repo) {
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
    /// renamed. Asked only of a `repository` that reads as `<owner>/<name>`.
    fn renamed_origin(&self, repository: &str) -> Result<Option<String>, Exit> {
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
        let own = self.identity(origin)?;
        Ok(own
            .name_with_owner
            .eq_ignore_ascii_case(repository)
            .then(|| origin.to_owned()))
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

    /// The `origin` URL of a fork whose own repository is `repository`: this
    /// fork's checkout's, or a registry entry's.
    fn origin_named(&self, repository: &str) -> Option<&str> {
        let names =
            |url: &&str| remote_slug(url).is_some_and(|slug| slug.eq_ignore_ascii_case(repository));
        self.fork
            .checkout
            .remotes
            .get(ORIGIN)
            .map(String::as_str)
            .filter(names)
            .or_else(|| {
                self.registry
                    .repos
                    .values()
                    .map(|entry| entry.origin.as_str())
                    .find(names)
            })
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
    use super::{restated, with_requirements};
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
