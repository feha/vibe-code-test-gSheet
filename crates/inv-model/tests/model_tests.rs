//! Contract tests for the shared data model. Written FIRST (TDD RED).

use inv_model::{
    Class, ClassId, FieldDef, FieldType, FieldValue, Instance, InstanceId, Workspace, WorkspaceId,
};
use std::str::FromStr;

#[test]
fn workspace_id_roundtrips_through_string() {
    let id = WorkspaceId::new_random();
    let s = id.to_string();
    let parsed = WorkspaceId::from_str(&s).unwrap();
    assert_eq!(id, parsed);
}

#[test]
fn ids_are_unique_per_new_random() {
    assert_ne!(InstanceId::new_random(), InstanceId::new_random());
    assert_ne!(ClassId::new_random(), ClassId::new_random());
}

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
fn workspace_to_bytes_then_from_bytes_preserves_contents() {
    let mut ws = Workspace::new(WorkspaceId::new_random());
    let cid = ClassId::new_random();
    ws.classes.insert(
        cid,
        Class {
            id: cid,
            name: "Box".into(),
            fields: vec![FieldDef {
                name: "color".into(),
                field_type: FieldType::Text,
                required: false,
            }],
            created_at: 1,
        },
    );
    let iid = InstanceId::new_random();
    ws.instances.insert(
        iid,
        Instance {
            id: iid,
            class_id: cid,
            name: "Red box".into(),
            fields: Default::default(),
            tags: Default::default(),
            parent: None,
            photos: vec![],
            relationships: vec![],
            created_at: 1,
            updated_at: 1,
        },
    );

    let bytes = ws.to_bytes().unwrap();
    let back = Workspace::from_bytes(&bytes).unwrap();
    assert_eq!(ws, back);
}
