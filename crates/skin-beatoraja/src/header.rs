//! The header: what a player shows before the skin is built, the user's
//! options, file choices and offsets among it.

use std::collections::BTreeSet;
use std::path::Path;

use skin_model::{Root, RootError, ScreenKind};

use crate::error::{ImportError, InputLimit};
use crate::limits::{
    MAX_FILE_CHOICES, MAX_LIST_ENTRIES, MAX_OFFSETS, MAX_OPTION_ITEMS, MAX_OPTIONS,
};
use crate::resolve::{Definition, Resolved, resolve};
use crate::tree::{NONE, Scalar};

/// The skin sizes accepted, width by height: the 16:9 entries of the
/// source's size list (`Resolution.java:9-23` at `0.8.9`). The source
/// draws any other size as 1280×720 (JSONSkinLoader.java:265-271); here
/// it is refused.
pub const SIZES: [[u32; 2]; 5] = [
    [1280, 720],
    [1600, 900],
    [1920, 1080],
    [2560, 1440],
    [3840, 2160],
];

/// A definition's header, as resolved for one set of enabled options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// The screen the skin is made for, as its `type` names it.
    pub kind: ScreenKind,
    pub name: Option<String>,
    pub author: Option<String>,
    /// Width and height, one of [`SIZES`].
    pub size: [u32; 2],
    pub options: Vec<OptionSpec>,
    pub files: Vec<FileSpec>,
    pub offsets: Vec<OffsetSpec>,
}

/// A user option (`property`): a choice among items, each enabling one
/// option number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionSpec {
    pub name: Option<String>,
    pub items: Vec<OptionItem>,
    /// The default item's name.
    pub def: Option<String>,
}

/// An item of an option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionItem {
    pub name: Option<String>,
    /// The option number the item enables; 0 when omitted.
    pub op: i32,
}

/// A file choice (`filepath`): a path whose last component holds `*`
/// wildcards and may end in a `|tag|`, and the folder entries that match
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSpec {
    pub name: Option<String>,
    /// The path as written, relative to the definition's folder, tag
    /// included: the key source paths are substituted by.
    pub path: Option<String>,
    /// The default entry's name, or that name without its extension.
    pub def: Option<String>,
    /// The matching files and folders, sorted by name. Empty when the
    /// folder does not exist.
    pub candidates: Vec<String>,
}

/// A user offset (`offset`): which of a placement's parts the user may
/// move.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the source format's offset is six independent flags"
)]
pub struct OffsetSpec {
    pub name: Option<String>,
    pub id: i32,
    pub x: bool,
    pub y: bool,
    pub w: bool,
    pub h: bool,
    pub r: bool,
    pub a: bool,
}

/// Reads definition `file` in skin folder `root` and its header with no
/// option enabled, as the source's header pass does
/// (JSONSkinLoader.java:91-113). The definition is returned unresolved
/// so that [`resolve`] can run again with the options the user picked.
///
/// # Errors
///
/// What [`Definition::read`], [`resolve`] or [`Resolved::header`] refuse.
pub fn read_header(root: &Path, file: &str) -> Result<(Definition, Header), ImportError> {
    let mut def = Definition::read(root, file)?;
    let header = resolve(&mut def, &BTreeSet::new())?.header()?;
    Ok((def, header))
}

