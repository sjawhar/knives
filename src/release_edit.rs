//! The release-edit verbs: `include`, `drop` and `advance`.
//!
//! Each is one jj sequence over the release's parent set rather than a report
//! with a renderer, so the decision lives beside the write. Membership is the
//! parent set; a branch joins or moves only through a verb here, and every
//! write records the parent set it left behind.

use knives::bind::Fork;
use knives::cli::Exit;
use knives::commands::claim::Bound;
use knives::commands::release;
use knives::forge::github::CliForge;
use knives::ids::{BranchName, CommitId, ReleaseScheme, RepoName};
use knives::ledger::{Draft, Ledger, inline_human_text, shell_word};
use knives::release_model::{
    EarlierWrites, MemberEvidence, MemberHistory, MemberLookup, MemberSuccession, SeriesFit,
    StackedHistoryContext, carried_from_tips, carries_member_line, last_recorded_parents,
    member_history, member_parents, members_event_text, read_release, series_fit,
    trunk_positions_in,
};

use super::scribe_for;

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

/// What an edit decided: a new parent set to write, or an already-reported
/// end.
enum EditOutcome {
    Done(EditDone),
    Settled(Exit),
}

/// A new parent set to write, with the delta that describes it
/// ([`apply_edit`]).
struct EditDone {
    parents: Vec<knives::ids::CommitId>,
    delta: String,
    /// The target of a drop whose parent no remaining member's history
    /// carries: whether the release loses its content is read from the
    /// written tree ([`apply_edit`]).
    uncarried: Option<String>,
    /// Each parent the edit brings in ([`BroughtParent`]).
    brought: Vec<BroughtParent>,
}

/// A parent an edit brings in, with the parent it replaces for an advance:
/// what spec R11's check reads as brought in ([`knives::jj::Brought`]).
type BroughtParent = (CommitId, Option<CommitId>);

/// The release in hand: a flat merge of feature and fix branches.
///
/// Every parent is a member, except an earlier write of the release a
/// fast-forward push gave it, a carrier, which no new composition keeps. The
/// upstream base is never a direct parent — it is reachable through every
/// member, because members fork from it — so there is no role to classify
/// and nothing for a landed member to be mistaken for.
struct ReleaseInHand {
    name: String,
    commit: knives::ids::CommitId,
    /// The members: the release's parents, its carriers left out.
    parents: Vec<knives::ids::CommitId>,
    /// The parents that are earlier writes of the release
    /// ([`knives::release_model::EarlierWrites::carriers`]).
    carriers: Vec<knives::ids::CommitId>,
    /// Every earlier write of the name, which the write hides once it leaves
    /// one a head nothing holds.
    writes: EarlierWrites,
    /// Where the trunk sits, upstream and locally. The trunk is not a feature
    /// branch, so neither position is ever a member.
    trunk_tips: Vec<knives::ids::CommitId>,
}

impl ReleaseInHand {
    /// Read the release's members and carriers, the carriers from the
    /// ledger's writes of the name and `published`, the publish remote's
    /// commit of it in jj's view; and its trunk positions from `tips`, every
    /// bookmark the caller read.
    fn read(
        opened: &knives::jj::Repo,
        entry: &knives::config::RepoEntry,
        name: String,
        (ledger, tips, published): (
            &[knives::ledger::Entry],
            &knives::detect::BookmarkTips,
            Option<&knives::ids::CommitId>,
        ),
    ) -> anyhow::Result<Self> {
        let commit = opened.resolve_commit(&name)?;
        let writes = EarlierWrites::read(opened, &name, ledger, published)?;
        let reading = read_release(opened, (&writes, &commit))?;
        Ok(Self {
            name,
            commit,
            parents: reading.members,
            carriers: reading.carriers,
            writes,
            trunk_tips: trunk_positions_in(tips, entry),
        })
    }

    /// The release's name for a terminal line: a name read from the
    /// repository, which a remote can give a C1 character.
    fn shown_name(&self) -> String {
        inline_human_text(&self.name)
    }
}

