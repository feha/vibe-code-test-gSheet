//! `inv-core`: domain logic over [`inv_model::Inventory`].
//!
//! Since `Inventory` is a foreign type we cannot add inherent methods, so the API
//! is exposed through the [`InventoryExt`] extension trait implemented for
//! `Inventory`. Time is injected: every mutator takes `now: i64` (unix millis),
//! keeping the logic deterministic and testable.
//!
//! Object identity is a store-native `i64`; classes are keyed by name. There are
//! NO accounts, NO app-minted identity, and NO UUIDs anywhere.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use inv_model::{
    Class, FieldDef, FieldType, FieldValue, Instance, Inventory, Photo, Relationship,
};

/// Errors produced by the domain operations in this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// No instance with the given id exists.
    NotFound(i64),
    /// The requested move would introduce a cycle in the containment tree.
    WouldCycle,
    /// The requested parent is not a valid container (e.g. does not exist).
    InvalidParent(i64),
    /// A class cannot be deleted while one or more instances still reference it.
    ClassInUse,
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::NotFound(id) => write!(f, "instance not found: {id}"),
            CoreError::WouldCycle => write!(f, "operation would create a cycle"),
            CoreError::InvalidParent(id) => write!(f, "invalid parent: {id}"),
            CoreError::ClassInUse => write!(f, "class is in use by one or more instances"),
        }
    }
}

impl std::error::Error for CoreError {}

/// A patch describing edits to apply to an [`Instance`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstancePatch {
    /// Rename the instance, if `Some`.
    pub name: Option<String>,
    /// Field values to set (also extend the class schema for new names).
    pub set_fields: BTreeMap<String, FieldValue>,
    /// Field names to remove from the instance.
    pub remove_fields: Vec<String>,
}

/// How [`InventoryExt::remove_instance`] handles an instance's children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveMode {
    /// Remove the entire subtree rooted at the instance.
    Cascade,
    /// Reparent direct children onto the removed instance's parent, then remove it.
    Reparent,
}

/// A search filter over instances. Fields combine with AND semantics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchQuery {
    /// Case-insensitive substring match on the instance name.
    pub text: Option<String>,
    /// Exact tag match.
    pub tag: Option<String>,
    /// Class name match.
    pub class: Option<String>,
}

/// Infer the [`FieldType`] a [`FieldValue`] should map to when extending a schema.
fn infer_field_type(v: &FieldValue) -> FieldType {
    match v {
        FieldValue::Text(_) => FieldType::Text,
        FieldValue::Number(_) => FieldType::Number,
        FieldValue::Bool(_) => FieldType::Bool,
        FieldValue::Date(_) => FieldType::Date,
        FieldValue::Empty => FieldType::Text,
    }
}

/// Domain operations over an [`Inventory`].
pub trait InventoryExt {
    fn ensure_class(&mut self, name: &str, now: i64) -> &Class;
    fn class_exists(&self, name: &str) -> bool;
    fn get_class(&self, name: &str) -> Option<&Class>;
    fn add_instance(
        &mut self,
        class_name: &str,
        name: &str,
        fields: BTreeMap<String, FieldValue>,
        parent: Option<i64>,
        now: i64,
    ) -> Result<i64, CoreError>;
    fn get(&self, id: i64) -> Option<&Instance>;
    fn children_of(&self, id: i64) -> Vec<i64>;
    fn descendants_of(&self, id: i64) -> Vec<i64>;
    fn path_of(&self, id: i64) -> Vec<i64>;
    fn roots(&self) -> Vec<i64>;
    fn edit_instance(&mut self, id: i64, patch: InstancePatch, now: i64)
        -> Result<(), CoreError>;
    fn move_instance(
        &mut self,
        id: i64,
        new_parent: Option<i64>,
        now: i64,
    ) -> Result<(), CoreError>;
    fn remove_instance(&mut self, id: i64, mode: RemoveMode) -> Result<Vec<i64>, CoreError>;
    fn add_tag(&mut self, id: i64, tag: &str, now: i64) -> Result<(), CoreError>;
    fn remove_tag(&mut self, id: i64, tag: &str, now: i64) -> Result<(), CoreError>;
    fn add_relationship(
        &mut self,
        id: i64,
        kind: &str,
        target: i64,
        now: i64,
    ) -> Result<(), CoreError>;
    fn remove_relationship(
        &mut self,
        id: i64,
        kind: &str,
        target: i64,
        now: i64,
    ) -> Result<(), CoreError>;
    fn attach_photo(&mut self, id: i64, photo: Photo, now: i64) -> Result<(), CoreError>;
    fn detach_photo(&mut self, id: i64, key: &str, now: i64) -> Result<(), CoreError>;
    fn duplicate_instance(&mut self, id: i64, deep: bool, now: i64) -> Result<i64, CoreError>;
    fn change_class(&mut self, id: i64, new_class: &str, now: i64) -> Result<(), CoreError>;
    fn delete_class(&mut self, name: &str) -> Result<(), CoreError>;
    fn search(&self, q: &SearchQuery) -> Vec<i64>;
}

impl InventoryExt for Inventory {
    fn ensure_class(&mut self, name: &str, now: i64) -> &Class {
        self.classes.entry(name.to_string()).or_insert_with(|| Class {
            name: name.to_string(),
            fields: Vec::new(),
            created_at: now,
        })
    }

    fn class_exists(&self, name: &str) -> bool {
        self.classes.contains_key(name)
    }

    fn get_class(&self, name: &str) -> Option<&Class> {
        self.classes.get(name)
    }