/// Whether `name` matches `pattern`, already in ASCII lower case, where
/// `*` stands for any run of bytes and every other byte matches itself
/// ignoring ASCII case.
fn wildcard(name: &[u8], pattern: &[u8]) -> bool {
    let (mut n, mut p) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == name[n].to_ascii_lowercase() => {
                n += 1;
                p += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// Whether a folder entry matches a file choice's pattern, `folded` to
/// ASCII lower case once for the choice. Case is ignored: the source
/// globs its `{lower,upper}` pattern (`SkinConfigurationView.java:374-389`
/// at `0.8.9`) on the target's case-insensitive file system. Only ASCII
/// letters fold, an intentional subset of that file system's folding.
fn matches(name: &str, folded: &str) -> bool {
    wildcard(name.as_bytes(), folded.as_bytes())
}

impl Resolved<'_> {
    /// The header's records decoded, each count checked before its record
    /// is added, and each file choice's folder listed.
    ///
    /// Records are read literally: ordinary fields, case-sensitive, the
    /// last of duplicates winning; a record that is no object has every
    /// field at its default. An option's `item` must be an array of
    /// objects, whose members, `include` included, are fields
    /// (`JsonSkin.PropertyItem` has no serializer). `category` is not
    /// read.
    ///
    /// # Errors
    ///
    /// A field of the wrong type or out of range, a file choice's path
    /// that is not a folder and one wildcard name or that leaves the skin
    /// folder, or a count over [`crate::limits`].
    pub fn header(&mut self) -> Result<Header, ImportError> {
        let d = &mut *self.def;
        let name = opt_string(d, self.name, "name")?;
        let author = opt_string(d, self.author, "author")?;

        let mut options = Vec::new();
        let mut item_count = 0;
        for &rec in &self.options {
            if options.len() >= MAX_OPTIONS {
                return Err(over(d, rec, InputLimit::Options));
            }
            let [name, item, def] = d.fields(rec, ["name", "item", "def"]);
            let mut items = Vec::new();
            if let Some(list) = item {
                if matches!(d.arena.scalar(list), Scalar::Callable) {
                    return Err(d.callable_error(list, "item"));
                }
                if !d.arena.is_array(list) {
                    return Err(d.type_error(list, "item", "an array"));
                }
                // Sibling links, so nothing is collected before the item
                // count is checked.
                let mut next = d.arena.node(list).first_child;
                while next != NONE {
                    let it = next;
                    next = d.arena.node(it).next_sibling;
                    if item_count >= MAX_OPTION_ITEMS {
                        return Err(over(d, it, InputLimit::OptionItems));
                    }
                    item_count += 1;
                    if matches!(d.arena.scalar(it), Scalar::Callable) {
                        return Err(d.callable_error(it, "item"));
                    }
                    if !d.arena.is_object(it) {
                        return Err(d.type_error(it, "item", "an object"));
                    }
                    let [n, op] = d.fields(it, ["name", "op"]);
                    items.push(OptionItem {
                        name: opt_string(d, n, "name")?,
                        op: op.map_or(Ok(0), |id| d.int(id, "op"))?,
                    });
                }
            }
            options.push(OptionSpec {
                name: opt_string(d, name, "name")?,
                items,
                def: opt_string(d, def, "def")?,
            });
        }

        let mut files = Vec::new();
        for &rec in &self.files {
            if files.len() >= MAX_FILE_CHOICES {
                return Err(over(d, rec, InputLimit::FileChoices));
            }
            let [name, path, def] = d.fields(rec, ["name", "path", "def"]);
            let name = opt_string(d, name, "name")?;
            let def = opt_string(d, def, "def")?;
            let (path, candidates) = match path {
                Some(id) => match d.string(id, "path")? {
                    Some(p) => {
                        let candidates = candidates(d, id, &p)?;
                        (Some(p), candidates)
                    }
                    None => (None, Vec::new()),
                },
                None => (None, Vec::new()),
            };
            files.push(FileSpec {
                name,
                path,
                def,
                candidates,
            });
        }

        let mut offsets = Vec::new();
        for &rec in &self.offsets {
            if offsets.len() >= MAX_OFFSETS {
                return Err(over(d, rec, InputLimit::Offsets));
            }
            let [name, id, x, y, w, h, r, a] =
                d.fields(rec, ["name", "id", "x", "y", "w", "h", "r", "a"]);
            let flag =
                |d: &Definition, f: Option<u32>, field| f.map_or(Ok(false), |id| d.bool(id, field));
            offsets.push(OffsetSpec {
                name: opt_string(d, name, "name")?,
                id: id.map_or(Ok(0), |id| d.int(id, "id"))?,
                x: flag(d, x, "x")?,
                y: flag(d, y, "y")?,
                w: flag(d, w, "w")?,
                h: flag(d, h, "h")?,
                r: flag(d, r, "r")?,
                a: flag(d, a, "a")?,
            });
        }

        Ok(Header {
            kind: self.kind,
            name,
            author,
            size: self.size,
            options,
            files,
            offsets,
        })
    }
}

fn over(d: &Definition, id: u32, limit: InputLimit) -> ImportError {
    ImportError::Input {
        at: Some(d.loc(id)),
        limit,
    }
}

fn opt_string(
    d: &mut Definition,
    id: Option<u32>,
    field: &'static str,
) -> Result<Option<String>, ImportError> {
    Ok(match id {
        Some(id) => d.string(id, field)?,
        None => None,
    })
}

/// File choice `path` without its tag. A last component may end in
/// `|tag|`, which only tells choices over one pattern apart: the source
/// lists the text up to the first `|` and after the last
/// (`SkinConfigurationView.java:374-381` at `0.8.9`) and keys the choice
/// by the whole path. The form taken is exactly two `|`, the closing one
/// last, and a tag that is not empty and holds no `/`, `\`, `*` or `|`,
/// so that the tag follows every `*` and the path's last `*` stays the
/// pattern's. `None` for any other `|`.
fn untagged(path: &str) -> Option<&str> {
    let Some(body) = path.strip_suffix('|') else {
        return (!path.contains('|')).then_some(path);
    };
    let (pattern, tag) = body.rsplit_once('|')?;
    (!tag.is_empty() && !tag.contains(['/', '\\', '*']) && !pattern.contains('|'))
        .then_some(pattern)
}

/// The entries matching file choice `path` (node `id`): its folder,
/// joined to the definition's folder, listed through the skin folder's
/// root with what remains of the entry allowance, and filtered by its
/// last component without its tag ([`untagged`]). The whole name, tag
/// included, as written and joined to the definition's folder, has the
/// form [`Root::check_name`] requires before it is split. Only the last
/// component may hold wildcards, and only `*` (`?`, `[`, `]`, `{` and `}`
/// in the pattern, and any `|` but a tag's, are refused rather than given
/// the source's meanings); all of this is checked before any folder is
/// listed. A folder that does not exist has no entries, as the source
/// skips it (SkinConfigurationView.java:383-386).
fn candidates(d: &mut Definition, id: u32, path: &str) -> Result<Vec<String>, ImportError> {
    let entry_dir = d.entry_dir();
    let full = if entry_dir.is_empty() {
        path.to_owned()
    } else {
        format!("{entry_dir}/{path}")
    };
    if let Err(source) = Root::check_name(path).and_then(|()| Root::check_name(&full)) {
        return Err(ImportError::Read {
            at: Some(d.loc(id)),
            name: path.to_owned(),
            source,
        });
    }
    let Some(untagged) = untagged(path) else {
        return Err(ImportError::Pattern { at: d.loc(id) });
    };
    let (folder, pattern) = untagged.rsplit_once(['/', '\\']).unwrap_or(("", untagged));
    if folder.contains('*') || pattern.contains(['?', '[', ']', '{', '}']) {
        return Err(ImportError::Pattern { at: d.loc(id) });
    }
    let folded = pattern.to_ascii_lowercase();
    let dir = full
        .rsplit_once(['/', '\\'])
        .map_or(String::new(), |(dir, _)| dir.to_owned());
    let remaining = MAX_LIST_ENTRIES - d.run.entries;
    #[cfg(test)]
    LISTED.with(|l| l.set([l.get()[0] + 1, l.get()[1]]));
    let listing = match d.root().list(&dir, remaining) {
        Ok(listing) => listing,
        Err(RootError::Missing | RootError::NotAFolder) => return Ok(Vec::new()),
        Err(RootError::TooManyEntries { .. }) => {
            return Err(ImportError::Input {
                at: Some(d.loc(id)),
                limit: InputLimit::ListEntries,
            });
        }
        Err(source) => {
            return Err(ImportError::Read {
                at: Some(d.loc(id)),
                name: dir,
                source,
            });
        }
    };
    d.run.entries += listing.enumerated;
    Ok(listing
        .entries
        .into_iter()
        .filter(|e| {
            #[cfg(test)]
            LISTED.with(|l| l.set([l.get()[0], l.get()[1] + 1]));
            matches(&e.name, &folded)
        })
        .map(|e| e.name)
        .collect())
}

#[cfg(test)]
thread_local! {
    /// This thread's file choice listings: the folders it asked the root
    /// to list, and the listed entries the pattern examined.
    static LISTED: std::cell::Cell<[usize; 2]> = const { std::cell::Cell::new([0, 0]) };
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Header, LISTED, matches, read_header};
    use crate::error::{ImportError, InputLimit};
    use crate::limits::MAX_LIST_ENTRIES;

    fn write(root: &Path, name: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    /// The header of `s.json` in `root` with file choices `paths`, and the
    /// listings it asked for and the listed entries its patterns examined.
    fn observed(root: &Path, paths: &[&str]) -> (Result<Header, ImportError>, [usize; 2]) {
        let records: Vec<String> = paths
            .iter()
            .map(|p| format!(r#"{{"path": "{p}"}}"#))
            .collect();
        let text = format!(r#"{{"type": 0, "filepath": [{}]}}"#, records.join(","));
        std::fs::write(root.join("s.json"), text).unwrap();
        LISTED.with(|l| l.set([0, 0]));
        let header = read_header(root, "s.json").map(|(_, h)| h);
        (header, LISTED.with(std::cell::Cell::get))
    }

    #[test]
    fn a_malformed_tag_is_refused_before_any_listing() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "bg/a.png");
        write(d.path(), "bg/b.txt");
        // The positive control: a well-formed tag over the same folder
        // lists it once and examines both entries.
        let (h, seen) = observed(d.path(), &["bg/*.png|x|"]);
        assert_eq!(h.unwrap().files[0].candidates, ["a.png"]);
        assert_eq!(seen, [1, 2]);
        for path in ["bg/*.png|x", "bg/*.png|x|y|", "bg/*.png||", "bg|x|/*.png"] {
            let (h, seen) = observed(d.path(), &[path]);
            assert!(matches!(h, Err(ImportError::Pattern { .. })), "{path}");
            assert_eq!(seen, [0, 0], "{path}");
        }
        // A malformed tag after a well-formed choice stops before its own
        // listing.
        let (h, seen) = observed(d.path(), &["bg/*.png|x|", "bg/*.png|y"]);
        assert!(matches!(h, Err(ImportError::Pattern { .. })));
        assert_eq!(seen, [1, 2]);
    }

    #[test]
    fn a_tagged_listing_past_the_allowance_examines_none_of_its_entries() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..MAX_LIST_ENTRIES / 2 {
            let ext = if i % 2 == 0 { "png" } else { "txt" };
            write(d.path(), &format!("a/{i}.{ext}"));
        }
        let tagged = ["a/*.png|x|", "a/*.png|y|"];
        let (h, seen) = observed(d.path(), &tagged);
        let h = h.unwrap();
        assert_eq!(h.files[0].candidates.len(), MAX_LIST_ENTRIES / 4);
        assert_eq!(h.files[0].candidates, h.files[1].candidates);
        assert_eq!(seen, [2, MAX_LIST_ENTRIES]);
        write(d.path(), "a/one-more.txt");
        let (h, seen) = observed(d.path(), &tagged);
        assert!(matches!(
            h,
            Err(ImportError::Input {
                limit: InputLimit::ListEntries,
                ..
            })
        ));
        // Both listings were asked for; only the first one's entries were
        // examined.
        assert_eq!(seen, [2, MAX_LIST_ENTRIES / 2 + 1]);
    }

    #[test]
    fn patterns_match_ignoring_ascii_case() {
        assert!(matches("a.png", "*.png"));
        assert!(matches("A.PNG", "*.png"));
        assert!(matches("background.Png", "*.png"));
        assert!(!matches("\u{c9}.png", "\u{e9}.png"));
        assert!(matches("bg", "*"));
        assert!(matches("x_1.png", "x_*.png"));
        assert!(!matches("y_1.png", "x_*.png"));
        assert!(matches("ab", "a*b*"));
        assert!(matches("abab", "*ab"));
        assert!(!matches("a.png.bak", "*.png"));
    }
}
