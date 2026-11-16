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
