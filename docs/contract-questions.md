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

## From the reference model (crates/model), 2026-11-24

Questions on the plan (docs/sync.md) as much as on the contract, numbered
M1 and on. Each gives the reading the model uses meanwhile.

M1. Mass change: what counts. sync.md 3.11 pauses "a plan that would
delete or overwrite more than 50 files and more than a quarter of a
mount on either side". A plan is not observable from outside the
engine. Reading: the checker counts files an engine run removes (from
its disk, or from Hearth's tree) between two outside events concerning
that device, per mount, against the mount's files at the start of that
run; overwrites are not counted (an overwrite keeps the old content as
a version or a conflict copy). (crates/model/src/check.rs,
`check_mass_change`.) Should overwrites count, and is "a quarter of a
mount" the mount's file count or bytes?

M2. Conflict copy names: the extension. 3.5.1 says the extension is
never shortened but not where it starts. Reading: from the last dot,
when that dot is neither the first nor the last character; `.bashrc`
and `notes.` have none, `a.tar.gz` has `.gz`. (name.rs,
`split_extension`.)

M3. Conflict copy names: the device label. "Cut to 30 characters and
cleaned of characters any OS refuses." Reading: remove `< > : " / \ | ?
*` and control characters, cut to 30 characters (not bytes), then
remove trailing dots and spaces (Windows refuses a name ending so, and
the label is followed by a comma, but a reader would be surprised by
`Laptop. ,`). (name.rs, `conflict_device_label`.)

M4. Two local files that differ only by Unicode normalisation (an ext4
or NTFS volume holds both `Café` composed and decomposed) want one name
on Hearth, which stores NFC. The creation rows of 3.5 do not cover two
local items. Reading: the first by identity is uploaded under the NFC
name; the other stays local, untouched, listed as `name_clash`.
(reconcile.rs, `resolve_server_clashes`.)

M5. A replaced root confirmed. 3.11: a root without its marker pauses
and "nothing is deleted anywhere" until the person confirms. What does
"go on" do with an empty folder there? Reading: the folder becomes a
new, empty root; everything on Hearth is downloaded into it; nothing is
deleted on Hearth. (The model's test sync does this; the checkers only
require that nothing is deleted on Hearth while the root is replaced.)

M6. A share that leaves with local changes not uploaded. sync.md 4.2:
they "stay on disk, in a folder renamed '<name> (no longer shared)',
and are listed". Is that folder synced into the home mount (it sits in
the home part of the tree), or kept local and listed? Reading: kept
local, listed with `no_longer_shared`, never uploaded (the person
decides). Where exactly does it sit: beside where the mount was?
Reading: yes, at the mount's place, named after the mount's last name.

M7. Case folding. 3.8 says "Unicode simple folding". Which Unicode
version, and the Turkish rules (status T in CaseFolding.txt) left out?
Reading: status C and S, no T, so `İ` (U+0130) folds to nothing and
`ı` stays itself; `ß` does not fold to `ss`. (name.rs, `simple_fold`.)

M8. A file moved out of a folder that the other side deletes. The table
says local delete against remote move is "delete on Hearth", so a file
someone deliberately moved elsewhere is deleted because its old folder
was deleted on the other side. Reading: as the table says (the file is
in Hearth's trash). Intended?

M9. A read-only mount where Hearth deleted a file that this device
edited (B in a read-only mount): "uploaded again as a new file" cannot
happen. Not in the Permissions rows. Reading: the edited file stays
local, untouched, listed as `read_only_edit`. (reconcile.rs, stage 3.)

M10. Hearth renames a file to a name this device cannot hold (`a:b` on
Windows). 3.8 covers a remote file "not synced to a Windows device" but
not one the device already holds under an old name. Reading: the local
file stays at its old name, untouched, listed `device_refused:...`; the
synced tree keeps its old state so nothing is retried. (reconcile.rs,
stage 8.)

M11. In the creation row "file and folder: the file becomes a conflict
copy on its own side", when the file is Hearth's, whose device name and
time does the copy carry? Reading: the device that makes the copy, and
the file's own mtime. (reconcile.rs, `rename_to_copy`.)

M12. Holding area expiry: 3.7 empties entries "after 7 days ... or when
the volume runs short of space ... only for entries whose hash Hearth
still has". The checker requires the hash condition (through I1) but
not the age, since running short of space may empty earlier. Correct?
