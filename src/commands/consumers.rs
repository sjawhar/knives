//! `knives consumers`: compare consumer pins with live published releases.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::cli::Exit;
use crate::config::{RepoEntry, Role};
use crate::consumer_pins::{
    ConsumerHeadMemo, ConsumerPinSource, scan_consumer_for, scan_consumer_slug_with_heads,
};
use crate::ids::{CommitId, ReleaseScheme, RepoName, short_id, strict_dated_release};
use crate::jj::{self, Repo};
use crate::pins::{Pin, PinVerdict};
use crate::release_model::{ConsumerScan, newest_release, repo_slug};

/// Inputs for one fork's consumer-pin census.
pub struct Request<'a> {
    pub fork: &'a RepoName,
    pub entry: &'a RepoEntry,
    pub slugs: &'a [String],
    pub locals: &'a [PathBuf],
    pub forge: &'a dyn ConsumerPinSource,
    pub cache_root: Option<&'a Path>,
    pub heads: &'a ConsumerHeadMemo,
}

impl std::fmt::Debug for Request<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Request")
            .field("fork", self.fork)
            .field("entry", self.entry)
            .field("slugs", &self.slugs)
            .field("locals", &self.locals)
            .field("forge", &"<Forge>")
            .field("cache_root", &self.cache_root)
            .field("heads", self.heads)
            .finish()
    }
}

/// The release against which the census classified pins.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Release {
    pub reference: String,
    pub commit: String,
    pub source: String,
}

/// One pin observed in a consumer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PinRow {
    pub file: String,
    pub line: usize,
    pub reference: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<PinVerdict>,
}

/// One checkout that may consume the fork's releases.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsumerRow {
    pub consumer: String,
    pub pins: Vec<PinRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// Consumer pins and the live release refs that answered for them.
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub fork: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest: Option<Release>,
    pub consumers: Vec<ConsumerRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

struct Positions {
    refs: BTreeMap<String, CommitId>,
    newest: Option<Release>,
    notes: Vec<String>,
    problems: Vec<String>,
}

struct ConsumerContext<'a> {
    fork: &'a str,
    slug: Option<&'a str>,
    scheme: &'a ReleaseScheme,
    repo_path: &'a Path,
    live: &'a BTreeMap<String, CommitId>,
    newest: Option<&'a Release>,
    forge: &'a dyn ConsumerPinSource,
    cache_root: Option<&'a Path>,
    heads: &'a ConsumerHeadMemo,
}

/// Classify every registered and explicitly named consumer against live release refs.
pub fn gather(request: &Request<'_>) -> Report {
    let scheme = request.entry.release_scheme();
    let positions = positions(request.entry, &scheme);
    let slug = repo_slug(request.entry);
    let context = ConsumerContext {
        fork: request.fork.as_str(),
        slug: slug.as_deref(),
        scheme: &scheme,
        live: &positions.refs,
        newest: positions.newest.as_ref(),
        forge: request.forge,
        cache_root: request.cache_root,
        heads: request.heads,
        repo_path: &request.entry.path,
    };
    let mut consumers = request
        .slugs
        .iter()
        .map(|consumer| consumer_slug_row(consumer, &context))
        .chain(
            request
                .locals
                .iter()
                .map(|consumer| consumer_row(consumer, &context)),
        )
        .collect::<Vec<_>>();
    consumers.sort_by(|left, right| left.consumer.cmp(&right.consumer));
    let mut notes = positions.notes;
    add_skew_note(&consumers, &mut notes);
    Report {
        fork: request.fork.to_string(),
        newest: positions.newest,
        consumers,
        notes,
        problems: positions.problems,
    }
}

fn positions(entry: &RepoEntry, scheme: &ReleaseScheme) -> Positions {
    let mut notes = Vec::new();
    let mut problems = Vec::new();
    let local = local_positions(entry, scheme, &mut problems);
    let pattern = release_pattern(scheme);
    let remote = entry.remote(Role::Release);
    match jj::remote_refs(remote, &[&pattern]) {
        Ok(refs) => {
            let newest = newest_live(&refs, scheme).map(|(reference, commit)| Release {
                reference,
                commit: commit.to_string(),
                source: "live".to_owned(),
            });
            if newest.is_none() {
                problems.push(format!(
                    "release remote {remote} has no configured release ref"
                ));
            }
            if let (Some(local), Some(remote)) = (local.newest.as_ref(), newest.as_ref())
                && let Some(note) = local_remote_skew_note(local, remote)
            {
                notes.push(note);
            }
            Positions {
                refs,
                newest,
                notes,
                problems,
            }
        }
        Err(error) => {
            problems.push(format!(
                "could not read live release refs from {remote}: {error}"
            ));
            if let Some(newest) = local.newest.as_ref() {
                notes.push(format!(
                    "live release refs unavailable; local view has {} @ {}, but pins are unclassified",
                    newest.reference, newest.commit
                ));
            }
            Positions {
                refs: BTreeMap::new(),
                newest: None,
                notes,
                problems,
            }
        }
    }
}

