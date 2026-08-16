//! `knives notch`: read what happened here, or add to it.
//!
//! Two moods on one command, split by `-m`, because reading and writing the same
//! record are the same act from opposite ends. Reading is intentional: nothing
//! injects notches into a session, so the bare form has to answer the question an
//! agent actually has — what happened in this fork lately — rather than making
//! them name a subject they do not know yet.

use crate::cli::Exit;
use crate::config::{default_config_path, load};
use crate::ids::{BranchName, BranchTarget, RepoName};
use crate::ledger::{
    Draft, Entry, Filter, Kind, Ledger, LedgerError, Scribe, body_human_text, inline_human_text,
    select,
};
use crate::store::{Store, default_state_path};

/// How many entries a bare read shows.
///
/// A cap on the unfiltered view only: a reader who named a subject or a pull
/// request asked for that chronology and gets all of it.
const RECENT: usize = 20;

#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
pub enum Report {
    Read {
        repo: String,
        entries: Vec<Entry>,
        matched: usize,
    },
    Written {
        #[serde(skip)]
        repo: String,
        wrote: Entry,
    },
}

/// What one invocation asks for.
#[derive(Debug)]
pub struct Request<'a> {
    pub repo: &'a RepoName,
    pub subject: Option<&'a str>,
    /// Present for a write, absent for a read.
    pub message: Option<&'a str>,
    pub evidence: &'a [String],
    pub pr: Option<u64>,
}

/// Entries for one repository, filtered.
pub fn read(ledger: &Ledger, repo: &RepoName, filter: &Filter<'_>) -> Result<Report, LedgerError> {
    let entries = ledger.entries()?;
    let (selected, matched) = select(&entries, filter);
    Ok(Report::Read {
        repo: repo.to_string(),
        entries: selected.into_iter().cloned().collect(),
        matched,
    })
}

pub fn render(report: &Report) -> String {
    match report {
        Report::Written { repo, wrote } => wrote_line(repo, wrote),
        Report::Read {
            repo,
            entries,
            matched,
        } => {
            if entries.is_empty() {
                return format!("{repo}  no notches yet");
            }
            let mut lines = vec![format!("{repo}  {matched} notch(es)")];
            if *matched > entries.len() {
                lines.push(format!(
                    "  showing the newest {} of {}",
                    entries.len(),
                    matched
                ));
            }
            for entry in entries {
                lines.push(format!(
                    "  {}  {:<5}  {}",
                    entry.ts,
                    entry.kind,
                    heading(entry)
                ));
                lines.push(format!(
                    "    {}",
                    body_human_text(&entry.text).replace('\n', "\n    ")
                ));
                if !entry.evidence.is_empty() {
                    let evidence: Vec<String> = entry
                        .evidence
                        .iter()
                        .map(|item| inline_human_text(item))
                        .collect();
                    lines.push(format!("    evidence  {}", evidence.join(", ")));
                }
            }
            lines.join("\n")
        }
    }
}

/// Subject, anchor and stated pull request on one line, each omitted when absent.
fn heading(entry: &Entry) -> String {
    let subject = entry
        .subject
        .as_deref()
        .map_or_else(|| "(this repo)".to_owned(), inline_human_text);
    let mut parts = vec![subject];
    if let Some(anchor) = &entry.anchor {
        parts.push(format!("@{}", short(anchor)));
    }
    if let Some(number) = entry.pr {
        parts.push(format!("#{number}"));
    }
    parts.join("  ")
}

fn wrote_line(repo: &str, entry: &Entry) -> String {
    let subject = entry
        .subject
        .as_deref()
        .map_or_else(|| format!("{repo} itself"), inline_human_text);
    entry.anchor.as_deref().map_or_else(
        || format!("notched {subject}"),
        |anchor| format!("notched {subject} at {}", short(anchor)),
    )
}

/// Short form for display. Full ids are correct and unreadable.
fn short(id: &str) -> String {
    inline_human_text(id).chars().take(12).collect()
}

