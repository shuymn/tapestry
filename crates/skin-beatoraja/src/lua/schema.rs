//! Where the source reads a Lua skin's returned table (design.md
//! "beatoraja Lua Skins", per-frame closures): the classes of its skin
//! definition (`JsonSkin.java` at `0.8.9`) as far as they lead to a
//! property field, an `op` list or a font's fallbacks.
//!
//! The source deserializes the table by reflection
//! (`LuaSkinLoader.java:171-210`): at a class, each key equal to a field's
//! name, byte for byte, takes that field's type, and any other key is
//! never read; at an array, every entry under any key is an element. A
//! property field's value goes to its serializer
//! (`LuaSkinLoader.java:120-168`): a function becomes a callable the source
//! runs, and a string its name loader does not resolve a script it
//! compiles and runs. A function anywhere else is never read by the
//! source: it is inert.

use Serializer as S;

use crate::import::{int_by_name, ops, str_by_name};

/// The serializer of a property field: what a string there is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Serializer {
    /// `BooleanProperty`: `draw`, `condition`.
    Boolean,
    /// A `DestinationOption`, an `op` entry: a number is an option id,
    /// a string a `BooleanProperty`.
    Op,
    /// `IntegerProperty`: a value's or an image set's `value`.
    Integer,
    /// `FloatProperty`: a float value's, slider's or graph's `value`.
    Float,
    /// `StringProperty`: a text's `value`.
    String,
    /// `TimerProperty`: every `timer`.
    Timer,
    /// `Event`: `act`, `action`.
    Event,
    /// `FloatWriter`: a slider's `event`.
    FloatWriter,
    /// `StringWriter`: a text's `event`.
    StringWriter,
}

impl Serializer {
    /// Whether the source's name loader for this field resolves `name`
    /// without compiling it, by a recognizer that accepts the source's
    /// names exactly or a subset of them: any other string is a script.
    pub(crate) fn resolves(self, name: &str) -> bool {
        match self {
            // `BooleanPropertyFactory.getBooleanProperty(String)`.
            Self::Boolean | Self::Op => ops::named(name).is_some(),
            // `IntegerPropertyFactory.getIntegerProperty(String)`.
            Self::Integer => int_by_name(name).is_some(),
            // `StringPropertyFactory.getStringProperty(String)`.
            Self::String => str_by_name(name).is_some(),
            // No name loader, or one tapestry does not model: a script.
            Self::Float | Self::Timer | Self::Event | Self::FloatWriter | Self::StringWriter => {
                false
            }
        }
    }
}

/// A position in the returned table, as the source's deserializer reads
/// it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pos {
    /// An instance of a class.
    Class(&'static Class),
    /// An array of a class's instances.
    Array(&'static Class),
    /// A destination's `op` list.
    OpArray,
    /// A property field.
    Prop(Serializer),
    /// A font's `fallback` list.
    FontFallbackArray,
    /// One of a font's fallbacks: read with `get("path")` and
    /// `get("type")`, which run a metatable's `__index` for a key it lacks
    /// (`LuaSkinLoader.java:108-118`).
    FontFallback,
    /// Anything else: the source reads no function or script here.
    Other,
}

impl Pos {
    /// The position of each entry of a table at this position, under any
    /// key.
    pub(crate) fn element(self) -> Self {
        match self {
            Self::Array(c) => Self::Class(c),
            Self::OpArray => Self::Prop(Serializer::Op),
            Self::FontFallbackArray => Self::FontFallback,
            _ => Self::Other,
        }
    }

    /// Whether the source reads every entry of a table here.
    pub(crate) fn lists(self) -> bool {
        matches!(
            self,
            Self::Array(_) | Self::OpArray | Self::FontFallbackArray
        )
    }
}

/// A class's fields that lead to a property field, sorted by their bytes;
/// any other field is [`Pos::Other`].
#[derive(Debug)]
pub(crate) struct Class {
    /// The source's class name, which the schema is checked against.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the schema's test and Debug")
    )]
    pub(crate) name: &'static str,
    pub(crate) fields: &'static [(&'static str, Pos)],
}

