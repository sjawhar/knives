//! The release-edit verbs: `include`, `drop` and `advance`.
//!
//! Each is one jj sequence over the release's parent set rather than a report
//! with a renderer, so the decision lives beside the write. Membership is the
//! parent set; a branch joins or moves only through a verb here, and every
//! write records the parent set it left behind.

use knives::cli::Exit;
use knives::commands::release;
use knives::forge::github::CliForge;
use knives::ids::{BranchName, ReleaseScheme, RepoName};
use knives::ledger::{Draft, Kind, Ledger};
use knives::release_model::{
    MemberEvidence, MemberLookup, MemberSuccession, StackedHistoryContext, carried_from_tips,
    last_recorded_parents, member_parents, members_event_text, trunk_positions,
};

use super::{bookmark_tip, cut_request, parent_sources, scribe_for, selected, short12};

/// One deliberate change to the release in hand.
pub(crate) enum ReleaseEdit {
    Include {
        branch: String,
        why: Option<String>,
    },
    Drop {
        branch: String,
        why: String,
    },
    Advance {
        branches: Vec<BranchName>,
        from: Option<String>,
    },
}

impl ReleaseEdit {
    const fn verb(&self) -> &'static str {
        match self {
            Self::Include { .. } => "include",
            Self::Drop { .. } => "drop",
            Self::Advance { .. } => "advance",
        }
    }
}

/// What an edit decided: a new parent set to write with the delta that describes
/// it, or an already-reported end.
enum EditOutcome {
    Done(Vec<knives::ids::CommitId>, String),
    Settled(Exit),
}

/// The release in hand: a flat merge of feature and fix branches.
///
/// Every parent is a member. The upstream base is never a direct parent — it
/// is reachable through every member, because members fork from it — so there
/// is no role to classify and nothing for a landed member to be mistaken for.
struct ReleaseInHand {
    name: String,
    commit: knives::ids::CommitId,
    parents: Vec<knives::ids::CommitId>,
    /// Where the trunk sits, upstream and locally. The trunk is not a feature
    /// branch, so neither position is ever a member.
    trunk_tips: Vec<knives::ids::CommitId>,
}

impl ReleaseInHand {
    /// Read the release's parents: the members.
    fn read(
        opened: &knives::jj::Repo,
        entry: &knives::config::RepoEntry,
        name: String,
    ) -> anyhow::Result<Self> {
        let commit = opened.resolve_commit(&name)?;
        let parents: Vec<knives::ids::CommitId> = opened
            .parents_of(&name)?
            .into_iter()
            .map(|parent| parent.commit)
            .collect();
        Ok(Self {
            name,
            commit,
            parents,
            trunk_tips: trunk_positions(opened, entry)?,
        })
    }
}

/// Everything an edit reads: whose release, which repository, which release.
struct EditContext<'a> {
    repo: &'a RepoName,
    opened: &'a knives::jj::Repo,
    release: &'a ReleaseInHand,
    /// The release's last recorded parent set, from this repository's ledger.
    ///
    /// Ancestry and change ids cover a branch that grew or was rebased by jj. A
    /// branch rebased outside jj — `git rebase`, the forge's "update branch" —
    /// comes back with new commit ids AND new change ids, and a member that
    /// landed upstream is reached by every branch forked since; in both cases
    /// nothing in the repository ties the released parent to its branch name.
    /// The cut and edit events recorded that pairing each time the parent set
    /// was written, every bookmark at each parent, and it is what keeps
    /// `include` from carrying the branch twice and lets `advance` still move it.
    recorded: &'a [knives::ledger::RecordedParent],
    stacked: StackedHistoryContext<'a>,
}

