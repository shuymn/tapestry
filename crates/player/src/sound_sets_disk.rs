//! The sound sets' recovery state on disk (design.md "Threat Model", T18),
//! with the count of a file's names that `game` cannot read on Windows: a
//! candidate that shares its file with another name, as a hard link does,
//! counts in its set's identity by its name alone.

use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use windows::Win32::Storage::FileSystem::{
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use game::sound_sets::{DiskFs, RecoveryFs};

use crate::chart_images::file_information;

/// The recovery state on disk, counting a candidate's names with
/// [`link_count`].
pub fn recovery_fs() -> Box<dyn RecoveryFs> {
    Box::new(DiskFs::new(link_count))
}

/// The count of names of the file at `path`, read from its open handle,
/// `None` when it cannot be opened or the system gives no count. Lookup
/// admits only a regular file, so the open never reaches a pipe or a
/// device. The handle asks only to read attributes, which no share mode
/// guards, so a file another process holds without sharing still counts
/// rather than changing its set's identity for that launch. The writers in
/// the data root count a link's names with it too ([`game::clear_link`]).
pub fn link_count(path: &Path) -> Option<u64> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .open(path)
        .ok()?;
    file_information(&file).map(|info| u64::from(info.nNumberOfLinks))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};

    use super::*;

    /// A plain file has one name, a hard link gives it a second, and a
    /// missing file has no count.
    // Covers: T18
    #[test]
    fn link_count_counts_hard_links() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("scratch.wav");
        fs::write(&file, b"sound").unwrap();
        assert_eq!(link_count(&file), Some(1));
        fs::hard_link(&file, dir.path().join("tapestry.log")).unwrap();
        assert_eq!(link_count(&file), Some(2));
        assert_eq!(link_count(&dir.path().join("missing.wav")), None);
    }

    /// A file another handle holds without sharing, as another process
    /// may, still gives its count: the count asks for no access to the
    /// file's data, which is what a share mode guards.
    // Covers: T18
    #[test]
    fn a_file_held_without_sharing_still_counts() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("scratch.wav");
        fs::write(&file, b"sound").unwrap();
        let _held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&file)
            .unwrap();
        assert!(File::open(&file).is_err(), "the hold blocks a read");
        assert_eq!(link_count(&file), Some(1));
    }

    /// The recovery state the player opens counts names with
    /// [`link_count`], not with the default that counts none.
    // Covers: T18
    #[test]
    fn the_recovery_state_counts_names() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("scratch.wav");
        fs::write(&file, b"sound").unwrap();
        let links = recovery_fs().links();
        assert_eq!(links(&file), Some(1));
        fs::hard_link(&file, dir.path().join("tapestry.log")).unwrap();
        assert_eq!(links(&file), Some(2));
    }
}