    fn add_instance(
        &mut self,
        class_name: &str,
        name: &str,
        fields: BTreeMap<String, FieldValue>,
        parent: Option<i64>,
        now: i64,
    ) -> Result<i64, CoreError> {
        if let Some(p) = parent {
            if !self.instances.contains_key(&p) {
                return Err(CoreError::InvalidParent(p));
            }
        }
        self.ensure_class(class_name, now);
        extend_class_schema(self, class_name, &fields);

        let id = self.next_instance_id();
        self.instances.insert(
            id,
            Instance {
                id,
                class: class_name.to_string(),
                name: name.to_string(),
                fields,
                tags: BTreeSet::new(),
                parent,
                photos: Vec::new(),
                relationships: Vec::new(),
                created_at: now,
                updated_at: now,
            },
        );
        Ok(id)
    }

    fn get(&self, id: i64) -> Option<&Instance> {
        self.instances.get(&id)
    }

    fn children_of(&self, id: i64) -> Vec<i64> {
        let mut kids: Vec<i64> = self
            .instances
            .values()
            .filter(|i| i.parent == Some(id))
            .map(|i| i.id)
            .collect();
        sort_by_name_then_id(self, &mut kids);
        kids
    }

    fn descendants_of(&self, id: i64) -> Vec<i64> {
        let mut out = Vec::new();
        let mut stack = self.children_of(id);
        while let Some(cur) = stack.pop() {
            out.push(cur);
            stack.extend(self.children_of(cur));
        }
        out
    }

    fn path_of(&self, id: i64) -> Vec<i64> {
        // Build root..=id, guarding against malformed cycles so we always terminate.
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            if !self.instances.contains_key(&c) || !seen.insert(c) {
                break;
            }
            chain.push(c);
            cur = self.instances[&c].parent;
        }
        chain.reverse();
        chain
    }

    fn roots(&self) -> Vec<i64> {
        let mut roots: Vec<i64> = self
            .instances
            .values()
            .filter(|i| i.parent.is_none())
            .map(|i| i.id)
            .collect();
        sort_by_name_then_id(self, &mut roots);
        roots
    }

    fn edit_instance(
        &mut self,
        id: i64,
        patch: InstancePatch,
        now: i64,
    ) -> Result<(), CoreError> {
        if !self.instances.contains_key(&id) {
            return Err(CoreError::NotFound(id));
        }
        let class_name = self.instances[&id].class.clone();
        extend_class_schema(self, &class_name, &patch.set_fields);

        let inst = self.instances.get_mut(&id).expect("checked above");
        if let Some(name) = patch.name {
            inst.name = name;
        }
        for (k, v) in patch.set_fields {
            inst.fields.insert(k, v);
        }
        for k in patch.remove_fields {
            inst.fields.remove(&k);
        }
        inst.updated_at = now;
        Ok(())
    }

    fn move_instance(
        &mut self,
        id: i64,
        new_parent: Option<i64>,
        now: i64,
    ) -> Result<(), CoreError> {
        if !self.instances.contains_key(&id) {
            return Err(CoreError::NotFound(id));
        }
        if let Some(p) = new_parent {
            if !self.instances.contains_key(&p) {
                return Err(CoreError::InvalidParent(p));
            }
            // Reject self-parent and cycles: new parent must not be id, nor a
            // descendant of id. Equivalently, id must not be on p's root path.
            if p == id || self.path_of(p).contains(&id) {
                return Err(CoreError::WouldCycle);
            }
        }
        let inst = self.instances.get_mut(&id).expect("checked above");
        inst.parent = new_parent;
        inst.updated_at = now;
        Ok(())
    }

    fn remove_instance(&mut self, id: i64, mode: RemoveMode) -> Result<Vec<i64>, CoreError> {
        if !self.instances.contains_key(&id) {
            return Err(CoreError::NotFound(id));
        }
        let removed: Vec<i64> = match mode {
            RemoveMode::Cascade => {
                let mut all = self.descendants_of(id);
                all.push(id);
                all
            }
            RemoveMode::Reparent => {
                let new_parent = self.instances[&id].parent;
                let children = self.children_of(id);
                for child in children {
                    if let Some(c) = self.instances.get_mut(&child) {
                        c.parent = new_parent;
                    }
                }
                vec![id]
            }
        };

        let removed_set: BTreeSet<i64> = removed.iter().copied().collect();
        for rid in &removed_set {
            self.instances.remove(rid);
        }
        // Strip any dangling relationship edges pointing at removed instances.
        for inst in self.instances.values_mut() {
            inst.relationships
                .retain(|r| !removed_set.contains(&r.target));
        }
        Ok(removed)
    }

    fn add_tag(&mut self, id: i64, tag: &str, now: i64) -> Result<(), CoreError> {
        let inst = self.instances.get_mut(&id).ok_or(CoreError::NotFound(id))?;
        inst.tags.insert(tag.to_string());
        inst.updated_at = now;
        Ok(())
    }

    fn remove_tag(&mut self, id: i64, tag: &str, now: i64) -> Result<(), CoreError> {
        let inst = self.instances.get_mut(&id).ok_or(CoreError::NotFound(id))?;
        inst.tags.remove(tag);
        inst.updated_at = now;
        Ok(())
    }

    fn add_relationship(
        &mut self,
        id: i64,
        kind: &str,
        target: i64,
        now: i64,
    ) -> Result<(), CoreError> {
        if !self.instances.contains_key(&id) {
            return Err(CoreError::NotFound(id));
        }
        if !self.instances.contains_key(&target) {
            return Err(CoreError::NotFound(target));
        }
        let inst = self.instances.get_mut(&id).expect("checked above");
        let rel = Relationship {
            kind: kind.to_string(),
            target,
        };
        if !inst.relationships.contains(&rel) {
            inst.relationships.push(rel);
        }
        inst.updated_at = now;
        Ok(())
    }

    fn remove_relationship(
        &mut self,
        id: i64,
        kind: &str,
        target: i64,
        now: i64,
    ) -> Result<(), CoreError> {
        let inst = self.instances.get_mut(&id).ok_or(CoreError::NotFound(id))?;
        inst.relationships
            .retain(|r| !(r.kind == kind && r.target == target));
        inst.updated_at = now;
        Ok(())
    }

    fn attach_photo(&mut self, id: i64, photo: Photo, now: i64) -> Result<(), CoreError> {
        let inst = self.instances.get_mut(&id).ok_or(CoreError::NotFound(id))?;
        if !inst.photos.iter().any(|p| p.key == photo.key) {
            inst.photos.push(photo);
        }
        inst.updated_at = now;
        Ok(())
    }

    fn detach_photo(&mut self, id: i64, key: &str, now: i64) -> Result<(), CoreError> {
        let inst = self.instances.get_mut(&id).ok_or(CoreError::NotFound(id))?;
        inst.photos.retain(|p| p.key != key);
        inst.updated_at = now;
        Ok(())
    }

    fn duplicate_instance(&mut self, id: i64, deep: bool, now: i64) -> Result<i64, CoreError> {
        if !self.instances.contains_key(&id) {
            return Err(CoreError::NotFound(id));
        }

        // Determine which originals to copy and assign fresh ids up front so we
        // can remap parent pointers and in-set relationship targets.
        let originals: Vec<i64> = if deep {
            let mut v = self.descendants_of(id);
            v.push(id);
            v
        } else {
            vec![id]
        };
        let id_map: BTreeMap<i64, i64> = originals
            .iter()
            .map(|&old| (old, self.next_instance_id()))
            .collect();

        let mut new_instances: Vec<Instance> = Vec::with_capacity(originals.len());
        for &old in &originals {
            let src = &self.instances[&old];
            let new_id = id_map[&old];

            // The root copy keeps the original's parent; non-root copies remap to
            // their copied parent (which is always inside the copied set).
            let new_parent = if old == id {
                src.parent
            } else {
                src.parent.map(|p| id_map.get(&p).copied().unwrap_or(p))
            };

            // Remap relationship targets that point inside the copied set.
            let relationships = src
                .relationships
                .iter()
                .map(|r| Relationship {
                    kind: r.kind.clone(),
                    target: id_map.get(&r.target).copied().unwrap_or(r.target),
                })
                .collect();

            new_instances.push(Instance {
                id: new_id,
                class: src.class.clone(),
                name: src.name.clone(),
                fields: src.fields.clone(),
                tags: src.tags.clone(),
                parent: new_parent,
                photos: src.photos.clone(),
                relationships,
                created_at: now,
                updated_at: now,
            });
        }

        for inst in new_instances {
            self.instances.insert(inst.id, inst);
        }
        Ok(id_map[&id])
    }

    fn change_class(&mut self, id: i64, new_class: &str, now: i64) -> Result<(), CoreError> {
        if !self.instances.contains_key(&id) {
            return Err(CoreError::NotFound(id));
        }
        self.ensure_class(new_class, now);
        // Auto-extend the target class with the instance's current fields,
        // inferring each FieldType from its value (same rule add_instance uses).
        let fields = self.instances[&id].fields.clone();
        extend_class_schema(self, new_class, &fields);

        let inst = self.instances.get_mut(&id).expect("checked above");
        inst.class = new_class.to_string();
        inst.updated_at = now;
        Ok(())
    }

    fn delete_class(&mut self, name: &str) -> Result<(), CoreError> {
        if self.instances.values().any(|i| i.class == name) {
            return Err(CoreError::ClassInUse);
        }
        // Idempotent: removing an absent class is a no-op success.
        self.classes.remove(name);
        Ok(())
    }

    fn search(&self, q: &SearchQuery) -> Vec<i64> {
        let text_lc = q.text.as_ref().map(|t| t.to_lowercase());

        let mut matches: Vec<i64> = self
            .instances
            .values()
            .filter(|inst| {
                if let Some(t) = &text_lc {
                    if !inst.name.to_lowercase().contains(t) {
                        return false;
                    }
                }
                if let Some(tag) = &q.tag {
                    if !inst.tags.contains(tag) {
                        return false;
                    }
                }
                if let Some(class) = &q.class {
                    if &inst.class != class {
                        return false;
                    }
                }
                true
            })
            .map(|inst| inst.id)
            .collect();
        sort_by_name_then_id(self, &mut matches);
        matches
    }
}