fn local_remote_skew_note(local: &Release, remote: &Release) -> Option<String> {
    (local.reference != remote.reference || local.commit != remote.commit).then(|| {
        format!(
            "local view has {} @ {}, remote has {} @ {} — the checkout is behind or ahead of the remote",
            local.reference, local.commit, remote.reference, remote.commit
        )
    })
}

struct LocalPositions {
    newest: Option<Release>,
}

fn local_positions(
    entry: &RepoEntry,
    scheme: &ReleaseScheme,
    problems: &mut Vec<String>,
) -> LocalPositions {
    let Ok(repo) = Repo::open(&entry.path) else {
        problems.push(format!(
            "could not open local checkout {}",
            entry.path.display()
        ));
        return LocalPositions { newest: None };
    };
    let Ok(tips) = repo.bookmark_tips() else {
        problems.push(format!(
            "could not read local release refs from {}",
            entry.path.display()
        ));
        return LocalPositions { newest: None };
    };
    let newest =
        newest_release(&tips, scheme, entry.publish_remote()).map(|(reference, commit)| Release {
            reference: reference.branch().to_string(),
            commit: commit.to_string(),
            source: "local".to_owned(),
        });
    LocalPositions { newest }
}

fn release_pattern(scheme: &ReleaseScheme) -> String {
    match scheme {
        ReleaseScheme::Dated => "refs/heads/release/*".to_owned(),
        ReleaseScheme::Fixed(name) => format!("refs/heads/{name}"),
    }
}

fn newest_live(
    refs: &BTreeMap<String, CommitId>,
    scheme: &ReleaseScheme,
) -> Option<(String, CommitId)> {
    match scheme {
        ReleaseScheme::Dated => refs
            .iter()
            .filter_map(|(full, commit)| {
                let name = full.strip_prefix("refs/heads/")?;
                strict_dated_release(name).map(|order| (order, name, commit))
            })
            .max_by_key(|(order, _, _)| order.clone())
            .map(|(_, name, commit)| (name.to_owned(), commit.clone())),
        ReleaseScheme::Fixed(name) => refs
            .get(&format!("refs/heads/{name}"))
            .cloned()
            .map(|commit| (name.to_string(), commit)),
    }
}

fn consumer_row(consumer: &Path, context: &ConsumerContext<'_>) -> ConsumerRow {
    consumer_row_from_scan(
        consumer.display().to_string(),
        scan_consumer_for(consumer, context.slug, context.scheme),
        context,
    )
}

fn consumer_slug_row(consumer: &str, context: &ConsumerContext<'_>) -> ConsumerRow {
    consumer_row_from_scan(
        consumer.to_owned(),
        scan_consumer_slug_with_heads(
            context.forge,
            context.cache_root,
            context.repo_path,
            consumer,
            context.slug,
            context.scheme,
            context.heads,
        ),
        context,
    )
}

fn consumer_row_from_scan(
    consumer: String,
    scan: ConsumerScan,
    context: &ConsumerContext<'_>,
) -> ConsumerRow {
    let mut notes = scan.notes;
    let problem = (!scan.problems.is_empty()).then(|| scan.problems.join("; "));
    if problem.is_none() && scan.pins.is_empty() {
        notes.push(format!("does not pin {}", context.fork));
    } else if !scan.pins.is_empty() && context.newest.is_none() {
        notes.push("cannot classify pins: no newest release is available".to_owned());
    }
    ConsumerRow {
        consumer,
        pins: scan
            .pins
            .iter()
            .map(|pin| pin_row(pin, context.live, context.newest))
            .collect(),
        notes,
        problem,
    }
}

fn pin_row(pin: &Pin, live: &BTreeMap<String, CommitId>, newest: Option<&Release>) -> PinRow {
    PinRow {
        file: pin.file.clone(),
        line: pin.line,
        reference: pin.reference.clone(),
        kind: pin.kind.to_string(),
        locked: pin.locked.clone(),
        verdict: newest.map(|newest| verdict(pin, live, &newest.reference)),
    }
}