/// Everything an edit reads: whose release, which repository, which release,
/// and where the editor stands.
struct EditContext<'a> {
    repo: &'a RepoName,
    opened: &'a knives::jj::Repo,
    release: &'a ReleaseInHand,
    /// The registry entry, bound to the series this command acts on.
    entry: &'a knives::config::RepoEntry,
    /// The fork the current directory is inside, by the name its claims are
    /// kept under; the ledger event's author is derived from it.
    bound: Bound<'a>,
    /// The mark the edit's ledger event ends with when knives could not check
    /// the release in hand against its recorded writes
    /// ([`release::unchecked_suffix`]); empty otherwise.
    unchecked: &'a str,
    /// The reason `--allow-loss` gave: with it, an edit that spec R11's
    /// check refuses is written, and its loss recorded ([`apply_edit`]).
    allow_loss: Option<&'a str>,
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
    /// Refuse, saying why, a branch that belongs to another release series or
    /// to none; `false` for a member of the selected series. An entry without
    /// named series has one series, so every branch belongs to it.
    fn refuse_if_other_series(
        &self,
        branch: &str,
        tip: &knives::ids::CommitId,
        before: &str,
    ) -> anyhow::Result<bool> {
        if self.entry.series.is_empty() {
            return Ok(false);
        }
        match series_fit(self.opened, self.entry, tip)? {
            SeriesFit::Selected => Ok(false),
            SeriesFit::Other(label) => {
                println!(
                    "{}: {} forks from the base of the {label}, not {}'s; \
                     refusing {before}: `knives release --series <name>` acts on its series",
                    self.repo,
                    inline_human_text(branch),
                    self.release.shown_name()
                );
                Ok(true)
            }
            SeriesFit::None(why) => {
                // `why` names repos.toml bases and commit ids (`series_fit`).
                println!(
                    "{}: {} belongs to no release series: {why}; refusing {before}",
                    self.repo,
                    inline_human_text(branch)
                );
                Ok(true)
            }
        }
    }

    /// Refuse, saying why, a branch whose history past the trunk carries more
    /// than its own line: `None` after refusing. Otherwise whether its history
    /// carries merges whose side lines the release's other members already
    /// carry, said once when it does. `replacing` are the parents the branch
    /// would replace: they do not count as carried, exactly as the plan
    /// measures a member against the others once it is in.
    ///
    /// Membership is the parent set, and a parent that carries a release merge
    /// carries every member of that release: the cut is not flat however many
    /// parents it lists, and the plan would report the member the moment it got
    /// in. Refusing here is what keeps the plan from pointing at an `include`
    /// it would then flag. A merge of lines the release already carries brings
    /// nothing but the branch's own commits ([`member_history`]), so it is
    /// admitted and named.
    fn admit_history(
        &self,
        branch: &str,
        tip: &knives::ids::CommitId,
        (before, replacing): (&str, &[knives::ids::CommitId]),
    ) -> anyhow::Result<Option<bool>> {
        let carried: Vec<knives::ids::CommitId> = self
            .stacked
            .carried
            .iter()
            .filter(|parent| !replacing.contains(parent))
            .cloned()
            .collect();
        let context = StackedHistoryContext {
            carried: &carried,
            replaced: replacing.first(),
            ..self.stacked
        };
        match member_history(context, branch, tip)? {
            MemberHistory::Linear => Ok(Some(false)),
            MemberHistory::CarriedMerges { merges, own } => {
                let listed: Vec<&str> = merges.iter().map(knives::ids::CommitId::short).collect();
                println!(
                    "{}: {}'s history carries merge(s) {} whose other lines {} already \
                     carries; it brings only its own {own} commit(s)",
                    self.repo,
                    inline_human_text(branch),
                    listed.join(", "),
                    self.release.shown_name()
                );
                Ok(Some(true))
            }
            MemberHistory::Stacked(stacked) => {
                println!(
                    "{}: {}; rebase it off the trunk before {before}",
                    self.repo,
                    inline_human_text(&stacked.detail)
                );
                Ok(None)
            }
        }
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

    /// Whether `branch`, at `tip`, builds on the members it descends from and
    /// so joins beside them through `include`
    /// ([`knives::release_model::integrates`]).
    fn integrates(
        &self,
        branch: &str,
        tip: &knives::ids::CommitId,
        lookup: &MemberLookup,
    ) -> anyhow::Result<bool> {
        Ok(knives::release_model::integrates(
            self.stacked,
            self.recorded,
            lookup,
            (branch, tip),
        )?)
    }

    /// Whether a bare `advance` takes `branch`, at `tip`, as a candidate. A
    /// branch carrying more than its own line is refused, and one whose merges
    /// the release already carries is left to the verb named for it: a bare
    /// advance would read a branch that merges several members as the
    /// successor of each.
    fn bare_advance_candidate(
        &self,
        branch: &str,
        tip: &knives::ids::CommitId,
    ) -> anyhow::Result<bool> {
        let (repo, release) = (self.repo, self.release);
        match member_history(self.stacked, branch, tip)? {
            MemberHistory::Linear => Ok(true),
            MemberHistory::CarriedMerges { .. } => {
                if !release.parents.contains(tip) {
                    let lookup = self.member_parents(branch, tip)?;
                    let replaced = (!self.integrates(branch, tip, &lookup)?)
                        .then(|| {
                            knives::release_model::replaced_member(self.recorded, &lookup, branch)
                        })
                        .flatten();
                    if let Some(member) = replaced
                        && let MemberHistory::Stacked(stacked) =
                            carries_member_line(self.stacked, member, (branch, tip))?
                    {
                        println!(
                            "{repo}: {}; rebase it off the trunk before advancing onto it",
                            inline_human_text(&stacked.detail)
                        );
                        return Ok(false);
                    }
                    let verb = if replaced.is_some() {
                        "advance"
                    } else {
                        "include"
                    };
                    println!(
                        "{repo}: {}'s history carries merges of lines {} already \
                         carries; a bare advance leaves it alone: {verb} it by name",
                        inline_human_text(branch),
                        release.shown_name()
                    );
                }
                Ok(false)
            }
            MemberHistory::Stacked(stacked) => {
                println!(
                    "{repo}: {}; rebase it off the trunk before advancing onto it",
                    inline_human_text(&stacked.detail)
                );
                Ok(false)
            }
        }
    }

    /// The members a named `advance` of `branch`, at `tip`, would move, or
    /// `None` after refusing and saying why.
    ///
    /// A branch carrying more than its own line is refused before anything
    /// reads its parents, so no message points at a verb that would refuse it
    /// too; one that builds on the members it merges is sent to `include`.
    /// Matched by commit rather than by position, so no index invariant has to
    /// hold. Succession covers a grown branch and a rebased one alike; the last
    /// cut or edit's record covers a branch rebased outside jj or landed
    /// upstream, where nothing but the name the record wrote down ties them.
    /// Said out loud in both cases, since a bookmark name reused for unrelated
    /// work would be moved onto that member.
    fn named_advance_lookup(
        &self,
        branch: &str,
        tip: &knives::ids::CommitId,
    ) -> anyhow::Result<Option<MemberLookup>> {
        let (repo, release) = (self.repo, self.release);
        if let MemberHistory::Stacked(stacked) = member_history(self.stacked, branch, tip)? {
            println!(
                "{repo}: {}; rebase it off the trunk before advancing onto it",
                inline_human_text(&stacked.detail)
            );
            return Ok(None);
        }
        let lookup = self.member_parents(branch, tip)?;
        if self.refuse_if_trunk_has(branch, tip, lookup.parents.first())? {
            return Ok(None);
        }
        let shown_branch = inline_human_text(branch);
        match (lookup.parents.first(), lookup.evidence) {
            (Some(recorded), MemberEvidence::Record) => println!(
                "{repo}: {shown_branch} matched {} by the last cut or edit record only; nothing \
                 in the repository ties them (no ancestry, no shared change id)",
                recorded.short()
            ),
            (Some(recorded), MemberEvidence::LandedRecord) => println!(
                "{repo}: {shown_branch} matched {} by the last cut or edit record; that parent \
                 has landed upstream, so the branch's tip continues it only by name",
                recorded.short()
            ),
            (_, MemberEvidence::Succession) | (None, _) => {}
        }
        if self.integrates(branch, tip, &lookup)? {
            let listed: Vec<&str> = lookup.parents.iter().map(CommitId::short).collect();
            println!(
                "{repo}: {shown_branch} merges {} member(s) of {} ({}) and builds on them; \
                 `knives release include {}` adds it beside them",
                lookup.parents.len(),
                release.shown_name(),
                listed.join(", "),
                shell_word(branch)
            );
            return Ok(None);
        }
        Ok(Some(lookup))
    }

    /// Refuse, saying why, a tip the trunk already reaches.
    ///
    /// A commit the trunk has is a base, never a member: taken as a parent it
    /// gives the release a legacy cut's shape and moves its shared base under
    /// every other member, and the next rebase retires it as redundant.
    /// `carried` is the parent of the branch the release holds, when the record
    /// names one — then it is the landed parent a rebase retires.
    fn refuse_if_trunk_has(
        &self,
        branch: &str,
        tip: &knives::ids::CommitId,
        carried: Option<&knives::ids::CommitId>,
    ) -> anyhow::Result<bool> {
        if !knives::release_model::trunk_reaches(self.opened, &self.release.trunk_tips, tip)? {
            return Ok(false);
        }
        let shown_branch = inline_human_text(branch);
        match carried {
            None => println!(
                "{}: the trunk already has {shown_branch}, tip included, so it is not a member \
                 of {}: a parent the trunk reaches is a base, and taking it as one would move \
                 the shared base; `knives release rebase` onto a trunk that has it brings it in",
                self.repo,
                self.release.shown_name()
            ),
            Some(parent) => println!(
                "{}: the trunk already has the whole of {shown_branch}, past the {} that {} \
                 carries for it; moving the member onto a tip the trunk reaches would make it a \
                 base parent; `knives release rebase` retires the landed parent and brings the \
                 rest in",
                self.repo,
                parent.short(),
                self.release.shown_name()
            ),
        }
        Ok(true)
    }
}

/// Apply one stated change to the fork's release in hand, with the reason
/// `--allow-loss` gave, where it gave one ([`apply_edit`]).
pub(crate) fn run_release_edit(
    fork: &Fork<'_>,
    extra_consumers: &[&std::path::Path],
    change: (&ReleaseEdit, Option<&str>),
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let mut locals = extra_consumers
        .iter()
        .map(|path| path.to_path_buf())
        .collect::<Vec<_>>();
    locals.sort();
    locals.dedup();
    let forge = CliForge;
    let cache_root = knives::forge_cache::cache_root();
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    edit_release(
        fork,
        &locals,
        change,
        &forge,
        cache_root.as_deref(),
        &heads,
        bound,
    )
}

/// Why a published dated release must not move ([`release::published_release`]),
/// as the line to print after the repo name, or `None`. `verb` is the gerund
/// and the imperative of the move refused.
fn published_refusal(
    opened: &knives::jj::Repo,
    ledger: &[knives::ledger::Entry],
    release: release::ReleaseAt<'_>,
    verb: (&str, &str),
) -> anyhow::Result<Option<String>> {
    Ok(release::published_release(opened, ledger, release)?
        .map(|evidence| published_refusal_text(release.name, &evidence, verb)))
}

/// [`published_refusal`]'s line for the release `name`, which `evidence` marks
/// published.
fn published_refusal_text(name: &str, evidence: &str, (doing, todo): (&str, &str)) -> String {
    // The release is a name read from the repository; the evidence quotes a
    // tag or a ledger entry.
    format!(
        "{} is published ({}); {doing} it would move a published release: cut a new dated \
         release first, then {todo} that",
        inline_human_text(name),
        inline_human_text(evidence)
    )
}

/// Refuse, saying so, a release write while the upstream trunk does not
/// resolve: `true` after refusing.
///
/// Fail closed: parents are classified against the upstream trunk, and an
/// unresolvable trunk would make every base look like a member.
pub(crate) fn refuse_unresolved_trunk(
    opened: &knives::jj::Repo,
    repo: &RepoName,
    entry: &knives::config::RepoEntry,
) -> bool {
    if opened.resolve_commit(&entry.trunk_ref()).is_ok() {
        return false;
    }
    println!(
        "{repo}: cannot resolve {}; release edits classify parents against the upstream \
         trunk, so fetch upstream first",
        entry.trunk_ref()
    );
    true
}

