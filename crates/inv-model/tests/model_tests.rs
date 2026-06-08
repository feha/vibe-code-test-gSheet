//! Contract tests for the shared data model (store-backed, no UUIDs).

use inv_model::{
    Class, FieldDef, FieldType, FieldValue, Instance, Inventory, Photo, Relationship,
};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn field_value_serde_roundtrip_for_every_variant() {
    for v in [
        FieldValue::Text("hello".into()),
        FieldValue::Number(3.5),
        FieldValue::Bool(true),
        FieldValue::Date(1_234_567),
        FieldValue::Empty,
    ] {
        let j = serde_json::to_string(&v).unwrap();
        let back: FieldValue = serde_json::from_str(&j).unwrap();
        assert_eq!(v, back);
    }
}

#[test]
fn next_instance_id_advances_counter() {
    let mut inv = Inventory::new();
    let a = inv.next_instance_id();
    let b = inv.next_instance_id();
    assert_eq!(a, 1);
    assert_eq!(b, 2);
    assert_eq!(inv.next_id, 3);
}

#[test]
fn inventory_to_json_then_from_json_preserves_contents() {
    let mut inv = Inventory::new();
    inv.classes.insert(
        "Box".into(),
        Class {
            name: "Box".into(),
            fields: vec![FieldDef {
                name: "color".into(),
                field_type: FieldType::Text,
                required: false,
            }],
            created_at: 1,
        },
    );
    let iid = inv.next_instance_id();
    let mut fields = BTreeMap::new();
    fields.insert("color".to_string(), FieldValue::Text("red".to_string()));
    let mut tags = BTreeSet::new();
    tags.insert("storage".to_string());
    inv.instances.insert(
        iid,
        Instance {
            id: iid,
            class: "Box".into(),
            name: "Red box".into(),
            fields,
            tags,
            parent: None,
            photos: vec![Photo {
                key: format!("{iid}-0"),
                mime: "image/jpeg".into(),
                name: "box.jpg".into(),
            }],
            relationships: vec![Relationship {
                kind: "contains".into(),
                target: 99,
            }],
            created_at: 1,
            updated_at: 1,
        },
    );

    let bytes = inv.to_json_bytes().unwrap();
    let back = Inventory::from_json_bytes(&bytes).unwrap();
    assert_eq!(inv, back);
}

#[test]
fn on_disk_format_is_pretty() {
    let inv = Inventory::new();
    let text = String::from_utf8(inv.to_json_bytes().unwrap()).unwrap();
    assert!(text.contains('\n'), "on-disk JSON must be pretty-printed");
}
