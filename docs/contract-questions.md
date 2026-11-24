# Questions about the contract

Where docs/sync-api.md (version 1, at the commit `CONTRACT` pins) is
unclear or seems wrong for the engine. Each question says what the engine
does meanwhile: always the conservative reading, the one that cannot lose
data. The contract is changed only in the Hearth repository (its section
0.3); when an answer lands there, the code and `CONTRACT` move together and
the question is closed here with the commit.

## Q1. The holding area needs to know whether Hearth still has some bytes

docs/sync.md 3.7 empties a holding-area entry after its days "only for
entries whose hash Hearth still has in files, versions or trash". The
contract's hash check (12) answers for current files only; nothing answers
for versions or trash.

Meanwhile: the engine records, when it holds something, why the bytes are
safe elsewhere: removed because Hearth deleted it on a mount with a trash
(`RemoteTrash`), replaced by a newer version on a mount with versions
(`RemoteVersions`), equal to what replaced it, or the current file of an
unselected folder. Those are emptied after 7 days. An entry without such a
reason (`Unproven`, for example a delete on external storage, which has no
trash) is emptied only when the hash check finds the bytes in a current
file; otherwise it stays. The person emptying Hearth's trash after the
engine recorded `RemoteTrash` is the limit docs/sync.md 2 already states.

Asked: should `/sync/hashes/have` (or a named feature) answer for versions
and trash too?

## Q2. A mount that disappears while the device holds it

Section 4 says a share the person leaves, or that is withdrawn, disappears
from the mounts list, and refers to docs/sync.md 4.2 for what the client
does with local changes not yet uploaded. The plan does not say it in
words the engine can follow.

Meanwhile: the engine leaves the mount's local folder exactly as it is,
forgets Hearth's picture and the base of everything in it, marks it not
synced, and lists it once ("no longer available; the local copy is
kept"). Nothing is deleted on either side, nothing is uploaded elsewhere.
If the mount comes back with the same id, it is listed again.

Asked: is that the intended behaviour, and should the client offer to move
not-yet-uploaded changes into the person's own files?

## Q3. A folder that moves between two mounts of the person

Section 6.6 rule 4 moves a held entry to the mount whose feed now shows
it. For a folder, the old mount marks it and everything below it
departed (rule 5), but the new mount's feed carries only the folder: what
is inside keeps numbers the new mount's feed will not send, and `list` is
documented as meaningless for an id the client already holds (6.3). Read
literally, the contents would be resolved as removed by rule 7.

Meanwhile: when a folder arrives in a mount by rule 4, the engine lists
it in that mount (6.4: "when in doubt, listing is always correct"), so its
contents arrive there by rule 4 too before rule 7 can resolve anything.

Asked: should 6.3 say that `list` is set (or must be acted on) when the
folder came from another mount of the same person?

## Q4. The size of a remote folder

docs/sync.md 3.13 asks before syncing new remote folders above 500 MB.
Folders carry no size on this API (5.1), so the client can only add up a
listing.

Meanwhile: step 1 does not ask by size (only mounts with `prompt: ask` wait
for the person). Not a risk to data; a missing courtesy.

Asked: is a folder's total size worth a field on `/sync/tree` for its root?

## Q6. An entry moved into a folder that is then trashed is never announced

6.10 says a folder deleted gives one `gone` for the folder and nothing for
what was in it, and 6.3 makes markers only for entries that leave the
mount. Take `a/f` moved to `b/f` (a move inside the mount: no marker) and
then `b` trashed (one marker, for `b`). A client whose cursor is before
both reads no upsert for `f` (it is no longer in the mount) and a `gone`
for `b`, which it may hold without `f` below it: it still holds `f` at
`a/f`. F5 ("holds exactly the entries that were in the mount") then does
not hold. The core's random tests found this.

Meanwhile: the core needs nothing new (a marker for `f` is applied by rule
5 like any other; one for an id it does not hold is ignored by rule 6).
The test kit's Hearth gives a marker, in the folder's change, to every
entry below the folder (a narrower rule, only entries newer than the
folder's own number, misses a folder moved after something moved into
it); the simulated Hearth should do the same until the contract says
otherwise.

Asked: should 6.10 say that trashing or deleting a folder also makes a
marker for every entry below it whose `seq` is above the folder's own (or,
simpler, for every entry below it)?

## Q5. A replace with an equal hash but a different mtime

9.2 says a replace whose hash is equal but whose mtime or exec differs is
a change of those alone. The engine never sends such a replace: it plans
an upload only when the content differs from the base. Changes of the
execute bit or the modification time alone are not synced by step 1 (the
plan's 3.9 syncs the execute bit; `PATCH /sync/files/{id}` is the way).
Recorded so it is a choice, not an oversight; no question for the contract.