/// Refuse, saying what to do, a write over a release in hand that carries a
/// change knives did not write or adopt ([`release::in_hand_refusal`]):
/// `Err` after refusing, otherwise how the release stands. `verb` names the
/// write refused.
///
/// Written over such a release, the write would carry the change forward,
/// and the next write would read it as knives' own, though nothing measured
/// what it kept of each member. So it refuses before it writes, naming the
/// `adopt` that measures and records it, or the remedy `adopt` itself gives.
fn refuse_unrecorded(
    (repo, verb): (&RepoName, &str),
    entry: &knives::config::RepoEntry,
    release: &release::InHand<'_>,
) -> anyhow::Result<Result<release::InHandWrite, Exit>> {
    let standing = release::in_hand_write(release)?;
    let Some(refusal) = release::in_hand_refusal(release, &standing, entry)? else {
        return Ok(Ok(standing));
    };
    println!("{repo}: {verb} refused, nothing written: {refusal}");
    Ok(Err(Exit::Incomplete))
}

/// Say so before a write over a release in hand knives could not check
/// against its recorded writes, a name the ledger records no write of
/// included ([`release::unchecked_line`]), and give the mark the write's
/// ledger event ends with ([`release::unchecked_suffix`]): nothing for any
/// other release.
pub(crate) fn announce_unchecked(
    repo: &RepoName,
    release: &release::InHand<'_>,
    standing: &release::InHandWrite,
) -> String {
    if let Some(line) = release::unchecked_line(release.name, release.commit, standing) {
        println!("{repo}: {line}");
    }
    release::unchecked_suffix(standing)
}

/// Refuse, saying why, an edit or a rebase of the release in hand that
/// carries a change knives did not write or adopt ([`refuse_unrecorded`]),
/// or one that is published ([`published_refusal`]): `Err` after refusing,
/// otherwise the mark the write's event ends with ([`announce_unchecked`]).
/// `verb` names the write refused, and `moving` is the gerund and the
/// imperative of the move a published release refuses.
pub(crate) fn refuse_edit_in_hand(
    (repo, verb): (&RepoName, &str),
    entry: &knives::config::RepoEntry,
    in_hand: &release::InHand<'_>,
    moving: (&str, &str),
) -> anyhow::Result<Result<String, Exit>> {
    let standing = match refuse_unrecorded((repo, verb), entry, in_hand)? {
        Ok(standing) => standing,
        Err(exit) => return Ok(Err(exit)),
    };
    if let Some(refusal) = published_refusal(
        in_hand.repo,
        in_hand.ledger,
        release::ReleaseAt {
            scheme: &entry.release_scheme(),
            name: in_hand.name,
            commits: &[Some(in_hand.commit), in_hand.published],
        },
        moving,
    )? {
        println!("{repo}: {refusal}");
        return Ok(Err(Exit::Incomplete));
    }
    Ok(Ok(announce_unchecked(repo, in_hand, &standing)))
}

/// Refuse, saying what the plan could not answer, a write from a plan with
/// problems: `true` after refusing. `verb` names the write.
pub(crate) fn refuse_plan_problems(repo: &RepoName, plan: &release::Plan, verb: &str) -> bool {
    if plan.problems.is_empty() {
        return false;
    }
    println!("{}", release::render(plan));
    // The plan's own render escaped these one line above; this summary
    // quotes the same jj, forge and consumer text, so it escapes each
    // problem too ([`knives::ledger::problem_human_text`]).
    println!(
        "{repo}: {verb} not applied; the plan could not answer: {}",
        plan.problems
            .iter()
            .map(|problem| knives::ledger::problem_human_text(problem))
            .collect::<Vec<_>>()
            .join("; ")
    );
    true
}

/// Refuse, saying so, an edit of a release every pin of which is frozen:
/// `true` after refusing. Follows from who pins it, exactly as for a rebase: a
/// consumer that follows the branch sees the edit, one frozen on a revision
/// does not. The way out differs by scheme, because a fixed branch cannot take
/// a dated name.
fn refuse_if_frozen(
    repo: &RepoName,
    scheme: &ReleaseScheme,
    plan: &release::Plan,
    release_name: &str,
) -> bool {
    if release::repair_effect(
        &plan.pins,
        knives::ids::BookmarkRef::parse(release_name).branch(),
        plan.published.as_ref(),
    ) != release::RepairEffect::NewDatedName
    {
        return false;
    }
    let shown_release = inline_human_text(release_name);
    match scheme {
        ReleaseScheme::Dated | ReleaseScheme::Series(_) => println!(
            "{repo}: every pin of {shown_release} is frozen, so editing it would reach nobody; \
             cut a new dated release first"
        ),
        ReleaseScheme::Fixed(_) => println!(
            "{repo}: every pin of {shown_release} is frozen, so editing the fixed branch would \
             reach nobody; update the frozen consumer pins, or change the release scheme before \
             editing it (fixed branches cannot reach revision pins)"
        ),
    }
    true
}

/// Edit the release in hand: one change, nothing else moves.
///
/// The whole command is the jj sequence agents fumble — duplicate the release
/// onto the changed parent set, describe it, move its name — with the same pin
/// gate a rebase has. A file whose conflict the release resolved keeps that
/// resolution only where the plain merge of the new parents leaves the same
/// conflict there. Elsewhere the write puts that plain merge in the file and
/// names the file ([`knives::jj::edit_candidate`]), whether or not an adopt
/// recorded the resolution. Every conflict the write leaves is reported. The
/// write is checked before it is made, and an edit that would lose a
/// remaining member's lines writes nothing, unless `--allow-loss` gave a
/// reason ([`apply_edit`]).
#[allow(
    clippy::too_many_arguments,
    reason = "release-edit state and the shared consumer-scan collaborators are independently owned command inputs"
)]
fn edit_release(
    fork: &Fork<'_>,
    locals: &[std::path::PathBuf],
    (change, allow_loss): (&ReleaseEdit, Option<&str>),
    forge: &dyn knives::consumer_pins::ConsumerPinSource,
    cache_root: Option<&std::path::Path>,
    heads: &knives::consumer_pins::ConsumerHeadMemo,
    bound: Bound<'_>,
) -> anyhow::Result<Exit> {
    let repo = &fork.name;
    let entry = fork.entry;
    let opened = knives::jj::Repo::open(&fork.checkout.path)?;
    let consumers = release::ConsumerInputs {
        slugs: &entry.consumers,
        locals,
        forge,
        cache_root,
        heads,
    };
    let plan = release::pulled_plan(fork, bound.registry, &consumers)?;
    if refuse_plan_problems(repo, &plan, change.verb()) {
        return Ok(Exit::Incomplete);
    }
    let Some(release_name) = plan.release.clone() else {
        println!("{repo}: no release to edit; cut one first");
        return Ok(Exit::Incomplete);
    };
    if refuse_if_frozen(repo, &entry.release_scheme(), &plan, &release_name) {
        return Ok(Exit::Incomplete);
    }
    // An unresolvable trunk would let include refuse for the wrong reason,
    // and drop or advance touch the base, which is `rebase`'s domain.
    if refuse_unresolved_trunk(&opened, repo, entry) {
        return Ok(Exit::Incomplete);
    }
    if !release_is_locally_movable(&opened, repo, &release_name) {
        return Ok(Exit::Incomplete);
    }
    // The pulled plan refused on any problem this read finds.
    let ledger = Ledger::for_fork(fork).read()?.entries;
    let tips = opened.bookmark_tips()?;
    let release = ReleaseInHand::read(
        &opened,
        entry,
        release_name,
        (&ledger, &tips, plan.published.as_ref()),
    )?;
    let in_hand = release::InHand {
        repo: &opened,
        ledger: &ledger,
        name: &release.name,
        commit: &release.commit,
        writes: &release.writes,
        published: plan.published.as_ref(),
    };
    let unchecked =
        match refuse_edit_in_hand((repo, change.verb()), entry, &in_hand, ("editing", "edit"))? {
            Ok(unchecked) => unchecked,
            Err(exit) => return Ok(exit),
        };
    let releases = knives::release_model::release_refs_by_commit(
        &tips,
        &entry.release_scheme(),
        entry.publish_remote(),
    );
    let written =
        knives::release_model::recorded_release_changes(&opened, &ledger, &entry.release_scheme());
    let context = EditContext {
        repo,
        opened: &opened,
        release: &release,
        entry,
        recorded: last_recorded_parents(&ledger, &release.name),
        bound,
        unchecked: &unchecked,
        allow_loss,
        stacked: StackedHistoryContext {
            repo: &opened,
            trunks: &release.trunk_tips,
            releases: &releases,
            carried: &release.parents,
            written: &written,
            replaced: None,
        },
    };
    let outcome = match change {
        ReleaseEdit::Include { branch, why } => include_edit(&context, branch, why.as_deref())?,
        ReleaseEdit::Drop { branch, why } => drop_edit(&context, branch, why)?,
        ReleaseEdit::Advance { branches, from } => {
            advance_edit(&context, entry, branches, from.as_deref())?
        }
    };
    let done = match outcome {
        EditOutcome::Settled(exit) => return Ok(exit),
        EditOutcome::Done(done) => done,
    };
    release::with_last_write_headings(|last| apply_edit(&context, fork, &done, last))
}

