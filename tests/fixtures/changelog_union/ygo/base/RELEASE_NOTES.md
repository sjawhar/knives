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
