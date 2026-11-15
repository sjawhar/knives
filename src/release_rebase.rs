//! `knives release rebase`: move the whole composition onto an upstream commit.
//!
//! One ordered gate sequence — frozen pins, a published release in hand, stale
//! bases, the target's provenance, the landed members a bare rebase sheds —
//! around one `jj rebase -b <release> -d <target>`, with the restore of every
//! other pushed or published release name, the cleanup of the copies it makes
//! of earlier releases, and the report of what moved. The cut lives in
//! `release_cut`; the membership verbs in `release_edit`.

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::Bound;
use knives::commands::release;
use knives::forge::PullRequest;
use knives::forge::github::CliForge;
use knives::ids::{ReleaseScheme, RepoName};
use knives::ledger::inline_human_text;
use knives::release_model::{
    BranchSuccessions, EarlierWrites, carried_from_tips, carrier_label, trunk_positions,
};

use super::release_edit::{
    EditRecord, published_refusal, record_edit_event, refuse_unrecorded,
    release_is_locally_movable, release_writes, report_hidden, report_replaced, unchecked_mark,
};

/// Rebase the whole composition onto an upstream commit: `jj rebase -b <release> -d <target>`.
///
/// Every member branch's commits move onto the target and the release merge
/// moves with them, member bookmarks following their rewritten commits —
/// recorded conflict resolutions replay as ordinary rebase semantics. The
/// upstream base is never a release parent; this is how the release's members
/// change theirs. A cut deliberately does not do this: which upstream commit to
/// move onto, and whether to move at all, is a judgment. A published release in
/// hand is refused ([`release::published_release`]). Every other release name
/// that is pushed or published is put back on its commit, and one never pushed
/// follows its rewrite; tags stay where they were, and the copies the rebase
/// makes of pushed, published or tagged releases are abandoned unless something
/// rests on
/// them. After a bare rebase, members whose pull requests landed and carry
/// nothing more are dropped — the work reaches the release through its new
/// base — unless `--no-drop` keeps them.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "rebasing is one ordered stateful gate sequence; its inputs remain explicit and its stages must not be separated"
)]
pub(crate) fn run_rebase(
    fork: &Fork<'_>,
    reference: Option<&str>,
    no_drop: bool,
    extra_consumers: &[&std::path::Path],
    cache_root: Option<&std::path::Path>,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let repo = &fork.name;
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let mut locals = extra_consumers
        .iter()
        .map(|path| path.to_path_buf())
        .collect::<Vec<_>>();
    locals.sort();
    locals.dedup();
    let forge = CliForge;
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    let opened = knives::jj::Repo::open(path)?;
    let consumers = release::ConsumerInputs {
        slugs: &entry.consumers,
        locals: &locals,
        forge: &forge,
        cache_root,
        heads: &heads,
    };
    let plan = release::pulled_plan(fork, bound.registry, &consumers)?;
    if !plan.problems.is_empty() {
        println!("{}", release::render(&plan));
        return Ok(Exit::Incomplete);
    }
    let Some(release_name) = plan.release.clone() else {
        println!("{repo}: no release to move");
        return Ok(Exit::Ok);
    };
    if !release_is_locally_movable(&opened, repo, &release_name) {
        return Ok(Exit::Incomplete);
    }
    if let Some(exit) = stale_rule_exit(fork)? {
        return Ok(exit);
    }
    let Some(destination) = rebase_target(RebaseTargetInput {
        fork,
        opened: &opened,
        reference,
        cache_root,
    })?
    else {
        return Ok(Exit::Incomplete);
    };
    let onto = destination.onto.clone();
    let reference = destination.reference.clone();
    let release_commit = opened.resolve_commit(&release_name)?;
    // Both gates come before the already-contains path: it can still drop
    // landed members, which moves the release name.
    let scheme = entry.release_scheme();
    if let Some(exit) = frozen_rebase_exit(
        repo,
        &release_name,
        &scheme,
        release::repair_effect(
            &plan.pins,
            knives::ids::BookmarkRef::parse(&release_name).branch(),
            plan.published.as_ref(),
        ),
    ) {
        return Ok(exit);
    }
    let ledger = knives::ledger::Ledger::for_fork(fork).read()?.entries;
    // An earlier write of the release a fast-forward push gave it is no
    // member: nothing classifies it, and the write before the rebase leaves
    // it out, so the rebase moves the members and the release only.
    let writes = EarlierWrites::read(&opened, &release_name, &ledger, plan.published.as_ref())?;
    // A release that carries a change knives did not write or adopt would
    // carry it onto the new base unmeasured, as every write would.
    let in_hand = release::InHand {
        repo: &opened,
        ledger: &ledger,
        name: &release_name,
        commit: &release_commit,
        writes: &writes,
        published: plan.published.as_ref(),
    };
    let standing = match refuse_unrecorded((repo, "rebase"), entry, &in_hand)? {
        Ok(standing) => standing,
        Err(exit) => return Ok(exit),
    };
    if let Some(refusal) = published_refusal(
        &opened,
        &ledger,
        release::ReleaseAt {
            scheme: &scheme,
            name: &release_name,
            commits: &[Some(&release_commit), plan.published.as_ref()],
        },
        ("rebasing", "rebase"),
    )? {
        println!("{repo}: {refusal}");
        return Ok(Exit::Incomplete);
    }
    let unchecked = unchecked_mark(repo, &in_hand, &standing);
    if let Some(exit) = existing_rebase_exit(ExistingRebaseInput {
        opened: &opened,
        fork,
        release_name: &release_name,
        release_commit: &release_commit,
        destination: &destination,
        no_drop,
        bound,
        unchecked: &unchecked,
    })? {
        return Ok(exit);
    }
    let carriers = writes.carriers(&opened.parent_commits(release_commit.as_str())?);
    let context = RebaseContext {
        repo,
        entry,
        opened: &opened,
        carriers: &carriers,
    };
    let Some((members, shed)) = classify_rebase_parents(&context, &release_name, &onto)? else {
        return Ok(Exit::Incomplete);
    };
    // The release is a name read from the repository.
    let shown_release = inline_human_text(&release_name);
    if all_landed(&opened, &members, &onto)? {
        println!(
            "{repo}: every member of {shown_release} has landed in {reference}; rebasing \
             would make the trunk the only parent, so nothing moved \u{2014} reap the release \
             or include new work"
        );
        return Ok(Exit::Incomplete);
    }
    if members.is_empty() {
        println!("{repo}: {shown_release} has no member parents to move; nothing to rebase");
        return Ok(Exit::Incomplete);
    }
    // Every refusal is behind: the one write the rebase makes before it
    // moves anything sheds the stale bases and leaves the carriers out.
    let shed_write = shed_stale_bases(
        fork,
        (&release_name, &release_commit),
        (&members, shed),
        (&carriers, writes.commits()),
    )?;
    // The release the rebase moves: the write, when there was one.
    let (before_rebase, shed_unions) = shed_write.map_or_else(
        || (release_commit.clone(), Vec::new()),
        |written| (written.commit, written.unions),
    );
    // Everything below reads the release the write left, not the old one.
    let opened = knives::jj::Repo::open(path)?;
    let in_hand = opened.resolve_commit(&release_name)?;
    // Every earlier write of the name still visible, other than the release
    // in hand and the write just made, is one more commit the rebase may
    // copy: a carrier a conflicted write left visible, which nothing hid.
    // Its copy is cleaned up as a tagged release's is.
    let releases_own = [
        opened.change_id_of(release_commit.as_str())?,
        opened.change_id_of(in_hand.as_str())?,
    ];
    let mut earlier_originals = Vec::new();
    for earlier in
        EarlierWrites::read(&opened, &release_name, &ledger, plan.published.as_ref())?.commits()
    {
        if opened.is_visible(earlier)?
            && !releases_own.contains(&opened.change_id_of(earlier.as_str())?)
        {
            earlier_originals.push(earlier.clone());
        }
    }
    // A pushed or published release name stays where it is: the rebase
    // rewrites each descendant of a moved member, earlier release merges among
    // them, and a push of the moved name would move what consumers were given.
    // A name never pushed follows its rewrite: nothing outside this repository
    // names it, and putting it back would make the old member commits visible
    // beside their rebased copies, divergent.
    let tips = opened.bookmark_tips()?;
    let publish_remote = knives::ids::RemoteName::new(entry.publish_remote());
    let mut kept_releases: Vec<(String, knives::ids::CommitId)> = Vec::new();
    let mut followers: Vec<String> = Vec::new();
    for (reference, commit) in &tips {
        let knives::ids::BookmarkRef::Local(branch) = reference else {
            continue;
        };
        if branch.as_str() == release_name || !scheme.owns(branch.as_str()) {
            continue;
        }
        let pushed = tips.get(&knives::ids::BookmarkRef::Remote {
            branch: branch.clone(),
            remote: publish_remote.clone(),
        }) == Some(commit);
        let published = release::published_release(
            &opened,
            &ledger,
            release::ReleaseAt {
                scheme: &scheme,
                name: branch.as_str(),
                commits: &[Some(commit)],
            },
        )?
        .is_some();
        if pushed || published {
            kept_releases.push((branch.to_string(), commit.clone()));
        } else if members.iter().try_fold(false, |found, member| {
            Ok::<bool, knives::jj::JjError>(found || opened.is_ancestor(member, commit)?)
        })? {
            followers.push(branch.to_string());
        }
    }
    let tagged = knives::jj::tagged_commits_a_rebase_copies(path, &release_name, &onto)?;
    knives::jj::rebase_branch_onto(path, &release_name, &onto)?;
    let restored = knives::jj::restore_local_bookmarks(
        path,
        &kept_releases,
        &format!("knives: {release_name}: keep every pushed release where it was"),
    )?;
    // Release names read from the repository.
    for (name, commit) in &restored {
        println!(
            "{repo}: {} stays at {}: it is pushed or published, and a rebase moves only the \
             release in hand and its members",
            inline_human_text(name),
            commit.short()
        );
    }
    for name in &followers {
        println!(
            "{repo}: {} follows the rebase: it was never pushed or published",
            inline_human_text(name)
        );
    }
    report_rebased_release(
        fork,
        RebasedRelease {
            name: &release_name,
            reference: &reference,
            onto: &onto,
            shed,
            before: &before_rebase,
            shed_unions,
            unchecked: &unchecked,
        },
        bound,
    )?;
    // After the provenance is recorded, so a failed cleanup cannot leave the
    // rebased release described with its pre-rebase parents.
    let mut originals = tagged;
    for commit in restored
        .iter()
        .map(|(_, commit)| commit)
        .chain(&earlier_originals)
    {
        if !originals.contains(commit) {
            originals.push(commit.clone());
        }
    }
    let rebased = knives::jj::Repo::open(path)?;
    let cleaned = knives::jj::abandon_tagged_copies(
        path,
        &originals,
        (
            &onto,
            knives::jj::RebasedName {
                name: &release_name,
                publish_remote: entry.publish_remote(),
                commit: &rebased.resolve_commit(&release_name)?,
                earlier: EarlierWrites::read(
                    &rebased,
                    &release_name,
                    &ledger,
                    plan.published.as_ref(),
                )?
                .commits(),
            },
        ),
        &format!("knives: {release_name}: abandon rebased copies of earlier releases"),
    )?;
    report_tagged_copies(
        repo,
        &cleaned.copies,
        (&restored, &earlier_originals),
        &release_name,
    );
    report_hidden(repo, &release_name, &cleaned.hidden);
    if no_drop {
        return Ok(Exit::Ok);
    }
    // The rebase recorded the release it moved, so what it drops next is
    // dropped from a recorded write.
    drop_landed_members(fork, &release_name, &destination, (bound, ""))
}