impl Class {
    /// The position of field `key`: one binary search.
    pub(crate) fn field(&self, key: &[u8]) -> Pos {
        self.fields
            .binary_search_by(|(name, _)| name.as_bytes().cmp(key))
            .ok()
            .and_then(|i| self.fields.get(i))
            .map_or(Pos::Other, |&(_, pos)| pos)
    }
}

/// `JsonSkin.Skin` (`JsonSkin.java:7-57`): the returned table.
pub(crate) static ROOT_SCHEMA: Class = Class {
    name: "Skin",
    fields: &[
        ("customEvents", Pos::Array(&CUSTOM_EVENT)),
        ("customTimers", Pos::Array(&CUSTOM_TIMER)),
        ("destination", Pos::Array(&DESTINATION)),
        ("floatvalue", Pos::Array(&FLOAT_VALUE)),
        ("font", Pos::Array(&FONT)),
        ("graph", Pos::Array(&GRAPH)),
        ("hiddenCover", Pos::Array(&HIDDEN_COVER)),
        ("image", Pos::Array(&IMAGE)),
        ("imageset", Pos::Array(&IMAGE_SET)),
        ("judge", Pos::Array(&JUDGE)),
        ("liftCover", Pos::Array(&LIFT_COVER)),
        ("note", Pos::Class(&NOTE_SET)),
        ("slider", Pos::Array(&SLIDER)),
        ("songlist", Pos::Class(&SONG_LIST)),
        ("text", Pos::Array(&TEXT)),
        ("value", Pos::Array(&VALUE)),
    ],
};

/// `Font` (`JsonSkin.java:100-105`).
static FONT: Class = Class {
    name: "Font",
    fields: &[("fallback", Pos::FontFallbackArray)],
};

/// `Image` (`JsonSkin.java:112-127`).
static IMAGE: Class = Class {
    name: "Image",
    fields: &[("act", Pos::Prop(S::Event)), ("timer", Pos::Prop(S::Timer))],
};

/// `ImageSet` (`JsonSkin.java:129-136`).
static IMAGE_SET: Class = Class {
    name: "ImageSet",
    fields: &[
        ("act", Pos::Prop(S::Event)),
        ("value", Pos::Prop(S::Integer)),
    ],
};

/// `Value` (`JsonSkin.java:138-157`).
static VALUE: Class = Class {
    name: "Value",
    fields: &[
        ("offset", Pos::Array(&VALUE)),
        ("timer", Pos::Prop(S::Timer)),
        ("value", Pos::Prop(S::Integer)),
    ],
};

/// `FloatValue` (`JsonSkin.java:159-181`).
static FLOAT_VALUE: Class = Class {
    name: "FloatValue",
    fields: &[
        ("offset", Pos::Array(&VALUE)),
        ("timer", Pos::Prop(S::Timer)),
        ("value", Pos::Prop(S::Float)),
    ],
};

/// `Text` (`JsonSkin.java:183-201`).
static TEXT: Class = Class {
    name: "Text",
    fields: &[
        ("event", Pos::Prop(S::StringWriter)),
        ("value", Pos::Prop(S::String)),
    ],
};

/// `Slider` (`JsonSkin.java:203-223`).
static SLIDER: Class = Class {
    name: "Slider",
    fields: &[
        ("event", Pos::Prop(S::FloatWriter)),
        ("timer", Pos::Prop(S::Timer)),
        ("value", Pos::Prop(S::Float)),
    ],
};

/// `Graph` (`JsonSkin.java:225-242`).
static GRAPH: Class = Class {
    name: "Graph",
    fields: &[
        ("timer", Pos::Prop(S::Timer)),
        ("value", Pos::Prop(S::Float)),
    ],
};

/// `HiddenCover` (`JsonSkin.java:380-393`).
static HIDDEN_COVER: Class = Class {
    name: "HiddenCover",
    fields: &[("timer", Pos::Prop(S::Timer))],
};