pub fn run(request: &Request<'_>, output: crate::cli::Output) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(request.repo) else {
        let known: Vec<String> = registry.names().map(|name| name.to_string()).collect();
        eprintln!("unknown repo {}; known: {}", request.repo, known.join(", "));
        return Ok(Exit::Usage);
    };
    let ledger = Ledger::for_repo(request.repo);
    let report = match request.message {
        Some(text) => {
            // The store is read, never written: a notch changes no intent, and a
            // ledger append needs no store lock.
            let store = Store::open(default_state_path())?;
            let pr = request.pr.or_else(|| {
                request.subject.and_then(|subject| {
                    store.tracked_pull(&BranchTarget::new(
                        request.repo.clone(),
                        BranchName::new(subject),
                    ))
                })
            });
            let owner = crate::commands::claim::current_owner(&std::env::current_dir()?)?;
            let scribe = Scribe::new(ledger, request.repo.clone(), entry.path.clone(), owner);
            let written = scribe.record(&Draft {
                subject: request.subject,
                kind: Kind::Note,
                text: text.to_owned(),
                evidence: request.evidence.to_vec(),
                pr,
            })?;
            Report::Written {
                repo: request.repo.to_string(),
                wrote: written,
            }
        }
        None => read(
            &ledger,
            request.repo,
            &Filter {
                subject: request.subject,
                pr: request.pr,
                limit: (request.subject.is_none() && request.pr.is_none()).then_some(RECENT),
            },
        )?,
    };
    if let Some(payload) = crate::cli::machine_payload(output, &report)? {
        println!("{payload}");
    } else {
        println!("{}", render(&report));
    }
    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn entry(subject: Option<&str>, kind: Kind, text: &str) -> Entry {
        Entry {
            ts: "2026-08-15T22:14:03Z".to_owned(),
            owner: "ses_fff688".to_owned(),
            subject: subject.map(str::to_owned),
            kind,
            text: text.to_owned(),
            evidence: Vec::new(),
            anchor: Some("6c42fe71aaaaaaaa".to_owned()),
            pr: Some(1157),
        }
    }

    #[test]
    fn a_read_names_the_subject_the_anchor_and_the_stated_pull_request() {
        let report = Report::Read {
            repo: "a-repo".to_owned(),
            entries: vec![entry(
                Some("feat/log-queue"),
                Kind::Note,
                "superseded by #1157",
            )],
            matched: 1,
        };
        let text = render(&report);
        assert!(text.contains("feat/log-queue"), "was: {text}");
        assert!(text.contains("@6c42fe71aaaa"), "was: {text}");
        assert!(text.contains("#1157"), "was: {text}");
        assert!(text.contains("note"), "was: {text}");
        assert!(text.contains("superseded by #1157"), "was: {text}");
    }

    #[test]
    fn human_rendering_escapes_control_characters_from_ledger_fields() {
        let report = Report::Read {
            repo: "a-repo".to_owned(),
            entries: vec![Entry {
                subject: Some("feat/\u{1b}queue".to_owned()),
                evidence: vec!["review\u{1b}link\rnext".to_owned()],
                anchor: Some("6c42\u{1b}anchor\r".to_owned()),
                ..entry(Some("feat/ignored"), Kind::Note, "parked\u{1b}now\ragain")
            }],
            matched: 1,
        };

        let text = render(&report);
        assert!(!text.contains('\u{1b}'), "was: {text:?}");
        assert!(!text.contains('\r'), "was: {text:?}");
        assert!(text.contains('\u{fffd}'), "was: {text:?}");
    }

    #[test]
    fn body_rendering_preserves_line_breaks_and_escapes_other_controls() {
        let report = Report::Read {
            repo: "a-repo".to_owned(),
            entries: vec![entry(
                Some("feat/alpha"),
                Kind::Note,
                "one\ntwo\u{1b}three\rfour",
            )],
            matched: 1,
        };

        let text = render(&report);
        assert!(text.contains("    one\n    two"), "was: {text:?}");
        assert!(!text.contains('\u{1b}'), "was: {text:?}");
        assert!(!text.contains('\r'), "was: {text:?}");
        assert!(text.contains('\u{fffd}'), "was: {text:?}");
    }

    #[test]
    fn a_truncated_read_says_how_many_it_did_not_show() {
        // A window that does not announce itself is how a reader concludes a
        // branch has no older history.
        let report = Report::Read {
            repo: "a-repo".to_owned(),
            entries: vec![entry(Some("feat/alpha"), Kind::Event, "claimed")],
            matched: 57,
        };
        assert!(
            render(&report).contains("showing the newest 1 of 57"),
            "was: {}",
            render(&report)
        );
    }

    #[test]
    fn an_empty_read_says_so_rather_than_printing_a_bare_repo_name() {
        let report = Report::Read {
            repo: "a-repo".to_owned(),
            entries: Vec::new(),
            matched: 0,
        };
        assert_eq!(render(&report), "a-repo  no notches yet");
    }

    #[test]
    fn a_repo_level_entry_is_headed_by_the_repo_rather_than_an_empty_subject() {
        let report = Report::Written {
            repo: "a-repo".to_owned(),
            wrote: Entry {
                anchor: None,
                pr: None,
                ..entry(None, Kind::Note, "the fork needs a cut")
            },
        };
        assert_eq!(render(&report), "notched a-repo itself");
    }
}