fn frozen_rebase_exit(
    repo: &RepoName,
    release_name: &str,
    scheme: &ReleaseScheme,
    effect: release::RepairEffect,
) -> Option<Exit> {
    if effect != release::RepairEffect::NewDatedName {
        return None;
    }
    let shown_release = inline_human_text(release_name);
    match scheme {
        ReleaseScheme::Dated | ReleaseScheme::Series(_) => println!(
            "{repo}: every pin of {shown_release} is frozen, so moving it would reach \
             nobody; cut a new dated release instead"
        ),
        ReleaseScheme::Fixed(_) => println!(
            "{repo}: every pin of {shown_release} is frozen, so moving the fixed branch \
             would reach nobody; update the frozen consumer pins, or change the release \
             scheme before advancing it (fixed branches cannot reach revision pins)"
        ),
    }
    Some(Exit::Incomplete)
}

/// Refuse while the checkout's jj config states a rule an earlier `knives start`
/// wrote that is no longer this fork's: the tag-pinning rule knives wrote before
/// its own release tags were exempt froze every member beneath a tagged release,
/// and `jj rebase` would fail with jj's bare "is immutable". Refreshing is
/// `start`'s job; a rule a human stated is theirs, and `status` reports it.
fn stale_rule_exit(fork: &Fork<'_>) -> anyhow::Result<Option<Exit>> {
    let rule = fork.entry.immutable_heads();
    let Some(stated) = knives::jj::repo_immutable_heads(&fork.checkout.path)? else {
        return Ok(None);
    };
    if !stated.written_by_knives || stated.rule == rule {
        return Ok(None);
    }
    // The rule is read back from this checkout's jj config, which an earlier
    // `knives start` wrote but a person may have edited since, so it reaches
    // the terminal escaped; `rule` is the entry's `repos.toml` value.
    println!(
        "{}: this checkout's jj config states immutable_heads() = `{}`, written by an \
         earlier `knives start`; this fork runs under `{rule}`. Any `knives start <branch>` \
         in this fork refreshes it; rebase again after that. Nothing moved",
        fork.name,
        knives::ledger::inline_human_text(&stated.rule)
    );
    Ok(Some(Exit::Incomplete))
}