/// Write the edited release — duplicated onto its new parent set by spec
/// R11's rule ([`knives::jj::edit_candidate`]), described, its name moved —
/// record the parent set in the ledger, and report. Each file the rule
/// wrote as the plain merge of the new parents is named after the lines on
/// the replaced commit ([`plain_merged_lines`]), before the conflict report.
///
/// The write is built first as a scratch candidate and checked
/// ([`edit_losses`]). Where the check counts a line of a remaining member
/// that the write would lose, the edit names each path, member and line and
/// the remedy, writes nothing, and exits 3 ([`refusal_lines`]). With
/// `--allow-loss` it writes the candidate, names the same lines as lost
/// ([`allowed_loss_lines`]), and records the loss in its ledger event
/// ([`loss_event_text`]). Where the check counts nothing, the flag changes
/// nothing.
///
/// `uncarried` is the target of a drop whose parent no remaining member's
/// history carries. Whether the release lost that content is read from the
/// written tree: jj merges the remaining parents over their real merge base,
/// so the content can survive, and then the drop says so instead of a loss.
/// The release it writes is read through one opening as soon as the write
/// is done ([`release::ReportedWrite`]) and left in `last`, the command's
/// last write: its conflict report prints from that reading, and its
/// repeated headings print once after the command's steps, whether the
/// record after the write fails or not
/// ([`release::with_last_write_headings`]).
fn apply_edit(
    context: &EditContext<'_>,
    fork: &Fork<'_>,
    done: &EditDone,
    last: &mut Option<release::ReportedWrite>,
) -> anyhow::Result<Exit> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    let (entry, new_parents, delta) = (fork.entry, done.parents.as_slice(), done.delta.as_str());
    let provenance = release::parent_sources(opened, entry, &entry.release_scheme(), new_parents)?;
    let message = release::composition_message(&release.name, &provenance, delta);
    let operation = format!("knives: {}: {delta}", release.name);
    let write = knives::jj::ReleaseWrite {
        source: Some(&release.commit),
        parents: new_parents,
        // What the release composes leaves its carriers out.
        measured: knives::jj::Measured::WithoutCarriers(&release.carriers),
        message: Some(&message),
        bookmark: Some(&release.name),
        publish_remote: Some(entry.publish_remote()),
        earlier: release.writes.commits(),
        operation: &operation,
    };
    let candidate = knives::jj::edit_candidate(&fork.checkout.path, &write)?;
    let losses = edit_losses(context, &candidate, (&provenance, &done.brought))?;
    if !losses.is_empty() && context.allow_loss.is_none() {
        for line in refusal_lines(repo, &release.name, &losses) {
            println!("{line}");
        }
        return Ok(Exit::Incomplete);
    }
    let (written, plain_merged) = candidate.publish()?;
    let reported = &*last.insert(release::ReportedWrite::read(
        &fork.checkout.path,
        written.commit.clone(),
    ));
    record_edit_event(
        fork,
        opened,
        &EditRecord {
            release: &release.name,
            delta,
            created: &written.commit,
            change: &written.change,
            provenance: &provenance,
            unions: &written.unions,
            loss: &loss_event_text(&losses, context.allow_loss),
            unchecked: context.unchecked,
        },
        context.bound,
    )?;
    if let Some(target) = &done.uncarried {
        let shown_target = inline_human_text(target);
        if written.same_tree_as_source {
            println!(
                "{repo}: {shown_target} left the parent set of {}, and the release's content did \
                 not change",
                release.shown_name()
            );
        } else {
            println!(
                "{repo}: no remaining member carries {shown_target}'s content; the release loses \
                 it"
            );
        }
    }
    // The delta names branches read from the repository, and the reason the
    // caller gave; the ledger keeps it raw.
    println!(
        "{repo}: {} now has {} parent(s): {}",
        release.shown_name(),
        new_parents.len(),
        inline_human_text(delta)
    );
    report_replaced(repo, &release.name, &written);
    for line in plain_merged_lines(repo, &plain_merged)
        .into_iter()
        .chain(allowed_loss_lines(repo, &release.name, &losses))
    {
        println!("{line}");
    }
    println!(
        "{}",
        release::conflict_report(&release.name, reported.conflicts(), &written.unions)
    );
    Ok(Exit::Ok)
}

/// What a remaining member is to spec R11's check, its fork points read
/// ([`edit_losses`], [`knives::jj::Brought`]).
enum BroughtFrom {
    Nothing,
    Whole,
    /// An advanced member: the fork point of its old tip, and that tip.
    Since(CommitId, CommitId),
}

/// Spec R11's check of `candidate`
/// ([`knives::jj::EditCandidate::lost_lines`]). Each remaining member is a
/// parent of `provenance`, named as the release's description names it,
/// and read from its fork point with the upstream trunk to its tip;
/// `brought` gives each parent the edit brings in, with the one it replaces
/// for an advance.
fn edit_losses(
    context: &EditContext<'_>,
    candidate: &knives::jj::EditCandidate<'_>,
    (provenance, brought): (&[(String, CommitId)], &[BroughtParent]),
) -> anyhow::Result<Vec<knives::jj::EditLoss>> {
    let opened = context.opened;
    let trunk = opened.resolve_commit(&context.entry.trunk_ref())?;
    let mut read = Vec::with_capacity(provenance.len());
    for (_, tip) in provenance {
        let from = match brought.iter().find(|(new, _)| new == tip) {
            None => BroughtFrom::Nothing,
            Some((_, None)) => BroughtFrom::Whole,
            Some((_, Some(old))) => {
                BroughtFrom::Since(opened.fork_point(old, &trunk)?, old.clone())
            }
        };
        read.push((opened.fork_point(tip, &trunk)?, from));
    }
    let members: Vec<knives::jj::EditMember<'_>> = provenance
        .iter()
        .zip(&read)
        .map(
            |((label, tip), (fork_point, from))| knives::jj::EditMember {
                label,
                change: (fork_point, tip),
                brought: match from {
                    BroughtFrom::Nothing => knives::jj::Brought::Nothing,
                    BroughtFrom::Whole => knives::jj::Brought::Whole,
                    BroughtFrom::Since(old_fork_point, old) => {
                        knives::jj::Brought::Since((old_fork_point, old))
                    }
                },
            },
        )
        .collect();
    Ok(candidate.lost_lines(&members)?)
}

/// The lines an edit prints when spec R11's check counts lines of a
/// remaining member that its write would lose: for each path and member,
/// that it refuses and writes nothing, then each counted line; then the
/// remedy. The release, each path, member and line are read from the
/// repository, so each is escaped.
fn refusal_lines(repo: &RepoName, release: &str, losses: &[knives::jj::EditLoss]) -> Vec<String> {
    let shown_release = inline_human_text(release);
    let mut lines = Vec::new();
    for loss in losses {
        lines.push(format!(
            "{repo}: refusing to write {shown_release}: {} would lose {}'s lines; nothing \
             written",
            inline_human_text(&loss.path),
            inline_human_text(&loss.lines.label)
        ));
        lines.extend(release::lost_lines(&loss.lines, release::LOST_LINE_INDENT));
    }
    lines.push(format!(
        "{repo}: run the edit again with `--allow-loss <reason>`, put the named lines back by \
         hand, and record that with `knives release adopt {} --why <reason>`",
        shell_word(release)
    ));
    lines
}

