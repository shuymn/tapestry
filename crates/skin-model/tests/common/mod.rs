//! The test schema shared by the integration tests.

use skin_model::{EnumDef, EnumId, Export, Field, FieldTy, OptionDef, Schema, TimerDef};

pub const GRADES: &[&str] = &["perfect", "great", "good", "bad", "poor"];
pub const KINDS: &[&str] = &["normal", "hard"];
pub const STYLES: &[&str] = &["classic", "modern", "mini"];

const fn field(name: &'static str, ty: FieldTy) -> Field {
    Field {
        name,
        ty,
        export: Export::Exportable,
    }
}

/// Fields by slot: `full` (all of `i32`), `small` (`[-10, 10]`), `ms`
/// (`[0, 1000]`), `zero` (`[0, 0]`), `flag`, `grade` and `gauge` (enums).
pub static FIELDS: &[Field] = &[
    field(
        "full",
        FieldTy::Int {
            lo: i32::MIN,
            hi: i32::MAX,
        },
    ),
    field("small", FieldTy::Int { lo: -10, hi: 10 }),
    field("ms", FieldTy::Int { lo: 0, hi: 1000 }),
    field("zero", FieldTy::Int { lo: 0, hi: 0 }),
    field("flag", FieldTy::Bool),
    field("grade", FieldTy::Enum(EnumId(0))),
    field("gauge", FieldTy::Enum(EnumId(1))),
];

/// Timers by slot.
pub static TIMERS: &[TimerDef] = &[
    TimerDef {
        name: "play",
        export: Export::Exportable,
    },
    TimerDef {
        name: "judge.great",
        export: Export::TapestryOnly,
    },
];

pub static SCHEMA: Schema = Schema {
    fields: FIELDS,
    timers: TIMERS,
    enums: &[
        EnumDef {
            name: "grade",
            variants: GRADES,
        },
        EnumDef {
            name: "gauge_kind",
            variants: KINDS,
        },
    ],
};

/// The skin's options: the toggle `cover` and the one-of `style`.
pub fn options(cover: bool, style: usize) -> [OptionDef<'static>; 2] {
    [
        OptionDef::Toggle {
            name: "cover",
            on: cover,
        },
        OptionDef::OneOf {
            name: "style",
            choices: STYLES,
            chosen: style,
        },
    ]
}