impl EditContext<'_> {
    /// Refuse a branch whose history past the trunk carries a merge, saying so;
    /// `false` for a linear one.
    ///
    /// Membership is the parent set, and a parent that carries a release merge
    /// carries every member of that release: the cut is not flat however many
    /// parents it lists, and the plan would report the member the moment it got
    /// in. Refusing here is what keeps the plan from pointing at an `include`
    /// it would then flag.
    fn refuse_if_stacked(
        &self,
        branch: &str,
        tip: &knives::ids::CommitId,
        before: &str,
    ) -> anyhow::Result<bool> {
        let Some(stacked) = knives::release_model::stacked_history(self.stacked, branch, tip)?
        else {
            return Ok(false);
        };
        println!(
            "{}: {}; rebase it off the trunk before {before}",
            self.repo, stacked.detail
        );
        Ok(true)
    }

    /// Which current parents `branch`, at `tip`, continues, and on what evidence.
    fn member_parents(
        &self,
        branch: &str,
        tip: &knives::ids::CommitId,
    ) -> anyhow::Result<MemberLookup> {
        let succession = MemberSuccession::of(self.opened, &self.release.trunk_tips, tip)?;
        Ok(member_parents(
            &succession,
            &self.release.parents,
            self.recorded,
            branch,
        )?)
    }
}

/// Apply one stated change to each chosen repo's release in hand.
pub(crate) fn run_release_edit(
    name: &str,
    extra_consumers: &[&std::path::Path],
    change: &ReleaseEdit,
) -> anyhow::Result<Exit> {
    let chosen = match selected(Some(name), false)? {
        Ok(list) => list,
        Err(exit) => return Ok(exit),
    };
    let mut locals = extra_consumers
        .iter()
        .map(|path| path.to_path_buf())
        .collect::<Vec<_>>();
    locals.sort();
    locals.dedup();
    let mut worst = Exit::Ok;
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    for (repo, entry) in chosen {
        worst = worst.worst(edit_release(
            &repo,
            &entry,
            &locals,
            change,
            &forge,
            cache_root.as_deref(),
            &heads,
        )?);
    }
    Ok(worst)
}

/// Edit the release in hand: one change, nothing else moves.
///
/// The whole command is the jj sequence agents fumble — duplicate the release
/// onto the changed parent set, describe it, move its name — with the same pin
/// gate a rebase has. The duplicate preserves recorded conflict resolutions;
/// only the change itself can surface new conflicts, and they are reported.
#[allow(
    clippy::too_many_arguments,
    reason = "release-edit state and the shared consumer-scan collaborators are independently owned command inputs"
)]
fn edit_release(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    locals: &[std::path::PathBuf],
    change: &ReleaseEdit,
    forge: &dyn knives::consumer_pins::ConsumerPinSource,
    cache_root: Option<&std::path::Path>,
    heads: &knives::consumer_pins::ConsumerHeadMemo,
) -> anyhow::Result<Exit> {
    let opened = knives::jj::Repo::open(&entry.path)?;
    let consumers = release::ConsumerInputs {
        slugs: &entry.consumers,
        locals,
        forge,
        cache_root,
        heads,
    };
    let plan = release::plan(repo, entry, &consumers, &Ledger::for_repo(repo).entries()?)?;
    if !plan.problems.is_empty() {
        println!("{}", release::render(&plan));
        println!(
            "{repo}: {} not applied; the plan could not answer: {}",
            change.verb(),
            plan.problems.join("; ")
        );
        return Ok(Exit::Incomplete);
    }
    let Some(release_name) = plan.release.clone() else {
        println!("{repo}: no release to edit; cut one first");
        return Ok(Exit::Incomplete);
    };
    // Follows from who pins it, exactly as for a rebase: a consumer that follows
    // the branch sees the edit, one frozen on a revision does not. The way out
    // differs by scheme, because a fixed branch cannot take a dated name.
    if release::repair_effect(
        &plan.pins,
        knives::ids::BookmarkRef::parse(&release_name).branch(),
    ) == release::RepairEffect::NewDatedName
    {
        match entry.release_scheme() {
            ReleaseScheme::Dated => println!(
                "{repo}: every pin of {release_name} is frozen, so editing it would reach \
                 nobody; cut a new dated release first"
            ),
            ReleaseScheme::Fixed(_) => println!(
                "{repo}: every pin of {release_name} is frozen, so editing the fixed branch \
                 would reach nobody; update the frozen consumer pins, or change the release \
                 scheme before editing it (fixed branches cannot reach revision pins)"
            ),
        }
        return Ok(Exit::Incomplete);
    }
    // Fail closed: parents are classified against the upstream trunk, and an
    // unresolvable trunk would make every base look like a member — include
    // would refuse for the wrong reason, and drop or advance could touch the
    // base, which is `rebase`'s domain.
    if opened.resolve_commit(&entry.upstream_trunk()).is_err() {
        println!(
            "{repo}: cannot resolve {}; release edits classify parents against the upstream \
             trunk, so fetch upstream first",
            entry.upstream_trunk()
        );
        return Ok(Exit::Incomplete);
    }
    if !release_is_locally_movable(&opened, repo, &release_name)? {
        return Ok(Exit::Incomplete);
    }
    let release = ReleaseInHand::read(&opened, entry, release_name)?;
    let ledger = Ledger::for_repo(repo).entries()?;
    let releases = knives::release_model::release_refs_by_commit(
        &opened.bookmark_tips()?,
        &entry.release_scheme(),
        entry.publish_remote(),
    );
    let context = EditContext {
        repo,
        opened: &opened,
        release: &release,
        recorded: last_recorded_parents(&ledger, &release.name),
        stacked: StackedHistoryContext {
            repo: &opened,
            trunks: &release.trunk_tips,
            releases: &releases,
        },
    };
    let outcome = match change {
        ReleaseEdit::Include { branch, why } => include_edit(&context, branch, why.as_deref())?,
        ReleaseEdit::Drop { branch, why } => drop_edit(&context, branch, why)?,
        ReleaseEdit::Advance { branches, from } => {
            advance_edit(&context, entry, branches, from.as_deref())?
        }
    };
    let (new_parents, delta) = match outcome {
        EditOutcome::Settled(exit) => return Ok(exit),
        EditOutcome::Done(parents, delta) => (parents, delta),
    };
    apply_edit(&context, entry, &new_parents, &delta)
}