/// Rewrite the release to its member parents only, shedding stale bases and
/// leaving out its carriers, the earlier writes of it a fast-forward push gave
/// it, in one write; what the write made, or `None` when there was neither to
/// leave out.
///
/// A carrier left in would make the rebase copy it: `jj rebase -b` rewrites
/// every descendant of the members it moves, and a carrier is a merge of
/// them. Written out first, the release's resolutions over it become its own
/// change and move with it, and the carrier is hidden in the same write when
/// nothing holds it.
fn shed_stale_bases(
    fork: &Fork<'_>,
    (release_name, release_commit): (&str, &knives::ids::CommitId),
    (members, shed): (&[knives::ids::CommitId], usize),
    (carriers, earlier): (&[knives::ids::CommitId], &[knives::ids::CommitId]),
) -> anyhow::Result<Option<knives::jj::ReleaseWritten>> {
    let mut done = Vec::new();
    if shed > 0 {
        done.push(format!("shed {shed} stale base parent(s)"));
    }
    if !carriers.is_empty() {
        done.push(format!(
            "leave out {} earlier write(s) of the release",
            carriers.len()
        ));
    }
    if done.is_empty() {
        return Ok(None);
    }
    let written = knives::jj::write_release(
        &fork.checkout.path,
        &knives::jj::ReleaseWrite {
            source: Some(release_commit),
            parents: members,
            carriers,
            message: None,
            bookmark: Some(release_name),
            publish_remote: Some(fork.entry.publish_remote()),
            earlier,
            operation: &format!("knives: {release_name}: {}", done.join("; ")),
        },
    )?;
    report_replaced(&fork.name, release_name, &written);
    Ok(Some(written))
}

