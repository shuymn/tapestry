//! Why a definition was refused, and where.

use std::fmt;

use skin_model::RootError;

use crate::limits::{
    MAX_DEPTH, MAX_EXPANDED, MAX_FILE_BYTES, MAX_FILE_CHOICES, MAX_INCLUDE_DEPTH,
    MAX_INCLUDE_PARSES, MAX_INPUT_BYTES, MAX_LIST_ENTRIES, MAX_NODES, MAX_OFFSETS,
    MAX_OPTION_ITEMS, MAX_OPTIONS, MAX_STRING_BYTES,
};

/// A place in a definition file: its name relative to the skin folder,
/// and a 1-based line and column (in characters).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loc {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

impl fmt::Display for Loc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.col)
    }
}

/// What the reader did not accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    /// Not UTF-8 (an intentional policy: the source reads with the
    /// platform's charset).
    InvalidUtf8,
    /// A character that cannot start or continue what is expected here.
    Unexpected,
    /// The input ends inside a value, an object or an array.
    UnexpectedEnd,
    /// A quoted string with no closing quote.
    UnterminatedString,
    /// A `/*` comment with no `*/`.
    UnterminatedComment,
    /// A backslash not followed by `"`, `\`, `/`, `b`, `f`, `n`, `r`, `t`
    /// or `u` and four hex digits.
    InvalidEscape,
    /// A `\u` escape of half a surrogate pair (an intentional policy:
    /// decoded strings are UTF-8).
    LoneSurrogate,
    /// A member name not followed by `:`.
    ExpectedColon,
    /// Two members or items on one line with no comma between them.
    ExpectedSeparator,
    /// A closing bracket of the other kind.
    MismatchedClose,
    /// Something after the root value.
    TrailingContent,
}

impl fmt::Display for Syntax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidUtf8 => "the file is not UTF-8",
            Self::Unexpected => "unexpected character",
            Self::UnexpectedEnd => "unexpected end of the file",
            Self::UnterminatedString => "a string with no closing quote",
            Self::UnterminatedComment => "a comment with no closing */",
            Self::InvalidEscape => "an invalid escape",
            Self::LoneSurrogate => "half a surrogate pair",
            Self::ExpectedColon => "a name with no colon after it",
            Self::ExpectedSeparator => "two values on one line with no comma between them",
            Self::MismatchedClose => "a closing bracket of the wrong kind",
            Self::TrailingContent => "content after the root value",
        })
    }
}

/// An input bound ([`crate::limits`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputLimit {
    FileBytes,
    InputBytes,
    Depth,
    Nodes,
    StringBytes,
    DecodedBytes,
    Expanded,
    IncludeDepth,
    IncludeParses,
    ListEntries,
    Options,
    OptionItems,
    FileChoices,
    Offsets,
}

impl fmt::Display for InputLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileBytes => write!(f, "a definition file over {MAX_FILE_BYTES} bytes"),
            Self::InputBytes => write!(f, "definition files over {MAX_INPUT_BYTES} bytes in all"),
            Self::Depth => write!(f, "nesting deeper than {MAX_DEPTH}"),
            Self::Nodes => write!(f, "more than {MAX_NODES} values"),
            Self::StringBytes => write!(f, "a string over {MAX_STRING_BYTES} bytes"),
            Self::DecodedBytes => f.write_str("too many bytes of decoded strings"),
            Self::Expanded => write!(f, "more than {MAX_EXPANDED} expanded values"),
            Self::IncludeDepth => write!(f, "includes nested deeper than {MAX_INCLUDE_DEPTH}"),
            Self::IncludeParses => write!(f, "more than {MAX_INCLUDE_PARSES} included files"),
            Self::ListEntries => write!(f, "more than {MAX_LIST_ENTRIES} folder entries"),
            Self::Options => write!(f, "more than {MAX_OPTIONS} options"),
            Self::OptionItems => write!(f, "more than {MAX_OPTION_ITEMS} option items"),
            Self::FileChoices => write!(f, "more than {MAX_FILE_CHOICES} file choices"),
            Self::Offsets => write!(f, "more than {MAX_OFFSETS} offsets"),
        }
    }
}

/// Why a definition was refused.
#[derive(Debug)]
pub enum ImportError {
    /// A file or folder could not be read; `at` is where an included file
    /// or a file choice named it.
    Read {
        at: Option<Loc>,
        name: String,
        source: RootError,
    },
    Syntax {
        at: Loc,
        what: Syntax,
    },
    /// An input bound was reached; `at` is where, when it is in a file.
    Input {
        at: Option<Loc>,
        limit: InputLimit,
    },
    /// A file includes itself, directly or through others: the chain of
    /// names from the entry file to the repeated one.
    IncludeCycle {
        at: Loc,
        chain: Vec<String>,
    },
    /// An include name with a wildcard: the source substitutes file
    /// choices into it, which this reader does not.
    IncludeWildcard {
        at: Loc,
    },
    /// A file choice's path is not a folder and one last component with
    /// `*` wildcards.
    Pattern {
        at: Loc,
    },
    /// A field holding a value of the wrong JSON type.
    Type {
        at: Loc,
        field: &'static str,
        expected: &'static str,
    },
    /// An integer field outside `i32` (an intentional policy: the source
    /// wraps or saturates).
    Range {
        at: Loc,
        field: &'static str,
    },
    /// The definition is not of the 7-key play type, or names no type.
    NotPlay7 {
        at: Option<Loc>,
    },
    /// The skin's size is not one of [`crate::SIZES`].
    Resolution {
        at: Option<Loc>,
        w: i32,
        h: i32,
    },
}

/// `"<loc>: "`, or nothing.
struct At<'a>(&'a Option<Loc>);

impl fmt::Display for At<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(loc) => write!(f, "{loc}: "),
            None => Ok(()),
        }
    }
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The cause is `source()`, not repeated here.
            Self::Read { at, name, .. } => write!(f, "{}cannot read {name}", At(at)),
            Self::Syntax { at, what } => write!(f, "{at}: {what}"),
            Self::Input { at, limit } => write!(f, "{}{limit}", At(at)),
            Self::IncludeCycle { at, chain } => {
                write!(f, "{at}: an include cycle: {}", chain.join(" -> "))
            }
            Self::IncludeWildcard { at } => {
                write!(f, "{at}: an include name with a wildcard")
            }
            Self::Pattern { at } => write!(
                f,
                "{at}: a file choice's path must be a folder and one name with `*` wildcards"
            ),
            Self::Type {
                at,
                field,
                expected,
            } => write!(f, "{at}: `{field}` must be {expected}"),
            Self::Range { at, field } => write!(f, "{at}: `{field}` is outside i32"),
            Self::NotPlay7 { at } => write!(f, "{}not a 7-key play skin", At(at)),
            Self::Resolution { at, w, h } => {
                write!(f, "{}an unsupported skin size {w}x{h}", At(at))
            }
        }
    }
}

impl std::error::Error for ImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            _ => None,
        }
    }
}
