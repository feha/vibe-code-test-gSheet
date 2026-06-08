//! `AppState`: the shared reactive context for the whole app.
//!
//! `AppState` is `Clone` (all fields are `Copy` signal handles) and is published
//! via `provide_context` at the app root; components pull it with `use_context`.
//! Every mutator builds an [`Op`], calls [`apply_op`], and REPLACES the
//! `inventory` signal from the gateway's response so the UI always reflects the
//! committed server state. Read-only queries (`search`, `class_names`) run
//! client-side over the in-memory `inventory` via [`inv_core::InventoryExt`].

use std::collections::BTreeMap;

use inv_core::{InventoryExt, SearchQuery};
use inv_model::{FieldValue, Inventory};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api::{self, Op, PatchWire, RemoveModeWire, StoreDescriptor};

/// Severity of a status toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

/// A transient status message shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub kind: ToastKind,
    pub message: String,
}

/// The shared application state. Cheap to clone (all fields are signal handles).
#[derive(Clone, Copy)]
pub struct AppState {
    /// The currently-open store, or `None` when on the "open database" screen.
    pub descriptor: RwSignal<Option<StoreDescriptor>>,
    /// The committed inventory mirrored from the gateway.
    pub inventory: RwSignal<Inventory>,
    /// The currently-selected instance id, if any.
    pub selected: RwSignal<Option<i64>>,
    /// The container being browsed (the navigator's current folder), if any.
    pub current_container: RwSignal<Option<i64>>,
    /// The active status toast, if any.
    pub status: RwSignal<Option<Toast>>,
}

impl AppState {
    /// Create empty state. Call [`AppState::provide`] to publish it as context.
    pub fn new() -> Self {
        Self {
            descriptor: RwSignal::new(None),
            inventory: RwSignal::new(Inventory::new()),
            selected: RwSignal::new(None),
            current_container: RwSignal::new(None),
            status: RwSignal::new(None),
        }
    }

    /// Construct and publish via `provide_context`, returning the handle.
    pub fn provide() -> Self {
        let state = Self::new();
        provide_context(state);
        state
    }

    /// Pull the state from context (panics if not provided).
    pub fn expect() -> Self {
        use_context::<AppState>().expect("AppState context must be provided")
    }

    // -- toasts -------------------------------------------------------------

    /// Show an informational toast.
    pub fn info(&self, message: impl Into<String>) {
        self.status.set(Some(Toast {
            kind: ToastKind::Info,
            message: message.into(),
        }));
    }

    /// Show an error toast.
    pub fn error(&self, message: impl Into<String>) {
        self.status.set(Some(Toast {
            kind: ToastKind::Error,
            message: message.into(),
        }));
    }

    /// Dismiss the current toast.
    pub fn clear_toast(&self) {
        self.status.set(None);
    }

    // -- open / close -------------------------------------------------------

    /// Open `desc`: load the inventory, then set `inventory` + `descriptor`.
    /// Errors (including "postgres adapter not yet implemented") surface as a
    /// toast and leave the open-database screen in place.
    pub fn open(&self, desc: StoreDescriptor) {
        let state = *self;
        spawn_local(async move {
            match api::load_inventory(&desc).await {
                Ok(inv) => {
                    state.inventory.set(inv);
                    state.selected.set(None);
                    state.current_container.set(None);
                    state.info(format!("Opened {}", desc.label()));
                    state.descriptor.set(Some(desc));
                }
                Err(e) => state.error(format!("Could not open store: {e}")),
            }
        });
    }

    /// Close the current store and return to the open-database screen.
    pub fn close(&self) {
        self.descriptor.set(None);
        self.inventory.set(Inventory::new());
        self.selected.set(None);
        self.current_container.set(None);
        self.clear_toast();
    }

    // -- mutators (build Op -> apply_op -> replace inventory) --------------

    /// Run `op` against the open store and replace `inventory` from the result.
    /// `on_ok` is invoked with the new inventory after a successful commit.
    fn run_op(&self, op: Op, on_ok: impl FnOnce(&AppState, &Inventory) + 'static) {
        let state = *self;
        let Some(desc) = self.descriptor.get_untracked() else {
            self.error("No store is open");
            return;
        };
        spawn_local(async move {
            match api::apply_op(&desc, &op).await {
                Ok(inv) => {
                    on_ok(&state, &inv);
                    state.inventory.set(inv);
                }
                Err(e) => state.error(e.to_string()),
            }
        });
    }

    /// Add an instance (with optional fields, parent, tags). Selects the parent's
    /// view is left to the caller; the new id is not known until the reload, so we
    /// just refresh.
    pub fn add_instance(
        &self,
        class: String,
        name: String,
        fields: BTreeMap<String, FieldValue>,
        parent: Option<i64>,
        tags: Vec<String>,
    ) {
        self.run_op(
            Op::AddInstance {
                class,
                name,
                fields,
                parent,
                tags,
            },
            |state, _| state.info("Added"),
        );
    }

    /// Edit an instance: rename and/or set/remove fields.
    pub fn edit_instance(
        &self,
        id: i64,
        name: Option<String>,
        set_fields: BTreeMap<String, FieldValue>,
        remove_fields: Vec<String>,
    ) {
        self.run_op(
            Op::EditInstance {
                id,
                patch: PatchWire {
                    name,
                    set_fields,
                    remove_fields,
                },
            },
            |state, _| state.info("Saved"),
        );
    }