/// One repo mid-rebase: what parent classification needs to read and say.
struct RebaseContext<'a> {
    repo: &'a RepoName,
    entry: &'a knives::config::RepoEntry,
    opened: &'a knives::jj::Repo,
    /// The release's parents that are earlier writes of it, which no
    /// classification reads.
    carriers: &'a [knives::ids::CommitId],
}

/// The release's moving members and the count of shed legacy base parents, or
/// `None` after printing the stale-parent refusal.
///
/// A stale parent is the old tip of a branch that has moved on: rebasing
/// carries pre-rewrite code onto the new base. A held parent — any bookmark
/// still on it whose branch is neither a release name nor the trunk — is a
/// member and moves. An unheld parent already reachable from the target is a
/// legacy trunk parent: the base is never a parent, so it is shed. A carrier
/// is neither: the write before the rebase leaves it out.
fn classify_rebase_parents(
    context: &RebaseContext<'_>,
    release_name: &str,
    onto: &knives::ids::CommitId,
) -> anyhow::Result<Option<(Vec<knives::ids::CommitId>, usize)>> {
    let (repo, entry, opened) = (context.repo, context.entry, context.opened);
    let scheme = entry.release_scheme();
    let parents: Vec<knives::detect::ReleaseParent> = opened
        .parents_of(release_name)?
        .into_iter()
        .filter(|parent| !context.carriers.contains(&parent.commit))
        .collect();
    let mut members: Vec<knives::ids::CommitId> = Vec::new();
    let mut shed = 0usize;
    for parent in &parents {
        let held = parent.bookmarks.iter().any(|reference| {
            !knives::ids::is_release_name(reference.branch(), &scheme)
                && reference.branch().as_str() != entry.trunk()
        });
        if held {
            members.push(parent.commit.clone());
            continue;
        }
        if opened.is_ancestor(&parent.commit, onto)? {
            shed += 1;
            continue;
        }
        let no_bookmark = if parent.bookmarks.is_empty() {
            "; no bookmark points at it"
        } else {
            ""
        };
        // The branch this parent belonged to, found by ancestry or by change
        // id, so a member rebased onto the newer trunk is named as itself and
        // the refusal says how it moves — never "fix the branch", which read as
        // an instruction to put a copy back on the old base.
        let moved = stale_parent_moved_branches(opened, entry, &parent.commit)?;
        // The release is a name read from the repository.
        let shown_release = inline_human_text(release_name);
        match moved {
            // `moved` is jj's account of where the member went, built from the
            // bookmarks and ids it holds, so the refusal escapes it.
            Some(moved) => eprintln!(
                "{repo}: refusing to rebase {shown_release}: parent {} is stale{no_bookmark}; it \
                 was {}. `knives release advance` moves the member to its branch, then \
                 re-run; carrying the old commit could ship pre-rewrite code.",
                parent.commit.short(),
                inline_human_text(&moved),
            ),
            None => eprintln!(
                "{repo}: refusing to rebase {shown_release}: parent {} is stale{no_bookmark}, \
                 and no local branch continues it. Drop it from the release (`knives release \
                 drop {}`) or restore its branch, then re-run; carrying it could ship \
                 pre-rewrite code.",
                parent.commit.short(),
                parent.commit.short(),
            ),
        }
        return Ok(None);
    }
    Ok(Some((members, shed)))
}

/// A resolved rebase destination: the commit, the label the report and
/// provenance use, and which of our pull requests the forge says landed by it.
struct RebaseDestination {
    onto: knives::ids::CommitId,
    reference: String,
    /// Empty for an explicit reference: dropping is the bare default's job,
    /// because only it knows the target covers every landing.
    landed: Vec<PullRequest>,
}

#[derive(Clone, Copy)]
struct RebaseTargetInput<'a> {
    fork: &'a Fork<'a>,
    opened: &'a knives::jj::Repo,
    reference: Option<&'a str>,
    cache_root: Option<&'a std::path::Path>,
}

#[derive(Clone, Copy)]
struct ExistingRebaseInput<'a> {
    opened: &'a knives::jj::Repo,
    fork: &'a Fork<'a>,
    release_name: &'a str,
    release_commit: &'a knives::ids::CommitId,
    destination: &'a RebaseDestination,
    no_drop: bool,
    bound: Bound<'a>,
    /// The mark the drop's event ends with ([`release::unchecked_suffix`]).
    unchecked: &'a str,
}

fn existing_rebase_exit(input: ExistingRebaseInput<'_>) -> anyhow::Result<Option<Exit>> {
    let ExistingRebaseInput {
        opened,
        fork,
        release_name,
        release_commit,
        destination,
        no_drop,
        bound,
        unchecked,
    } = input;
    if !opened.is_ancestor(&destination.onto, release_commit)? {
        return Ok(None);
    }
    // The release is a name read from the repository; the destination is the
    // caller's argument or a commit id this run resolved.
    println!(
        "{}: {} already contains {}",
        fork.name,
        inline_human_text(release_name),
        destination.reference
    );
    let exit = if no_drop {
        Exit::Ok
    } else {
        drop_landed_members(fork, release_name, destination, (bound, unchecked))?
    };
    Ok(Some(exit))
}