/// The lines an `--allow-loss` edit prints for the lines it wrote without:
/// for each path and member, that they are lost by this write, then each
/// line; then how to put them back. Nothing where it lost none. Each text
/// is read from the repository, so it is escaped.
fn allowed_loss_lines(
    repo: &RepoName,
    release: &str,
    losses: &[knives::jj::EditLoss],
) -> Vec<String> {
    if losses.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for loss in losses {
        lines.push(format!(
            "{repo}: {}: {}'s lines lost by this write (--allow-loss)",
            inline_human_text(&loss.path),
            inline_human_text(&loss.lines.label)
        ));
        lines.extend(release::lost_lines(&loss.lines, release::LOST_LINE_INDENT));
    }
    lines.push(format!(
        "{repo}: put the named lines back by hand and record that with `knives release adopt {} \
         --why <reason>`",
        shell_word(release)
    ));
    lines
}

/// `; lost: <path> (<member>: <n> line(s))` for each path and member whose
/// lines an `--allow-loss` write lost, then `; why lost: <reason>`; nothing
/// where it lost none, or where no reason allowed a loss. An edit's event
/// holds it after its delta and before its parents ([`record_edit_event`]).
/// The ledger keeps each text raw.
fn loss_event_text(losses: &[knives::jj::EditLoss], reason: Option<&str>) -> String {
    use std::fmt::Write as _;
    let Some(reason) = reason.filter(|_| !losses.is_empty()) else {
        return String::new();
    };
    let mut text = String::new();
    for loss in losses {
        // Writing to a String does not fail.
        let _ = write!(
            text,
            "; lost: {} ({}: {} line(s))",
            loss.path,
            loss.lines.label,
            loss.lines.lost.len()
        );
    }
    let _ = write!(text, "; why lost: {reason}");
    text
}

/// The lines an edit prints for each file its write put as the plain merge
/// of the new parents (spec R11): that it did, and whether the file holds a
/// conflict; then, where the release had also changed lines outside the
/// conflict there, that this hand change was not carried. A path is read
/// from the tree, so it is escaped.
fn plain_merged_lines(repo: &RepoName, files: &[knives::jj::PlainMergedFile]) -> Vec<String> {
    let mut lines = Vec::new();
    for file in files {
        let path = inline_human_text(&file.path);
        let left = if file.conflicted {
            "which leaves a conflict"
        } else {
            "with no conflict"
        };
        lines.push(format!(
            "{repo}: {path}: written as the plain merge of the new parents (the resolution \
             answers another conflict), {left}"
        ));
        if file.hand_change_left_out {
            lines.push(format!(
                "{repo}: {path}: the hand change outside the resolved conflict was not carried"
            ));
        }
    }
    lines
}

/// One line on the commit a release write replaced under `release`: abandoned,
/// or kept with what rests on it. Silent when the write replaced nothing.
pub(crate) fn report_replaced(
    repo: &RepoName,
    release: &str,
    written: &knives::jj::ReleaseWritten,
) {
    for line in replaced_lines(repo, release, written) {
        println!("{line}");
    }
}

/// The lines [`report_replaced`] prints. The release is a name read from the
/// repository, and what keeps the old commit (`what_keeps`, src/jj.rs) names
/// bookmarks, tags and jj workspaces, so each is escaped.
fn replaced_lines(
    repo: &RepoName,
    release: &str,
    written: &knives::jj::ReleaseWritten,
) -> Vec<String> {
    let shown_release = inline_human_text(release);
    let mut lines = Vec::new();
    if let Some(replaced) = &written.replaced {
        let old = replaced.commit.short();
        lines.push(replaced.kept.as_ref().map_or_else(
            || format!("{repo}: abandoned {old}, the {shown_release} commit this write replaced"),
            |reason| {
                format!(
                    "{repo}: note: kept {old}, the {shown_release} commit this write replaced: {}",
                    inline_human_text(reason)
                )
            },
        ));
    }
    lines.extend(abandoned_earlier_lines(
        repo,
        release,
        &written.abandoned_earlier,
    ));
    lines.extend(hidden_lines(repo, release, &written.hidden));
    lines
}

/// One line per earlier write of `release` abandoned because nothing rests on
/// it any more.
pub(crate) fn report_abandoned_earlier(
    repo: &RepoName,
    release: &str,
    abandoned: &[knives::ids::CommitId],
) {
    for line in abandoned_earlier_lines(repo, release, abandoned) {
        println!("{line}");
    }
}

fn abandoned_earlier_lines(
    repo: &RepoName,
    release: &str,
    abandoned: &[knives::ids::CommitId],
) -> Vec<String> {
    let shown_release = inline_human_text(release);
    abandoned
        .iter()
        .map(|commit| {
            format!(
                "{repo}: abandoned {}, an earlier {shown_release} commit nothing rests on any more",
                commit.short()
            )
        })
        .collect()
}

/// One line per head a write left that nothing held, and abandoned in the
/// same operation (`hide_exposed`, src/jj.rs).
pub(crate) fn report_hidden(repo: &RepoName, release: &str, hidden: &[knives::jj::HiddenHead]) {
    for line in hidden_lines(repo, release, hidden) {
        println!("{line}");
    }
}

fn hidden_lines(repo: &RepoName, release: &str, hidden: &[knives::jj::HiddenHead]) -> Vec<String> {
    let shown_release = inline_human_text(release);
    hidden
        .iter()
        .map(|head| {
            let commit = head.commit.short();
            head.superseded_by.as_ref().map_or_else(
                || {
                    format!(
                        "{repo}: abandoned {commit}, an earlier {shown_release} commit this write \
                         left a head nothing holds"
                    )
                },
                |by| {
                    format!(
                        "{repo}: abandoned {commit}, which this write left a head nothing holds; \
                         its rewrite {} is visible",
                        by.short()
                    )
                },
            )
        })
        .collect()
}

/// Every earlier write of `release` ([`EarlierWrites`]): the fork's ledger's
/// writes of the name, and the publish remote's commit of it in `opened`'s
/// view.
pub(crate) fn release_writes(
    fork: &Fork<'_>,
    opened: &knives::jj::Repo,
    release: &str,
) -> anyhow::Result<EarlierWrites> {
    let entries = Ledger::for_fork(fork).read()?.entries;
    let published = opened
        .bookmark_tips()?
        .remove(&knives::ids::BookmarkRef::Remote {
            branch: BranchName::new(release),
            remote: knives::ids::RemoteName::new(fork.entry.publish_remote()),
        });
    Ok(EarlierWrites::read(
        opened,
        release,
        &entries,
        published.as_ref(),
    )?)
}

/// What one release edit wrote: the name, the delta, the new merge with its
/// change id, its parent set with the branch each parent came from, the
/// release logs it unioned, the loss an `--allow-loss` write recorded
/// ([`loss_event_text`]), and the mark of a release in hand knives could not
/// check ([`release::unchecked_suffix`]).
pub(crate) struct EditRecord<'a> {
    pub(crate) release: &'a str,
    pub(crate) delta: &'a str,
    pub(crate) created: &'a knives::ids::CommitId,
    pub(crate) change: &'a knives::ids::ChangeId,
    pub(crate) provenance: &'a [(String, knives::ids::CommitId)],
    pub(crate) unions: &'a [knives::jj::UnionedFile],
    /// `; lost: …` and `; why lost: …`, or empty.
    pub(crate) loss: &'a str,
    pub(crate) unchecked: &'a str,
}

