//! `inv-core`: domain logic over [`inv_model::Workspace`].
//!
//! Since `Workspace` is a foreign type we cannot add inherent methods, so the API
//! is exposed through the [`WorkspaceExt`] extension trait implemented for
//! `Workspace`. Time is injected: every mutator takes `now: i64` (unix millis),
//! keeping the logic deterministic and testable.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use inv_model::{
    Class, ClassId, FieldDef, FieldType, FieldValue, Instance, InstanceId, PhotoId, Relationship,
    Workspace,
};

/// Errors produced by the domain operations in this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// No instance with the given id exists.
    NotFound(InstanceId),
    /// No class matched (by name).
    ClassNotFound,
    /// The requested move would introduce a cycle in the containment tree.
    WouldCycle,
    /// The requested parent is not a valid container (e.g. does not exist).
    InvalidParent(InstanceId),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::NotFound(id) => write!(f, "instance not found: {id}"),
            CoreError::ClassNotFound => write!(f, "class not found"),
            CoreError::WouldCycle => write!(f, "operation would create a cycle"),
            CoreError::InvalidParent(id) => write!(f, "invalid parent: {id}"),
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

/// How [`WorkspaceExt::remove_instance`] handles an instance's children.
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

/// Domain operations over a [`Workspace`].
pub trait WorkspaceExt {
    fn add_class(&mut self, name: &str, fields: Vec<FieldDef>, now: i64) -> ClassId;
    fn class_by_name(&self, name: &str) -> Option<ClassId>;
    fn ensure_class(&mut self, name: &str, now: i64) -> ClassId;
    fn add_instance(
        &mut self,
        class_name: &str,
        name: &str,
        fields: BTreeMap<String, FieldValue>,
        parent: Option<InstanceId>,
        now: i64,
    ) -> Result<InstanceId, CoreError>;
    fn get(&self, id: InstanceId) -> Option<&Instance>;
    fn children_of(&self, id: InstanceId) -> Vec<InstanceId>;
    fn descendants_of(&self, id: InstanceId) -> Vec<InstanceId>;
    fn path_of(&self, id: InstanceId) -> Vec<InstanceId>;
    fn roots(&self) -> Vec<InstanceId>;
    fn edit_instance(
        &mut self,
        id: InstanceId,
        patch: InstancePatch,
        now: i64,
    ) -> Result<(), CoreError>;
    fn move_instance(
        &mut self,
        id: InstanceId,
        new_parent: Option<InstanceId>,
        now: i64,
    ) -> Result<(), CoreError>;
    fn remove_instance(
        &mut self,
        id: InstanceId,
        mode: RemoveMode,
    ) -> Result<Vec<InstanceId>, CoreError>;
    fn add_tag(&mut self, id: InstanceId, tag: &str, now: i64) -> Result<(), CoreError>;
    fn remove_tag(&mut self, id: InstanceId, tag: &str, now: i64) -> Result<(), CoreError>;
    fn add_relationship(
        &mut self,
        id: InstanceId,
        kind: &str,
        target: InstanceId,
        now: i64,
    ) -> Result<(), CoreError>;
    fn remove_relationship(
        &mut self,
        id: InstanceId,
        kind: &str,
        target: InstanceId,
        now: i64,
    ) -> Result<(), CoreError>;
    fn attach_photo(&mut self, id: InstanceId, photo: PhotoId, now: i64) -> Result<(), CoreError>;
    fn detach_photo(&mut self, id: InstanceId, photo: PhotoId, now: i64) -> Result<(), CoreError>;
    fn duplicate_instance(
        &mut self,
        id: InstanceId,
        deep: bool,
        now: i64,
    ) -> Result<InstanceId, CoreError>;
    fn search(&self, q: &SearchQuery) -> Vec<InstanceId>;
}

impl WorkspaceExt for Workspace {
    fn add_class(&mut self, name: &str, fields: Vec<FieldDef>, now: i64) -> ClassId {
        let id = ClassId::new_random();
        self.classes.insert(
            id,
            Class {
                id,
                name: name.to_string(),
                fields,
                created_at: now,
            },
        );
        id
    }

    fn class_by_name(&self, name: &str) -> Option<ClassId> {
        self.classes
            .values()
            .filter(|c| c.name == name)
            .map(|c| c.id)
            .min()
    }

    fn ensure_class(&mut self, name: &str, now: i64) -> ClassId {
        if let Some(id) = self.class_by_name(name) {
            id
        } else {
            self.add_class(name, vec![], now)
        }
    }

