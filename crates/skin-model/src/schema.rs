//! The static schema expressions are compiled against.
//!
//! A schema is const-constructible, so a screen's schema is a `static`. A
//! field's or timer's slot is its index in its table, and an [`EnumId`] is
//! an index into the enum table, so a slot cannot drift from its entry.

#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use crate::expr::{MAX_NAME_BYTES, RESERVED, is_tail};

/// The most entries one schema table may hold, so every slot fits a `u16`.
const MAX_ENTRIES: usize = 65_535;

/// An index into [`Schema::enums`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnumId(pub u16);

/// The type of an expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Bool,
    Int,
    Enum(EnumId),
}

/// The declared type of a field. An integer field declares its range,
/// which bounds every expression that reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldTy {
    Bool,
    Int { lo: i32, hi: i32 },
    Enum(EnumId),
}

/// Whether a field or timer has a counterpart in the exported skin format.
/// The exporter keeps the name-to-ID table; every `Exportable` entry must
/// have one there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Export {
    Exportable,
    TapestryOnly,
}

/// A state field: a name, its declared type and whether it exports.
#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub ty: FieldTy,
    pub export: Export,
}

/// A timer: read only through `on(name)` and `elapsed(name)`.
#[derive(Clone, Copy, Debug)]
pub struct TimerDef {
    pub name: &'static str,
    pub export: Export,
}

/// An enumeration. A value is its variant's index; expressions compare
/// enums only for equality, since the export remaps the indices.
#[derive(Clone, Copy, Debug)]
pub struct EnumDef {
    pub name: &'static str,
    pub variants: &'static [&'static str],
}

/// The state one screen exposes to expressions.
#[derive(Clone, Copy, Debug)]
pub struct Schema {
    /// Fields; a field's slot is its index.
    pub fields: &'static [Field],
    /// Timers; a timer's slot is its index.
    pub timers: &'static [TimerDef],
    /// Enumerations; an [`EnumId`] is an index.
    pub enums: &'static [EnumDef],
}

/// Why [`Schema::check`] refused a schema.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    #[error("`{0}` is declared twice")]
    Duplicate(&'static str),
    #[error("`{0}` is not a valid or free name")]
    BadName(&'static str),
    #[error("`{0}` has an empty range")]
    EmptyRange(&'static str),
    #[error("`{0}` names a missing or empty enum")]
    BadEnum(&'static str),
    #[error("the schema has more than {MAX_ENTRIES} entries in one table")]
    TooMany,
}

impl Schema {
    /// Checks the schema: names are unique across fields and timers, match
    /// `[a-z_][a-z0-9_]*(\.[a-z0-9_]+)*`, are at most [`MAX_NAME_BYTES`]
    /// long and are not reserved; integer ranges are not empty; every
    /// [`EnumId`] names an enum; enums are not empty and their variants are
    /// unique single-segment names; no table holds more than 65535 entries.
    /// Every static schema has a unit test calling this.
    pub fn check(&self) -> Result<(), SchemaError> {
        if self.fields.len() > MAX_ENTRIES
            || self.timers.len() > MAX_ENTRIES
            || self.enums.len() > MAX_ENTRIES
        {
            return Err(SchemaError::TooMany);
        }
        let names = || {
            self.fields
                .iter()
                .map(|f| f.name)
                .chain(self.timers.iter().map(|t| t.name))
        };
        for (i, name) in names().enumerate() {
            if !is_name(name, true) {
                return Err(SchemaError::BadName(name));
            }
            if names().take(i).any(|earlier| earlier == name) {
                return Err(SchemaError::Duplicate(name));
            }
        }
        for field in self.fields {
            match field.ty {
                FieldTy::Bool => {}
                FieldTy::Int { lo, hi } => {
                    if lo > hi {
                        return Err(SchemaError::EmptyRange(field.name));
                    }
                }
                FieldTy::Enum(id) => {
                    if self.variants(id).is_none_or(<[_]>::is_empty) {
                        return Err(SchemaError::BadEnum(field.name));
                    }
                }
            }
        }
        for def in self.enums {
            if def.variants.is_empty() {
                return Err(SchemaError::BadEnum(def.name));
            }
            if def.variants.len() > MAX_ENTRIES {
                return Err(SchemaError::TooMany);
            }
            for (i, &variant) in def.variants.iter().enumerate() {
                if !is_name(variant, false) {
                    return Err(SchemaError::BadName(variant));
                }
                if def.variants.iter().take(i).any(|&v| v == variant) {
                    return Err(SchemaError::Duplicate(variant));
                }
            }
        }
        Ok(())
    }

    /// The field named `name` and its slot.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<(u16, &Field)> {
        let (i, f) = self
            .fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == name)?;
        Some((u16::try_from(i).ok()?, f))
    }

    /// The timer named `name` and its slot.
    #[must_use]
    pub fn timer(&self, name: &str) -> Option<(u16, &TimerDef)> {
        let (i, t) = self
            .timers
            .iter()
            .enumerate()
            .find(|(_, t)| t.name == name)?;
        Some((u16::try_from(i).ok()?, t))
    }

    /// The variants of enum `id`.
    pub(crate) fn variants(&self, id: EnumId) -> Option<&'static [&'static str]> {
        self.enums.get(usize::from(id.0)).map(|e| e.variants)
    }
}