/// The edit's parent set goes to the ledger the way a cut's does, so a later
/// `advance` or `include` can still tell which parent is which branch after a
/// rebase that left no ancestry or change id behind. Like a cut event, it names
/// the written commit, its change id and its parents, and the commit leads the
/// evidence: it is the position `audit` holds the release against. The release
/// logs the edit unioned close the text ([`union_event_text`]). The loss an
/// `--allow-loss` write recorded follows the delta, before the parents.
pub(crate) fn record_edit_event(
    fork: &Fork<'_>,
    opened: &knives::jj::Repo,
    record: &EditRecord<'_>,
    bound: Bound<'_>,
) -> anyhow::Result<()> {
    scribe_for(fork, bound)?.record(&Draft {
        evidence: std::iter::once(record.created.as_str().to_owned())
            .chain(
                record
                    .provenance
                    .iter()
                    .map(|(_, commit)| commit.as_str().to_owned()),
            )
            .collect(),
        parents: provenance_parents(opened, fork.entry, record.provenance)?,
        ..Draft::event(
            Some(record.release),
            format!(
                "{}{}{}",
                edit_event_text(
                    record.release,
                    (record.created, record.change),
                    record.provenance,
                    &format!("{}{}", record.delta, record.loss)
                ),
                union_event_text(record.unions),
                record.unchecked
            ),
            None,
        )
    })?;
    Ok(())
}

/// An edit event's text through its parents: `edited <release> as <commit>
/// (change <id>) with <n> parent(s): <delta>; parents: <provenance>`. A
/// write's unions and its unchecked mark close the text after it. The delta
/// an edit's event holds ends with the loss an `--allow-loss` write
/// recorded ([`record_edit_event`]), so a reason that holds `; unchecked: …`
/// or `; parents: …` is never read as the event's mark or its parents.
pub(crate) fn edit_event_text(
    release: &str,
    (created, change): (&knives::ids::CommitId, &knives::ids::ChangeId),
    provenance: &[(String, knives::ids::CommitId)],
    delta: &str,
) -> String {
    format!(
        "edited {release} as {} (change {}) with {} parent(s): {delta}; parents: {}",
        created.short(),
        change.short(),
        provenance.len(),
        members_event_text(provenance)
    )
}