/// Write the edited release — duplicated onto its new parent set, described,
/// its name moved — record the parent set in the ledger, and report.
fn apply_edit(
    context: &EditContext<'_>,
    entry: &knives::config::RepoEntry,
    new_parents: &[knives::ids::CommitId],
    delta: &str,
) -> anyhow::Result<Exit> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    // Built through `cut_request` so an edited release's description reads exactly
    // like a fresh cut's, from the same (source, commit) pairs.
    let provenance = parent_sources(opened, entry, &entry.release_scheme(), new_parents)?;
    let message = format!(
        "{}\n\n{delta}",
        cut_request(release.name.clone(), &provenance).message()
    );
    let created = knives::jj::write_release(
        &entry.path,
        &knives::jj::ReleaseWrite {
            source: Some(&release.commit),
            parents: new_parents,
            message: Some(&message),
            bookmark: Some(&release.name),
            operation: &format!("knives: {}: {delta}", release.name),
        },
    )?;
    record_edit_event(
        repo,
        entry,
        opened,
        &EditRecord {
            release: &release.name,
            delta,
            created: &created,
            provenance: &provenance,
        },
    )?;
    println!(
        "{repo}: {} now has {} parent(s): {delta}",
        release.name,
        new_parents.len()
    );
    match knives::jj::conflicted_files(&entry.path, created.as_str()) {
        Ok(files) => println!("{}", release::conflict_guidance(&files)),
        Err(error) => println!("  could not list conflicts: {error}"),
    }
    Ok(Exit::Ok)
}

/// What one release edit wrote: the name, the delta, the new merge and its
/// parent set with the branch each parent came from.
pub(crate) struct EditRecord<'a> {
    pub(crate) release: &'a str,
    pub(crate) delta: &'a str,
    pub(crate) created: &'a knives::ids::CommitId,
    pub(crate) provenance: &'a [(String, knives::ids::CommitId)],
}

