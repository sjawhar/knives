## v1.55.1-sami.14

This fork release fixes a merge that let a struct parked over clocks a
document holds take those clocks, and cuts the answer to a SyncStep1 in a
room holding parked structs or deletions from the room's own structs, as
v1.55.1-sami.12 cut every other room's. There are no wire format changes.

### A struct parked over clocks a document holds

**Who is affected: stores that merge the updates a document took, and
documents that hold a struct parked over clocks they hold.** A merge in which
no two insertions claim one clock returns the same bytes as before.

A struct that names a struct the document lacks parks until that struct
arrives, even when the document holds the struct's own clocks, as in Yjs.
`MergeUpdatesV1` and `MergeUpdatesV2` kept, on each clock, the struct that
started first and was longest, so such a struct took the clocks from the
structs that held them. A reader of the merge parked it again, and everything
of its client after it. A stored log of 1,000 keystrokes and one
1,000-character update of that kind read back as 3 characters.
`EncodeStateAsUpdateV1`, `EncodeStateAsUpdateV2` and `SplitStateAsUpdateV1`,
which merge a document's parked structs into its state, read the document
that applied that log back as 3 characters as well.

Where the struct the old rule keeps names another origin, right origin,
parent or key than the struct the earliest update carried on that clock, the
two are different insertions, and the earlier update's struct is now kept.
The other keeps only the clocks past it, as an apply of the updates in order
integrates only those. The log above now reads back as 1,000 characters
through the merge and through each of the three.

A merge cannot tell which struct a document integrated, so a log that stores
the update after the first 3 keystrokes and ahead of the other 997 still
merges to 3 characters. A document that applies that log in order holds all
1,000 and parks the struct; its encoded state, in V1 and V2, and the split of
it now read back as 1,000 characters, where they read back as 3.

### The answer to a join in a room holding parked structs

v1.55.1-sami.12 cut a room holding parked structs or deletions from its
encoded state with `crdt.SplitUpdateV1`, which decodes every struct of that
state, because what the room parks reaches the state through a merge of its
own. `crdt.SplitStateAsUpdateV1` now cuts such a room from its own structs
too: it writes each parked struct as `EncodeStateAsUpdateV1` merges it into
the state, cut at its client's clock in the room, reads back only those, and
merges them among themselves. The answer is the same bytes, and what it
allocates for parked structs grows with what the room parks, not with its
structs. `EncodeStateAsUpdateV1` and `EncodeStateAsUpdateV2` cut each parked
struct the same way before the merge, which leaves the state they return the
same bytes.

Measured on a room of 1,000,000 one-character structs, each typed at the
front, holding one parked struct, three runs a side, one answer cut into
updates of at most 65,530 bytes; the peak is of heap above the room, sampled
every 2 ms:

| | v1.55.1-sami.13 | this release |
| --- | --- | --- |
| allocations per answer | 6,005,774 to 6,005,994 | 683 to 735 |
| bytes allocated per answer | 843.8 to 844.4 MB | 8.4 to 9.0 MB |
| peak heap above the room | 464.3 to 473.5 MB | 8.4 to 9.0 MB |
| updates in the answer | 108 | 108 |

The same room with nothing parked takes 618 to 685 allocations of 8.4 to
9.0 MB in v1.55.1-sami.13. Grown to 1,100,000 structs, past the 2^20 structs
an update's decode accepts, the room with one struct parked was refused with
`crdt: invalid update`; it is now answered in 119 updates.

The documentation of `Server.OnStateAnswer` and `StateAnswerHook` no longer
says that such a room decodes its state whole into a merge store.

### How the output was checked

`TestUnit_SplitStateAsUpdateV1_MatchesTheSplitOfTheEncodedState` compares
`SplitStateAsUpdateV1` with the split of the encoded state byte for byte over
15 rooms, five receivers each and a sixth for each room holding parked
structs or deletions, and three update sizes. Seven of the rooms hold parked
structs or deletions: parked structs and deletions, structs and deletions
parked behind a clock gap, parked map entries and a parked nested map, and
four rooms each holding a struct parked over clocks it holds.
`TestUnit_SplitStateAsUpdateV1_AReceiverEndsHoldingWhatAParkedRoomHolds`
applies the answer to an empty document for each of the seven and requires
the room's state vector, content and parked clocks, with nothing parked
before the last update; it replaces
`TestUnit_SplitStateAsUpdateV1_CutsARoomHoldingParkedStructsFromItsEncodedState`.
`TestUnit_PendingQueue_HoldsEachClockOfAClientOnce` checks what the merge of
the parked structs relies on: the pending queue holds each clock of a client
once. `TestUnit_EncodeStateAsUpdate_ParkedOverHeldClocks`,
`TestUnit_MergeUpdates_ParkedOverHeldClocksReadsBackWhole` and
`TestUnit_EncodeStateAsUpdate_StructStoredAheadOfTheKeystrokesReadsBackWhole`
check the merge and the reads above.

## v1.55.1-sami.12

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
