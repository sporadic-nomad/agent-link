//! Tolerant projections of one transcript line for the ambient sources. A source reads
//! whatever CLI version is installed, so a field of an unexpected type, an unknown field
//! and an unknown record kind never make a line fail: each leaf is a [`Json`] that keeps
//! only what it is (a string borrowed from the line when it holds no escapes), objects are
//! [`Obj`]s decoded only when they are objects, content arrays are [`Blocks`] decoded only when
//! they are arrays. Anything else is skipped with [`IgnoredAny`], which builds no tree, so
//! a line costs little more than its own bytes (bounded by `TurnOptions::line_bytes`) and
//! at most [`MAX_ITEMS`] elements per array. Plain `serde` on every platform; the
//! controlled tier's raw projections (`profiles::turns::json`) are Unix-only and strict.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

/// Maximum retained elements. Block kinds past the limit still inform turn decisions.
pub(super) const MAX_ITEMS: usize = 256;

/// `line` as an object of shape `T`; `None` when it is not one JSON object (a line of
/// another kind is no record of any CLI here).
pub(super) fn decode<'a, T: Deserialize<'a>>(line: &'a [u8]) -> Option<T> {
    // Derived structs also accept positional arrays: only an object is a record.
    if !line.trim_ascii_start().starts_with(b"{") {
        return None;
    }
    serde_json::from_slice(line).ok()
}

/// One JSON value, kept only as far as a source reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum Json<'a> {
    /// The key was not there.
    #[default]
    Missing,
    Null,
    Bool(bool),
    /// A nonnegative integer that fits `u64`.
    Int(u64),
    /// Any other number.
    Number,
    Str(Cow<'a, str>),
    /// An array or an object, skipped.
    Container,
}

impl Json<'_> {
    /// The string, when it is one.
    pub(super) fn str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
            _ => None,
        }
    }

    /// Whether it is this string.
    pub(super) fn is(&self, text: &str) -> bool {
        self.str() == Some(text)
    }

    /// Whether it is `true`.
    pub(super) fn is_true(&self) -> bool {
        *self == Self::Bool(true)
    }

    /// The integer, when it is a nonnegative one.
    pub(super) fn int(&self) -> Option<u64> {
        match self {
            Self::Int(n) => Some(*n),
            _ => None,
        }
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Json<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Leaf<'a>(PhantomData<&'a ()>);

        impl<'de: 'a, 'a> Visitor<'de> for Leaf<'a> {
            type Value = Json<'a>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(Json::Bool(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(u64::try_from(v).map_or(Json::Number, Json::Int))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(Json::Int(v))
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(Json::Number)
            }

            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(Json::Str(Cow::Borrowed(v)))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Json::Str(Cow::Owned(v.to_owned())))
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(Json::Str(Cow::Owned(v)))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Json::Null)
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Json::Null)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                skip_seq(seq).map(|()| Json::Container)
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                skip_map(map).map(|()| Json::Container)
            }
        }

        deserializer.deserialize_any(Leaf(PhantomData))
    }
}

fn skip_seq<'de, A: SeqAccess<'de>>(mut seq: A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn skip_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

/// Visits any value and skips it, so a wrapper can take only the kind it wants.
macro_rules! skip_scalars {
    ($value:expr) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($value)
        }
    };
}

/// An object decoded as `T` (whose fields should all be tolerant themselves); `None` when
/// the value is missing or of another kind.
pub(super) struct Obj<T>(pub(super) Option<T>);

impl<T> Default for Obj<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Obj<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Object<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for Object<T> {
            type Value = Obj<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }

            skip_scalars!(Obj(None));

            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(Obj(None))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                skip_seq(seq).map(|()| Obj(None))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(|t| Obj(Some(t)))
            }
        }

        deserializer.deserialize_any(Object(PhantomData))
    }
}

/// A content block as the four CLIs write them: its `type` and, for text, its `text`.
#[derive(Default, serde::Deserialize)]
#[serde(default)]
pub(super) struct Block<'a> {
    #[serde(rename = "type", borrow)]
    pub(super) kind: Json<'a>,
    #[serde(borrow)]
    pub(super) text: Json<'a>,
}

/// A bounded projection of a content array. Cursor needs only the suffix starting
/// at the last tool use. All block kinds still count when classifying a prompt or
/// deciding whether a Pi message contains a tool call.
#[derive(Default)]
pub(super) struct Blocks<'a, const AFTER_TOOL: bool = false> {
    pub(super) items: Option<Vec<Obj<Block<'a>>>>,
    pub(super) more: bool,
    pub(super) pi_tool_call: bool,
    pub(super) cursor_prompt: bool,
    /// Claude also writes a prompt as a plain string.
    #[cfg(feature = "questions")]
    pub(super) string_prompt: bool,
}

impl<'a, const AFTER_TOOL: bool> Blocks<'a, AFTER_TOOL> {
    pub(super) fn blocks(&self) -> impl Iterator<Item = &Block<'a>> {
        self.items
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|item| item.0.as_ref())
    }
}