/// The commit a rebase moves onto, with the label the report and provenance use.
///
/// An explicit reference is taken at its word. Without one, the default is the
/// first upstream trunk commit that contains every merged pull request — merged,
/// not closed, because closed landed nothing.
fn rebase_target(input: RebaseTargetInput<'_>) -> anyhow::Result<Option<RebaseDestination>> {
    let RebaseTargetInput {
        fork,
        opened,
        reference,
        cache_root,
    } = input;
    if let Some(explicit) = reference {
        return Ok(Some(RebaseDestination {
            onto: opened.resolve_commit(explicit)?,
            reference: explicit.to_owned(),
            landed: Vec::new(),
        }));
    }
    merged_rebase_target(fork, opened, cache_root)
}

fn select_merged_numbers(discovery: &knives::snapshot::Discovery<'_>, trunk: &str) -> Vec<u64> {
    knives::forge::merged_onto(&discovery.ours(), trunk)
        .iter()
        .map(|pull| pull.number)
        .collect()
}

/// The bare-rebase default target, or `None` with its reason already printed.
///
/// Rebasing to this point makes every merged branch's work part of the members'
/// shared history without carrying anything upstream has not accepted. With
/// nothing merged there is no such point, and which commit to move onto goes
/// back to being a judgment the caller must make.
fn merged_rebase_target(
    fork: &Fork<'_>,
    opened: &knives::jj::Repo,
    cache_root: Option<&std::path::Path>,
) -> anyhow::Result<Option<RebaseDestination>> {
    let repo = &fork.name;
    let entry = fork.entry;
    let trunk = entry.trunk_ref();
    let forge = CliForge;
    let opened_snapshot = match knives::snapshot::open(knives::snapshot::SnapshotConfig {
        forge: &forge,
        path: &fork.checkout.path,
        upstream: entry.remote(knives::config::Role::Upstream),
        remotes: [
            entry.remote(knives::config::Role::Origin),
            entry.remote(knives::config::Role::Release),
        ],
        cache_root,
    }) {
        Ok(opened_snapshot) => opened_snapshot,
        Err(error) => {
            // The forge's own text, which prints more than one line.
            eprintln!(
                "{repo}: could not ask the forge which pull requests merged: {}; \
                 provide a commit to rebase onto",
                knives::ledger::problem_human_text(&error.to_string())
            );
            return Ok(None);
        }
    };
    let snapshot = match opened_snapshot.complete_with(entry.trunk(), select_merged_numbers) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            // The forge's own text, as above.
            eprintln!(
                "{repo}: could not ask the forge which pull requests merged: {}; \
                 provide a commit to rebase onto",
                knives::ledger::problem_human_text(&error.to_string())
            );
            return Ok(None);
        }
    };
    let candidates = knives::forge::merged_onto(snapshot.ours(), entry.trunk());
    let result: anyhow::Result<Option<RebaseDestination>> = (|| {
        let Some(landed) = verified_merged_candidates(repo, &snapshot, &candidates, entry.trunk())
        else {
            return Ok(None);
        };
        if landed.is_empty() {
            println!(
                "{repo}: no pull request has merged, so there is no default target; \
                 provide a commit to rebase onto"
            );
            return Ok(None);
        }
        let tip = opened.resolve_commit(&trunk)?;
        let mut placed: Vec<(u64, knives::ids::CommitId)> = Vec::new();
        let mut unplaced: Vec<u64> = Vec::new();
        for pull in &landed {
            let oid = pull.merge_commit.as_ref().map(|merge| merge.oid.clone());
            let number = pull.number;
            // Unrecorded, unresolvable and out-of-trunk merge commits are one fact
            // here: the local trunk does not carry that landing yet.
            match oid.and_then(|oid| opened.resolve_commit(&oid).ok()) {
                Some(commit) if opened.is_ancestor(&commit, &tip)? => placed.push((number, commit)),
                _ => unplaced.push(number),
            }
        }
        if !unplaced.is_empty() {
            println!(
                "{repo}: the merge commit(s) of {} are not in the local {trunk}; \
                 run knives sync, or provide a commit to rebase onto",
                numbered(&unplaced)
            );
            return Ok(None);
        }
        let Some((onto, reference)) = covering_commit(repo, opened, &placed, &trunk)? else {
            return Ok(None);
        };
        Ok(Some(RebaseDestination {
            onto,
            reference,
            landed,
        }))
    })();
    if let Err(note) = snapshot.persist(None) {
        // The note carries the OS's message and the cache path it names.
        eprintln!(
            "{repo}: {}",
            knives::ledger::problem_human_text(&note.to_string())
        );
    }
    result
}

fn verified_merged_candidates(
    repo: &RepoName,
    snapshot: &knives::snapshot::CompletedSnapshot<'_>,
    candidates: &[knives::forge::PullSummary],
    trunk: &str,
) -> Option<Vec<knives::forge::PullRequest>> {
    let unanswered: Vec<u64> = candidates
        .iter()
        .filter(|candidate| snapshot.fact(candidate.number).is_none())
        .map(|candidate| candidate.number)
        .collect();
    if !unanswered.is_empty() {
        eprintln!(
            "{repo}: could not ask the forge which pull requests merged: it did not report \
             facts for {}; provide a commit to rebase onto",
            numbered(&unanswered)
        );
        return None;
    }
    let mut landed = Vec::new();
    for candidate in candidates {
        let fact = snapshot.fact(candidate.number)?;
        let pull = &fact.pull;
        if pull.is_merged() && pull.base_ref_name.as_deref() == Some(trunk) {
            landed.push(pull.clone());
        }
    }
    Some(landed)
}

