//! The decision table of docs/sync.md 3.5, as data: one match over the two
//! sides' changes, exhaustive by the compiler. Adding a change kind without
//! deciding every pair does not build.
//!
//! This module knows nothing of stores, names or rights: given how each
//! side changed against the base, it names the table's cell and the next
//! single step that cell takes. The planner (the parent module) then
//! checks the facts the table assumes (a free name, a folder to put things
//! in, the rights, no cycle) and may wait or list a problem instead.

/// How one side of a file changed against the base (3.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileChange {
    /// Same as the base.
    None,
    /// Content differs (hash).
    Edit,
    /// Parent or name differs.
    Move,
    /// Edit and move.
    Both,
    /// In the base, absent now.
    Delete,
}

/// How one side of a folder changed against the base (folders have no
/// content of their own).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FolderChange {
    /// Same as the base.
    None,
    /// Parent or name differs.
    Move,
    /// In the base, absent now.
    Delete,
}

/// A cell of the files table, named as in 3.5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileCell {
    /// none / none.
    Nothing,
    /// none / edit.
    Download,
    /// none / move.
    MoveLocally,
    /// none / both: move, then download.
    MoveLocallyThenDownload,
    /// none / delete: remove locally through the holding area (H).
    RemoveLocally,
    /// edit / none.
    Upload,
    /// edit / edit: A.
    A,
    /// edit / move: upload to the id; move locally.
    UploadToIdAndMoveLocally,
    /// edit / both: A, at the remote's new place.
    AAtRemotePlace,
    /// edit / delete and both / delete: B, the edit is uploaded as new.
    B,
    /// move / none.
    MoveOnHearth,
    /// move / edit: move on Hearth; download.
    MoveOnHearthAndDownload,
    /// move / move: C.
    C,
    /// move / both: C, then download.
    CThenDownload,
    /// move / delete: D, the delete is applied locally (H).
    D,
    /// both / none: move and upload.
    MoveAndUpload,
    /// both / edit: A, at the local's new place.
    AAtLocalPlace,
    /// both / move: C, then upload.
    CThenUpload,
    /// both / both: C for the place, A for the content.
    CPlaceAContent,
    /// delete / none.
    DeleteOnHearth,
    /// delete / edit and delete / both: E, the edit is downloaded again.
    E,
    /// delete / move: delete on Hearth (D mirrored).
    DeleteOnHearthAfterMove,
    /// delete / delete: drop the node.
    DropNode,
}

/// The files table (3.5): rows are the local change, columns the remote.
pub fn file_cell(local: FileChange, remote: FileChange) -> FileCell {
    use FileCell as C;
    use FileChange::*;
    match (local, remote) {
        (None, None) => C::Nothing,
        (None, Edit) => C::Download,
        (None, Move) => C::MoveLocally,
        (None, Both) => C::MoveLocallyThenDownload,
        (None, Delete) => C::RemoveLocally,
        (Edit, None) => C::Upload,
        (Edit, Edit) => C::A,
        (Edit, Move) => C::UploadToIdAndMoveLocally,
        (Edit, Both) => C::AAtRemotePlace,
        (Edit, Delete) => C::B,
        (Move, None) => C::MoveOnHearth,
        (Move, Edit) => C::MoveOnHearthAndDownload,
        (Move, Move) => C::C,
        (Move, Both) => C::CThenDownload,
        (Move, Delete) => C::D,
        (Both, None) => C::MoveAndUpload,
        (Both, Edit) => C::AAtLocalPlace,
        (Both, Move) => C::CThenUpload,
        (Both, Both) => C::CPlaceAContent,
        (Both, Delete) => C::B,
        (Delete, None) => C::DeleteOnHearth,
        (Delete, Edit) => C::E,
        (Delete, Move) => C::DeleteOnHearthAfterMove,
        (Delete, Both) => C::E,
        (Delete, Delete) => C::DropNode,
    }
}

/// A cell of the folders table (3.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FolderCell {
    /// none / none.
    Nothing,
    /// none / move.
    MoveLocally,
    /// none / delete: F, applied child by child.
    RemoveLocallyF,
    /// move / none.
    MoveOnHearth,
    /// move / move: C.
    C,
    /// move / delete: G, F at the local new place.
    GLocalMoveRemoteDelete,
    /// delete / none: F, delete on Hearth.
    DeleteOnHearthF,
    /// delete / move: G, F at Hearth's new place.
    GLocalDeleteRemoteMove,
    /// delete / delete: drop the node.
    DropNode,
}

/// The folders table (3.5).
pub fn folder_cell(local: FolderChange, remote: FolderChange) -> FolderCell {
    use FolderCell as C;
    use FolderChange::*;
    match (local, remote) {
        (None, None) => C::Nothing,
        (None, Move) => C::MoveLocally,
        (None, Delete) => C::RemoveLocallyF,
        (Move, None) => C::MoveOnHearth,
        (Move, Move) => C::C,
        (Move, Delete) => C::GLocalMoveRemoteDelete,
        (Delete, None) => C::DeleteOnHearthF,
        (Delete, Move) => C::GLocalDeleteRemoteMove,
        (Delete, Delete) => C::DropNode,
    }
}

/// The one next step a file cell takes, given whether the two places agree
/// now and whether the two contents are equal. Composite cells take their
/// place step first: once it is done the base records the agreed place, the
/// node is planned again, and the cell it is in then is the content step
/// (for example none/both becomes none/edit after the local move).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileStep {
    /// Nothing to do.
    Nothing,
    /// Record the base as both sides are (they agree).
    Adopt,
    /// Download into the local place (A's conflict copy is made by the
    /// replace when the local content is not the base's: 3.7 step 5).
    Download,
    /// Download to Hearth's place, the local file being gone (E).
    DownloadNew,
    /// Upload to the existing id.
    Upload,
    /// Upload as a new file at the local place (B).
    UploadNew,
    /// Move locally to Hearth's place.
    MoveLocal,
    /// Move on Hearth to the local place.
    MoveRemote,
    /// Take the local file into the holding area.
    DeleteLocal,
    /// Delete on Hearth.
    DeleteRemote,
    /// Forget the node.
    Drop,
}

