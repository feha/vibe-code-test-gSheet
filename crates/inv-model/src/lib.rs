//! `inv-model`: the shared data contract for the bring-your-own-database
//! inventory app.
//!
//! Pure data + serde only — no I/O, no time, no platform calls (works on host and
//! `wasm32`). Timestamps are plain `i64` unix-millis injected by callers, which keeps
//! this crate deterministic and trivially testable.
//!
//! There are NO accounts, NO app-minted identity, and NO UUIDs. Object identity is
//! a store-native `i64`; classes are keyed by name. Photo bytes live in the Store
//! and are addressed by a string key.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The kind of a class field. Inferred when new fields appear on an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Text,
    Number,
    Bool,
    Date,
}

/// A field declaration on a [`Class`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    pub required: bool,
}

/// A concrete value stored for a field on an [`Instance`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum FieldValue {
    Text(String),
    Number(f64),
    Bool(bool),
    /// Unix milliseconds.
    Date(i64),
    /// Present-but-unset.
    Empty,
}

/// An object class: a (possibly auto-generated) schema shared by many instances.
/// The class is keyed by its `name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Class {
    pub name: String,
    pub fields: Vec<FieldDef>,
    /// Unix milliseconds.
    pub created_at: i64,
}

/// A directed, typed relationship from one instance to another (a general graph
/// edge, distinct from the containment tree carried by [`Instance::parent`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relationship {
    pub kind: String,
    /// Store-native id of the target instance.
    pub target: i64,
}

/// A photo attached to an instance. The actual bytes live in the Store and are
/// addressed by `key` (no UUID; a key may look like "<instance_id>-<n>").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Photo {
    pub key: String,
    pub mime: String,
    pub name: String,
}

/// A concrete object: a member of a [`Class`] that may also *contain* other
/// instances (via their `parent` pointer) and link to them via relationships.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    /// Store-native identity.
    pub id: i64,
    /// The name of this instance's [`Class`].
    pub class: String,
    pub name: String,
    pub fields: BTreeMap<String, FieldValue>,
    pub tags: BTreeSet<String>,
    /// The container this instance lives inside, if any (the containment tree).
    pub parent: Option<i64>,
    pub photos: Vec<Photo>,
    pub relationships: Vec<Relationship>,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

/// The full inventory state. This is the option-1 on-disk document: it serializes
/// to clean, human-editable pretty JSON. Photo bytes are stored separately in the
/// Store and referenced by [`Photo::key`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    pub classes: BTreeMap<String, Class>,
    pub instances: BTreeMap<i64, Instance>,
    /// The next store-native id to hand out.
    pub next_id: i64,
}

impl Inventory {
    /// Create an empty inventory. Ids start at 1.
    pub fn new() -> Self {
        Self {
            classes: BTreeMap::new(),
            instances: BTreeMap::new(),
            next_id: 1,
        }
    }

    /// Return the next instance id, advancing the counter.
    pub fn next_instance_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Serialize to clean, human-editable pretty JSON bytes (the on-disk format).
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }

    /// Deserialize from JSON bytes.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

impl Default for Inventory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_instance_id_assigns_sequentially() {
        let mut inv = Inventory::new();
        assert_eq!(inv.next_id, 1);
        assert_eq!(inv.next_instance_id(), 1);
        assert_eq!(inv.next_instance_id(), 2);
        assert_eq!(inv.next_instance_id(), 3);
        assert_eq!(inv.next_id, 4);
    }

    #[test]
    fn serde_roundtrip_pretty_json() {
        let mut inv = Inventory::new();
        let id = inv.next_instance_id();

        inv.classes.insert(
            "Widget".to_string(),
            Class {
                name: "Widget".to_string(),
                fields: vec![FieldDef {
                    name: "color".to_string(),
                    field_type: FieldType::Text,
                    required: false,
                }],
                created_at: 5,
            },
        );

        let mut fields = BTreeMap::new();
        fields.insert("color".to_string(), FieldValue::Text("red".to_string()));
        fields.insert("qty".to_string(), FieldValue::Number(3.0));
        fields.insert("active".to_string(), FieldValue::Bool(true));
        fields.insert("when".to_string(), FieldValue::Date(1234));
        fields.insert("blank".to_string(), FieldValue::Empty);

        let mut tags = BTreeSet::new();
        tags.insert("fresh".to_string());

        inv.instances.insert(
            id,
            Instance {
                id,
                class: "Widget".to_string(),
                name: "thing".to_string(),
                fields,
                tags,
                parent: None,
                photos: vec![Photo {
                    key: format!("{id}-0"),
                    mime: "image/png".to_string(),
                    name: "front.png".to_string(),
                }],
                relationships: vec![Relationship {
                    kind: "ref".to_string(),
                    target: 7,
                }],
                created_at: 5,
                updated_at: 5,
            },
        );

        let bytes = inv.to_json_bytes().unwrap();
        let back = Inventory::from_json_bytes(&bytes).unwrap();
        assert_eq!(inv, back);
    }

    #[test]
    fn pretty_json_is_human_editable() {
        // The on-disk format must be pretty-printed (multi-line, indented).
        let inv = Inventory::new();
        let bytes = inv.to_json_bytes().unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains('\n'), "pretty JSON must be multi-line");
        assert!(text.contains("\"next_id\""));
    }

    #[test]
    fn empty_inventory_roundtrips() {
        let inv = Inventory::new();
        let bytes = inv.to_json_bytes().unwrap();
        let back = Inventory::from_json_bytes(&bytes).unwrap();
        assert_eq!(inv, back);
        assert!(back.classes.is_empty());
        assert!(back.instances.is_empty());
        assert_eq!(back.next_id, 1);
    }
}
