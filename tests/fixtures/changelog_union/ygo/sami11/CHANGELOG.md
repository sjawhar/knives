# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.55.1-sami.11] — 2026-11-12

Adds one field to the apply report. No wire format changes, and a document
that sets no report callback behaves exactly as before.

### Added

- **`crdt.ApplyReport.StateBytes`** — the bytes the apply's transaction added
  to the document's encoded state, as `EncodeStateAsUpdateV1(doc, nil)`
  measures it, so an owner that encodes a room once and adds each report's
  `StateBytes` keeps the state's length, or more, without encoding again. It
  is a signed delta: below zero where a delete's content is collected, split
  halves merge back, or a drain drops a parked struct. `Bytes` is not this
  number (it is the content the budget charges), nor are the bytes an update
  logs, which fell short of the state: 24 frames of 900 KiB parked behind gaps
  logged nothing while the state reached 22,119,318 bytes, and a
  100,000-character run with every other character deleted logged 291,366
  bytes against a state of 1,019,797. On those shapes the report gives
  22,118,664 and 725,230, each the encode's growth to the byte. It is counted
  as the apply makes each change, not by encoding (53.0 ms on a 494,309-struct
  room): a keystroke on that room applies in 4.3 µs with the report against
  2.2 µs without, and in 4.0 µs with 100,000 structs parked.

  The contract is cumulative. The encode writes the state directly while
  nothing is pending (no struct and no delete parked) and merges the parked
  parts in otherwise, and the two forms write two classes of value at
  different lengths: an integer outside int32 that a float32 holds exactly,
  such as 2^40, is 9 bytes direct and 5 merged, and an integer in an embed or
  format value that a float64 rounds up a digit, such as 9999999999999999
  from a V2 update, is a byte longer merged. The report counts each at its
  longer form, so a total started from a room holding neither is never below
  the state in either form (588 bytes above at most, in 2,400 applies of
  parks, fills, overwrites, embeds and deletes). A base encode of a room that
  already holds such values is short by 4 bytes a wide integer if taken while
  anything is pending and a byte a rounding integer if taken while nothing
  is; a room loaded through V1 holds no rounding integer. Where the report
  cannot know a part of the
  state it takes the most the part can add: a byte for a delete range added
  to a client of more than 254 structs where the range count's varuint does
  not grow, and 2 × (varuint client + varuint clock) for each struct appended,
  split or merged at clocks a parked struct shares (730 bytes over on the
  worst of 12,000 applies of re-sent states).

  The report covers its own apply's transaction alone. On a document whose
  text root was read with `GetText`, the format cleanup that may follow a
  remote apply is a local transaction of its own, after the report: with GC
  off, a delete of "abc" reaching a room that holds a concurrent bold of its
  last character reports 9 while the state grows by 18, the cleanup's delete
  adding a 9-byte delete-set entry. An owner bounding state counts those
  commits too.

## [1.55.1-sami.10] — 2026-11-12

Fork release on [1.55.1-sami.9](https://github.com/sjawhar/ygo/releases/tag/v1.55.1-sami.9),
adding two opt-in features. No wire format changes, and a document that sets
no report callback and a server that sets no write timeout behave exactly as
before.

### Added

- **`crdt.WithApplyReport` / `Doc.SetApplyReport`** — a callback invoked as
  each remote apply commits, under the document lock and inside the apply's
  transaction, so an owner can track a room's size without keeping its own
  pre-apply estimate. It carries `Structs` (the document's structs, parked
  included) as a total and `Integrated`, `Bytes`, `Split` and `Decoded` as the
  apply's own deltas — the four counts `ApplyBudget` bounds. A refused apply
  reports nothing. The struct total is two counters rather than a whole-state
  encode, for the reason the field exists: on a 494,309-struct room the total
  is 13 µs against 53 ms to encode. The callback runs behind the commit, so a
  panic in it propagates to the caller without skipping the commit's own work;
  it must not read the document (it holds the write lock).
- **`provider/websocket` `Server.PeerWriteTimeout`** — the deadline on one
  write to a peer, in the shape of `PeerWriteQueueBytes`. Unset keeps the
  existing 10-second default, so no deployment's behaviour moves; an embedder
  with its own slow-peer bound can set a shorter one so a reader draining a
  backlog inside that bound is not dropped by the package default.

## [1.55.1-sami.9] — 2026-11-09

Fork release on [1.55.1-sami.8](https://github.com/sjawhar/ygo/releases/tag/v1.55.1-sami.8),