/// Deterministically sort instance ids by `(name, id)`.
fn sort_by_name_then_id(inv: &Inventory, ids: &mut [i64]) {
    ids.sort_by(|a, b| {
        let na = inv.instances.get(a).map(|i| i.name.as_str()).unwrap_or("");
        let nb = inv.instances.get(b).map(|i| i.name.as_str()).unwrap_or("");
        na.cmp(nb).then_with(|| a.cmp(b))
    });
}

/// Extend a class's field list with any field names present in `fields` that are
/// not already declared, inferring the [`FieldType`] from each value. Adds each
/// missing field exactly once.
fn extend_class_schema(
    inv: &mut Inventory,
    class_name: &str,
    fields: &BTreeMap<String, FieldValue>,
) {
    let Some(class) = inv.classes.get_mut(class_name) else {
        return;
    };
    for (fname, fval) in fields {
        if !class.fields.iter().any(|f| &f.name == fname) {
            class.fields.push(FieldDef {
                name: fname.clone(),
                field_type: infer_field_type(fval),
                required: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv() -> Inventory {
        Inventory::new()
    }

    #[test]
    fn ensure_class_creates_then_reuses() {
        let mut w = inv();
        w.ensure_class("Item", 1);
        assert!(w.class_exists("Item"));
        assert_eq!(w.get_class("Item").unwrap().created_at, 1);
        // re-ensuring does not overwrite created_at and does not duplicate
        w.ensure_class("Item", 2);
        assert_eq!(w.get_class("Item").unwrap().created_at, 1);
        assert_eq!(w.classes.len(), 1);
    }

    #[test]
    fn class_exists_and_get_class() {
        let mut w = inv();
        assert!(!w.class_exists("Box"));
        assert!(w.get_class("Box").is_none());
        w.ensure_class("Box", 1);
        assert!(w.class_exists("Box"));
        assert_eq!(w.get_class("Box").unwrap().name, "Box");
    }

    #[test]
    fn add_instance_auto_creates_class_and_extends_fields() {
        let mut w = inv();
        let mut fields = BTreeMap::new();
        fields.insert("color".to_string(), FieldValue::Text("red".to_string()));
        fields.insert("qty".to_string(), FieldValue::Number(3.0));
        let id = w.add_instance("Widget", "thing", fields, None, 5).unwrap();

        let inst = w.get(id).unwrap();
        assert_eq!(inst.id, id);
        assert_eq!(inst.class, "Widget");
        assert_eq!(inst.name, "thing");
        assert_eq!(inst.created_at, 5);
        assert_eq!(inst.updated_at, 5);
        assert_eq!(inst.parent, None);

        let class = w.get_class("Widget").unwrap();
        // class extended with both fields, exactly once each
        assert_eq!(class.fields.len(), 2);
        let names: Vec<&str> = class.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"color"));
        assert!(names.contains(&"qty"));
        let color = class.fields.iter().find(|f| f.name == "color").unwrap();
        assert_eq!(color.field_type, FieldType::Text);
        let qty = class.fields.iter().find(|f| f.name == "qty").unwrap();
        assert_eq!(qty.field_type, FieldType::Number);
    }

    #[test]
    fn add_instance_assigns_sequential_ids() {
        let mut w = inv();
        let a = w.add_instance("C", "a", BTreeMap::new(), None, 1).unwrap();
        let b = w.add_instance("C", "b", BTreeMap::new(), None, 1).unwrap();
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(w.next_id, 3);
    }

    #[test]
    fn add_instance_rejects_missing_parent() {
        let mut w = inv();
        let bogus = 999;
        let err = w
            .add_instance("C", "x", BTreeMap::new(), Some(bogus), 1)
            .unwrap_err();
        assert_eq!(err, CoreError::InvalidParent(bogus));
    }

    fn add(w: &mut Inventory, class: &str, name: &str, parent: Option<i64>) -> i64 {
        w.add_instance(class, name, BTreeMap::new(), parent, 1).unwrap()
    }

    #[test]
    fn tree_navigation() {
        let mut w = inv();
        let root = add(&mut w, "C", "root", None);
        let b = add(&mut w, "C", "b", Some(root));
        let a = add(&mut w, "C", "a", Some(root));
        let a1 = add(&mut w, "C", "a1", Some(a));

        // children sorted by name then id
        assert_eq!(w.children_of(root), vec![a, b]);
        assert_eq!(w.children_of(a), vec![a1]);
        assert_eq!(w.children_of(a1), Vec::<i64>::new());

        // roots
        assert_eq!(w.roots(), vec![root]);

        // descendants include all of subtree, deterministic
        let mut desc = w.descendants_of(root);
        desc.sort();
        let mut expected = vec![a, b, a1];
        expected.sort();
        assert_eq!(desc, expected);

        // path root..=id
        assert_eq!(w.path_of(a1), vec![root, a, a1]);
        assert_eq!(w.path_of(root), vec![root]);
    }

    #[test]
    fn edit_instance_patch() {
        let mut w = inv();
        let id = w.add_instance("C", "old", BTreeMap::new(), None, 1).unwrap();
        let mut set = BTreeMap::new();
        set.insert("note".to_string(), FieldValue::Text("hi".to_string()));
        set.insert("drop".to_string(), FieldValue::Bool(true));
        w.edit_instance(
            id,
            InstancePatch {
                name: Some("new".to_string()),
                set_fields: set,
                remove_fields: vec![],
            },
            10,
        )
        .unwrap();

        // remove the field we just set
        w.edit_instance(
            id,
            InstancePatch {
                name: None,
                set_fields: BTreeMap::new(),
                remove_fields: vec!["drop".to_string()],
            },
            20,
        )
        .unwrap();

        let inst = w.get(id).unwrap();
        assert_eq!(inst.name, "new");
        assert_eq!(inst.updated_at, 20);
        assert_eq!(inst.created_at, 1);
        assert_eq!(
            inst.fields.get("note"),
            Some(&FieldValue::Text("hi".to_string()))
        );
        assert!(!inst.fields.contains_key("drop"));

        // schema was extended for both new fields (exactly once each)
        let names: Vec<&str> = w
            .get_class("C")
            .unwrap()
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert!(names.contains(&"note"));
        assert!(names.contains(&"drop"));
        assert_eq!(w.get_class("C").unwrap().fields.len(), 2);
    }

    #[test]
    fn edit_missing_instance_errs() {
        let mut w = inv();
        let bogus = 999;
        let err = w
            .edit_instance(bogus, InstancePatch::default(), 1)
            .unwrap_err();
        assert_eq!(err, CoreError::NotFound(bogus));
    }

    #[test]
    fn move_instance_basic_and_cycle_rejection() {
        let mut w = inv();
        let a = add(&mut w, "C", "a", None);
        let b = add(&mut w, "C", "b", Some(a));
        let c = add(&mut w, "C", "c", Some(b));

        // valid move: c -> root
        w.move_instance(c, None, 5).unwrap();
        assert_eq!(w.get(c).unwrap().parent, None);
        assert_eq!(w.get(c).unwrap().updated_at, 5);

        // put it back
        w.move_instance(c, Some(b), 6).unwrap();

        // self-parent rejected
        assert_eq!(
            w.move_instance(a, Some(a), 7).unwrap_err(),
            CoreError::WouldCycle
        );
        // descendant-parent rejected (a -> c, c is descendant of a)
        assert_eq!(
            w.move_instance(a, Some(c), 7).unwrap_err(),
            CoreError::WouldCycle
        );

        // missing target instance
        let bogus = 999;
        assert_eq!(
            w.move_instance(bogus, None, 7).unwrap_err(),
            CoreError::NotFound(bogus)
        );
        // missing new parent
        assert_eq!(
            w.move_instance(a, Some(bogus), 7).unwrap_err(),
            CoreError::InvalidParent(bogus)
        );
    }

    #[test]
    fn move_cycle_rejection_no_mutation() {
        let mut w = inv();
        let a = add(&mut w, "C", "a", None);
        let b = add(&mut w, "C", "b", Some(a));
        let c = add(&mut w, "C", "c", Some(b));
        let before = w.clone();
        let err = w.move_instance(a, Some(c), 99).unwrap_err();
        assert_eq!(err, CoreError::WouldCycle);
        assert_eq!(w, before);
    }

    #[test]
    fn remove_cascade_strips_subtree_and_edges() {
        let mut w = inv();
        let root = add(&mut w, "C", "root", None);
        let a = add(&mut w, "C", "a", Some(root));
        let a1 = add(&mut w, "C", "a1", Some(a));
        let other = add(&mut w, "C", "other", None);
        // other points at a1 via relationship -> should be stripped
        w.add_relationship(other, "ref", a1, 1).unwrap();

        let mut removed = w.remove_instance(a, RemoveMode::Cascade).unwrap();
        removed.sort();
        let mut expected = vec![a, a1];
        expected.sort();
        assert_eq!(removed, expected);

        assert!(w.get(a).is_none());
        assert!(w.get(a1).is_none());
        assert!(w.get(root).is_some());
        // dangling edge stripped
        assert!(w.get(other).unwrap().relationships.is_empty());
    }

    #[test]
    fn remove_reparent_moves_children_up() {
        let mut w = inv();
        let root = add(&mut w, "C", "root", None);
        let a = add(&mut w, "C", "a", Some(root));
        let a1 = add(&mut w, "C", "a1", Some(a));
        let a2 = add(&mut w, "C", "a2", Some(a));

        let removed = w.remove_instance(a, RemoveMode::Reparent).unwrap();
        assert_eq!(removed, vec![a]);
        assert!(w.get(a).is_none());
        // children reparented onto root (a's parent)
        assert_eq!(w.get(a1).unwrap().parent, Some(root));
        assert_eq!(w.get(a2).unwrap().parent, Some(root));
    }

    #[test]
    fn remove_missing_errs() {
        let mut w = inv();
        let bogus = 999;
        assert_eq!(
            w.remove_instance(bogus, RemoveMode::Cascade).unwrap_err(),
            CoreError::NotFound(bogus)
        );
    }

    #[test]
    fn tags_relationships_photos() {
        let mut w = inv();
        let a = add(&mut w, "C", "a", None);
        let b = add(&mut w, "C", "b", None);

        // tags: idempotent insert, remove
        w.add_tag(a, "x", 2).unwrap();
        w.add_tag(a, "x", 3).unwrap();
        assert_eq!(w.get(a).unwrap().tags.len(), 1);
        assert_eq!(w.get(a).unwrap().updated_at, 3);
        w.remove_tag(a, "x", 4).unwrap();
        assert!(w.get(a).unwrap().tags.is_empty());

        // relationships: idempotent insert (no dup), remove
        w.add_relationship(a, "ref", b, 5).unwrap();
        w.add_relationship(a, "ref", b, 6).unwrap();
        assert_eq!(w.get(a).unwrap().relationships.len(), 1);
        w.remove_relationship(a, "ref", b, 7).unwrap();
        assert!(w.get(a).unwrap().relationships.is_empty());
        // relationship to missing target errs
        let bogus = 999;
        assert_eq!(
            w.add_relationship(a, "ref", bogus, 8).unwrap_err(),
            CoreError::NotFound(bogus)
        );

        // photos: idempotent attach (by key), detach by key
        let p = Photo {
            key: format!("{a}-0"),
            mime: "image/png".to_string(),
            name: "front.png".to_string(),
        };
        w.attach_photo(a, p.clone(), 9).unwrap();
        w.attach_photo(a, p.clone(), 10).unwrap();
        assert_eq!(w.get(a).unwrap().photos.len(), 1);
        w.detach_photo(a, &p.key, 11).unwrap();
        assert!(w.get(a).unwrap().photos.is_empty());
    }

    #[test]
    fn duplicate_shallow() {
        let mut w = inv();
        let parent = add(&mut w, "C", "parent", None);
        let mut set = BTreeMap::new();
        set.insert("k".to_string(), FieldValue::Number(1.0));
        let a = w.add_instance("C", "a", set, Some(parent), 1).unwrap();
        let child = add(&mut w, "C", "child", Some(a));
        w.add_tag(a, "t", 1).unwrap();

        let copy = w.duplicate_instance(a, false, 50).unwrap();
        assert_ne!(copy, a);
        let c = w.get(copy).unwrap();
        // same parent, name, fields, tags
        assert_eq!(c.parent, Some(parent));
        assert_eq!(c.name, "a");
        assert_eq!(c.fields.get("k"), Some(&FieldValue::Number(1.0)));
        assert!(c.tags.contains("t"));
        assert_eq!(c.created_at, 50);
        assert_eq!(c.updated_at, 50);
        // shallow: the child is NOT copied (copy has no children)
        assert!(w.children_of(copy).is_empty());
        // original child still under original a
        assert_eq!(w.get(child).unwrap().parent, Some(a));
    }

    #[test]
    fn duplicate_deep_remaps() {
        let mut w = inv();
        let parent = add(&mut w, "C", "parent", None);
        let a = add(&mut w, "C", "a", Some(parent));
        let b = add(&mut w, "C", "b", Some(a));
        let c = add(&mut w, "C", "c", Some(a));
        let outside = add(&mut w, "C", "outside", None);
        // relationship inside the subtree: b -> c (should remap)
        w.add_relationship(b, "in", c, 1).unwrap();
        // relationship to outside the subtree: b -> outside (should stay)
        w.add_relationship(b, "out", outside, 1).unwrap();

        let before_count = w.instances.len();
        let new_root = w.duplicate_instance(a, true, 99).unwrap();

        // 3 new instances added (a,b,c)
        assert_eq!(w.instances.len(), before_count + 3);
        // root copy keeps original parent
        assert_eq!(w.get(new_root).unwrap().parent, Some(parent));

        // structural isomorphism: copy has 2 children named b and c
        let copy_kids = w.children_of(new_root);
        assert_eq!(copy_kids.len(), 2);
        let kid_names: Vec<&str> = copy_kids
            .iter()
            .map(|k| w.get(*k).unwrap().name.as_str())
            .collect();
        assert!(kid_names.contains(&"b"));
        assert!(kid_names.contains(&"c"));

        // find the copied b and c
        let copy_b = *copy_kids
            .iter()
            .find(|k| w.get(**k).unwrap().name == "b")
            .unwrap();
        let copy_c = *copy_kids
            .iter()
            .find(|k| w.get(**k).unwrap().name == "c")
            .unwrap();

        // id sets disjoint from originals
        assert_ne!(copy_b, b);
        assert_ne!(copy_c, c);

        // relationship inside set remapped to copy_c; outside kept as-is
        let cb = w.get(copy_b).unwrap();
        let in_rel = cb.relationships.iter().find(|r| r.kind == "in").unwrap();
        assert_eq!(in_rel.target, copy_c);
        let out_rel = cb.relationships.iter().find(|r| r.kind == "out").unwrap();
        assert_eq!(out_rel.target, outside);

        // original subtree unchanged
        assert_eq!(w.get(a).unwrap().parent, Some(parent));
        assert_eq!(w.get(b).unwrap().parent, Some(a));
        let orig_in = w
            .get(b)
            .unwrap()
            .relationships
            .iter()
            .find(|r| r.kind == "in")
            .unwrap();
        assert_eq!(orig_in.target, c);
    }

    #[test]
    fn search_and_semantics() {
        let mut w = inv();
        let apple = w
            .add_instance("Fruit", "Apple", BTreeMap::new(), None, 1)
            .unwrap();
        let apricot = w
            .add_instance("Fruit", "Apricot", BTreeMap::new(), None, 1)
            .unwrap();
        let banana = w
            .add_instance("Veg", "Banana", BTreeMap::new(), None, 1)
            .unwrap();
        w.add_tag(apple, "fresh", 1).unwrap();
        w.add_tag(apricot, "fresh", 1).unwrap();

        // text only, case-insensitive substring
        let q = SearchQuery {
            text: Some("ap".to_string()),
            ..Default::default()
        };
        let mut res = w.search(&q);
        res.sort();
        let mut exp = vec![apple, apricot];
        exp.sort();
        assert_eq!(res, exp);

        // class only
        let q = SearchQuery {
            class: Some("Veg".to_string()),
            ..Default::default()
        };
        assert_eq!(w.search(&q), vec![banana]);

        // AND semantics: text + tag + class
        let q = SearchQuery {
            text: Some("apr".to_string()),
            tag: Some("fresh".to_string()),
            class: Some("Fruit".to_string()),
        };
        assert_eq!(w.search(&q), vec![apricot]);

        // empty query returns all, deterministic (by name then id)
        let q = SearchQuery::default();
        assert_eq!(w.search(&q), vec![apple, apricot, banana]);

        // no match
        let q = SearchQuery {
            tag: Some("nope".to_string()),
            ..Default::default()
        };
        assert!(w.search(&q).is_empty());

        // unknown class name -> nothing matches
        let q = SearchQuery {
            class: Some("Nope".to_string()),
            ..Default::default()
        };
        assert!(w.search(&q).is_empty());
    }

    #[test]
    fn change_class_moves_instance_and_extends_target() {
        let mut w = inv();
        let mut fields = BTreeMap::new();
        fields.insert("color".to_string(), FieldValue::Text("red".to_string()));
        fields.insert("qty".to_string(), FieldValue::Number(2.0));
        let id = w.add_instance("Old", "thing", fields, None, 1).unwrap();
        assert_eq!(w.get(id).unwrap().class, "Old");

        // Target class "New" does not exist yet -> change_class auto-creates it.
        assert!(!w.class_exists("New"));
        w.change_class(id, "New", 42).unwrap();

        let inst = w.get(id).unwrap();
        assert_eq!(inst.class, "New");
        assert_eq!(inst.updated_at, 42);
        // fields remain on the instance
        assert_eq!(
            inst.fields.get("color"),
            Some(&FieldValue::Text("red".to_string()))
        );

        // The new class was created and extended with the instance's fields,
        // with FieldType inferred from each value, exactly once each.
        let class = w.get_class("New").unwrap();
        assert_eq!(class.created_at, 42);
        assert_eq!(class.fields.len(), 2);
        let color = class.fields.iter().find(|f| f.name == "color").unwrap();
        assert_eq!(color.field_type, FieldType::Text);
        let qty = class.fields.iter().find(|f| f.name == "qty").unwrap();
        assert_eq!(qty.field_type, FieldType::Number);
    }

    #[test]
    fn change_class_missing_instance_errs() {
        let mut w = inv();
        let bogus = 999;
        let err = w.change_class(bogus, "New", 1).unwrap_err();
        assert_eq!(err, CoreError::NotFound(bogus));
    }

    #[test]
    fn delete_class_in_use_errs_with_no_mutation() {
        let mut w = inv();
        let id = w.add_instance("Box", "a", BTreeMap::new(), None, 1).unwrap();
        assert!(w.class_exists("Box"));
        let before = w.clone();

        let err = w.delete_class("Box").unwrap_err();
        assert_eq!(err, CoreError::ClassInUse);
        // no mutation: the class and instance are untouched
        assert!(w.class_exists("Box"));
        assert!(w.get(id).is_some());
        assert_eq!(w, before);
    }

    #[test]
    fn delete_class_succeeds_once_unused_and_is_idempotent() {
        let mut w = inv();
        let id = w.add_instance("Box", "a", BTreeMap::new(), None, 1).unwrap();
        // Still in use -> error.
        assert_eq!(w.delete_class("Box").unwrap_err(), CoreError::ClassInUse);

        // Remove the only instance referencing it.
        w.remove_instance(id, RemoveMode::Cascade).unwrap();
        assert!(w.class_exists("Box"));

        // Now deletion succeeds.
        w.delete_class("Box").unwrap();
        assert!(!w.class_exists("Box"));

        // Idempotent: deleting an absent class is a no-op success.
        w.delete_class("Box").unwrap();
        assert!(!w.class_exists("Box"));
        // Deleting a never-existed class is also a no-op success.
        w.delete_class("NeverExisted").unwrap();
    }

    #[test]
    fn change_class_then_delete_old_class() {
        let mut w = inv();
        let id = w.add_instance("Old", "a", BTreeMap::new(), None, 1).unwrap();
        // Old class is in use, cannot delete yet.
        assert_eq!(w.delete_class("Old").unwrap_err(), CoreError::ClassInUse);

        // Move the instance to a different class.
        w.change_class(id, "New", 5).unwrap();
        assert_eq!(w.get(id).unwrap().class, "New");

        // Now nothing references "Old" -> it can be deleted.
        assert!(w.class_exists("Old"));
        w.delete_class("Old").unwrap();
        assert!(!w.class_exists("Old"));
        // "New" still present and in use.
        assert!(w.class_exists("New"));
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    fn inv() -> Inventory {
        Inventory::new()
    }

    /// Returns true if any instance is reachable from itself by following parent
    /// pointers (i.e. the containment graph contains a cycle).
    fn has_cycle(w: &Inventory) -> bool {
        for &start in w.instances.keys() {
            let mut seen = BTreeSet::new();
            let mut cur = w.instances[&start].parent;
            while let Some(c) = cur {
                if c == start {
                    return true;
                }
                if !seen.insert(c) {
                    // hit a pre-existing cycle not involving `start`
                    break;
                }
                cur = w.instances.get(&c).and_then(|i| i.parent);
            }
        }
        false
    }

    /// A random operation applied to the inventory during invariant 1.
    #[derive(Debug, Clone)]
    enum Op {
        /// Add an instance under the parent at the given index (modulo count), or
        /// as a root.
        Add {
            class: u8,
            as_root: bool,
            parent_idx: usize,
        },
        /// Move the instance at `idx` to the parent at `parent_idx` (or to root).
        Move {
            idx: usize,
            to_root: bool,
            parent_idx: usize,
        },
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..4, any::<bool>(), 0usize..32)
                .prop_map(|(class, as_root, parent_idx)| Op::Add {
                    class,
                    as_root,
                    parent_idx
                }),
            (0usize..32, any::<bool>(), 0usize..32)
                .prop_map(|(idx, to_root, parent_idx)| Op::Move {
                    idx,
                    to_root,
                    parent_idx
                }),
        ]
    }

    proptest! {
        // Invariant 1: applying random add/move ops never creates a cycle, and
        // path_of always terminates (it returns and the root has no parent).
        #[test]
        fn prop_acyclic(ops in proptest::collection::vec(op_strategy(), 0..60)) {
            let mut w = inv();
            let mut ids: Vec<i64> = Vec::new();
            let mut now = 0i64;
            for op in ops {
                now += 1;
                match op {
                    Op::Add { class, as_root, parent_idx } => {
                        let parent = if as_root || ids.is_empty() {
                            None
                        } else {
                            Some(ids[parent_idx % ids.len()])
                        };
                        let cname = format!("C{class}");
                        let name = format!("n{now}");
                        if let Ok(id) = w.add_instance(&cname, &name, BTreeMap::new(), parent, now) {
                            ids.push(id);
                        }
                    }
                    Op::Move { idx, to_root, parent_idx } => {
                        if ids.is_empty() {
                            continue;
                        }
                        let id = ids[idx % ids.len()];
                        let new_parent = if to_root {
                            None
                        } else {
                            Some(ids[parent_idx % ids.len()])
                        };
                        // Result may be Err(WouldCycle) — that's fine; we only
                        // assert the invariant holds afterwards either way.
                        let _ = w.move_instance(id, new_parent, now);
                    }
                }
                prop_assert!(!has_cycle(&w), "cycle introduced by op");
            }
            // path_of terminates for every instance, starts at a root, ends at id.
            for &id in &ids {
                if w.get(id).is_none() {
                    continue;
                }
                let path = w.path_of(id);
                prop_assert!(!path.is_empty());
                prop_assert_eq!(*path.last().unwrap(), id);
                let root = path[0];
                prop_assert_eq!(w.get(root).unwrap().parent, None);
            }
        }

        // Invariant 2: chain a>b>c; move_instance(a, Some(c)) is rejected and the
        // inventory is byte-identical (and value-identical) to before.
        #[test]
        fn prop_cycle_rejected(seed in any::<u64>()) {
            let _ = seed; // structure is fixed; seed just drives multiple runs
            let mut w = inv();
            let a = w.add_instance("C", "a", BTreeMap::new(), None, 1).unwrap();
            let b = w.add_instance("C", "b", BTreeMap::new(), Some(a), 2).unwrap();
            let c = w.add_instance("C", "c", BTreeMap::new(), Some(b), 3).unwrap();
            let before = w.clone();
            let before_bytes = before.to_json_bytes().unwrap();

            let err = w.move_instance(a, Some(c), 99).unwrap_err();
            prop_assert_eq!(err, CoreError::WouldCycle);
            prop_assert_eq!(&w, &before);
            prop_assert_eq!(w.to_json_bytes().unwrap(), before_bytes);
        }

        // Invariant 3: deep-duplicate a random small subtree; subtree sizes equal,
        // id sets disjoint, structural isomorphism, original unchanged.
        #[test]
        fn prop_deep_duplicate(
            // shape: for each of up to 6 nodes (besides root), pick an existing
            // node index to attach under.
            parents in proptest::collection::vec(0usize..16, 0..6),
        ) {
            let mut w = inv();
            let root = w.add_instance("K", "root", BTreeMap::new(), None, 1).unwrap();
            let mut nodes = vec![root];
            let mut now = 1i64;
            for p in parents {
                now += 1;
                let parent = nodes[p % nodes.len()];
                let id = w
                    .add_instance("K", &format!("n{now}"), BTreeMap::new(), Some(parent), now)
                    .unwrap();
                // give each node a tag so isomorphism check is meaningful
                w.add_tag(id, &format!("t{now}"), now).unwrap();
                nodes.push(id);
            }

            let orig_subtree: BTreeSet<i64> =
                w.descendants_of(root).into_iter().chain(std::iter::once(root)).collect();
            let before = w.clone();

            let copy_root = w.duplicate_instance(root, true, 1000).unwrap();
            let copy_subtree: BTreeSet<i64> =
                w.descendants_of(copy_root).into_iter().chain(std::iter::once(copy_root)).collect();

            // sizes equal
            prop_assert_eq!(orig_subtree.len(), copy_subtree.len());
            // id sets disjoint
            prop_assert!(orig_subtree.is_disjoint(&copy_subtree));

            // original subtree unchanged (every original instance equals before)
            for id in &orig_subtree {
                prop_assert_eq!(w.get(*id), before.get(*id));
            }

            // structural isomorphism: compare normalized (name, class-name, tags,
            // fields, child-multiset) signatures of both trees.
            fn signature(w: &Inventory, id: i64) -> Vec<String> {
                let mut out = Vec::new();
                let mut stack = vec![(id, 0usize)];
                while let Some((cur, depth)) = stack.pop() {
                    let inst = w.get(cur).unwrap();
                    let tags: Vec<String> = inst.tags.iter().cloned().collect();
                    out.push(format!(
                        "{depth}|{}|{}|{:?}|{:?}",
                        inst.name, inst.class, tags, inst.fields
                    ));
                    let mut kids = w.children_of(cur);
                    // sort children by name for stable shape comparison
                    kids.sort_by(|a, b| {
                        w.get(*a).unwrap().name.cmp(&w.get(*b).unwrap().name)
                    });
                    for k in kids {
                        stack.push((k, depth + 1));
                    }
                }
                out.sort();
                out
            }
            prop_assert_eq!(signature(&w, root), signature(&w, copy_root));
            // root copy keeps original's parent
            prop_assert_eq!(w.get(copy_root).unwrap().parent, w.get(root).unwrap().parent);
        }

        // Invariant 4: ensure_class is idempotent.
        #[test]
        fn prop_ensure_class_idempotent(name in "[A-Za-z][A-Za-z0-9 ]{0,8}") {
            let mut w = inv();
            w.ensure_class(&name, 1);
            let created = w.get_class(&name).unwrap().created_at;
            w.ensure_class(&name, 2);
            // created_at unchanged and exactly one class with this name
            prop_assert_eq!(w.get_class(&name).unwrap().created_at, created);
            let count = w.classes.values().filter(|c| c.name == name).count();
            prop_assert_eq!(count, 1);
        }

        // Invariant 5: auto field-extension adds each new field exactly once.
        #[test]
        fn prop_field_extension_once(
            field in "[a-z]{1,6}",
            n in 1usize..6,
        ) {
            let mut w = inv();
            for i in 0..n {
                let mut fields = BTreeMap::new();
                fields.insert(field.clone(), FieldValue::Text(format!("v{i}")));
                w.add_instance("E", &format!("i{i}"), fields, None, i as i64).unwrap();
            }
            let occurrences =
                w.get_class("E").unwrap().fields.iter().filter(|f| f.name == field).count();
            prop_assert_eq!(occurrences, 1);
        }

        // Invariant 6: cascade remove deletes the whole subtree, leaves no parent
        // pointer into the removed set, and no relationship targeting a removed id.
        #[test]
        fn prop_cascade_remove(
            parents in proptest::collection::vec(0usize..16, 0..8),
            rel_pairs in proptest::collection::vec((0usize..16, 0usize..16), 0..8),
        ) {
            let mut w = inv();
            let root = w.add_instance("R", "root", BTreeMap::new(), None, 1).unwrap();
            let mut nodes = vec![root];
            let mut now = 1i64;
            for p in parents {
                now += 1;
                let parent = nodes[p % nodes.len()];
                let id = w
                    .add_instance("R", &format!("n{now}"), BTreeMap::new(), Some(parent), now)
                    .unwrap();
                nodes.push(id);
            }
            // sprinkle relationships between random nodes
            for (s, t) in rel_pairs {
                now += 1;
                let src = nodes[s % nodes.len()];
                let tgt = nodes[t % nodes.len()];
                let _ = w.add_relationship(src, "ref", tgt, now);
            }

            let expected_removed: BTreeSet<i64> =
                w.descendants_of(root).into_iter().chain(std::iter::once(root)).collect();

            let removed = w.remove_instance(root, RemoveMode::Cascade).unwrap();
            let removed_set: BTreeSet<i64> = removed.into_iter().collect();
            prop_assert_eq!(&removed_set, &expected_removed);

            // none of the removed survive
            for id in &removed_set {
                prop_assert!(w.get(*id).is_none());
            }
            // no surviving instance has a parent in the removed set, and no
            // relationship targets a removed id.
            for inst in w.instances.values() {
                if let Some(p) = inst.parent {
                    prop_assert!(!removed_set.contains(&p));
                }
                for r in &inst.relationships {
                    prop_assert!(!removed_set.contains(&r.target));
                }
            }
        }
    }
}
