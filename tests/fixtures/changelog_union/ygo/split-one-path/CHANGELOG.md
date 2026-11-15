# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.55.1-sami.14] — 2026-11-14

Fixes a merge that let a struct parked over clocks a document holds take
those clocks, and cuts the answer to a SyncStep1 in a room holding parked
structs or deletions from the room's own structs too. No wire format changes.

### Changed

- **`crdt.SplitStateAsUpdateV1` cuts a document holding parked structs or
  deletions from the document's own structs too.** 1.55.1-sami.12 cut such a
  document from its encoded state with `SplitUpdateV1`, which decodes every
  struct of that state. It now writes each parked struct as
  `EncodeStateAsUpdateV1` merges it into the state, cut at its client's clock
  in the document, reads back only those, and merges them among themselves:
  the series is the same bytes as the split of the encoded state, and what it
  allocates for parked structs grows with what the document parks, not with
  its structs. For a room of 1,000,000 one-character structs, each typed at
  the front, holding one parked struct, one answer cut into updates of at most
  65,530 bytes took 6,005,774 to 6,005,994 allocations of 844 MB, with a peak
  of 464 to 474 MB of heap above the room, in 1.55.1-sami.13; it now takes 683
  to 735 allocations of 8.4 to 9.0 MB, as the same room with nothing parked
  does. The same room at 1,100,000 structs, past the 2^20 structs an update's
  decode accepts, was refused with `crdt: invalid update` and is now answered.
  `EncodeStateAsUpdateV1` and `EncodeStateAsUpdateV2` cut each parked struct
  the same way before they merge it into the state, which returns the same
  bytes.
- **`provider/websocket`: the documentation of `Server.OnStateAnswer` and
  `StateAnswerHook`** no longer says that a room holding parked structs or
  deletions decodes its state whole into a merge store before it cuts it.

### Fixed

- **`crdt`: a merge keeps an earlier update's struct where a later update
  names other content on its clocks.** A struct that names a struct no update
  carries parks, also over clocks the document holds, because the apply looks
  up what a struct names before it checks the struct's clocks, as Yjs does.
  `MergeUpdatesV1` and `MergeUpdatesV2` kept, on each clock, the struct that
  started first and was longest, so such a struct took those clocks from the
  structs that held them. A reader of the merge parked it again, and
  everything of its client after it: a stored log of 1,000 keystrokes and one
  such update read back as 3 characters. `EncodeStateAsUpdateV1`,
  `EncodeStateAsUpdateV2` and `SplitStateAsUpdateV1` merge a document's
  parked structs into its state, and read the document that applied that log
  back as 3 characters too. Where the struct the old rule keeps names another
  origin, right origin, parent or key than the struct of the earliest update
  on that clock, the earlier update's struct is now kept, and the other keeps
  only the clocks past it, as an apply of the updates in order integrates
  only those. A merge in which no two insertions claim one clock returns the
  same bytes as before. A merge cannot tell which struct a document
  integrated, so a log that stores such an update ahead of the keystrokes
  that take its clocks still merges to 3 characters; a document that applies
  that log in order holds all 1,000, and its encoded state now reads back as
  1,000.

## [1.55.1-sami.12] — 2026-11-13

Cuts a room's answer to a SyncStep1 from the room's own structs. No wire
format changes: every frame of an answer carries the bytes it carried in
1.55.1-sami.10.

### Added

- **`crdt.SplitStateAsUpdateV1(doc, sv, maxBytes)`** returns what
  `SplitUpdateV1` returns for `EncodeStateAsUpdateV1(doc, sv)`, byte for
  byte, without encoding the state or decoding it back: it reads the
  document's own structs under the document lock and writes each one once.
  For a document whose values came in updates, what it allocates grows with
  the updates it returns and the document's clients, not with its structs. A
  document holding parked structs or deletions is still cut from its encoded
  state, by `SplitUpdateV1`, because what it parks reaches its state only
  through a merge of its own. Both cuts place the structs with the walk
  1.55.1-sami.7 made linear, so a document whose structs chain across clients
  costs neither of them work that grows with the square of its clients.

### Changed

- **`sync.EncodeSyncStep2Split` cuts the state with `SplitStateAsUpdateV1`.**
  Measured on the server's answer to one join of a room of 20,000 characters
  typed into the middle (`BenchmarkJoinAnswer_FragmentedRoom`, y-websocket
  framing, 10 samples a side): 98 allocations of 1.585 MiB in all and 2.7 ms
  of CPU time, against 80,328 allocations of 9.133 MiB and 11.5 ms in
  1.55.1-sami.10. For a room of 16,000 small structs
  (`BenchmarkJoinAnswer_MixedRoom`): 5,154 allocations of 1.924 MiB and
  3.6 ms, against 107,320 of 9.107 MiB and 13.1 ms. The frames are the same:
  7 frames of 347,056 bytes in all, and 6 of 287,270. Each message of the
  answer is one allocation.
- **`crdt.SplitUpdateV1` reads a struct's dependencies as its encoding names
  them.** Cut at the receiver's clock, a struct names the clock before the
  cut as its origin, and no parent. For an update already cut at the
  receiver's state vector, as `EncodeStateAsUpdateV1` writes one, the series
  is unchanged. Given a whole update and a state vector, `SplitUpdateV1` now
  sends such a struct without waiting for the origin of the struct it was cut
  from, which the receiver does not wait for either.
- **`provider/websocket`: the documentation of `Server.OnStateAnswer` and
  `StateAnswerHook`** says what one answer holds: every frame of the room's
  state at once, and, in a room holding parked structs or deletions, that
  state decoded whole into a merge store before it is cut.

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