/// `LiftCover` (`JsonSkin.java:395-408`).
static LIFT_COVER: Class = Class {
    name: "LiftCover",
    fields: &[("timer", Pos::Prop(S::Timer))],
};

/// `NoteSet` (`JsonSkin.java:339-367`).
static NOTE_SET: Class = Class {
    name: "NoteSet",
    fields: &[
        ("bpm", Pos::Array(&DESTINATION)),
        ("group", Pos::Array(&DESTINATION)),
        ("stop", Pos::Array(&DESTINATION)),
        ("time", Pos::Array(&DESTINATION)),
    ],
};

/// `Judge` (`JsonSkin.java:429-435`).
static JUDGE: Class = Class {
    name: "Judge",
    fields: &[
        ("images", Pos::Array(&DESTINATION)),
        ("numbers", Pos::Array(&DESTINATION)),
    ],
};

/// `SongList` (`JsonSkin.java:437-451`).
static SONG_LIST: Class = Class {
    name: "SongList",
    fields: &[
        ("graph", Pos::Class(&DESTINATION)),
        ("label", Pos::Array(&DESTINATION)),
        ("lamp", Pos::Array(&DESTINATION)),
        ("level", Pos::Array(&DESTINATION)),
        ("listoff", Pos::Array(&DESTINATION)),
        ("liston", Pos::Array(&DESTINATION)),
        ("playerlamp", Pos::Array(&DESTINATION)),
        ("rivallamp", Pos::Array(&DESTINATION)),
        ("text", Pos::Array(&DESTINATION)),
        ("trophy", Pos::Array(&DESTINATION)),
    ],
};

/// `Destination` (`JsonSkin.java:453-482`).
static DESTINATION: Class = Class {
    name: "Destination",
    fields: &[
        ("draw", Pos::Prop(S::Boolean)),
        ("op", Pos::OpArray),
        ("timer", Pos::Prop(S::Timer)),
    ],
};

/// `CustomEvent` (`JsonSkin.java:546-551`).
static CUSTOM_EVENT: Class = Class {
    name: "CustomEvent",
    fields: &[
        ("action", Pos::Prop(S::Event)),
        ("condition", Pos::Prop(S::Boolean)),
    ],
};

