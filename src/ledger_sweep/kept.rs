//! The sweep record: what sweeps did to this machine's ledger that nobody
//! running them saw, kept for `status` to say.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{Destination, SweepError, Tally, remove_if_present, sweep_record};
use crate::ids::UpstreamName;

/// What sweeps did to this machine's ledger that `status` reports, since a
/// sweep a write hands off has nowhere to say it ([`Pulled::backlog`]).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Kept {
    /// By fork: the repeated transitions every sweep so far has discarded.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    discarded: BTreeMap<String, Discards>,
    /// By fork directory, then by destination: the entries other machines
    /// sent through that destination that this machine does not read, as
    /// the last sweep to pull from it found them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    skipped: BTreeMap<String, BTreeMap<String, usize>>,
}

/// How many repeated transitions of one fork sweeps have discarded, and when
/// the newest went.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Discards {
    count: usize,
    last: String,
}

impl Kept {
    /// What `status` says about `fork` from this record.
    pub(super) fn notes_for(&self, fork: &UpstreamName) -> Vec<String> {
        let mut notes: Vec<String> = self
            .skipped
            .get(fork.as_str())
            .into_iter()
            .flatten()
            .map(|(through, &count)| {
                let (entries, are) = if count == 1 {
                    ("entry", "is")
                } else {
                    ("entries", "are")
                };
                format!(
                    "{count} {entries} other machines sent through {through} {are} not read \
                     here: this machine's repos.toml does not send {fork}'s ledger there"
                )
            })
            .collect();
        if let Some(discards) = self.discarded.get(fork.as_str()) {
            notes.push(format!(
                "sweeps here discarded {} repeated transition{} another machine had already \
                 committed, the newest at {}",
                discards.count,
                if discards.count == 1 { "" } else { "s" },
                discards.last
            ));
        }
        notes
    }

    /// The [`sweep_record`] beside `root`; empty when there is none.
    pub(super) fn read(root: &Path) -> Result<Self, SweepError> {
        let path = sweep_record(root);
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|source| SweepError::Record { path, source })
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(SweepError::Read { path, source }),
        }
    }

    /// Replace the [`sweep_record`] beside `root` whole, or remove it when
    /// there is nothing to keep.
    fn write(&self, root: &Path) -> Result<(), SweepError> {
        let path = sweep_record(root);
        let unwritable = |source| SweepError::Write {
            path: path.clone(),
            source,
        };
        if self.discarded.is_empty() && self.skipped.is_empty() {
            return remove_if_present(path);
        }
        let directory = path.parent().unwrap_or(root);
        let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(unwritable)?;
        serde_json::to_writer_pretty(&mut temporary, self)
            .map_err(|error| unwritable(std::io::Error::other(error)))?;
        temporary
            .persist(&path)
            .map_err(|error| unwritable(error.error))?;
        Ok(())
    }

    /// Take `skipped`, what a pull through `through` just found, as all that
    /// is skipped through it now.
    pub(super) fn pulled_through(&mut self, through: &str, skipped: &BTreeMap<String, usize>) {
        for by in self.skipped.values_mut() {
            by.remove(through);
        }
        for (fork, &count) in skipped {
            self.skipped
                .entry(fork.clone())
                .or_default()
                .insert(through.to_owned(), count);
        }
        self.skipped.retain(|_, by| !by.is_empty());
    }
}

/// Fold what the passes since the last call did into the [`sweep_record`]
/// beside `root`: each tally's discards are added to its fork's, and what
/// each destination's pull found skipped replaces what was kept for it; a
/// destination no longer here is dropped. Called with the lock held, so no
/// other sweep's fold is lost. A discard stays in its tally to fold again
/// until the record holding it is written.
pub(super) fn keep(
    root: &Path,
    destinations: &[Destination],
    tallies: &mut [Tally],
) -> Result<(), SweepError> {
    let mut kept = Kept::read(root)?;
    let now = jiff::Timestamp::now().to_string();
    let mut present = BTreeSet::new();
    for (destination, tally) in destinations.iter().zip(tallies.iter()) {
        let through = destination.describe();
        for (fork, &count) in &tally.unkept {
            let discards = kept.discarded.entry(fork.clone()).or_insert(Discards {
                count: 0,
                last: String::new(),
            });
            discards.count += count;
            discards.last.clone_from(&now);
        }
        if let Some(skipped) = &tally.skipped {
            kept.pulled_through(&through, skipped);
        }
        present.insert(through);
    }
    for by in kept.skipped.values_mut() {
        by.retain(|through, _| present.contains(through));
    }
    kept.skipped.retain(|_, by| !by.is_empty());
    kept.write(root)?;
    for tally in tallies {
        tally.unkept.clear();
    }
    Ok(())
}