/// Compare one pin with the known live release refs and their newest member.
pub fn verdict(pin: &Pin, live: &BTreeMap<String, CommitId>, newest: &str) -> PinVerdict {
    if !pin.on_scheme {
        return PinVerdict::OffScheme;
    }
    let reference = format!("refs/heads/{}", pin.reference);
    let Some(commit) = live.get(&reference) else {
        return PinVerdict::UnknownName;
    };
    if pin.reference != newest {
        return PinVerdict::BehindName {
            newest: newest.to_owned(),
        };
    }
    match &pin.locked {
        Some(locked) if !commit.as_str().starts_with(locked) => PinVerdict::StaleLock {
            expected: commit.to_string(),
        },
        Some(_) | None => PinVerdict::Current,
    }
}

fn add_skew_note(consumers: &[ConsumerRow], notes: &mut Vec<String>) {
    let mut pins = BTreeMap::new();
    for consumer in consumers {
        for pin in &consumer.pins {
            if matches!(pin.verdict, Some(PinVerdict::OffScheme)) {
                continue;
            }
            pins.entry(pin.reference.clone())
                .or_insert_with(|| consumer.consumer.clone());
        }
    }
    if pins.len() > 1 {
        let detail = pins
            .into_iter()
            .map(|(reference, consumer)| format!("{consumer} pins {reference}"))
            .collect::<Vec<_>>()
            .join(", ");
        notes.push(format!("consumers disagree: {detail}"));
    }
}

/// The command outcome, with unanswered checks outranking actionable pin findings.
pub fn exit_for(report: &Report) -> Exit {
    if !report.problems.is_empty()
        || report
            .consumers
            .iter()
            .any(|consumer| consumer.problem.is_some())
    {
        return Exit::Incomplete;
    }
    if report
        .consumers
        .iter()
        .flat_map(|consumer| &consumer.pins)
        .any(|pin| {
            matches!(
                pin.verdict,
                Some(
                    PinVerdict::StaleLock { .. }
                        | PinVerdict::BehindName { .. }
                        | PinVerdict::UnknownName
                )
            )
        })
    {
        Exit::Findings
    } else {
        Exit::Ok
    }
}

/// Render a compact census for an interactive terminal.
pub fn render(report: &Report) -> String {
    let mut lines = report.newest.as_ref().map_or_else(
        || format!("{}: newest release unavailable", report.fork),
        |newest| {
            format!(
                "{}: newest {} @ {} (release remote, {})",
                report.fork,
                newest.reference,
                short_id(&newest.commit),
                newest.source
            )
        },
    );
    for problem in &report.problems {
        let _ = write!(lines, "\n  PROBLEM: {problem}");
    }
    for consumer in &report.consumers {
        let _ = write!(lines, "\n  {}", consumer.consumer);
        if let Some(problem) = &consumer.problem {
            let _ = write!(lines, ": PROBLEM: {problem}");
        } else {
            lines.push(':');
            for pin in &consumer.pins {
                let _ = write!(
                    lines,
                    "\n    {}:{}  {}  {}{}  {}",
                    pin.file,
                    pin.line,
                    pin.reference,
                    pin.kind,
                    pin.locked
                        .as_deref()
                        .map_or_else(String::new, |locked| format!("  @{}", short_id(locked))),
                    pin.verdict
                        .as_ref()
                        .map_or_else(|| "unclassified".to_owned(), render_verdict)
                );
            }
        }
        for note in &consumer.notes {
            let _ = write!(lines, "\n    {note}");
        }
    }
    for note in &report.notes {
        let _ = write!(lines, "\n  note: {note}");
    }
    lines
}

