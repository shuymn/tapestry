//! The header: what a player shows before the skin is built, the user's
//! options, file choices and offsets among it.

use std::collections::BTreeSet;
use std::path::Path;

use skin_model::{Root, RootError};

use crate::error::{ImportError, InputLimit};
use crate::limits::{
    MAX_FILE_CHOICES, MAX_LIST_ENTRIES, MAX_OFFSETS, MAX_OPTION_ITEMS, MAX_OPTIONS,
};
use crate::resolve::{Definition, Resolved, resolve};
use crate::tree::{Kind, NONE};

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
/// wildcards, and the folder entries that match it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSpec {
    pub name: Option<String>,
    /// The path as written, relative to the definition's folder.
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
                if d.arena.node(list).kind != Kind::Array {
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
                    if d.arena.node(it).kind != Kind::Object {
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

/// The entries matching file choice `path` (node `id`): its folder,
/// joined to the definition's folder, listed through the skin folder's
/// root with what remains of the entry allowance, and filtered by its
/// last component. The whole name, as written and joined to the
/// definition's folder, has the form [`Root::check_name`] requires before
/// it is split. Only the last component may hold wildcards, and only `*`
/// (`|`, `?`, `[`, `]`, `{` and `}` are refused rather than given the
/// source's meanings). A folder that does not exist has no entries, as
/// the source skips it (SkinConfigurationView.java:383-386).
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
    let (folder, pattern) = path.rsplit_once(['/', '\\']).unwrap_or(("", path));
    if folder.contains('*') || pattern.contains(['|', '?', '[', ']', '{', '}']) {
        return Err(ImportError::Pattern { at: d.loc(id) });
    }
    let folded = pattern.to_ascii_lowercase();
    let dir = full
        .rsplit_once(['/', '\\'])
        .map_or(String::new(), |(dir, _)| dir.to_owned());
    let remaining = MAX_LIST_ENTRIES - d.run.entries;
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
        .filter(|e| matches(&e.name, &folded))
        .map(|e| e.name)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::matches;

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