/// `; union: CHANGELOG.md, RELEASE_NOTES.md` for the release logs a write
/// unioned, or nothing when it unioned none. A cut or edit event ends with
/// it, because every reader of those events parses only how the text starts
/// (`recorded_writes`, `last_recorded_cut`).
pub(crate) fn union_event_text(unions: &[knives::jj::UnionedFile]) -> String {
    if unions.is_empty() {
        return String::new();
    }
    let paths: Vec<&str> = unions.iter().map(|union| union.path.as_str()).collect();
    format!("; union: {}", paths.join(", "))
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

/// [`recorded_parents`] of a write's `provenance`, in its order.
pub(crate) fn provenance_parents(
    opened: &knives::jj::Repo,
    entry: &knives::config::RepoEntry,
    provenance: &[(String, knives::ids::CommitId)],
) -> anyhow::Result<Vec<knives::ledger::RecordedParent>> {
    let commits: Vec<knives::ids::CommitId> = provenance
        .iter()
        .map(|(_, commit)| commit.clone())
        .collect();
    recorded_parents(opened, entry, &commits)
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
) -> bool {
    if opened.local_bookmark_tip(name).is_some() {
        return true;
    }
    let shown = inline_human_text(name);
    match knives::ids::BookmarkRef::parse(name) {
        // The branch is one shell word inside the command; the remote is a
        // name read from the repository, escaped beside it.
        knives::ids::BookmarkRef::Remote { branch, remote } => println!(
            "{repo}: {shown} is here only as a remote ref, so there is no local bookmark to \
             move; `jj bookmark track {}@{}` first",
            shell_word(branch.as_str()),
            inline_human_text(remote.as_str())
        ),
        knives::ids::BookmarkRef::Local(_) => println!(
            "{repo}: {shown} has no single local position, so there is nothing to move; \
             resolve its divergence first"
        ),
    }
    false
}

fn include_edit(
    context: &EditContext<'_>,
    target: &str,
    why: Option<&str>,
) -> anyhow::Result<EditOutcome> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    // The caller typed the target, but these lines are the ones that print
    // the release's own name beside it, and the caller may have copied the
    // target from a remote bookmark jj imported.
    let (shown_target, shown_release) = (inline_human_text(target), release.shown_name());
    let tip = opened
        .local_bookmark_tip(target)
        .or_else(|| opened.resolve_commit(target).ok());
    let Some(tip) = tip else {
        println!("{repo}: {shown_target} is neither a local bookmark nor a resolvable revision");
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    };
    if release.parents.contains(&tip) {
        println!("{repo}: {shown_release} already carries {shown_target}");
        return Ok(EditOutcome::Settled(Exit::Ok));
    }
    if opened.is_ancestor(&release.commit, &tip)? {
        // Built on top of the release, not off the trunk. Including it would put
        // the cut in its own successor's ancestry, and it carries the whole
        // release with it rather than one branch's work.
        println!(
            "{repo}: {shown_target} is stacked on {shown_release}, so it is not a member of it; \
             rebase it off the trunk to include it"
        );
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    if opened.is_ancestor(&tip, &release.commit)? {
        // Reachable through another parent's history — a stacked branch, or one
        // whose content landed in the base. Membership is the parent set, so
        // this is not a member; say which situation holds rather than
        // pretending the include happened.
        println!(
            "{repo}: {shown_target}'s content is already in {shown_release} through another \
             parent's history; it is not a member parent itself, and whichever parent carries \
             it represents it"
        );
        return Ok(EditOutcome::Settled(Exit::Ok));
    }
    if release.trunk_tips.contains(&tip) {
        // A release is a flat merge of feature and fix branches; upstream enters
        // through the members' bases, never as a parent. Checked before the
        // member scan below, because a member that landed upstream is an
        // ancestor of the trunk and would otherwise answer for it.
        println!(
            "{repo}: {shown_target} is the trunk, not a feature or fix branch; {shown_release} \
             carries upstream through its members' bases, never as a parent"
        );
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    if context.refuse_if_other_series(target, &tip, "including it")? {
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    let Some(merged) = context.admit_history(target, &tip, ("including it", &[]))? else {
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    };
    let lookup = context.member_parents(target, &tip)?;
    if context.refuse_if_trunk_has(target, &tip, lookup.parents.first())? {
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    // A branch that merges the members it descends from is a new member built
    // on them, not a later state of any one of them: they stay, and it joins
    // beside them.
    let integrates = merged && context.integrates(target, &tip, &lookup)?;
    if integrates {
        let listed: Vec<&str> = lookup.parents.iter().map(CommitId::short).collect();
        println!(
            "{repo}: {shown_target} builds on {} member(s) of {shown_release} ({}), which stay \
             members",
            lookup.parents.len(),
            listed.join(", ")
        );
    } else if let Some(parent) =
        knives::release_model::replaced_member(context.recorded, &lookup, target)
            .or_else(|| lookup.parents.first())
    {
        // A successor that carries its member's line past the other members
        // (its own earlier tip merged back in) is refused by `advance` too.
        if merged
            && let MemberHistory::Stacked(stacked) =
                carries_member_line(context.stacked, parent, (target, &tip))?
        {
            println!(
                "{repo}: {}; rebase it off the trunk before advancing onto it",
                inline_human_text(&stacked.detail)
            );
            return Ok(EditOutcome::Settled(Exit::Incomplete));
        }
        // Moving a member is its own decision, and including it again would
        // carry it twice; say which situation the record describes.
        let parent = parent.short();
        // The branch is one shell word inside each command a person pastes.
        let word = shell_word(target);
        match lookup.evidence {
            MemberEvidence::Succession => println!(
                "{repo}: {shown_release} carries {parent} of {shown_target}, and the branch has \
                 moved on (grown or rebased); moving a member is its own decision: `knives \
                 release advance {word}`"
            ),
            MemberEvidence::Record => println!(
                "{repo}: {shown_release} carries {shown_target} as {parent} per its last cut or \
                 edit, and the branch has moved on without ancestry or change ids back to it \
                 (rebased outside jj?); including it again would carry it twice: `knives \
                 release advance {word}` moves it through that record"
            ),
            MemberEvidence::LandedRecord => println!(
                "{repo}: {shown_release} carries {shown_target} as {parent} per its last cut or \
                 edit, and {parent} has landed upstream; including the branch again would carry \
                 it twice: `knives release advance {word}` moves the member to the \
                 branch's tip, `knives release rebase` retires the landed parent"
            ),
        }
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    let mut parents = release.parents.clone();
    parents.push(tip.clone());
    let why = why.map_or_else(String::new, |why| format!(" ({why})"));
    Ok(EditOutcome::Done(EditDone {
        parents,
        delta: format!("included {target}{why}"),
        uncarried: None,
        brought: vec![(tip, None)],
    }))
}

/// Remove one member parent. The branch and its bookmark are untouched.
fn drop_edit(context: &EditContext<'_>, target: &str, why: &str) -> anyhow::Result<EditOutcome> {
    let (repo, opened, release) = (context.repo, context.opened, context.release);
    let (shown_target, shown_release) = (inline_human_text(target), release.shown_name());
    let mut candidates = Vec::new();
    if let Some(tip) = opened.local_bookmark_tip(target) {
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
        // A carrier is no member, but dropping it by its id is what a person
        // who read it in the parent list asks for: the write leaves it out.
        if release.parents.contains(&commit) || release.carriers.contains(&commit) {
            candidates.push(commit);
        }
    } else {
        println!("{repo}: {shown_target} is neither a local bookmark nor a resolvable revision");
        return Ok(EditOutcome::Settled(Exit::Incomplete));
    }
    match candidates.as_slice() {
        [] => {
            println!(
                "{repo}: {shown_release} carries no parent of {shown_target}; name the parent's \
                 commit id"
            );
            Ok(EditOutcome::Settled(Exit::Incomplete))
        }
        [parent] => {
            let mut parents = release.parents.clone();
            parents.retain(|kept| kept != parent);
            // A drop only removes the parent; whether the content survives
            // depends on the remaining members. Losing it is often the point
            // (a bad fix) and sometimes a surprise (a landed pull the
            // members' bases do not reach yet), so the fact is stated rather
            // than judged, once the written tree says which it is.
            let mut carried_elsewhere = false;
            for kept in &parents {
                if opened.is_ancestor(parent, kept)? {
                    carried_elsewhere = true;
                    break;
                }
            }
            let delta = format!("dropped {target}: {why}");
            Ok(EditOutcome::Done(EditDone {
                parents,
                delta,
                uncarried: (!carried_elsewhere).then(|| target.to_owned()),
                brought: Vec::new(),
            }))
        }
        many => {
            let listed: Vec<&str> = many.iter().map(CommitId::short).collect();
            println!(
                "{repo}: {shown_target} matches {} parents of {shown_release} ({}); name one by \
                 commit id",
                many.len(),
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
    // A bare advance considers only the selected series' branches: another
    // series' branch never succeeds one of this release's members, and one
    // that fits no series is refused when named, not guessed about here.
    let carried = if from.is_none() && branches.is_empty() {
        knives::release_model::series_members(context.opened, entry, carried)?.0
    } else {
        carried
    };
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
                context.repo,
                context.release.shown_name()
            );
        }
        return Ok(EditOutcome::Settled(Exit::Ok));
    }
    // Each member moved, with the parent it replaces: each advance keeps the
    // parents' places.
    let brought = context
        .release
        .parents
        .iter()
        .zip(&parents)
        .filter(|(old, new)| old != new)
        .map(|(old, new)| (new.clone(), Some(old.clone())))
        .collect();
    // Stacked members advanced to one tip collapse into a single parent.
    let mut deduped = Vec::new();
    for parent in parents {
        if !deduped.contains(&parent) {
            deduped.push(parent);
        }
    }
    let delta = format!("advanced {}", moved.join(", "));
    Ok(EditOutcome::Done(EditDone {
        parents: deduped,
        delta,
        uncarried: None,
        brought,
    }))
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
        if !context.bare_advance_candidate(branch, tip)? {
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
                // Bookmark names read from the repository.
                let names: Vec<String> = many.iter().map(|(branch, _)| branch.clone()).collect();
                ambiguous.push(format!(
                    "parent {} has several advanced branches ({})",
                    parent.short(),
                    inline_human_text(&names.join(", "))
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
            let listed: Vec<&str> = stale_parents.iter().map(CommitId::short).collect();
            println!(
                "{repo}: {} descends from {} parents of {} ({}); drop and include instead",
                inline_human_text(branch),
                stale_parents.len(),
                release.shown_name(),
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
    let shown_release = release.shown_name();
    let mut parents = release.parents.clone();
    let mut moved: Vec<String> = Vec::new();
    for branch in branches {
        let shown_branch = inline_human_text(branch.as_str());
        let named = carried
            .iter()
            .find(|(carried, _)| carried == branch.as_str())
            .map(|(_, tip)| tip.clone());
        let Some(tip) = named else {
            if opened.local_bookmark_tip(branch.as_str()).is_some() {
                // The trunk and our release names are the two bookmarks a release
                // can never carry, and naming one here reached the same mover the
                // bare advance deliberately keeps them away from.
                println!(
                    "{repo}: {shown_branch} is the trunk or a release name, so it is never a \
                     member of {shown_release}"
                );
            } else {
                println!("{repo}: no local bookmark named {shown_branch}");
            }
            return Ok(None);
        };
        if parents.contains(&tip) {
            println!("{repo}: {shown_branch} is already at its tip in {shown_release}");
            continue;
        }
        if opened.is_ancestor(&release.commit, &tip)? {
            println!(
                "{repo}: {shown_branch} is stacked on {shown_release}, so advancing a member \
                 onto it would fold in work nobody included and put the cut in its own ancestry"
            );
            return Ok(None);
        }
        if context.refuse_if_other_series(branch.as_str(), &tip, "advancing onto it")? {
            return Ok(None);
        }
        let Some(lookup) = context.named_advance_lookup(branch.as_str(), &tip)? else {
            return Ok(None);
        };
        // One member it continues, or among several the one recorded under
        // this branch's own name: the others it merged stay.
        let replaced =
            knives::release_model::replaced_member(context.recorded, &lookup, branch.as_str());
        match (lookup.parents.as_slice(), replaced) {
            ([], _) => {
                println!(
                    "{repo}: {shown_release} carries no parent of {shown_branch}; `knives \
                     release include {}` adds it",
                    shell_word(branch.as_str())
                );
                return Ok(None);
            }
            (_, Some(parent)) => {
                // Measured against the members it does not replace, as the plan
                // measures it once it is in.
                if context
                    .admit_history(
                        branch.as_str(),
                        &tip,
                        ("advancing onto it", std::slice::from_ref(parent)),
                    )?
                    .is_none()
                {
                    return Ok(None);
                }
                for slot in parents.iter_mut().filter(|slot| **slot == *parent) {
                    *slot = tip.clone();
                }
                moved.push(branch.to_string());
            }
            (many, None) => {
                let listed: Vec<&str> = many.iter().map(CommitId::short).collect();
                println!(
                    "{repo}: {shown_branch} descends from {} parents of {shown_release} ({}); \
                     drop and include instead",
                    many.len(),
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
    let (shown_branch, shown_release) = (inline_human_text(branch.as_str()), release.shown_name());
    let Some(tip) = carried
        .iter()
        .find(|(carried, _)| carried == branch.as_str())
        .map(|(_, tip)| tip.clone())
    else {
        if opened.local_bookmark_tip(branch.as_str()).is_some() {
            println!(
                "{repo}: {shown_branch} is the trunk or a release name, so it is never a member \
                 of {shown_release}"
            );
        } else {
            println!("{repo}: no local bookmark named {shown_branch}");
        }
        return Ok(None);
    };
    if opened.is_ancestor(&release.commit, &tip)? {
        println!(
            "{repo}: {shown_branch} is stacked on {shown_release}, so advancing a member onto \
             it would fold in work nobody included and put the cut in its own ancestry"
        );
        return Ok(None);
    }
    if context.refuse_if_other_series(branch.as_str(), &tip, "advancing onto it")? {
        return Ok(None);
    }
    if context
        .admit_history(
            branch.as_str(),
            &tip,
            ("advancing onto it", std::slice::from_ref(old)),
        )?
        .is_none()
    {
        return Ok(None);
    }
    if context.refuse_if_trunk_has(branch.as_str(), &tip, Some(old))? {
        return Ok(None);
    }
    let mut parents = release.parents.clone();
    if parents.contains(&tip) {
        println!("{repo}: {shown_branch} is already at its tip in {shown_release}");
        return Ok(Some((parents, Vec::new())));
    }
    let Some(index) = parents.iter().position(|parent| parent == old) else {
        println!(
            "{repo}: {} is not a parent of {shown_release}; `knives release include \
             {}` adds {shown_branch} instead",
            old.short(),
            shell_word(branch.as_str())
        );
        return Ok(None);
    };
    if let Some(slot) = parents.get_mut(index) {
        *slot = tip;
    }
    Ok(Some((parents, vec![branch.to_string()])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use knives::ids::ChangeId;
    use knives::jj::{ReleaseWritten, ReplacedRelease};

    #[test]
    fn a_kept_replaced_commit_names_its_workspace_and_release_escaped() {
        // `jj workspace add --name` accepts ESC and C1; `what_keeps` names
        // the workspace whose working copy rests on the replaced commit.
        let written = ReleaseWritten {
            commit: CommitId::new("aaaa"),
            change: ChangeId::new("zzzz"),
            replaced: Some(ReplacedRelease {
                commit: CommitId::new("bbbb"),
                kept: Some("the working copy of workspace ws\u{1b}x\u{9b}y rests on it".to_owned()),
            }),
            abandoned_earlier: vec![CommitId::new("cccc")],
            hidden: Vec::new(),
            unions: Vec::new(),
            same_tree_as_source: false,
        };

        let lines = replaced_lines(&RepoName::new("demo"), "release/x\u{9b}2J", &written);

        for line in &lines {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
        assert_eq!(
            lines,
            [
                "demo: note: kept bbbb, the release/x\u{fffd}2J commit this write replaced: the \
                 working copy of workspace ws\u{fffd}x\u{fffd}y rests on it",
                "demo: abandoned cccc, an earlier release/x\u{fffd}2J commit nothing rests on any \
                 more",
            ]
        );
    }

    /// What an edit loses of `member` at `path`: one added line, at its
    /// tip's line 2.
    fn loss(path: &str, member: &str, text: &str) -> knives::jj::EditLoss {
        let mut lines = knives::own_lines::MemberLines::new(
            member.to_owned(),
            CommitId::new("tip"),
            CommitId::new("fork"),
        );
        lines.reading = knives::own_lines::Reading::Missing;
        lines.lost = vec![knives::own_lines::Lost {
            path: path.to_owned(),
            kind: knives::own_lines::LostKind::Added,
            line: Some(2),
            text: Some(text.to_owned()),
            carried_forward: false,
        }];
        knives::jj::EditLoss {
            path: path.to_owned(),
            lines,
        }
    }

    #[test]
    fn a_refusal_names_each_path_member_and_line_and_the_remedy_escaped() {
        // Given: an edit that would lose a line of each of two members, one
        // path holding a C1 character, as a path read from a tree can.
        let losses = [
            loss("CHANGELOG.md", "feat/a", "- a entry"),
            loss("n\u{9b}2J.md", "feat/q", "- q entry"),
        ];

        // When: the refusal's lines are read.
        let lines = refusal_lines(&RepoName::new("demo"), "release/2026-08-04", &losses);

        // Then: each path and member, its line, then the remedy; no line holds
        // a control character.
        for line in &lines {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
        assert_eq!(
            lines,
            [
                "demo: refusing to write release/2026-08-04: CHANGELOG.md would lose feat/a's \
                 lines; nothing written",
                "      CHANGELOG.md:2: added line lost ‹- a entry›",
                "demo: refusing to write release/2026-08-04: n\u{fffd}2J.md would lose feat/q's \
                 lines; nothing written",
                "      n\u{fffd}2J.md:2: added line lost ‹- q entry›",
                "demo: run the edit again with `--allow-loss <reason>`, put the named lines back \
                 by hand, and record that with `knives release adopt release/2026-08-04 --why \
                 <reason>`",
            ]
        );
    }

    #[test]
    fn an_allowed_loss_names_its_lines_and_how_to_put_them_back() {
        let losses = [loss("CHANGELOG.md", "feat/a", "- a entry")];
        let repo = RepoName::new("demo");

        // A write that lost lines names them, then the adopt that records
        // them put back.
        assert_eq!(
            allowed_loss_lines(&repo, "release/2026-08-04", &losses),
            [
                "demo: CHANGELOG.md: feat/a's lines lost by this write (--allow-loss)",
                "      CHANGELOG.md:2: added line lost ‹- a entry›",
                "demo: put the named lines back by hand and record that with `knives release \
                 adopt release/2026-08-04 --why <reason>`",
            ]
        );
        // A write that lost none prints nothing.
        assert_eq!(
            allowed_loss_lines(&repo, "release/2026-08-04", &[]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_loss_follows_the_delta_before_the_parents_and_holds_no_mark() {
        // Given: a drop that `--allow-loss` let lose a line of feat/a, with a
        // reason that ends with an unchecked mark and holds `; parents: `.
        let losses = [loss("CHANGELOG.md", "feat/a", "- a entry")];
        let reason = "see; parents: x; unchecked: no record";
        let lost = loss_event_text(&losses, Some(reason));

        // When: the event's text is written, over a checked release.
        let text = edit_event_text(
            "release/2026-08-04",
            (
                &CommitId::new("a".repeat(40)),
                &ChangeId::new("k".repeat(32)),
            ),
            &[("feat/m".to_owned(), CommitId::new("b".repeat(40)))],
            &format!("dropped feat/d: x{lost}"),
        );

        // Then: the loss and its reason follow the delta and come before
        // knives' own parents, which stay last.
        assert_eq!(
            lost,
            "; lost: CHANGELOG.md (feat/a: 1 line(s)); why lost: see; parents: x; unchecked: no \
             record"
        );
        assert!(
            text.contains(&format!(
                "parent(s): dropped feat/d: x{lost}; parents: feat/m@"
            )),
            "{text}"
        );
        assert_eq!(
            text.rsplit_once("; parents: ").map(|(_, parents)| parents),
            Some("feat/m@bbbbbbbbbbbb")
        );
        assert!(!text.ends_with(reason), "{text}");
        // And: no loss, or no reason, writes nothing.
        assert_eq!(loss_event_text(&[], Some(reason)), "");
        assert_eq!(loss_event_text(&losses, None), "");
    }

    #[test]
    fn a_file_written_as_the_plain_merge_is_named_escaped_with_its_left_out_hand_change() {
        // Given: one file written with no conflict, whose release had a hand
        // change outside the conflict, and one left conflicted, its path
        // holding a C1 character, as a path read from a tree can.
        let files = [
            knives::jj::PlainMergedFile {
                path: "src/f.ml".to_owned(),
                conflicted: false,
                hand_change_left_out: true,
            },
            knives::jj::PlainMergedFile {
                path: "g\u{9b}2J".to_owned(),
                conflicted: true,
                hand_change_left_out: false,
            },
        ];

        // When: the edit's lines for them are read.
        let lines = plain_merged_lines(&RepoName::new("demo"), &files);

        // Then: each file is named with whether it holds a conflict, the
        // first with its hand change not carried, and no line holds a
        // control character.
        for line in &lines {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
        assert_eq!(
            lines,
            [
                "demo: src/f.ml: written as the plain merge of the new parents (the resolution \
                 answers another conflict), with no conflict",
                "demo: src/f.ml: the hand change outside the resolved conflict was not carried",
                "demo: g\u{fffd}2J: written as the plain merge of the new parents (the \
                 resolution answers another conflict), which leaves a conflict",
            ]
        );
    }

    #[test]
    fn a_hidden_head_is_named_as_an_earlier_write_or_with_its_rewrite() {
        // Given: a write that hid an earlier write of the release and a
        // member commit a visible rewrite supersedes.
        let hidden = [
            knives::jj::HiddenHead {
                commit: CommitId::new("dddd"),
                superseded_by: None,
            },
            knives::jj::HiddenHead {
                commit: CommitId::new("eeee"),
                superseded_by: Some(CommitId::new("ffff")),
            },
        ];

        // When: the write reports them.
        let lines = hidden_lines(&RepoName::new("demo"), "sami\u{9b}2J", &hidden);

        // Then: each says why it was the write's to hide, the name escaped.
        assert_eq!(
            lines,
            [
                "demo: abandoned dddd, an earlier sami\u{fffd}2J commit this write left a head \
                 nothing holds",
                "demo: abandoned eeee, which this write left a head nothing holds; its rewrite \
                 ffff is visible",
            ]
        );
    }
}