/// The first trunk commit containing every landing in `placed`: their maximum
/// by ancestry, which on a trunk is the newest of them.
fn covering_commit(
    repo: &RepoName,
    opened: &knives::jj::Repo,
    placed: &[(u64, knives::ids::CommitId)],
    trunk: &str,
) -> anyhow::Result<Option<(knives::ids::CommitId, String)>> {
    let Some(((_, first), rest)) = placed.split_first() else {
        anyhow::bail!("{repo}: no landed merge commit to cover; this is a bug");
    };
    let mut covering = first.clone();
    for (_, commit) in rest {
        if opened.is_ancestor(&covering, commit)? {
            covering = commit.clone();
        }
    }
    let numbers: Vec<u64> = placed.iter().map(|(number, _)| *number).collect();
    for (_, commit) in placed {
        if commit != &covering && !opened.is_ancestor(commit, &covering)? {
            // Merge commits that do not sit on one line cannot all be covered by
            // one of them; picking any would leave merged work out silently.
            println!(
                "{repo}: the merge commits of {} do not sit on one line of {trunk}; \
                 provide a commit to rebase onto",
                numbered(&numbers)
            );
            return Ok(None);
        }
    }
    println!(
        "{repo}: every merged pull request ({}) is in {trunk} by {}; rebasing onto it",
        numbered(&numbers),
        covering.short()
    );
    let label = covering.short().to_owned();
    Ok(Some((covering, label)))
}

/// `#7, #12` — pull requests named the way the forge shows them.
fn numbered(numbers: &[u64]) -> String {
    let numbers: Vec<String> = numbers.iter().map(|number| format!("#{number}")).collect();
    numbers.join(", ")
}