impl<'de: 'a, 'a, const AFTER_TOOL: bool> Deserialize<'de> for Blocks<'a, AFTER_TOOL> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Array<'a, const AFTER_TOOL: bool>(PhantomData<&'a ()>);

        impl<'de: 'a, 'a, const AFTER_TOOL: bool> Visitor<'de> for Array<'a, AFTER_TOOL> {
            type Value = Blocks<'a, AFTER_TOOL>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a content array")
            }

            skip_scalars!(Blocks::default());

            fn visit_str<E: de::Error>(self, _text: &str) -> Result<Self::Value, E> {
                Ok(Blocks {
                    #[cfg(feature = "questions")]
                    string_prompt: !_text.trim().is_empty(),
                    ..Blocks::default()
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                skip_map(map).map(|()| Blocks::default())
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Blocks::default();
                let mut items = Vec::new();
                let mut text = false;
                let mut prompt_kinds = true;
                while let Some(item) = seq.next_element::<Obj<Block<'a>>>()? {
                    if let Some(block) = &item.0 {
                        out.pi_tool_call |= block.kind.is("toolCall");
                        if AFTER_TOOL {
                            text |= block.kind.is("text");
                            prompt_kinds &= block.kind.is("text") || block.kind.is("image");
                            if block.kind.is("tool_use") {
                                items.clear();
                                out.more = false;
                            }
                        }
                    }
                    if items.len() < MAX_ITEMS {
                        items.push(item);
                    } else {
                        out.more = true;
                    }
                }
                out.items = Some(items);
                out.cursor_prompt = text && prompt_kinds;
                Ok(out)
            }
        }

        deserializer.deserialize_any(Array(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default, serde::Deserialize)]
    #[serde(default)]
    struct Row<'a> {
        #[serde(rename = "type", borrow)]
        kind: Json<'a>,
        #[serde(borrow)]
        content: Blocks<'a>,
        #[serde(borrow)]
        nested: Obj<Inner<'a>>,
    }

    #[derive(Default, serde::Deserialize)]
    #[serde(default)]
    struct Inner<'a> {
        #[serde(rename = "type", borrow)]
        kind: Json<'a>,
    }

    fn row(line: &str) -> Option<Row<'_>> {
        decode(line.as_bytes())
    }

    #[test]
    fn unknown_fields_and_unexpected_types_never_fail_a_line() {
        let r =
            row(r#"{"type":"x","extra":{"deep":[1,2,{"a":null}]},"content":"text","nested":7}"#)
                .unwrap();
        assert!(r.kind.is("x"));
        assert!(r.content.items.is_none(), "a string is no array");
        assert!(r.nested.0.is_none(), "a number is no object");
        let r = row(r#"{"type":["x"],"content":[1,"two",{"type":"text","text":3}]}"#).unwrap();
        assert_eq!(r.kind, Json::Container);
        assert_eq!(r.content.items.as_ref().unwrap().len(), 3);
        let blocks: Vec<_> = r.content.blocks().collect();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, Json::Int(3));
        assert_eq!(r.content.blocks().next().unwrap().text.str(), None);
    }

    #[test]
    fn missing_null_and_values_are_told_apart() {
        let r = row(r#"{"nested":{"type":null}}"#).unwrap();
        assert_eq!(r.kind, Json::Missing);
        assert_eq!(r.nested.0.unwrap().kind, Json::Null);
        assert!(row(r#"{"type":true}"#).unwrap().kind.is_true());
        assert_eq!(row(r#"{"type":-1}"#).unwrap().kind, Json::Number);
        assert_eq!(row(r#"{"type":12}"#).unwrap().kind.int(), Some(12));
    }

    #[test]
    fn strings_are_borrowed_unless_escaped() {
        let r = row(r#"{"type":"plain"}"#).unwrap();
        assert!(matches!(r.kind, Json::Str(Cow::Borrowed("plain"))));
        let r = row(r#"{"type":"two\nlines é"}"#).unwrap();
        assert_eq!(r.kind.str(), Some("two\nlines é"));
    }

    #[test]
    fn only_one_json_object_is_a_record() {
        for line in ["[\"x\"]", "\"x\"", "{", "{} {}", "not json", ""] {
            assert!(row(line).is_none(), "{line}");
        }
        assert!(row(" {}").is_some());
    }

    #[test]
    fn arrays_keep_at_most_the_item_bound() {
        let items = vec!["{}"; MAX_ITEMS + 2].join(",");
        let text = format!(r#"{{"content":[{items}]}}"#);
        let r = row(&text).unwrap();
        assert_eq!(r.content.items.as_ref().unwrap().len(), MAX_ITEMS);
        assert!(r.content.more);
        let items = vec!["{}"; MAX_ITEMS].join(",");
        let text = format!(r#"{{"content":[{items}]}}"#);
        assert!(!row(&text).unwrap().content.more);
    }
}
