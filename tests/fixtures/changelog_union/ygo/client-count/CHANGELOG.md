# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.55.1-sami.11] — 2026-11-14

Adds one method to `Doc`. No wire format changes, and nothing that exists
changes.

### Added

- **`crdt.Doc.ClientCount`** — how many clients the document holds structs
  from: every client with an item in the document, deleted or collected since
  included, so a client counts from the first of its writes that integrates
  and stays counted. A client that has only deleted is not counted, nor is a
  client whose every struct is parked until a missing dependency arrives, and
  a subdocument's clients count in its own `Doc`. It is the number of clients
  `StateVector` returns. A peer chooses its own client ID, and what a document
  costs follows its clients as well as its structs: encoding the state of
  100,000 one-character structs allocates 3.9 MB when one client wrote them
  and 30.0 MB when each came from a client of its own, and `StructCount` is
  100,000 for both. The call takes the document's read lock, so it must not be
  made inside a `Transact` callback; it allocates nothing and takes the same
  time however many clients the document holds.

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
