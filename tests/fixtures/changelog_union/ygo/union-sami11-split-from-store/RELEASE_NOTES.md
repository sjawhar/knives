## v1.55.1-sami.11

This fork release cuts a room's answer to a SyncStep1 from the room's own
structs. There are no wire format changes: every frame of an answer carries
the bytes it carried in v1.55.1-sami.10.

### What an answer cost

`sync.EncodeSyncStep2Split` cut a room's state by encoding it with
`crdt.EncodeStateAsUpdateV1` and handing the bytes to `crdt.SplitUpdateV1`,
which decoded them back into an item per struct, then encoded each struct
twice to size it and once more to place it. For a room of 20,000 characters
typed into the middle, one answer took 80,328 allocations of 9.133 MiB and
11.5 ms of CPU time.

### What it costs now

`crdt.SplitStateAsUpdateV1` cuts the same series from the document's own
structs, under the document lock: it writes each struct once, into the update
being packed, as the decode of the encoded state would have read it, and
`sync.EncodeSyncStep2Split` calls it. The same answer takes 98 allocations of
1.585 MiB and 2.7 ms of CPU time.

Measured with `BenchmarkJoinAnswer_FragmentedRoom` and
`BenchmarkJoinAnswer_MixedRoom`, which time the server's peer handling one
SyncStep1 and queueing the frames that answer it, 10 samples a side,
alternating, y-websocket framing, on a shared two-core machine; CPU time is
the process's user and system time across the same loop:

| | v1.55.1-sami.10 | this release |
| --- | --- | --- |
| 20,000 characters typed into the middle | | |
| allocations per answer | 80,328 | 98 |
| bytes allocated per answer | 9.133 MiB | 1.585 MiB |
| CPU time per answer | 11.5 ms | 2.7 ms |
| frames, and their bytes | 7, 347,056 | 7, 347,056 |
| 16,000 small structs: map entries, XML, formatted text | | |
| allocations per answer | 107,320 | 5,154 |
| bytes allocated per answer | 9.107 MiB | 1.924 MiB |
| CPU time per answer | 13.1 ms | 3.6 ms |
| frames, and their bytes | 6, 287,270 | 6, 287,270 |

The single SyncStep2 that v1.55.1-sami.3 replaced with the split, queued
through the same peer, takes 8 allocations of 1.03 MiB and 1.4 ms for the
first room, so the split answer still costs about twice its CPU time.

### Rooms holding parked structs

A room holding parked structs or deletions is still cut from its encoded
state, by `SplitUpdateV1`: what it parks reaches the state only through a
merge of its own, which the walk over the room's structs does not repeat, so
its answer costs what it cost in v1.55.1-sami.10. Both cuts place the structs
with the walk v1.55.1-sami.7 made linear, so a room whose structs chain
across clients costs neither of them work that grows with the square of its
clients.

### How the output was checked

`TestUnit_SplitStateAsUpdateV1_MatchesTheSplitOfTheEncodedState` compares
the two cuts byte for byte over nine rooms, five receivers and three update
sizes. The rooms hold typed text with and without deletions, interleaved
clients, a struct whose origin is another client's, structs longer than an
update, maps, nested types and XML, values the decode reads back as other Go
types, parked structs and deletions, and collected content.
`TestUnit_SplitStateAsUpdateV1_CutsARoomHoldingParkedStructsFromItsEncodedState`
checks that a room holding parked structs is answered with the split of its
encoded state, which the walk over its own structs does not produce, and that
a receiver applying that answer holds the room's text and state vector and
parks what the room parks. Two tests bound the walk's work on 4,000 chained
clients, one over decoded structs and one over a room's own.

### Also

`crdt.SplitUpdateV1` reads a struct's dependencies as its encoding names
them: cut at the receiver's clock, a struct names the clock before the cut as
its origin, and no parent. For an update already cut at the receiver's state
vector, as `EncodeStateAsUpdateV1` writes one, the series is unchanged. Given
a whole update and a state vector, it now sends such a struct without waiting
for the origin of the struct it was cut from, which the receiver does not wait
for either.

The documentation of `Server.OnStateAnswer` and `StateAnswerHook` says what
one answer holds: every frame of the room's state at once, and, in a room
holding parked structs or deletions, that state decoded whole into a merge
store before it is cut.

## v1.55.1-sami.11

This release adds one field to the apply report. There are no wire format
changes, and a document that sets no report callback behaves exactly as
before.

### The bytes each apply adds to the encoded state

`crdt.ApplyReport.StateBytes` is the bytes the apply's transaction added to
the document's encoded state, as `EncodeStateAsUpdateV1(doc, nil)` measures
it. An owner that encodes a room once and then adds each report's
`StateBytes` holds the state's length or more, and can refuse an update that
would take a room past a state cap without encoding the room again.

`Bytes` does not answer this: it is the content the budget charges, and a GC
struct or a split half adds state with no content. The bytes an update logs do
not answer it either, and they fell short of the state exactly where an owner
needed them: 24 frames of 900 KiB, each parked behind a gap of its own client,
fired no update and logged nothing while the encoded state reached 22,119,318
bytes; and a 100,000-character run with every other character deleted logged
291,366 bytes against an encoded state of 1,019,797, because each delete's
splits add item headers no update carries. On rooms of those shapes the report
gives 22,118,664 and 725,230 bytes, each the encode's growth to the byte.