/// Whether `s` is a free name: `[a-z_][a-z0-9_]*`, followed by
/// `(\.[a-z0-9_]+)*` when `dotted`, at most [`MAX_NAME_BYTES`] long and not
/// reserved.
fn is_name(s: &str, dotted: bool) -> bool {
    let mut segments = s.split('.');
    let head_ok = segments.next().is_some_and(|head| {
        head.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
            && head.bytes().all(is_tail)
    });
    let rest_ok = segments.all(|seg| dotted && !seg.is_empty() && seg.bytes().all(is_tail));
    head_ok && rest_ok && s.len() <= MAX_NAME_BYTES && !RESERVED.contains(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRADES: &[&str] = &["perfect", "great"];

    fn schema(fields: &'static [Field], timers: &'static [TimerDef]) -> Schema {
        Schema {
            fields,
            timers,
            enums: &[EnumDef {
                name: "grade",
                variants: GRADES,
            }],
        }
    }

    const fn int(name: &'static str, lo: i32, hi: i32) -> Field {
        Field {
            name,
            ty: FieldTy::Int { lo, hi },
            export: Export::Exportable,
        }
    }

    const fn timer(name: &'static str) -> TimerDef {
        TimerDef {
            name,
            export: Export::TapestryOnly,
        }
    }

    #[test]
    fn accepts_a_valid_schema_and_finds_slots() {
        static FIELDS: &[Field] = &[
            int("combo", 0, 99_999),
            Field {
                name: "judge.last",
                ty: FieldTy::Enum(EnumId(0)),
                export: Export::Exportable,
            },
        ];
        static TIMERS: &[TimerDef] = &[timer("play"), timer("judge.great")];
        let s = schema(FIELDS, TIMERS);
        assert_eq!(s.check(), Ok(()));
        assert_eq!(s.field("judge.last").map(|(slot, _)| slot), Some(1));
        assert_eq!(s.timer("judge.great").map(|(slot, _)| slot), Some(1));
        assert!(s.field("play").is_none());
        assert!(s.timer("combo").is_none());
    }

    #[test]
    fn refuses_bad_names() {
        for bad in [
            "", "Combo", "1st", "a..b", "a.", ".a", "a-b", "min", "option", "true", "if",
        ] {
            let fields: &'static [Field] = Box::leak(Box::new([int(bad, 0, 1)]));
            assert_eq!(
                schema(fields, &[]).check(),
                Err(SchemaError::BadName(bad)),
                "{bad:?}"
            );
        }
        let long: &'static str = "a".repeat(MAX_NAME_BYTES + 1).leak();
        let fields: &'static [Field] = Box::leak(Box::new([int(long, 0, 1)]));
        assert_eq!(schema(fields, &[]).check(), Err(SchemaError::BadName(long)));
        let fields: &'static [Field] =
            Box::leak(Box::new([int("a".repeat(MAX_NAME_BYTES).leak(), 0, 1)]));
        assert_eq!(schema(fields, &[]).check(), Ok(()));
    }

    #[test]
    fn refuses_duplicates_across_fields_and_timers() {
        static FIELDS: &[Field] = &[int("play", 0, 1)];
        static TIMERS: &[TimerDef] = &[timer("play")];
        assert_eq!(
            schema(FIELDS, TIMERS).check(),
            Err(SchemaError::Duplicate("play"))
        );
        static TWICE: &[TimerDef] = &[timer("t"), timer("t")];
        assert_eq!(schema(&[], TWICE).check(), Err(SchemaError::Duplicate("t")));
    }

    #[test]
    fn refuses_empty_ranges_and_bad_enums() {
        static EMPTY: &[Field] = &[int("x", 1, 0)];
        assert_eq!(
            schema(EMPTY, &[]).check(),
            Err(SchemaError::EmptyRange("x"))
        );
        static MISSING: &[Field] = &[Field {
            name: "g",
            ty: FieldTy::Enum(EnumId(1)),
            export: Export::Exportable,
        }];
        assert_eq!(schema(MISSING, &[]).check(), Err(SchemaError::BadEnum("g")));
        let no_variants = Schema {
            fields: &[],
            timers: &[],
            enums: &[EnumDef {
                name: "none",
                variants: &[],
            }],
        };
        assert_eq!(no_variants.check(), Err(SchemaError::BadEnum("none")));
        let twice = Schema {
            fields: &[],
            timers: &[],
            enums: &[EnumDef {
                name: "e",
                variants: &["a", "a"],
            }],
        };
        assert_eq!(twice.check(), Err(SchemaError::Duplicate("a")));
        let dotted = Schema {
            fields: &[],
            timers: &[],
            enums: &[EnumDef {
                name: "e",
                variants: &["a.b"],
            }],
        };
        assert_eq!(dotted.check(), Err(SchemaError::BadName("a.b")));
    }
}
