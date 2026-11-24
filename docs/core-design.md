# How the core is built

A note for whoever reads or changes `crates/core` and `crates/store`. The
plan (Hearth repository, docs/sync.md) says what the engine must do; the
contract (docs/sync-api.md) says what Hearth answers; this says how the
code is arranged to do it, and why. What can be observed from outside is
docs/observation.md.

## The pieces

    ids, names, ignore      values: typed ids, SHA-256, names and their keys,
                            the corpus of nasty names, gitignore rules
    tree                    the rows: three pictures per node, the journal,
                            transfers, problems, the holding area
    store, memstore         the Store interface (reads by index, batches
                            all or nothing) and the in-memory store
    fs, remote, env         the outside world as traits: disk, Hearth,
                            clock, randomness, scheduling
    view                    places and paths of nodes, read from the rows
    plan, plan::table       the decision table as data, and the planner
                            that checks what the table assumes
    engine                  the state machine: start-up, Hearth's side,
                            the disk's side, planning passes, operations
    failure                 every error classified once
    observe, guarantees     what checkers can see, and the guarantees as
                            functions over it
    photo                   the photo uploader's shapes (no behaviour yet)
    testkit                 a small fake disk and fake Hearth (feature)

`crates/store` maps the rows to SQLite tables and passes the same
conformance suite as `MemStore`.

## Three pictures and a node

Every file or folder the engine has linked is a node, with up to three
rows: Hearth's picture (what the feed last said), the disk's picture
(what the last scan or the engine's own write saw), and the base (the
last state both agreed on). The node id is the engine's own, because an
item may be on one side only and both outside ids can change.

Rows keep parents, never paths. Hearth's picture keeps the parent's
Hearth id (so entries whose parent has not arrived yet, which the feed
allows, need no special table); the disk's picture and the base keep the
parent's node.

## The step

`Engine::step` does one unit of work and commits what it learned before
returning: a request to Hearth, a folder read, a file hashed, a planning
pass, or one durable step of one operation. Nothing runs between steps,
so a simulated crash is simply dropping the engine between two calls.

The order inside a step is fixed (start-up, Hearth, disk, plan, operate,
trim), and the only choice the engine makes between equals, which
operation to step next, goes to the `Scheduler`. Everything else is
determined by the store's rows, which are always read in key order.

## Planning

The planner runs only when every followed mount has answered `more:
false`, no listing or departed entry waits (contract 6.6 rule 7), and no
scan is half done (so an item moved between two folders is never seen as
deleted from one before it is found in the other).

For each dirty node it decides one step. The table (plan::table) names
the cell from how each side changed against the base; the planner turns
the cell into its first step and checks that step's preconditions:

- the target folder exists on that side, else wait for that folder;
- the name is free on that side, or its holder leaves (wait for it), or
  Hearth keeps the name and the local item steps aside as a conflict copy;
- the device can hold Hearth's name, Hearth accepts the local name;
- the mount's rights allow the write, else the read-only rows of 3.5;
- no cycle (Hearth's move wins, the local one is undone);
- not excluded by selective sync, end-to-end encryption or ignore rules.

When the step completes, the base records what both sides now agree on
(a place, a content, or both), the node is dirty again, and the next pass
finds it in a simpler cell. "Move, then download" is two passes; the
second never acts on a plan made before the first.

A node that cannot go yet is parked with the node it waits for (its
folder, a name's holder). The batch that changes that node wakes it, in
the same transaction. Parked nodes are not read by later passes, so a
first sync of a million files does not plan the waiting ones again after
every completed download.

## Operations

Each operation is a row written before its first effect and removed in
the transaction that changes the pictures. Its durable points (planned,
transferring, verified, swapping, swapped, finishing) are the plan's
3.10. Recovery after a crash keeps only the operations whose restart
would redo a transfer or leave a file between two places, and looks at
the world before continuing them: the file ids say whether a swap
happened; Hearth's session status says which chunks arrived; the place an
old file was being put aside is written down before the rename. Every
other operation is dropped and its node marked dirty: the pictures still
say what is to be done, and every remote write carries its condition, so
one that had in fact succeeded is seen as such through the feed.

Remote writes cannot be written without a condition: the request types
of the `Remote` trait have no "unconditional" form (I3 by the compiler).

## The store

One `apply` per batch, all or nothing. The SQLite store runs a batch as
one IMMEDIATE transaction with `synchronous=FULL`; `MemStore` keeps an
undo list and replays it backwards on failure. Both refuse the same
writes (a duplicate name key in a folder, a remote id on two nodes, a
node removed while an operation or a child holds it), which the
conformance suite checks on random batches against each other.

`Store::fail_writes_after` makes the next batch fail at its n-th write,
for crash tests and for the simulator's power cuts of the database.

## Decisions taken here

See docs/decisions.md.