/// The edit's parent set goes to the ledger the way a cut's does, so a later
/// `advance` or `include` can still tell which parent is which branch after a
/// rebase that left no ancestry or change id behind.
pub(crate) fn record_edit_event(
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
    opened: &knives::jj::Repo,
    record: &EditRecord<'_>,
) -> anyhow::Result<()> {
    let parents: Vec<knives::ids::CommitId> = record
        .provenance
        .iter()
        .map(|(_, commit)| commit.clone())
        .collect();
    scribe_for(repo, entry)?.record(&Draft {
        subject: Some(record.release),
        kind: Kind::Event,
        disposition: None,
        text: format!(
            "edited {}: {}; parents: {}",
            record.release,
            record.delta,
            members_event_text(record.provenance)
        ),
        evidence: std::iter::once(record.created.as_str().to_owned())
            .chain(parents.iter().map(|commit| commit.as_str().to_owned()))
            .collect(),
        pr: None,
        parents: recorded_parents(opened, entry, &parents)?,
    })?;
    Ok(())
}

/// Each parent with every branch at it now, for a cut or edit event's record.
pub(crate) fn recorded_parents(
    opened: &knives::jj::Repo,
    entry: &knives::config::RepoEntry,
    parents: &[knives::ids::CommitId],
) -> anyhow::Result<Vec<knives::ledger::RecordedParent>> {
    Ok(knives::release_model::parents_with_branches(
        &opened.bookmark_tips()?,
        entry.trunk(),
        &entry.release_scheme(),
        parents,
    ))
}

/// Whether the release name has one local position to move, saying why not when
/// it has none.
///
/// Editing and rebasing both move a local bookmark. A release held only as a
/// remote-tracking ref — what a fetch of somebody else's cut leaves, since jj
/// creates no local bookmark for an untracked remote one — and one whose local
/// bookmark is divergent both lack a single local position. jj rejects
/// `name@remote` as a bookmark name outright, and it did so only after the
/// duplicate had been made and described.
pub(crate) fn release_is_locally_movable(
    opened: &knives::jj::Repo,
    repo: &RepoName,
    name: &str,
) -> anyhow::Result<bool> {
    if bookmark_tip(opened, name)?.is_some() {
        return Ok(true);
    }
    match knives::ids::BookmarkRef::parse(name) {
        knives::ids::BookmarkRef::Remote { branch, remote } => println!(
            "{repo}: {name} is here only as a remote ref, so there is no local bookmark to \
             move; `jj bookmark track {branch}@{remote}` first"
        ),
        knives::ids::BookmarkRef::Local(_) => println!(
            "{repo}: {name} has no single local position, so there is nothing to move; \
             resolve its divergence first"
        ),
    }
    Ok(false)
}