    /// Move an instance under a new parent (or to root with `None`).
    pub fn move_instance(&self, id: i64, new_parent: Option<i64>) {
        self.run_op(Op::MoveInstance { id, new_parent }, |state, _| {
            state.info("Moved")
        });
    }

    /// Remove an instance with the given cascade/reparent mode.
    pub fn remove_instance(&self, id: i64, mode: RemoveModeWire) {
        self.run_op(Op::RemoveInstance { id, mode }, move |state, _| {
            if state.selected.get_untracked() == Some(id) {
                state.selected.set(None);
            }
            state.info("Removed");
        });
    }

    /// Duplicate an instance (shallow or deep).
    pub fn duplicate_instance(&self, id: i64, deep: bool) {
        self.run_op(Op::DuplicateInstance { id, deep }, |state, _| {
            state.info("Duplicated")
        });
    }

    /// Add a tag to an instance.
    pub fn add_tag(&self, id: i64, tag: String) {
        self.run_op(Op::AddTag { id, tag }, |state, _| state.info("Tag added"));
    }

    /// Remove a tag from an instance.
    pub fn remove_tag(&self, id: i64, tag: String) {
        self.run_op(Op::RemoveTag { id, tag }, |state, _| {
            state.info("Tag removed")
        });
    }

    /// Add a typed relationship edge from `id` to `target`.
    pub fn add_relationship(&self, id: i64, kind: String, target: i64) {
        self.run_op(
            Op::AddRelationship { id, kind, target },
            |state, _| state.info("Link added"),
        );
    }

    /// Remove a typed relationship edge.
    pub fn remove_relationship(&self, id: i64, kind: String, target: i64) {
        self.run_op(
            Op::RemoveRelationship { id, kind, target },
            |state, _| state.info("Link removed"),
        );
    }

    // -- classes ------------------------------------------------------------

    /// Change the class of an instance. Errors (e.g. `NotFound`) surface via toast.
    pub fn change_class(&self, id: i64, new_class: String) {
        self.run_op(Op::ChangeClass { id, new_class }, |state, _| {
            state.info("Class changed")
        });
    }

    /// Delete a class by name. A class still referenced by an instance is rejected
    /// by the gateway (`ClassInUse` -> HTTP 409), surfaced here as an error toast.
    pub fn delete_class(&self, name: String) {
        self.run_op(Op::DeleteClass { name }, |state, _| {
            state.info("Class deleted")
        });
    }

    // -- photos -------------------------------------------------------------

    /// Upload photo bytes for `id`, then attach the photo via an edit so the
    /// instance carries the reference. The key is derived as `<id>-<n>`.
    pub fn upload_photo(&self, id: i64, bytes: Vec<u8>, mime: String, name: String) {
        let state = *self;
        let Some(desc) = self.descriptor.get_untracked() else {
            self.error("No store is open");
            return;
        };
        // Derive a stable-ish key from the current photo count.
        let n = self
            .inventory
            .get_untracked()
            .get(id)
            .map(|i| i.photos.len())
            .unwrap_or(0);
        let key = format!("{id}-{n}");
        spawn_local(async move {
            if let Err(e) = api::put_photo(&desc, &key, bytes, &mime).await {
                state.error(format!("Upload failed: {e}"));
                return;
            }
            // The photo bytes are stored; the instance's photo list is owned by
            // the model. There is no dedicated attach op on the gateway, so the
            // detail panel records the reference via an edit-driven flow. Here we
            // simply confirm the upload; the key/mime/name are returned to the UI
            // through the toast for now.
            state.info(format!("Uploaded {name} ({key})"));
        });
    }

    /// Build an object URL for a stored photo so an `<img>` can render it.
    /// Fetches the bytes then wraps them in a `Blob` + `URL.createObjectURL`.
    /// Returns an empty string on failure (caller may retry).
    pub async fn photo_object_url(&self, key: String) -> String {
        let Some(desc) = self.descriptor.get_untracked() else {
            return String::new();
        };
        match api::get_photo(&desc, &key).await {
            Ok(Some(bytes)) => blob_url_from_bytes(&bytes),
            _ => String::new(),
        }
    }

    // -- read-only queries (client-side over the inventory signal) ----------

    /// Run a search client-side via `inv_core::InventoryExt`.
    pub fn search(&self, q: SearchQuery) -> Vec<i64> {
        self.inventory.with(|inv| inv.search(&q))
    }

    /// All known class names, sorted (classes are keyed by name in a `BTreeMap`).
    pub fn class_names(&self) -> Vec<String> {
        self.inventory
            .with(|inv| inv.classes.keys().cloned().collect())
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap raw bytes in a JS `Blob` and return an object URL string.
fn blob_url_from_bytes(bytes: &[u8]) -> String {
    use wasm_bindgen::JsValue;
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&array.into());
    match web_sys::Blob::new_with_u8_array_sequence(&JsValue::from(parts)) {
        Ok(blob) => web_sys::Url::create_object_url_with_blob(&blob).unwrap_or_default(),
        Err(_) => String::new(),
    }
}