fn render_verdict(verdict: &PinVerdict) -> String {
    match verdict {
        PinVerdict::Current => "current".to_owned(),
        PinVerdict::StaleLock { expected } => {
            format!("stale lock: expected @{}", short_id(expected))
        }
        PinVerdict::BehindName { newest } => format!("behind: newest is {newest}"),
        PinVerdict::UnknownName => "unknown release".to_owned(),
        PinVerdict::OffScheme => "off-scheme reference".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::{
        ConsumerContext, Release, Report, consumer_row, consumer_slug_row, exit_for,
        local_remote_skew_note, render, verdict,
    };
    use crate::cli::Exit;
    use crate::consumer_pins::ConsumerHeadMemo;
    use crate::forge::{ConsumerHead, fake::FakeForge};
    use crate::ids::{CommitId, ReleaseScheme};
    use crate::pins::{Pin, PinKind, PinVerdict};

    fn pin(reference: &str, locked: Option<&str>) -> Pin {
        Pin {
            file: "uv.lock".to_owned(),
            line: 1,
            reference: reference.to_owned(),
            kind: PinKind::Frozen,
            locked: locked.map(str::to_owned),
            on_scheme: true,
            source: String::new(),
        }
    }

    fn release(reference: &str, commit: &str) -> Release {
        Release {
            reference: reference.to_owned(),
            commit: commit.to_owned(),
            source: "test".to_owned(),
        }
    }

    fn live() -> BTreeMap<String, CommitId> {
        BTreeMap::from([
            (
                "refs/heads/release/2026-08-04".to_owned(),
                CommitId::new("aabbccddeeff00112233445566778899"),
            ),
            (
                "refs/heads/release/2026-08-05".to_owned(),
                CommitId::new("11223344556677889900aabbccddeeff"),
            ),
        ])
    }

    #[test]
    fn a_malformed_lock_fragment_makes_the_consumer_incomplete_instead_of_current() {
        let consumer = tempfile::tempdir().expect("create consumer");
        std::fs::write(
            consumer.path().join("uv.lock"),
            "tool = { git = \"https://forge.invalid/acme/tool.git?rev=release%2F2026-08-05#12345\" }\n",
        )
        .expect("write malformed pin");
        let newest = release("release/2026-08-05", "11223344556677889900aabbccddeeff");
        let scheme = ReleaseScheme::Dated;
        let live = live();
        let forge = FakeForge::default();
        let heads = ConsumerHeadMemo::default();
        let context = ConsumerContext {
            fork: "demo",
            slug: Some("tool"),
            scheme: &scheme,
            repo_path: Path::new("/fork"),
            live: &live,
            newest: Some(&newest),
            forge: &forge,
            cache_root: None,
            heads: &heads,
        };

        let row = consumer_row(consumer.path(), &context);

        assert!(row.pins.is_empty());
        assert_eq!(
            row.problem.as_deref(),
            Some("uv.lock:1: malformed locked commit fragment `12345`")
        );
        assert!(!row.notes.iter().any(|note| note == "does not pin demo"));
    }

    #[test]
    fn verdict_marks_a_missing_release_name_unknown() {
        let found = verdict(
            &pin("release/2026-08-03", None),
            &live(),
            "release/2026-08-05",
        );

        assert_eq!(found, PinVerdict::UnknownName);
    }

    #[test]
    fn verdict_marks_a_known_older_release_behind() {
        let found = verdict(
            &pin("release/2026-08-04", None),
            &live(),
            "release/2026-08-05",
        );

        assert_eq!(
            found,
            PinVerdict::BehindName {
                newest: "release/2026-08-05".to_owned()
            }
        );
    }

    #[test]
    fn verdict_marks_an_unlocked_newest_release_current() {
        let found = verdict(
            &pin("release/2026-08-05", None),
            &live(),
            "release/2026-08-05",
        );

        assert_eq!(found, PinVerdict::Current);
    }

    #[test]
    fn verdict_accepts_a_short_lock_prefix_for_the_newest_release() {
        let found = verdict(
            &pin("release/2026-08-05", Some("11223344")),
            &live(),
            "release/2026-08-05",
        );

        assert_eq!(found, PinVerdict::Current);
    }

    #[test]
    fn verdict_marks_a_mismatched_newest_lock_stale() {
        let found = verdict(
            &pin("release/2026-08-05", Some("deadbeef")),
            &live(),
            "release/2026-08-05",
        );

        assert_eq!(
            found,
            PinVerdict::StaleLock {
                expected: "11223344556677889900aabbccddeeff".to_owned()
            }
        );
    }

    #[test]
    fn verdict_reports_an_off_scheme_pin_as_a_fact_not_a_missing_release() {
        let mut off_scheme = pin("agent-c-pin-0.4.47.dev7", Some("79ceb0832a61"));
        off_scheme.on_scheme = false;

        let found = verdict(&off_scheme, &live(), "release/2026-08-05");

        assert_eq!(found, PinVerdict::OffScheme);
    }

    #[test]
    fn an_off_scheme_lock_pin_is_reported_instead_of_reading_as_unpinned() {
        let consumer = tempfile::tempdir().expect("create consumer");
        std::fs::write(
            consumer.path().join("uv.lock"),
            "source = { git = \"https://forge.invalid/acme/tool.git?rev=acme-pin-0.4.47.dev7#79ceb0832a61a095c5c9819da2327675f5268753\" }\n",
        )
        .expect("write off-scheme pin");
        let newest = release("release/2026-08-05", "11223344556677889900aabbccddeeff");
        let scheme = ReleaseScheme::Dated;
        let live = live();
        let forge = FakeForge::default();
        let heads = ConsumerHeadMemo::default();
        let context = ConsumerContext {
            fork: "demo",
            slug: Some("tool"),
            scheme: &scheme,
            repo_path: Path::new("/fork"),
            live: &live,
            newest: Some(&newest),
            forge: &forge,
            cache_root: None,
            heads: &heads,
        };

        let row = consumer_row(consumer.path(), &context);

        assert!(!row.notes.iter().any(|note| note == "does not pin demo"));
        assert_eq!(row.pins.len(), 1);
        let reported = row.pins.first().expect("one off-scheme pin row");
        assert_eq!(reported.reference, "acme-pin-0.4.47.dev7");
        assert_eq!(reported.kind, "frozen");
        assert_eq!(
            reported.locked.as_deref(),
            Some("79ceb0832a61a095c5c9819da2327675f5268753")
        );
        assert_eq!(reported.verdict, Some(PinVerdict::OffScheme));
    }
    #[test]
    fn a_forge_failure_answers_from_cache_labeled_with_its_commit_and_exits_incomplete() {
        let cache = tempfile::tempdir().expect("create consumer cache");
        let consumer = "acme/consumer";
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let scheme = ReleaseScheme::Dated;
        let live = live();
        let newest = release("release/2026-08-05", "11223344556677889900aabbccddeeff");
        let priming_forge = FakeForge {
            heads: BTreeMap::from([(
                consumer.to_owned(),
                ConsumerHead {
                    branch: "main".to_owned(),
                    commit: commit.to_owned(),
                },
            )]),
            files: BTreeMap::from([(
                (consumer.to_owned(), commit.to_owned(), "uv.lock".to_owned()),
                "tool = { git = \"https://forge.invalid/acme/tool.git?rev=release%2F2026-08-05#112233445566\" }\n"
                    .to_owned(),
            )]),
            ..FakeForge::default()
        };
        let priming_heads = ConsumerHeadMemo::default();
        let priming_context = ConsumerContext {
            fork: "demo",
            slug: Some("tool"),
            scheme: &scheme,
            repo_path: Path::new("/fork"),
            live: &live,
            newest: Some(&newest),
            forge: &priming_forge,
            cache_root: Some(cache.path()),
            heads: &priming_heads,
        };
        let primed = consumer_slug_row(consumer, &priming_context);
        assert!(primed.problem.is_none(), "priming report: {primed:?}");

        let unavailable_forge = FakeForge {
            fail_consumer_head: true,
            ..FakeForge::default()
        };
        let unavailable_heads = ConsumerHeadMemo::default();
        let unavailable_context = ConsumerContext {
            fork: "demo",
            slug: Some("tool"),
            scheme: &scheme,
            repo_path: Path::new("/fork"),
            live: &live,
            newest: Some(&newest),
            forge: &unavailable_forge,
            cache_root: Some(cache.path()),
            heads: &unavailable_heads,
        };

        let row = consumer_slug_row(consumer, &unavailable_context);
        let report = Report {
            fork: "demo".to_owned(),
            newest: Some(newest),
            consumers: vec![row.clone()],
            notes: Vec::new(),
            problems: Vec::new(),
        };

        assert_eq!(row.pins.len(), 1, "cached pin: {row:?}");
        assert!(
            row.notes.iter().any(|note| note
                == "acme/consumer: forge unreachable; pins answered from cache at aaaaaaaaaaaa"),
            "notes: {:?}",
            row.notes
        );
        assert!(
            row.problem
                .as_deref()
                .is_some_and(|problem| problem.contains("acme/consumer: forge unreachable")),
            "problem: {:?}",
            row.problem
        );
        let rendered = render(&report);
        assert!(
            rendered.contains(
                "acme/consumer: forge unreachable; pins answered from cache at aaaaaaaaaaaa"
            ),
            "rendered: {rendered}"
        );
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn an_advanced_live_ref_with_the_same_name_records_view_disagreement() {
        let local = release("release/2026-08-05", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let remote = release("release/2026-08-05", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        assert_eq!(
            local_remote_skew_note(&local, &remote),
            Some(
                "local view has release/2026-08-05 @ aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, remote has release/2026-08-05 @ bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb — the checkout is behind or ahead of the remote"
                    .to_owned()
            )
        );
    }
}