fn include_edit(
    context: &EditContext<'_>,
    target: &str,
    why: Option<&str>,
) -> anyhow::Result<EditOutcome> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    let tip = bookmark_tip(opened, target)?.or_else(|| opened.resolve_commit(target).ok());
    let Some(tip) = tip else {
        println!("{repo}: {target} is neither a local bookmark nor a resolvable revision");
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    };
    if release.parents.contains(&tip) {
        println!("{repo}: {} already carries {target}", release.name);
        return Ok(EditOutcome::Settled(Exit::Ok));
    }
    if opened.is_ancestor(&release.commit, &tip)? {
        // Built on top of the release, not off the trunk. Including it would put
        // the cut in its own successor's ancestry, and it carries the whole
        // release with it rather than one branch's work.
        println!(
            "{repo}: {target} is stacked on {}, so it is not a member of it; rebase it off \
             the trunk to include it",
            release.name
        );
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    if opened.is_ancestor(&tip, &release.commit)? {
        // Reachable through another parent's history — a stacked branch, or one
        // whose content landed in the base. Membership is the parent set, so
        // this is not a member; say which situation holds rather than
        // pretending the include happened.
        println!(
            "{repo}: {target}'s content is already in {} through another parent's history; \
             it is not a member parent itself, and whichever parent carries it represents it",
            release.name
        );
        return Ok(EditOutcome::Settled(Exit::Ok));
    }
    if release.trunk_tips.contains(&tip) {
        // A release is a flat merge of feature and fix branches; upstream enters
        // through the members' bases, never as a parent. Checked before the
        // member scan below, because a member that landed upstream is an
        // ancestor of the trunk and would otherwise answer for it.
        println!(
            "{repo}: {target} is the trunk, not a feature or fix branch; {} carries \
             upstream through its members' bases, never as a parent",
            release.name
        );
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    if context.refuse_if_stacked(target, &tip, "including it")? {
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    let lookup = context.member_parents(target, &tip)?;
    if let Some(parent) = lookup.parents.first() {
        // Moving a member is its own decision, and including it again would
        // carry it twice; say which situation the record describes.
        let parent = short12(parent);
        match lookup.evidence {
            MemberEvidence::Succession => println!(
                "{repo}: {} carries {parent} of {target}, and the branch has moved on (grown or \
                 rebased); moving a member is its own decision: `knives release advance \
                 {target}`",
                release.name
            ),
            MemberEvidence::Record => println!(
                "{repo}: {} carries {target} as {parent} per its last cut or edit, and the \
                 branch has moved on without ancestry or change ids back to it (rebased \
                 outside jj?); including it again would carry it twice: `knives release \
                 advance {target}` moves it through that record",
                release.name
            ),
            MemberEvidence::LandedRecord => println!(
                "{repo}: {} carries {target} as {parent} per its last cut or edit, and \
                 {parent} has landed upstream; including the branch again would carry it \
                 twice: `knives release advance {target}` moves the member to the branch's \
                 tip, `knives release rebase` retires the landed parent",
                release.name
            ),
        }
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    let mut parents = release.parents.clone();
    parents.push(tip);
    let why = why.map_or_else(String::new, |why| format!(" ({why})"));
    let delta = format!("included {target}{why}");
    Ok(EditOutcome::Done(parents, delta))
}

/// Remove one member parent. The branch and its bookmark are untouched.
fn drop_edit(context: &EditContext<'_>, target: &str, why: &str) -> anyhow::Result<EditOutcome> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    let mut candidates = Vec::new();
    if let Some(tip) = bookmark_tip(opened, target)? {
        if release.parents.contains(&tip) {
            // The bookmark sits exactly on a parent: that parent is the branch.
            candidates.push(tip);
        } else {
            // Succession, then the record, for a branch that has advanced past or
            // been rebased off its released parent — outside jj too. Succession
            // can be ambiguous — a parent whose history the branch shares also
            // matches — and ambiguity refuses below.
            candidates.extend(context.member_parents(target, &tip)?.parents);
        }
    } else if let Ok(commit) = opened.resolve_commit(target) {
        if release.parents.contains(&commit) {
            candidates.push(commit);
        }
    } else {
        println!("{repo}: {target} is neither a local bookmark nor a resolvable revision");
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    match candidates.as_slice() {
        [] => {
            println!(
                "{repo}: {} carries no parent of {target}; name the parent's commit id",
                release.name
            );
            Ok(EditOutcome::Settled(Exit::Incomplete))
        }
        [parent] => {
            let mut parents = release.parents.clone();
            parents.retain(|kept| kept != parent);
            // A drop only removes the parent; whether the content survives
            // depends on the remaining members' ancestry. Losing it is often
            // the point (a bad fix) and sometimes a surprise (a landed pull
            // the members' bases do not reach yet), so the fact is stated
            // rather than judged.
            let mut carried_elsewhere = false;
            for kept in &parents {
                if opened.is_ancestor(parent, kept)? {
                    carried_elsewhere = true;
                    break;
                }
            }
            if !carried_elsewhere {
                println!(
                    "{repo}: no remaining member carries {target}'s content; the release \
                     loses it"
                );
            }
            let delta = format!("dropped {target}: {why}");
            Ok(EditOutcome::Done(parents, delta))
        }
        many => {
            let listed: Vec<String> = many.iter().map(short12).collect();
            println!(
                "{repo}: {target} matches {} parents of {} ({}); name one by commit id",
                many.len(),
                release.name,
                listed.join(", ")
            );
            Ok(EditOutcome::Settled(Exit::Incomplete))
        }
    }
}

/// Move member parents to their branches' current tips, and only those named.
///
/// Bare `advance` is parent-driven: every member whose branch moved on. Named
/// branches are strict — asking to advance something that is not a member is
/// answered, not improvised around. Both work from the same population, the
/// branches a release can carry: the trunk and our release names are not among
/// them, and a member advanced onto either would leave the release carrying a
/// second base or its own predecessor.
fn advance_edit(
    context: &EditContext<'_>,
    entry: &knives::config::RepoEntry,
    branches: &[BranchName],
    from: Option<&str>,
) -> anyhow::Result<EditOutcome> {
    let tips = context.opened.bookmark_tips()?;
    let carried = carried_from_tips(&tips, entry.trunk(), &entry.release_scheme());
    let outcome = if let Some(from) = from {
        let [branch] = branches else {
            println!(
                "{}: --from names the old commit one branch replaces; give exactly one branch",
                context.repo
            );
            return Ok(EditOutcome::Settled(Exit::Usage));
        };
        let Ok(old) = context.opened.resolve_commit(from) else {
            println!("{}: cannot resolve {from}", context.repo);
            return Ok(EditOutcome::Settled(Exit::Incomplete));
        };
        advance_named_member_from(context, &carried, branch, &old)?
    } else if branches.is_empty() {
        advance_every_member(context, &carried)?
    } else {
        advance_named_members(context, &carried, branches)?
    };
    let Some((parents, moved)) = outcome else {
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    };
    if moved.is_empty() {
        // Only a bare advance looked at every member, so only it can say so; a
        // named advance has already reported each branch it found at its tip.
        if branches.is_empty() {
            println!(
                "{}: every member of {} is at its branch tip",
                context.repo, context.release.name
            );
        }
        return Ok(EditOutcome::Settled(Exit::Ok));
    }
    // Stacked members advanced to one tip collapse into a single parent.
    let mut deduped = Vec::new();
    for parent in parents {
        if !deduped.contains(&parent) {
            deduped.push(parent);
        }
    }
    let delta = format!("advanced {}", moved.join(", "));
    Ok(EditOutcome::Done(deduped, delta))
}

/// Advance every member whose branch has moved on. `None` is a refusal that
/// has already said why.
///
/// Two-phase: every move and every ambiguity is found first, and any ambiguity
/// refuses the whole edit. A bare `advance` promises "every advanced member",
/// and advancing some while skipping others would deliver a composition nobody
/// asked for while reporting success.
fn advance_every_member(
    context: &EditContext<'_>,
    carried: &[(String, knives::ids::CommitId)],
) -> anyhow::Result<Option<(Vec<knives::ids::CommitId>, Vec<String>)>> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    // A branch built on top of the release descends from every member, so
    // ancestry alone would call it their advanced tip. Advancing onto one folds
    // work nobody included into the release and puts the cut in its own
    // successor's ancestry, so it is not a candidate at all.
    let mut off_release: Vec<(String, knives::ids::CommitId)> = Vec::new();
    for (branch, tip) in carried {
        if opened.is_ancestor(&release.commit, tip)? {
            continue;
        }
        if context.refuse_if_stacked(branch, tip, "advancing onto it")? {
            continue;
        }
        off_release.push((branch.clone(), tip.clone()));
    }
    let successions = off_release
        .iter()
        .map(|(_, tip)| MemberSuccession::of(opened, &release.trunk_tips, tip))
        .collect::<Result<Vec<_>, _>>()?;
    let mut parents = release.parents.clone();
    let mut advances: Vec<(usize, String, knives::ids::CommitId)> = Vec::new();
    let mut ambiguous: Vec<String> = Vec::new();
    for (index, parent) in parents.iter().enumerate() {
        let mut successors = Vec::new();
        for ((branch, tip), succession) in off_release.iter().zip(&successions) {
            if succession.succeeds(parent)? {
                successors.push((branch.clone(), tip.clone()));
            }
        }
        match successors.as_slice() {
            [] => {}
            [(branch, tip)] => advances.push((index, branch.clone(), tip.clone())),
            many => {
                let names: Vec<String> = many.iter().map(|(branch, _)| branch.clone()).collect();
                ambiguous.push(format!(
                    "parent {} has several advanced branches ({})",
                    short12(parent),
                    names.join(", ")
                ));
            }
        }
    }
    if !ambiguous.is_empty() {
        for line in &ambiguous {
            println!("{repo}: {line}");
        }
        println!("{repo}: nothing advanced; advance the ambiguous members by name");
        return Ok(None);
    }
    // The mirror image of the ambiguity above: one branch found as the sole
    // successor of more than one stale parent is not evidence it replaced all
    // of them. A branch stacked across several former members' tips (an
    // integration branch built across them, or a member rebuilt with `jj
    // duplicate` that left an unrelated branch still reachable from its old
    // tip) satisfies the ancestry check for each parent individually, and
    // deduping the result would silently fold distinct members into one,
    // discarding a match nobody asked for. Refuse instead of guessing.
    let mut overreaching: Vec<(String, Vec<knives::ids::CommitId>)> = Vec::new();
    for (index, branch, _tip) in &advances {
        let Some(stale) = parents.get(*index).cloned() else {
            continue;
        };
        match overreaching.iter_mut().find(|entry| &entry.0 == branch) {
            Some(entry) => entry.1.push(stale),
            None => overreaching.push((branch.clone(), vec![stale])),
        }
    }
    overreaching.retain(|(_, stale_parents)| stale_parents.len() > 1);
    if !overreaching.is_empty() {
        for (branch, stale_parents) in &overreaching {
            let listed: Vec<String> = stale_parents.iter().map(short12).collect();
            println!(
                "{repo}: {branch} descends from {} parents of {} ({}); drop and include instead",
                stale_parents.len(),
                release.name,
                listed.join(", ")
            );
        }
        println!("{repo}: nothing advanced; one candidate cannot silently replace several members");
        return Ok(None);
    }
    let mut moved: Vec<String> = Vec::new();
    for (index, branch, tip) in advances {
        if let Some(slot) = parents.get_mut(index) {
            *slot = tip;
        }
        moved.push(branch);
    }
    Ok(Some((parents, moved)))
}

/// Advance exactly the named branches, refusing anything unanswerable: `None`
/// is a refusal that has already said why.
fn advance_named_members(
    context: &EditContext<'_>,
    carried: &[(String, knives::ids::CommitId)],
    branches: &[BranchName],
) -> anyhow::Result<Option<(Vec<knives::ids::CommitId>, Vec<String>)>> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    let mut parents = release.parents.clone();
    let mut moved: Vec<String> = Vec::new();
    for branch in branches {
        let named = carried
            .iter()
            .find(|(carried, _)| carried == branch.as_str())
            .map(|(_, tip)| tip.clone());
        let Some(tip) = named else {
            if bookmark_tip(opened, branch.as_str())?.is_some() {
                // The trunk and our release names are the two bookmarks a release
                // can never carry, and naming one here reached the same mover the
                // bare advance deliberately keeps them away from.
                println!(
                    "{repo}: {branch} is the trunk or a release name, so it is never a member \
                     of {}",
                    release.name
                );
            } else {
                println!("{repo}: no local bookmark named {branch}");
            }
            return Ok(None);
        };
        if parents.contains(&tip) {
            println!("{repo}: {branch} is already at its tip in {}", release.name);
            continue;
        }
        if opened.is_ancestor(&release.commit, &tip)? {
            println!(
                "{repo}: {branch} is stacked on {}, so advancing a member onto it would fold \
                 in work nobody included and put the cut in its own ancestry",
                release.name
            );
            return Ok(None);
        }
        if context.refuse_if_stacked(branch.as_str(), &tip, "advancing onto it")? {
            return Ok(None);
        }
        // Matched by commit rather than by position, so no index invariant has to
        // hold and the count and the listing below cannot disagree. Succession
        // covers a grown branch and a rebased one alike; the last cut or edit's
        // record covers a branch rebased outside jj or landed upstream, where
        // nothing but the name the record wrote down ties them. Said out loud
        // in both cases, since a bookmark name reused for unrelated work would
        // be moved onto that member.
        let lookup = context.member_parents(branch.as_str(), &tip)?;
        match (lookup.parents.first(), lookup.evidence) {
            (Some(recorded), MemberEvidence::Record) => println!(
                "{repo}: {branch} matched {} by the last cut or edit record only; nothing in \
                 the repository ties them (no ancestry, no shared change id)",
                short12(recorded)
            ),
            (Some(recorded), MemberEvidence::LandedRecord) => println!(
                "{repo}: {branch} matched {} by the last cut or edit record; that parent has \
                 landed upstream, so the branch's tip continues it only by name",
                short12(recorded)
            ),
            (_, MemberEvidence::Succession) | (None, _) => {}
        }
        match lookup.parents.as_slice() {
            [] => {
                println!(
                    "{repo}: {} carries no parent of {branch}; `knives release include \
                     {branch}` adds it",
                    release.name
                );
                return Ok(None);
            }
            [parent] => {
                for slot in parents.iter_mut().filter(|slot| **slot == *parent) {
                    *slot = tip.clone();
                }
                moved.push(branch.to_string());
            }
            many => {
                let listed: Vec<String> = many.iter().map(short12).collect();
                println!(
                    "{repo}: {branch} descends from {} parents of {} ({}); drop and \
                     include instead",
                    many.len(),
                    release.name,
                    listed.join(", ")
                );
                return Ok(None);
            }
        }
    }
    Ok(Some((parents, moved)))
}

/// Advance one named branch onto the parent explicitly given by `old`,
/// bypassing the ancestry search entirely.
///
/// Ancestry is the right default: it survives an ordinary `jj rebase`, which
/// keeps a commit's descendants reachable from it. It cannot survive a `jj
/// duplicate` rebuild -- routine in these repos precisely because `jj rebase
/// -s` drags in whatever else is stacked on the branch -- which produces a new
/// change id sharing no ancestry with the commit it replaces. Without this,
/// the only way forward is `drop` then `include`, which loses the release's
/// recorded resolution for that member and rebuilds it from scratch.
fn advance_named_member_from(
    context: &EditContext<'_>,
    carried: &[(String, knives::ids::CommitId)],
    branch: &BranchName,
    old: &knives::ids::CommitId,
) -> anyhow::Result<Option<(Vec<knives::ids::CommitId>, Vec<String>)>> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    let Some(tip) = carried
        .iter()
        .find(|(carried, _)| carried == branch.as_str())
        .map(|(_, tip)| tip.clone())
    else {
        if bookmark_tip(opened, branch.as_str())?.is_some() {
            println!(
                "{repo}: {branch} is the trunk or a release name, so it is never a member \
                 of {}",
                release.name
            );
        } else {
            println!("{repo}: no local bookmark named {branch}");
        }
        return Ok(None);
    };
    if opened.is_ancestor(&release.commit, &tip)? {
        println!(
            "{repo}: {branch} is stacked on {}, so advancing a member onto it would fold in \
             work nobody included and put the cut in its own ancestry",
            release.name
        );
        return Ok(None);
    }
    if context.refuse_if_stacked(branch.as_str(), &tip, "advancing onto it")? {
        return Ok(None);
    }
    let mut parents = release.parents.clone();
    if parents.contains(&tip) {
        println!("{repo}: {branch} is already at its tip in {}", release.name);
        return Ok(Some((parents, Vec::new())));
    }
    let Some(index) = parents.iter().position(|parent| parent == old) else {
        println!(
            "{repo}: {} is not a parent of {}; `knives release include {branch}` adds \
             {branch} instead",
            short12(old),
            release.name
        );
        return Ok(None);
    };
    if let Some(slot) = parents.get_mut(index) {
        *slot = tip;
    }
    Ok(Some((parents, vec![branch.to_string()])))
}