    fn add_instance(
        &mut self,
        class_name: &str,
        name: &str,
        fields: BTreeMap<String, FieldValue>,
        parent: Option<InstanceId>,
        now: i64,
    ) -> Result<InstanceId, CoreError> {
        if let Some(p) = parent {
            if !self.instances.contains_key(&p) {
                return Err(CoreError::InvalidParent(p));
            }
        }
        let class_id = self.ensure_class(class_name, now);
        extend_class_schema(self, class_id, &fields);

        let id = InstanceId::new_random();
        self.instances.insert(
            id,
            Instance {
                id,
                class_id,
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

    fn get(&self, id: InstanceId) -> Option<&Instance> {
        self.instances.get(&id)
    }

    fn children_of(&self, id: InstanceId) -> Vec<InstanceId> {
        let mut kids: Vec<InstanceId> = self
            .instances
            .values()
            .filter(|i| i.parent == Some(id))
            .map(|i| i.id)
            .collect();
        sort_by_name_then_id(self, &mut kids);
        kids
    }

    fn descendants_of(&self, id: InstanceId) -> Vec<InstanceId> {
        let mut out = Vec::new();
        let mut stack = self.children_of(id);
        while let Some(cur) = stack.pop() {
            out.push(cur);
            stack.extend(self.children_of(cur));
        }
        out
    }

    fn path_of(&self, id: InstanceId) -> Vec<InstanceId> {
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

    fn roots(&self) -> Vec<InstanceId> {
        let mut roots: Vec<InstanceId> = self
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
        id: InstanceId,
        patch: InstancePatch,
        now: i64,
    ) -> Result<(), CoreError> {
        if !self.instances.contains_key(&id) {
            return Err(CoreError::NotFound(id));
        }
        let class_id = self.instances[&id].class_id;
        extend_class_schema(self, class_id, &patch.set_fields);

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
        id: InstanceId,
        new_parent: Option<InstanceId>,
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

    fn remove_instance(
        &mut self,
        id: InstanceId,
        mode: RemoveMode,
    ) -> Result<Vec<InstanceId>, CoreError> {
        if !self.instances.contains_key(&id) {
            return Err(CoreError::NotFound(id));
        }
        let removed: Vec<InstanceId> = match mode {
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

        let removed_set: BTreeSet<InstanceId> = removed.iter().copied().collect();
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

    fn add_tag(&mut self, id: InstanceId, tag: &str, now: i64) -> Result<(), CoreError> {
        let inst = self.instances.get_mut(&id).ok_or(CoreError::NotFound(id))?;
        inst.tags.insert(tag.to_string());
        inst.updated_at = now;
        Ok(())
    }

    fn remove_tag(&mut self, id: InstanceId, tag: &str, now: i64) -> Result<(), CoreError> {
        let inst = self.instances.get_mut(&id).ok_or(CoreError::NotFound(id))?;
        inst.tags.remove(tag);
        inst.updated_at = now;
        Ok(())
    }

    fn add_relationship(
        &mut self,
        id: InstanceId,
        kind: &str,
        target: InstanceId,
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
        id: InstanceId,
        kind: &str,
        target: InstanceId,
        now: i64,
    ) -> Result<(), CoreError> {
        let inst = self.instances.get_mut(&id).ok_or(CoreError::NotFound(id))?;
        inst.relationships
            .retain(|r| !(r.kind == kind && r.target == target));
        inst.updated_at = now;
        Ok(())
    }

    fn attach_photo(&mut self, id: InstanceId, photo: PhotoId, now: i64) -> Result<(), CoreError> {
        let inst = self.instances.get_mut(&id).ok_or(CoreError::NotFound(id))?;
        if !inst.photos.contains(&photo) {
            inst.photos.push(photo);
        }
        inst.updated_at = now;
        Ok(())
    }

    fn detach_photo(&mut self, id: InstanceId, photo: PhotoId, now: i64) -> Result<(), CoreError> {
        let inst = self.instances.get_mut(&id).ok_or(CoreError::NotFound(id))?;
        inst.photos.retain(|p| *p != photo);
        inst.updated_at = now;
        Ok(())
    }

    fn duplicate_instance(
        &mut self,
        id: InstanceId,
        deep: bool,
        now: i64,
    ) -> Result<InstanceId, CoreError> {
        if !self.instances.contains_key(&id) {
            return Err(CoreError::NotFound(id));
        }

        // Determine which originals to copy and assign fresh ids up front so we
        // can remap parent pointers and in-set relationship targets.
        let originals: Vec<InstanceId> = if deep {
            let mut v = self.descendants_of(id);
            v.push(id);
            v
        } else {
            vec![id]
        };
        let id_map: BTreeMap<InstanceId, InstanceId> = originals
            .iter()
            .map(|&old| (old, InstanceId::new_random()))
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
                class_id: src.class_id,
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

    fn search(&self, q: &SearchQuery) -> Vec<InstanceId> {
        let text_lc = q.text.as_ref().map(|t| t.to_lowercase());
        let target_class = q
            .class
            .as_ref()
            .map(|name| self.class_by_name(name));

        let mut matches: Vec<InstanceId> = self
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
                if let Some(class_opt) = &target_class {
                    // class name had no matching class -> nothing matches.
                    match class_opt {
                        Some(cid) => {
                            if inst.class_id != *cid {
                                return false;
                            }
                        }
                        None => return false,
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
fn sort_by_name_then_id(w: &Workspace, ids: &mut [InstanceId]) {
    ids.sort_by(|a, b| {
        let na = w.instances.get(a).map(|i| i.name.as_str()).unwrap_or("");
        let nb = w.instances.get(b).map(|i| i.name.as_str()).unwrap_or("");
        na.cmp(nb).then_with(|| a.cmp(b))
    });
}

/// Extend a class's field list with any field names present in `fields` that are
/// not already declared, inferring the [`FieldType`] from each value. Adds each
/// missing field exactly once.
fn extend_class_schema(
    w: &mut Workspace,
    class_id: ClassId,
    fields: &BTreeMap<String, FieldValue>,
) {
    let Some(class) = w.classes.get_mut(&class_id) else {
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
    use inv_model::{Workspace, WorkspaceId};

    fn ws() -> Workspace {
        Workspace::new(WorkspaceId::new_random())
    }

    #[test]
    fn add_class_then_lookup_by_name() {
        let mut w = ws();
        let cid = w.add_class("Box", vec![], 1);
        assert_eq!(w.class_by_name("Box"), Some(cid));
        assert_eq!(w.class_by_name("Nope"), None);
    }

    #[test]
    fn ensure_class_creates_then_reuses() {
        let mut w = ws();
        let a = w.ensure_class("Item", 1);
        let b = w.ensure_class("Item", 2);
        assert_eq!(a, b);
        assert_eq!(w.classes.len(), 1);
    }

    #[test]
    fn add_instance_auto_creates_class_and_extends_fields() {
        let mut w = ws();
        let mut fields = BTreeMap::new();
        fields.insert("color".to_string(), FieldValue::Text("red".to_string()));
        fields.insert("qty".to_string(), FieldValue::Number(3.0));
        let id = w
            .add_instance("Widget", "thing", fields, None, 5)
            .unwrap();

        let inst = w.get(id).unwrap();
        assert_eq!(inst.name, "thing");
        assert_eq!(inst.created_at, 5);
        assert_eq!(inst.updated_at, 5);
        assert_eq!(inst.parent, None);

        let cid = w.class_by_name("Widget").unwrap();
        let class = &w.classes[&cid];
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
    fn add_instance_rejects_missing_parent() {
        let mut w = ws();
        let bogus = InstanceId::new_random();
        let err = w
            .add_instance("C", "x", BTreeMap::new(), Some(bogus), 1)
            .unwrap_err();
        assert_eq!(err, CoreError::InvalidParent(bogus));
    }

    fn add(w: &mut Workspace, class: &str, name: &str, parent: Option<InstanceId>) -> InstanceId {
        w.add_instance(class, name, BTreeMap::new(), parent, 1).unwrap()
    }

    #[test]
    fn tree_navigation() {
        let mut w = ws();
        let root = add(&mut w, "C", "root", None);
        let b = add(&mut w, "C", "b", Some(root));
        let a = add(&mut w, "C", "a", Some(root));
        let a1 = add(&mut w, "C", "a1", Some(a));

        // children sorted by name then id
        assert_eq!(w.children_of(root), vec![a, b]);
        assert_eq!(w.children_of(a), vec![a1]);
        assert_eq!(w.children_of(a1), Vec::<InstanceId>::new());

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
        let mut w = ws();
        let id = w
            .add_instance("C", "old", BTreeMap::new(), None, 1)
            .unwrap();
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
        assert_eq!(inst.fields.get("note"), Some(&FieldValue::Text("hi".to_string())));
        assert!(!inst.fields.contains_key("drop"));

        // schema was extended for both new fields (exactly once each)
        let cid = w.class_by_name("C").unwrap();
        let names: Vec<&str> = w.classes[&cid].fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"note"));
        assert!(names.contains(&"drop"));
        assert_eq!(w.classes[&cid].fields.len(), 2);
    }

    #[test]
    fn edit_missing_instance_errs() {
        let mut w = ws();
        let bogus = InstanceId::new_random();
        let err = w.edit_instance(bogus, InstancePatch::default(), 1).unwrap_err();
        assert_eq!(err, CoreError::NotFound(bogus));
    }

    #[test]
    fn move_instance_basic_and_cycle_rejection() {
        let mut w = ws();
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
        assert_eq!(w.move_instance(a, Some(a), 7).unwrap_err(), CoreError::WouldCycle);
        // descendant-parent rejected (a -> c, c is descendant of a)
        assert_eq!(w.move_instance(a, Some(c), 7).unwrap_err(), CoreError::WouldCycle);

        // missing target instance
        let bogus = InstanceId::new_random();
        assert_eq!(w.move_instance(bogus, None, 7).unwrap_err(), CoreError::NotFound(bogus));
        // missing new parent
        assert_eq!(
            w.move_instance(a, Some(bogus), 7).unwrap_err(),
            CoreError::InvalidParent(bogus)
        );
    }

    #[test]
    fn move_cycle_rejection_no_mutation() {
        let mut w = ws();
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
        let mut w = ws();
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
        let mut w = ws();
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
        let mut w = ws();
        let bogus = InstanceId::new_random();
        assert_eq!(
            w.remove_instance(bogus, RemoveMode::Cascade).unwrap_err(),
            CoreError::NotFound(bogus)
        );
    }

    #[test]
    fn tags_relationships_photos() {
        let mut w = ws();
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
        let bogus = InstanceId::new_random();
        assert_eq!(
            w.add_relationship(a, "ref", bogus, 8).unwrap_err(),
            CoreError::NotFound(bogus)
        );

        // photos: idempotent attach, detach
        let p = PhotoId::new_random();
        w.attach_photo(a, p, 9).unwrap();
        w.attach_photo(a, p, 10).unwrap();
        assert_eq!(w.get(a).unwrap().photos.len(), 1);
        w.detach_photo(a, p, 11).unwrap();
        assert!(w.get(a).unwrap().photos.is_empty());
    }

    #[test]
    fn duplicate_shallow() {
        let mut w = ws();
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
        let mut w = ws();
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
        let kid_names: Vec<&str> =
            copy_kids.iter().map(|k| w.get(*k).unwrap().name.as_str()).collect();
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
        let orig_in = w.get(b).unwrap().relationships.iter().find(|r| r.kind == "in").unwrap();
        assert_eq!(orig_in.target, c);
    }

    #[test]
    fn search_and_semantics() {
        let mut w = ws();
        let apple = w.add_instance("Fruit", "Apple", BTreeMap::new(), None, 1).unwrap();
        let apricot = w.add_instance("Fruit", "Apricot", BTreeMap::new(), None, 1).unwrap();
        let banana = w.add_instance("Veg", "Banana", BTreeMap::new(), None, 1).unwrap();
        w.add_tag(apple, "fresh", 1).unwrap();
        w.add_tag(apricot, "fresh", 1).unwrap();

        // text only, case-insensitive substring
        let q = SearchQuery { text: Some("ap".to_string()), ..Default::default() };
        let mut res = w.search(&q);
        res.sort();
        let mut exp = vec![apple, apricot];
        exp.sort();
        assert_eq!(res, exp);

        // class only
        let q = SearchQuery { class: Some("Veg".to_string()), ..Default::default() };
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
        let q = SearchQuery { tag: Some("nope".to_string()), ..Default::default() };
        assert!(w.search(&q).is_empty());
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use inv_model::{Workspace, WorkspaceId};
    use proptest::prelude::*;

    fn ws() -> Workspace {
        Workspace::new(WorkspaceId::new_random())
    }

    /// Returns true if any instance is reachable from itself by following parent
    /// pointers (i.e. the containment graph contains a cycle).
    fn has_cycle(w: &Workspace) -> bool {
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

    /// A random operation applied to the workspace during invariant 1.
    #[derive(Debug, Clone)]
    enum Op {
        /// Add an instance under the parent at the given index (modulo count), or
        /// as a root.
        Add { class: u8, as_root: bool, parent_idx: usize },
        /// Move the instance at `idx` to the parent at `parent_idx` (or to root).
        Move { idx: usize, to_root: bool, parent_idx: usize },
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..4, any::<bool>(), 0usize..32)
                .prop_map(|(class, as_root, parent_idx)| Op::Add { class, as_root, parent_idx }),
            (0usize..32, any::<bool>(), 0usize..32)
                .prop_map(|(idx, to_root, parent_idx)| Op::Move { idx, to_root, parent_idx }),
        ]
    }

    proptest! {
        // Invariant 1: applying random add/move ops never creates a cycle, and
        // path_of always terminates (it returns and the root has no parent).
        #[test]
        fn prop_acyclic(ops in proptest::collection::vec(op_strategy(), 0..60)) {
            let mut w = ws();
            let mut ids: Vec<InstanceId> = Vec::new();
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
        // workspace is byte-identical (and value-identical) to before.
        #[test]
        fn prop_cycle_rejected(seed in any::<u64>()) {
            let _ = seed; // structure is fixed; seed just drives multiple runs
            let mut w = ws();
            let a = w.add_instance("C", "a", BTreeMap::new(), None, 1).unwrap();
            let b = w.add_instance("C", "b", BTreeMap::new(), Some(a), 2).unwrap();
            let c = w.add_instance("C", "c", BTreeMap::new(), Some(b), 3).unwrap();
            let before = w.clone();
            let before_bytes = before.to_bytes().unwrap();

            let err = w.move_instance(a, Some(c), 99).unwrap_err();
            prop_assert_eq!(err, CoreError::WouldCycle);
            prop_assert_eq!(&w, &before);
            prop_assert_eq!(w.to_bytes().unwrap(), before_bytes);
        }

        // Invariant 3: deep-duplicate a random small subtree; subtree sizes equal,
        // id sets disjoint, structural isomorphism, original unchanged.
        #[test]
        fn prop_deep_duplicate(
            // shape: for each of up to 6 nodes (besides root), pick an existing
            // node index to attach under.
            parents in proptest::collection::vec(0usize..16, 0..6),
        ) {
            let mut w = ws();
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

            let orig_subtree: BTreeSet<InstanceId> =
                w.descendants_of(root).into_iter().chain(std::iter::once(root)).collect();
            let before = w.clone();

            let copy_root = w.duplicate_instance(root, true, 1000).unwrap();
            let copy_subtree: BTreeSet<InstanceId> =
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
            fn signature(w: &Workspace, id: InstanceId) -> Vec<String> {
                let mut out = Vec::new();
                let mut stack = vec![(id, 0usize)];
                while let Some((cur, depth)) = stack.pop() {
                    let inst = w.get(cur).unwrap();
                    let class_name = w
                        .classes
                        .get(&inst.class_id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    let tags: Vec<String> = inst.tags.iter().cloned().collect();
                    out.push(format!(
                        "{depth}|{}|{}|{:?}|{:?}",
                        inst.name, class_name, tags, inst.fields
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
            let mut w = ws();
            let a = w.ensure_class(&name, 1);
            let b = w.ensure_class(&name, 2);
            prop_assert_eq!(a, b);
            let count = w.classes.values().filter(|c| c.name == name).count();
            prop_assert_eq!(count, 1);
        }

        // Invariant 5: auto field-extension adds each new field exactly once.
        #[test]
        fn prop_field_extension_once(
            field in "[a-z]{1,6}",
            n in 1usize..6,
        ) {
            let mut w = ws();
            for i in 0..n {
                let mut fields = BTreeMap::new();
                fields.insert(field.clone(), FieldValue::Text(format!("v{i}")));
                w.add_instance("E", &format!("i{i}"), fields, None, i as i64).unwrap();
            }
            let cid = w.class_by_name("E").unwrap();
            let occurrences =
                w.classes[&cid].fields.iter().filter(|f| f.name == field).count();
            prop_assert_eq!(occurrences, 1);
        }

        // Invariant 6: cascade remove deletes the whole subtree, leaves no parent
        // pointer into the removed set, and no relationship targeting a removed id.
        #[test]
        fn prop_cascade_remove(
            parents in proptest::collection::vec(0usize..16, 0..8),
            rel_pairs in proptest::collection::vec((0usize..16, 0usize..16), 0..8),
        ) {
            let mut w = ws();
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

            let expected_removed: BTreeSet<InstanceId> =
                w.descendants_of(root).into_iter().chain(std::iter::once(root)).collect();

            let removed = w.remove_instance(root, RemoveMode::Cascade).unwrap();
            let removed_set: BTreeSet<InstanceId> = removed.into_iter().collect();
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