It is a signed delta, below zero for an apply that leaves the state smaller: a
delete's content is written as its length once the commit collects it, split
halves merge back, and a parked struct a drain drops leaves the queue. A total
that could only grow would hold every deleted byte against a room for good.

It is counted as the apply makes each change rather than by encoding the
document, which costs 53.0 milliseconds on a 494,309-struct room. On the test
pod a keystroke on that room applies in 4.3 microseconds with the report
against 2.2 without, and in 4.0 with 100,000 structs parked; the first apply to
find a pending queue reads what it holds once, 2.6 milliseconds for those
100,000 structs.

The contract is cumulative, not per apply. The encode writes the state
directly while nothing is pending, with no struct and no delete parked
(`Doc.PendingStats`), and otherwise merges the parked parts into the
document's own encoding, and the two forms write two classes of value at
different lengths. An integer outside int32 that a float32 holds exactly,
such as 2^40, is 9 bytes in the direct form and 5 merged. An integer in an
embed or format value that a float64 rounds up to the next power of ten, such
as 9999999999999999, is its digits in the direct form and a digit longer
merged, where the document's V1 text is read back as a float64 - as every V1
decode and Yjs's `JSON.parse` read it, so only a document fed V2 or written
locally holds one, and none can whose writers send no embed and no format
value other than a string, a boolean or null. The report counts each value
at its longer form from the apply that brings it, so a total that starts from
a room holding neither class is never below the state in either form: it
runs 4 bytes a wide integer above a merged state and a byte a rounding
integer above a direct one, at most 588 bytes above across 2,400 applies of
parks, fills, overwrites, embeds and deletes. A base encode of a room that
already holds such values is short of the total's form by 4 bytes a wide
integer if taken while anything is pending, and by a byte a rounding integer
if taken while nothing is; a room loaded through V1 holds no rounding
integer, so its base taken while nothing is pending is exact.

Where the report cannot know a part of the state it takes the most that part
can add, never less:

- a delete range added to a client holding more than 254 structs counts one
  byte over where the varuint counting that client's ranges does not grow, as
  at the 1,001st range, which reports 14 against 13;
- a struct appended, split or merged at clocks a parked struct also holds, as
  where an update re-sends what the room holds parked, counts 2 × (the varuint
  of its client + the varuint of its clock) over, and the parked copy until it
  leaves the queue: across 12,000 applies of re-sent states the worst apply
  was 730 bytes over its growth;
- a client's section where what the queue holds of it is not known exactly,
  after a refused apply or a drain that dropped a struct, counts the skip,
  start clock and section header a park can add.

A park's merge into the queue can drop a parked struct for the copy an update
re-sends. The report reads what the queue holds before that merge; read after
it, the copy was counted against a gap the state never had, 2 bytes over on
10 of 6,000 applies of updates that arrive late and below the state on 129 of
12,000 applies of re-sent states.

The report covers its own apply's transaction alone. On a document whose text
root has been read with `GetText` (a raw root never runs it), a remote apply
that leaves format markers redundant is followed by the format cleanup, a
local transaction that commits after the report and emits its own update.
Measured with GC off: A deletes "abc" while B bolds its last character, and
the apply that brings A's delete to a room holding B's format reports 9 while
the state grows by 18, the cleanup's delete of the redundant marker adding a
delete-set entry of 9 bytes for B's client of 2^40 + 1. With GC on the same
case reports 6 and the state grows by 6. An owner bounding state counts those
commits too.

## v1.55.1-sami.10

This fork release retains the complete
[v1.55.1-sami.9 release](https://github.com/sjawhar/ygo/releases/tag/v1.55.1-sami.9)
and adds two opt-in features. There are no wire format changes, and a document
that sets no report callback and a server that sets no write timeout behave
exactly as before.

### A report of what each apply leaves the document holding

`crdt.WithApplyReport` and `Doc.SetApplyReport` register a callback that fires
as each remote apply commits — under the document lock, inside the apply's own
transaction. It lets an owner that admits or bounds remote updates track a
room's size from what the apply actually left, rather than keep a pre-apply
estimate of its own.

The report carries `Structs`, the document's structs with parked ones
included, as a running total; and `Integrated`, `Bytes`, `Split` and `Decoded`
as the apply's own deltas, which are the four counts `ApplyBudget` bounds. A
refused apply reports nothing: the document is unchanged, and its tally is in
the `RefusedUpdateError` instead. The total is two counters, not a whole-state
encode, because the encode does not scale: on a 494,309-struct room the total
is about 13 microseconds against 53 milliseconds to encode the state.

The callback runs behind the commit, so a panic in it reaches the caller
without skipping the commit's own cleanup, exactly as a panic in an observer
does. It must not read the document from inside the callback — it holds the
write lock, so a read would deadlock.

### A configurable peer write deadline
