//! `#WAV` names to files confined to the chart folder (design.md "Loading
//! Limits").

use std::path::{Path, PathBuf};

use crate::LoadError;

/// Extensions tried after the name as written.
const FALLBACK_EXTENSIONS: &[&str] = &["wav", "ogg", "flac", "mp3"];

/// Resolves `name` against the canonical chart folder `dir`.
///
/// Returns `Ok(None)` when no candidate file exists. The first candidate that
/// exists decides: inside the folder it is the result, outside it is an
/// error, so a link or junction cannot redirect a sound.
pub fn resolve(name: &str, line: usize, dir: &Path) -> Result<Option<PathBuf>, LoadError> {
    if !is_confined_name(name) {
        return Err(LoadError::SoundPathRejected { line });
    }
    let relative = PathBuf::from(name.replace('\\', "/"));
    let fallbacks = FALLBACK_EXTENSIONS
        .iter()
        .map(|ext| relative.with_extension(ext))
        .filter(|candidate| *candidate != relative);
    for candidate in std::iter::once(relative.clone()).chain(fallbacks) {
        let joined = dir.join(&candidate);
        let Ok(canonical) = joined.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(dir) {
            return Err(LoadError::SoundPathEscapes { line });
        }
        if canonical.is_file() {
            return Ok(Some(canonical));
        }
    }
    Ok(None)
}

/// Whether `name` is a relative path that cannot leave the folder by itself:
/// not empty, not rooted (`/`, `\`, UNC), no drive or stream colon, and no
/// `..` component.
fn is_confined_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['/', '\\'])
        && !name.contains(':')
        && name.split(['/', '\\']).all(|part| part != "..")
}

#[cfg(test)]
mod tests {
    use super::is_confined_name;

    #[test]
    fn names_leaving_the_folder_are_refused() {
        for bad in [
            "",
            "..\\x.wav",
            "../x.wav",
            "a/../../x.wav",
            "/etc/x.wav",
            "\\x.wav",
            "\\\\server\\share\\x.wav",
            "C:\\x.wav",
            "c:x.wav",
            "x.wav:stream",
        ] {
            assert!(!is_confined_name(bad), "{bad}");
        }
        for good in ["x.wav", "sub\\x.wav", "sub/x.wav", "a..b.wav", ".x.wav"] {
            assert!(is_confined_name(good), "{good}");
        }
    }
}