/// Whether every member has landed at or before `onto`. No members is not that:
/// it is its own refusal, with its own message.
fn all_landed(
    opened: &knives::jj::Repo,
    members: &[knives::ids::CommitId],
    onto: &knives::ids::CommitId,
) -> anyhow::Result<bool> {
    if members.is_empty() {
        return Ok(false);
    }
    for member in members {
        if !opened.is_ancestor(member, onto)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Drop the members whose pull requests landed at or before the rebase target.
///
/// Only a member carrying nothing past the target is dropped: a branch with
/// commits beyond its merged pull request still holds undelivered work, and
/// that is said instead. The drop duplicates the release onto the kept parents,
/// exactly as `drop` does, so recorded conflict resolutions carry forward, and
/// leaves out the release's carriers, as every write composing from its
/// parents does. `unchecked` is the mark its event ends with
/// ([`release::unchecked_suffix`]).
fn drop_landed_members(
    fork: &Fork<'_>,
    release_name: &str,
    destination: &RebaseDestination,
    (bound, unchecked): (Bound<'_>, &str),
) -> anyhow::Result<Exit> {
    if destination.landed.is_empty() {
        return Ok(Exit::Ok);
    }
    let repo = &fork.name;
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let opened = knives::jj::Repo::open(path)?;
    let writes = release_writes(fork, &opened, release_name)?;
    let (carriers, parents): (Vec<knives::ids::CommitId>, Vec<knives::ids::CommitId>) = opened
        .parent_commits(release_name)?
        .into_iter()
        .partition(|parent| writes.contains(parent));
    let mut kept = parents.clone();
    let mut deltas: Vec<String> = Vec::new();
    for pull in &destination.landed {
        let Some(tip) = opened.local_bookmark_tip(&pull.head_ref_name) else {
            continue;
        };
        if !parents.contains(&tip) {
            continue;
        }
        if knives::jj::carries_work_past(path, &destination.onto, &tip)? {
            // The head branch is the forge's name for it, a ref a remote gave.
            println!(
                "{repo}: kept {}: it carries work past #{}",
                inline_human_text(&pull.head_ref_name),
                pull.number
            );
            continue;
        }
        kept.retain(|parent| parent != &tip);
        deltas.push(format!(
            "dropped {}: landed upstream as #{}",
            pull.head_ref_name, pull.number
        ));
    }
    if deltas.is_empty() {
        return Ok(Exit::Ok);
    }
    let shown_release = inline_human_text(release_name);
    if kept.is_empty() {
        println!(
            "{repo}: every member of {shown_release} landed; dropping them all would leave it \
             without a parent, so nothing was dropped"
        );
        return Ok(Exit::Incomplete);
    }
    let release = opened.resolve_commit(release_name)?;
    let provenance = release::parent_sources(&opened, entry, &entry.release_scheme(), &kept)?;
    let delta = deltas.join("; ");
    let message = release::composition_message(release_name, &provenance, &delta);
    let written = knives::jj::write_release(
        path,
        &knives::jj::ReleaseWrite {
            source: Some(&release),
            parents: &kept,
            carriers: &carriers,
            message: Some(&message),
            bookmark: Some(release_name),
            publish_remote: Some(entry.publish_remote()),
            earlier: writes.commits(),
            operation: &format!("knives: {release_name}: {delta}"),
        },
    )?;
    let created = written.commit.clone();
    record_edit_event(
        fork,
        &opened,
        &EditRecord {
            release: release_name,
            delta: &delta,
            created: &created,
            change: &written.change,
            provenance: &provenance,
            unions: &written.unions,
            unchecked,
        },
        bound,
    )?;
    // The delta names the forge's head branches; the ledger keeps it raw.
    println!(
        "{repo}: {shown_release} now has {} parent(s): {}",
        kept.len(),
        inline_human_text(&delta)
    );
    report_replaced(repo, release_name, &written);
    match knives::jj::conflicted_files(path, created.as_str()) {
        Ok(files) => println!(
            "{}",
            release::conflict_guidance(release_name, &files, &written.unions)
        ),
        Err(error) => println!("  could not list conflicts: {error}"),
    }
    Ok(Exit::Ok)
}

/// A composition rebase that just happened: what moved, and onto what.
struct RebasedRelease<'a> {
    name: &'a str,
    reference: &'a str,
    onto: &'a knives::ids::CommitId,
    shed: usize,
    /// The release commit the rebase moved: what the fork had before it.
    before: &'a knives::ids::CommitId,
    /// The release logs the shed write unioned before the rebase.
    shed_unions: Vec<knives::jj::UnionedFile>,
    /// The mark the rebase's event ends with ([`release::unchecked_suffix`]).
    unchecked: &'a str,
}

/// Re-describe the rebased release so the recorded provenance names the
/// rewritten parents, writing in the same operation the union of each release
/// log the rebase left conflicted, and report what moved.
fn report_rebased_release(
    fork: &Fork<'_>,
    rebased: RebasedRelease<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<()> {
    let repo = &fork.name;
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let reopened = knives::jj::Repo::open(path)?;
    let created = reopened.resolve_commit(rebased.name)?;
    let new_parents = reopened.parent_commits(rebased.name)?;
    let provenance =
        release::parent_sources(&reopened, entry, &entry.release_scheme(), &new_parents)?;
    let delta = format!("rebased onto {}", rebased.reference);
    let message = release::composition_message(rebased.name, &provenance, &delta);
    let (described, described_unions) = knives::jj::describe_release(
        path,
        (&created, rebased.before),
        &message,
        &format!("knives: {}: record rebased provenance", rebased.name),
    )?;
    let conflicts = knives::jj::conflicted_files(path, described.as_str());
    let unions = rebase_unions(
        rebased.shed_unions,
        described_unions,
        conflicts.as_deref().unwrap_or_default(),
    );
    // A rebase rewrites every member's commit id; without this the ledger's
    // last (branch, commit) pairing for the release would name commits no
    // longer among its parents, and the next edit could not tell a rebuilt
    // branch from a stranger.
    // `describe` keeps the change id the rebased release already had.
    let change = reopened.change_id_of(created.as_str())?;
    record_edit_event(
        fork,
        &reopened,
        &EditRecord {
            release: rebased.name,
            delta: &delta,
            created: &described,
            change: &change,
            provenance: &provenance,
            unions: &unions,
            unchecked: rebased.unchecked,
        },
        bound,
    )?;
    let stale_bases = if rebased.shed > 0 {
        format!(", {} stale base parent(s) shed", rebased.shed)
    } else {
        String::new()
    };
    // The release is a name read from the repository; the reference is the
    // caller's argument or a commit id this run resolved.
    println!(
        "{repo}: {} rebased onto {} ({}), {} member(s) moved with it{stale_bases}",
        inline_human_text(rebased.name),
        rebased.reference,
        rebased.onto.short(),
        new_parents.len()
    );
    match conflicts {
        Ok(files) => println!(
            "{}",
            release::conflict_guidance(rebased.name, &files, &unions)
        ),
        Err(error) => println!("  could not list conflicts: {error}"),
    }
    Ok(())
}

/// What a rebase unioned, said once for its two writes: every union the
/// re-describe wrote, and each union the shed write made before the rebase
/// that the rebase neither unioned again nor left conflicted. A log the rebase
/// conflicted again and could not union is `conflicted`'s, and naming it as
/// unioned too would say the opposite of the tree.
fn rebase_unions(
    mut shed: Vec<knives::jj::UnionedFile>,
    mut described: Vec<knives::jj::UnionedFile>,
    conflicted: &[String],
) -> Vec<knives::jj::UnionedFile> {
    shed.retain(|union| {
        !conflicted.contains(&union.path) && !described.iter().any(|later| later.path == union.path)
    });
    described.append(&mut shed);
    described.sort_by(|first, second| first.path.cmp(&second.path));
    described
}

/// One line per copy the rebase made of an earlier release — tagged, named
/// by a release bookmark put back on it, or an earlier write of the release
/// in hand (`carriers`): abandoned, or kept with a note naming what rests on
/// it.
fn report_tagged_copies(
    repo: &RepoName,
    copies: &[knives::jj::TaggedCopy],
    (restored, carriers): (&[(String, knives::ids::CommitId)], &[knives::ids::CommitId]),
    release: &str,
) {
    for line in tagged_copy_lines(repo, copies, (restored, carriers), release) {
        println!("{line}");
    }
}

/// The lines [`report_tagged_copies`] prints. Tag and release names are read
/// from the repository, and what keeps a copy names bookmarks, tags and jj
/// workspaces (`what_keeps`, src/jj.rs), so each is escaped. A copy of an
/// earlier write of `release` is labelled as one, so no reader takes it for a
/// member's.
fn tagged_copy_lines(
    repo: &RepoName,
    copies: &[knives::jj::TaggedCopy],
    (restored, carriers): (&[(String, knives::ids::CommitId)], &[knives::ids::CommitId]),
    release: &str,
) -> Vec<String> {
    let mut lines = Vec::new();
    for copy in copies {
        let mut names: Vec<String> = copy.tags.iter().map(|tag| format!("tag {tag}")).collect();
        names.extend(
            restored
                .iter()
                .filter(|(_, commit)| *commit == copy.original)
                .map(|(name, _)| name.clone()),
        );
        let keeps_a_name = !names.is_empty();
        if carriers.contains(&copy.original) {
            names.push(carrier_label(release));
        }
        let of = format!(
            "the rebased copy of {} ({})",
            copy.original.short(),
            inline_human_text(&names.join(", "))
        );
        let copy_id = copy.copy.short();
        let original = if keeps_a_name {
            "the original keeps its name"
        } else {
            "the original stays where it was"
        };
        lines.push(copy.kept.as_ref().map_or_else(
            || format!("{repo}: abandoned {copy_id}, {of}; {original}"),
            |reason| {
                format!(
                    "{repo}: note: kept {copy_id}, {of}: {}",
                    inline_human_text(reason)
                )
            },
        ));
    }
    lines
}

/// `feat/alpha (now 1a2b3c4d5e6f)` for every maintained branch that continues a
/// stale parent — grown past it or rebased off it — or `None` when none does.
fn stale_parent_moved_branches(
    opened: &knives::jj::Repo,
    entry: &knives::config::RepoEntry,
    parent: &knives::ids::CommitId,
) -> Result<Option<String>, knives::jj::JjError> {
    let branches = carried_from_tips(
        &opened.bookmark_tips()?,
        entry.trunk(),
        &entry.release_scheme(),
    );
    let trunks = trunk_positions(opened, entry)?;
    let moved: Vec<String> = BranchSuccessions::of(opened, &trunks, &branches)?
        .successors_of(parent)?
        .into_iter()
        .map(|(branch, tip)| format!("{branch} (now {})", tip.short()))
        .collect();
    Ok((!moved.is_empty()).then(|| moved.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use knives::ids::CommitId;
    use knives::jj::TaggedCopy;

    #[test]
    fn a_rebased_copy_names_its_tags_releases_and_keepers_escaped() {
        let copies = [TaggedCopy {
            original: CommitId::new("aaaa"),
            tags: vec!["v1\u{9b}2J".to_owned()],
            copy: CommitId::new("bbbb"),
            kept: Some("the working copy of workspace ws\u{1b}x rests on it".to_owned()),
        }];
        let restored = [("release/2026-01-01\u{9d}".to_owned(), CommitId::new("aaaa"))];

        let lines = tagged_copy_lines(
            &RepoName::new("demo"),
            &copies,
            (&restored, &[]),
            "release/2026-01-02",
        );

        assert_eq!(
            lines,
            [
                "demo: note: kept bbbb, the rebased copy of aaaa (tag v1\u{fffd}2J, \
              release/2026-01-01\u{fffd}): the working copy of workspace ws\u{fffd}x rests on it"
            ]
        );
    }

    #[test]
    fn a_rebased_copy_of_an_earlier_write_is_labelled_a_carrier_copy() {
        // Given: a rebase's copy of an earlier write of the release in hand,
        // which no tag or release bookmark names, abandoned.
        let copies = [TaggedCopy {
            original: CommitId::new("aaaa"),
            tags: Vec::new(),
            copy: CommitId::new("bbbb"),
            kept: None,
        }];
        let carriers = [CommitId::new("aaaa")];

        // When: the rebase reports it.
        let lines = tagged_copy_lines(&RepoName::new("demo"), &copies, (&[], &carriers), "sami");

        // Then: the copy is named as an earlier write's, never a member's.
        assert_eq!(
            lines,
            [
                "demo: abandoned bbbb, the rebased copy of aaaa (sami earlier write (carrier, \
                 not a member)); the original stays where it was"
            ]
        );
    }

    fn unioned(path: &str, first_line: &str) -> knives::jj::UnionedFile {
        knives::jj::UnionedFile {
            path: path.to_owned(),
            first_lines: vec![first_line.to_owned()],
        }
    }

    #[test]
    fn a_rebase_names_each_union_once_and_never_one_it_left_conflicted() {
        // Given: a shed write that unioned three logs, a re-describe that
        // unioned one of them again, and a rebase that left another
        // conflicted.
        let shed = vec![
            unioned("CHANGELOG.md", "## shed"),
            unioned("RELEASE_NOTES.md", "## shed"),
            unioned("docs/CHANGELOG.md", "## shed"),
        ];
        let described = vec![unioned("CHANGELOG.md", "## described")];
        let conflicted = ["RELEASE_NOTES.md".to_owned()];

        // When: the rebase says what it unioned.
        let unions = rebase_unions(shed, described, &conflicted);

        // Then: the re-describe's union stands for its path, the shed write's
        // stands where the rebase kept it, and the conflicted log is not named.
        assert_eq!(
            unions,
            [
                unioned("CHANGELOG.md", "## described"),
                unioned("docs/CHANGELOG.md", "## shed"),
            ]
        );
    }
}