/// `CustomTimer` (`JsonSkin.java:553-556`).
static CUSTOM_TIMER: Class = Class {
    name: "CustomTimer",
    fields: &[("timer", Pos::Prop(S::Timer))],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_s_fields_are_sorted_for_its_search() {
        for c in [
            &ROOT_SCHEMA,
            &FONT,
            &IMAGE,
            &IMAGE_SET,
            &VALUE,
            &FLOAT_VALUE,
            &TEXT,
            &SLIDER,
            &GRAPH,
            &HIDDEN_COVER,
            &LIFT_COVER,
            &NOTE_SET,
            &JUDGE,
            &SONG_LIST,
            &DESTINATION,
            &CUSTOM_EVENT,
            &CUSTOM_TIMER,
        ] {
            assert!(
                c.fields
                    .windows(2)
                    .all(|w| w[0].0.as_bytes() < w[1].0.as_bytes()),
                "{c:?}"
            );
            for &(name, _) in c.fields {
                assert!(!matches!(c.field(name.as_bytes()), Pos::Other), "{name}");
            }
        }
        assert!(matches!(ROOT_SCHEMA.field(b"Destination"), Pos::Other));
        assert!(matches!(ROOT_SCHEMA.field(b"property"), Pos::Other));
    }

    /// Every field of `JsonSkin.java` at `0.8.9` that leads to a property
    /// field, an `op` list or a font's fallbacks, by class, written out
    /// from the source apart from the schema.
    const SOURCE: &[(&str, &str, &str)] = &[
        ("CustomEvent", "action", "Prop(Event)"),
        ("CustomEvent", "condition", "Prop(Boolean)"),
        ("CustomTimer", "timer", "Prop(Timer)"),
        ("Destination", "draw", "Prop(Boolean)"),
        ("Destination", "op", "OpArray"),
        ("Destination", "timer", "Prop(Timer)"),
        ("FloatValue", "offset", "Array(Value)"),
        ("FloatValue", "timer", "Prop(Timer)"),
        ("FloatValue", "value", "Prop(Float)"),
        ("Font", "fallback", "FontFallbackArray"),
        ("Graph", "timer", "Prop(Timer)"),
        ("Graph", "value", "Prop(Float)"),
        ("HiddenCover", "timer", "Prop(Timer)"),
        ("Image", "act", "Prop(Event)"),
        ("Image", "timer", "Prop(Timer)"),
        ("ImageSet", "act", "Prop(Event)"),
        ("ImageSet", "value", "Prop(Integer)"),
        ("Judge", "images", "Array(Destination)"),
        ("Judge", "numbers", "Array(Destination)"),
        ("LiftCover", "timer", "Prop(Timer)"),
        ("NoteSet", "bpm", "Array(Destination)"),
        ("NoteSet", "group", "Array(Destination)"),
        ("NoteSet", "stop", "Array(Destination)"),
        ("NoteSet", "time", "Array(Destination)"),
        ("Skin", "customEvents", "Array(CustomEvent)"),
        ("Skin", "customTimers", "Array(CustomTimer)"),
        ("Skin", "destination", "Array(Destination)"),
        ("Skin", "floatvalue", "Array(FloatValue)"),
        ("Skin", "font", "Array(Font)"),
        ("Skin", "graph", "Array(Graph)"),
        ("Skin", "hiddenCover", "Array(HiddenCover)"),
        ("Skin", "image", "Array(Image)"),
        ("Skin", "imageset", "Array(ImageSet)"),
        ("Skin", "judge", "Array(Judge)"),
        ("Skin", "liftCover", "Array(LiftCover)"),
        ("Skin", "note", "Class(NoteSet)"),
        ("Skin", "slider", "Array(Slider)"),
        ("Skin", "songlist", "Class(SongList)"),
        ("Skin", "text", "Array(Text)"),
        ("Skin", "value", "Array(Value)"),
        ("Slider", "event", "Prop(FloatWriter)"),
        ("Slider", "timer", "Prop(Timer)"),
        ("Slider", "value", "Prop(Float)"),
        ("SongList", "graph", "Class(Destination)"),
        ("SongList", "label", "Array(Destination)"),
        ("SongList", "lamp", "Array(Destination)"),
        ("SongList", "level", "Array(Destination)"),
        ("SongList", "listoff", "Array(Destination)"),
        ("SongList", "liston", "Array(Destination)"),
        ("SongList", "playerlamp", "Array(Destination)"),
        ("SongList", "rivallamp", "Array(Destination)"),
        ("SongList", "text", "Array(Destination)"),
        ("SongList", "trophy", "Array(Destination)"),
        ("Text", "event", "Prop(StringWriter)"),
        ("Text", "value", "Prop(String)"),
        ("Value", "offset", "Array(Value)"),
        ("Value", "timer", "Prop(Timer)"),
        ("Value", "value", "Prop(Integer)"),
    ];

    #[test]
    fn the_schema_is_the_source_s() {
        // Every class reachable from the returned table, each once.
        let mut seen: Vec<&str> = Vec::new();
        let mut pending = vec![&ROOT_SCHEMA];
        let mut got: Vec<(&str, &str, String)> = Vec::new();
        while let Some(c) = pending.pop() {
            if seen.contains(&c.name) {
                continue;
            }
            seen.push(c.name);
            for &(field, pos) in c.fields {
                let kind = match pos {
                    Pos::Class(d) => {
                        pending.push(d);
                        format!("Class({})", d.name)
                    }
                    Pos::Array(d) => {
                        pending.push(d);
                        format!("Array({})", d.name)
                    }
                    Pos::Prop(s) => format!("Prop({s:?})"),
                    other => format!("{other:?}"),
                };
                got.push((c.name, field, kind));
            }
        }
        got.sort();
        let want: Vec<(&str, &str, String)> = SOURCE
            .iter()
            .map(|&(c, f, k)| (c, f, k.to_owned()))
            .collect();
        assert_eq!(got, want);
    }
}
