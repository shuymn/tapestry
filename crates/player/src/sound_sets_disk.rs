//! The sound sets' recovery state on disk (design.md "Threat Model", T18),
//! with the count of a file's names that `game` cannot read on Windows: a
//! candidate that shares its file with another name, as a hard link does,
//! counts in its set's identity by its name alone.

use std::fs::File;
use std::path::Path;

use game::sound_sets::{DiskFs, RecoveryFs};

use crate::chart_images::file_information;

/// The recovery state on disk, counting a candidate's names with
/// [`link_count`].
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "nothing opens the sound sets' recovery state yet")
)]
pub fn recovery_fs() -> Box<dyn RecoveryFs> {
    Box::new(DiskFs::new(link_count))
}

/// The count of names of the file at `path`, read from its open handle,
/// `None` when it cannot be opened or the system gives no count. Lookup
/// admits only a regular file, so the open never reaches a pipe or a
/// device.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "nothing opens the sound sets' recovery state yet")
)]
pub fn link_count(path: &Path) -> Option<u64> {
    let file = File::open(path).ok()?;
    file_information(&file).map(|info| u64::from(info.nNumberOfLinks))
}

#[cfg(test)]
mod tests {
    use std::fs;

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