/// The next step of a file cell. `same_place`: the local and remote places
/// are equal now. `same_content`: the local and remote hashes are equal.
pub fn file_step(cell: FileCell, same_place: bool, same_content: bool) -> FileStep {
    use FileCell as C;
    use FileStep as S;
    match cell {
        C::Nothing => S::Nothing,
        C::Download => S::Download,
        C::MoveLocally | C::MoveLocallyThenDownload => S::MoveLocal,
        C::RemoveLocally | C::D => S::DeleteLocal,
        C::Upload => S::Upload,
        C::A => {
            if same_content {
                S::Adopt
            } else {
                S::Download
            }
        }
        C::UploadToIdAndMoveLocally | C::AAtRemotePlace => S::MoveLocal,
        C::B => S::UploadNew,
        C::MoveOnHearth | C::MoveOnHearthAndDownload | C::MoveAndUpload | C::AAtLocalPlace => {
            S::MoveRemote
        }
        C::C | C::CThenDownload | C::CThenUpload | C::CPlaceAContent => {
            // Same destination: the places agree, record it. Otherwise
            // Hearth's place wins and the local move is undone.
            if same_place { S::Adopt } else { S::MoveLocal }
        }
        C::DeleteOnHearth | C::DeleteOnHearthAfterMove => S::DeleteRemote,
        C::E => S::DownloadNew,
        C::DropNode => S::Drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE_CHANGES: [FileChange; 5] = [
        FileChange::None,
        FileChange::Edit,
        FileChange::Move,
        FileChange::Both,
        FileChange::Delete,
    ];

    /// The table as the plan prints it, row by row, so a typo in the match
    /// above cannot hide.
    #[test]
    fn every_file_cell_is_the_plans() {
        use FileCell::*;
        let expected = [
            [
                Nothing,
                Download,
                MoveLocally,
                MoveLocallyThenDownload,
                RemoveLocally,
            ],
            [Upload, A, UploadToIdAndMoveLocally, AAtRemotePlace, B],
            [MoveOnHearth, MoveOnHearthAndDownload, C, CThenDownload, D],
            [MoveAndUpload, AAtLocalPlace, CThenUpload, CPlaceAContent, B],
            [DeleteOnHearth, E, DeleteOnHearthAfterMove, E, DropNode],
        ];
        for (i, l) in FILE_CHANGES.iter().enumerate() {
            for (j, r) in FILE_CHANGES.iter().enumerate() {
                assert_eq!(file_cell(*l, *r), expected[i][j], "{l:?} / {r:?}");
            }
        }
    }

    #[test]
    fn every_folder_cell_is_the_plans() {
        use FolderCell::*;
        let ch = [FolderChange::None, FolderChange::Move, FolderChange::Delete];
        let expected = [
            [Nothing, MoveLocally, RemoveLocallyF],
            [MoveOnHearth, C, GLocalMoveRemoteDelete],
            [DeleteOnHearthF, GLocalDeleteRemoteMove, DropNode],
        ];
        for (i, l) in ch.iter().enumerate() {
            for (j, r) in ch.iter().enumerate() {
                assert_eq!(folder_cell(*l, *r), expected[i][j], "{l:?} / {r:?}");
            }
        }
    }

    /// The first step of each cell, place before content.
    #[test]
    fn steps_take_the_place_first() {
        use FileStep as S;
        let cases = [
            (FileCell::Nothing, true, true, S::Nothing),
            (FileCell::Download, true, false, S::Download),
            (FileCell::MoveLocally, false, true, S::MoveLocal),
            (
                FileCell::MoveLocallyThenDownload,
                false,
                false,
                S::MoveLocal,
            ),
            (FileCell::RemoveLocally, true, true, S::DeleteLocal),
            (FileCell::Upload, true, false, S::Upload),
            (FileCell::A, true, false, S::Download),
            (FileCell::A, true, true, S::Adopt),
            (
                FileCell::UploadToIdAndMoveLocally,
                false,
                false,
                S::MoveLocal,
            ),
            (FileCell::AAtRemotePlace, false, false, S::MoveLocal),
            (FileCell::B, false, false, S::UploadNew),
            (FileCell::MoveOnHearth, false, true, S::MoveRemote),
            (
                FileCell::MoveOnHearthAndDownload,
                false,
                false,
                S::MoveRemote,
            ),
            (FileCell::C, true, true, S::Adopt),
            (FileCell::C, false, true, S::MoveLocal),
            (FileCell::CThenDownload, false, false, S::MoveLocal),
            (FileCell::D, false, true, S::DeleteLocal),
            (FileCell::MoveAndUpload, false, false, S::MoveRemote),
            (FileCell::AAtLocalPlace, false, false, S::MoveRemote),
            (FileCell::CThenUpload, false, false, S::MoveLocal),
            (FileCell::CPlaceAContent, true, false, S::Adopt),
            (FileCell::DeleteOnHearth, false, false, S::DeleteRemote),
            (FileCell::E, false, false, S::DownloadNew),
            (
                FileCell::DeleteOnHearthAfterMove,
                false,
                false,
                S::DeleteRemote,
            ),
            (FileCell::DropNode, false, false, S::Drop),
        ];
        for (cell, place, content, step) in cases {
            assert_eq!(file_step(cell, place, content), step, "{cell:?}");
        }
    }
}
