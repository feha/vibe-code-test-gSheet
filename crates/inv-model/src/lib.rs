//! `inv-model`: the shared data contract for the whole system.
//!
//! Pure data + serde only — no I/O, no time, no platform calls (works on host and
//! `wasm32`). Timestamps are plain `i64` unix-millis injected by callers, which keeps
//! this crate deterministic and trivially testable.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Defines a strongly-typed UUID newtype with random construction, `Display`,
/// `FromStr`, and transparent serde (serializes as the bare UUID string).
macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub uuid::Uuid);

        impl $name {
            /// Mint a fresh random (v4) identifier.
            pub fn new_random() -> Self {
                Self(uuid::Uuid::new_v4())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(uuid::Uuid::parse_str(s)?))
            }
        }
    };
}

id_type!(
    /// Identifies a workspace (the unit of sharing + encryption).
    WorkspaceId
);
id_type!(
    /// Identifies an object *class* (a user-defined or auto-generated type).
    ClassId
);
id_type!(
    /// Identifies an object *instance*.
    InstanceId
);
id_type!(
    /// Identifies a stored photo blob.
    PhotoId
);

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Class {
    pub id: ClassId,
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
    pub target: InstanceId,
}

/// A concrete object: a member of a [`Class`] that may also *contain* other
/// instances (via their `parent` pointer) and link to them via relationships.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    pub id: InstanceId,
    pub class_id: ClassId,
    pub name: String,
    pub fields: BTreeMap<String, FieldValue>,
    pub tags: BTreeSet<String>,
    /// The container this instance lives inside, if any (the containment tree).
    pub parent: Option<InstanceId>,
    pub photos: Vec<PhotoId>,
    pub relationships: Vec<Relationship>,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

/// The full decrypted state of one workspace. This is what the client encrypts
/// into a single blob; photo bytes are stored separately and referenced by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub classes: BTreeMap<ClassId, Class>,
    pub instances: BTreeMap<InstanceId, Instance>,
}

impl Workspace {
    /// Create an empty workspace.
    pub fn new(id: WorkspaceId) -> Self {
        Self {
            id,
            classes: BTreeMap::new(),
            instances: BTreeMap::new(),
        }
    }

    /// Serialize to bytes (the plaintext that the client encrypts).
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Deserialize from bytes (after the client decrypts).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}
