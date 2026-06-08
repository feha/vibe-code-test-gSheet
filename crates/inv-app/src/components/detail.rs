//! `<DetailPanel/>`: view and edit the selected instance.
//!
//! Renders an empty state when nothing is selected; otherwise shows an inline
//! editor for the instance's name, typed fields, tags, photos and relationships,
//! plus duplicate / remove actions and the store-native id with timestamps. Every
//! mutation goes through [`AppState`], which applies the op on the gateway and
//! replaces the committed inventory signal, so this component stays a thin,
//! reactive view over that state.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use inv_core::SearchQuery;
use inv_model::{FieldValue, Instance};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;
use web_sys::{HtmlInputElement, HtmlSelectElement};

use crate::api::RemoveModeWire;
use crate::state::AppState;

#[component]
pub fn DetailPanel() -> impl IntoView {
    let state = AppState::expect();

    // The selected instance, cloned out of the inventory so the rest of the view
    // can read plain data. Recomputes whenever the selection or inventory change.
    let current = Memo::new(move |_| {
        let id = state.selected.get()?;
        state.inventory.with(|inv| inv.instances.get(&id).cloned())
    });

    view! {
        <section class="card detail-panel">
            <h2 class="card-title">"Details"</h2>
            <Show
                when=move || current.get().is_some()
                fallback=|| view! {
                    <p class="muted">"Select an item to see its details."</p>
                }
            >
                {move || current.get().map(|inst| view! { <Editor inst=inst/> })}
            </Show>
        </section>
    }
}

/// The full editor for one instance. Re-created (keyed implicitly by the parent's
/// `Show`) whenever the selected instance changes.
#[component]
fn Editor(inst: Instance) -> impl IntoView {
    let id = inst.id;

    view! {
        <div class="detail-body" style="display:flex; flex-direction:column; gap:18px;">
            <Header inst=inst.clone()/>
            <FieldsSection inst=inst.clone()/>
            <TagsSection inst=inst.clone()/>
            <PhotosSection inst=inst.clone()/>
            <RelationshipsSection inst=inst.clone()/>
            <Actions id=id/>
            <Meta inst=inst/>
        </div>
    }
}

// ---- header: rename ----------------------------------------------------------

#[component]
fn Header(inst: Instance) -> impl IntoView {
    let state = AppState::expect();
    let id = inst.id;
    let name = RwSignal::new(inst.name.clone());
    let current_class = inst.class.clone();
    // Editable class draft, seeded with the instance's current class. A datalist
    // suggests existing class names while still allowing a brand-new name.
    let class_draft = RwSignal::new(inst.class.clone());
    let class_for_compare = inst.class.clone();
    let class_list_id = format!("class-suggest-{id}");
    let class_list_id_input = class_list_id.clone();

    let save = move |_| {
        let new_name = name.get();
        if new_name.trim().is_empty() {
            state.error("Name cannot be empty");
            return;
        }
        state.edit_instance(id, Some(new_name), BTreeMap::new(), Vec::new());
    };

    let change_class = move |_| {
        let new_class = class_draft.get();
        let new_class = new_class.trim().to_string();
        if new_class.is_empty() {
            state.error("Class cannot be empty");
            return;
        }
        if new_class == class_for_compare {
            state.error("Already this class");
            return;
        }
        state.change_class(id, new_class);
    };

    view! {
        <div>
            <div class="muted" style="margin-bottom:6px;">{current_class}" #"{id}</div>
            <div style="display:flex; gap:8px; align-items:center;">
                <input
                    class="text-input"
                    style="margin-bottom:0; font-weight:600;"
                    r#type="text"
                    prop:value=move || name.get()
                    on:input=move |ev| name.set(input_value(&ev))
                />
                <button class="btn btn-primary" on:click=save>"Rename"</button>
            </div>
            <div style="display:flex; gap:8px; align-items:center; margin-top:8px;">
                <input
                    class="text-input"
                    style="margin-bottom:0; width:160px;"
                    r#type="text"
                    list=class_list_id_input
                    placeholder="class"
                    prop:value=move || class_draft.get()
                    on:input=move |ev| class_draft.set(input_value(&ev))
                />
                <datalist id=class_list_id>
                    <For
                        each=move || state.class_names()
                        key=|name| name.clone()
                        let:name
                    >
                        <option value=name.clone()>{name.clone()}</option>
                    </For>
                </datalist>
                <button class="btn btn-ghost" on:click=change_class>"Change class"</button>
            </div>
        </div>
    }
}

// ---- fields ------------------------------------------------------------------

/// Stable label for a field-type choice in the add-field selector.
const TYPE_TEXT: &str = "text";
const TYPE_NUMBER: &str = "number";
const TYPE_BOOL: &str = "bool";
const TYPE_DATE: &str = "date";

#[component]
fn FieldsSection(inst: Instance) -> impl IntoView {
    let id = inst.id;
    // Snapshot the fields as a sorted Vec for stable rendering.
    let fields: Vec<(String, FieldValue)> =
        inst.fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

    view! {
        <div class="detail-section">
            <h3 class="card-title" style="margin-bottom:8px;">"Fields"</h3>
            <Show
                when={
                    let empty = fields.is_empty();
                    move || !empty
                }
                fallback=|| view! { <p class="muted">"No fields yet."</p> }
            >
                <div style="display:flex; flex-direction:column; gap:8px;">
                    {fields
                        .clone()
                        .into_iter()
                        .map(|(key, value)| view! { <FieldRow id=id key=key value=value/> })
                        .collect_view()}
                </div>
            </Show>
            <AddFieldRow id=id/>
        </div>
    }
}

/// One editable field. The editor shape depends on the value's variant; "Save"
/// writes the parsed value back, "x" removes the field.
#[component]
fn FieldRow(id: i64, key: String, value: FieldValue) -> impl IntoView {
    let state = AppState::expect();
    let key_for_save = key.clone();
    let key_for_remove = key.clone();

    // A single text-backed draft covers Text / Number / Date; a bool draft covers
    // Bool. Empty fields edit as text.
    let draft = RwSignal::new(value_to_input_string(&value));
    let bool_draft = RwSignal::new(matches!(value, FieldValue::Bool(true)));
    let is_bool = matches!(value, FieldValue::Bool(_));
    let is_number = matches!(value, FieldValue::Number(_));
    let is_date = matches!(value, FieldValue::Date(_));

    let save = {
        let key = key_for_save.clone();
        let original = value.clone();
        move |_| {
            let new_val = if is_bool {
                FieldValue::Bool(bool_draft.get())
            } else {
                match parse_like(&original, &draft.get()) {
                    Ok(v) => v,
                    Err(msg) => {
                        state.error(msg);
                        return;
                    }
                }
            };
            let mut set = BTreeMap::new();
            set.insert(key.clone(), new_val);
            state.edit_instance(id, None, set, Vec::new());
        }
    };

    let remove = move |_| {
        state.edit_instance(id, None, BTreeMap::new(), vec![key_for_remove.clone()]);
    };

    let editor = if is_bool {
        view! {
            <label style="display:flex; align-items:center; gap:6px; flex:1;">
                <input
                    r#type="checkbox"
                    prop:checked=move || bool_draft.get()
                    on:change=move |ev| bool_draft.set(checkbox_checked(&ev))
                />
                <span class="muted">{move || if bool_draft.get() { "true" } else { "false" }}</span>
            </label>
        }
        .into_any()
    } else {
        let input_type = if is_number {
            "number"
        } else if is_date {
            "date"
        } else {
            "text"
        };
        view! {
            <input
                class="text-input"
                style="margin-bottom:0; flex:1;"
                r#type=input_type
                prop:value=move || draft.get()
                on:input=move |ev| draft.set(input_value(&ev))
            />
        }
        .into_any()
    };

    view! {
        <div style="display:flex; gap:8px; align-items:center;">
            <span style="min-width:90px; font-weight:600;">{key}</span>
            {editor}
            <button class="btn btn-ghost" on:click=save>"Save"</button>
            <button class="btn btn-ghost" title="Remove field" on:click=remove>"x"</button>
        </div>
    }
}

/// Add a brand-new typed field to the instance.
#[component]
fn AddFieldRow(id: i64) -> impl IntoView {
    let state = AppState::expect();
    let new_name = RwSignal::new(String::new());
    let new_type = RwSignal::new(TYPE_TEXT.to_string());
    let new_value = RwSignal::new(String::new());
    let new_bool = RwSignal::new(false);

    let add = move |_| {
        let name = new_name.get();
        let name = name.trim().to_string();
        if name.is_empty() {
            state.error("Field name required");
            return;
        }
        let ty = new_type.get();
        let value = match ty.as_str() {
            TYPE_BOOL => FieldValue::Bool(new_bool.get()),
            TYPE_NUMBER => match new_value.get().trim().parse::<f64>() {
                Ok(n) => FieldValue::Number(n),
                Err(_) => {
                    state.error("Enter a valid number");
                    return;
                }
            },
            TYPE_DATE => match date_string_to_millis(&new_value.get()) {
                Some(ms) => FieldValue::Date(ms),
                None => {
                    state.error("Enter a valid date");
                    return;
                }
            },
            _ => {
                let v = new_value.get();
                if v.is_empty() {
                    FieldValue::Empty
                } else {
                    FieldValue::Text(v)
                }
            }
        };
        let mut set = BTreeMap::new();
        set.insert(name, value);
        state.edit_instance(id, None, set, Vec::new());
        new_name.set(String::new());
        new_value.set(String::new());
        new_bool.set(false);
    };

    view! {
        <div
            style="display:flex; gap:8px; align-items:center; margin-top:10px; \
                   padding-top:10px; border-top:1px solid var(--line); flex-wrap:wrap;"
        >
            <input
                class="text-input"
                style="margin-bottom:0; width:120px;"
                r#type="text"
                placeholder="field name"
                prop:value=move || new_name.get()
                on:input=move |ev| new_name.set(input_value(&ev))
            />
            <select
                class="text-input"
                style="margin-bottom:0; width:auto;"
                on:change=move |ev| new_type.set(select_value(&ev))
            >
                <option value=TYPE_TEXT>"Text"</option>
                <option value=TYPE_NUMBER>"Number"</option>
                <option value=TYPE_BOOL>"Bool"</option>
                <option value=TYPE_DATE>"Date"</option>
            </select>
            <Show
                when=move || new_type.get() == TYPE_BOOL
                fallback=move || {
                    let input_type = move || match new_type.get().as_str() {
                        TYPE_NUMBER => "number",
                        TYPE_DATE => "date",
                        _ => "text",
                    };
                    view! {
                        <input
                            class="text-input"
                            style="margin-bottom:0; flex:1; min-width:120px;"
                            r#type=input_type
                            placeholder="value"
                            prop:value=move || new_value.get()
                            on:input=move |ev| new_value.set(input_value(&ev))
                        />
                    }
                }
            >
                <label style="display:flex; align-items:center; gap:6px; flex:1;">
                    <input
                        r#type="checkbox"
                        prop:checked=move || new_bool.get()
                        on:change=move |ev| new_bool.set(checkbox_checked(&ev))
                    />
                    <span class="muted">"true / false"</span>
                </label>
            </Show>
            <button class="btn btn-primary" on:click=add>"Add field"</button>
        </div>
    }
}

// ---- tags --------------------------------------------------------------------

#[component]
fn TagsSection(inst: Instance) -> impl IntoView {
    let state = AppState::expect();
    let id = inst.id;
    let tags: Vec<String> = inst.tags.iter().cloned().collect();
    let new_tag = RwSignal::new(String::new());

    let add = move |_| {
        let tag = new_tag.get();
        let tag = tag.trim().to_string();
        if tag.is_empty() {
            state.error("Tag cannot be empty");
            return;
        }
        state.add_tag(id, tag);
        new_tag.set(String::new());
    };

    view! {
        <div class="detail-section">
            <h3 class="card-title" style="margin-bottom:8px;">"Tags"</h3>
            <div style="display:flex; gap:6px; flex-wrap:wrap; margin-bottom:8px;">
                {if tags.is_empty() {
                    view! { <span class="muted">"No tags."</span> }.into_any()
                } else {
                    tags.into_iter()
                        .map(|tag| {
                            let tag_for_remove = tag.clone();
                            let remove = move |_| state.remove_tag(id, tag_for_remove.clone());
                            view! {
                                <span
                                    class="count-badge"
                                    style="display:inline-flex; align-items:center; gap:6px;"
                                >
                                    {tag}
                                    <button
                                        class="toast-close"
                                        title="Remove tag"
                                        on:click=remove
                                    >"x"</button>
                                </span>
                            }
                        })
                        .collect_view()
                        .into_any()
                }}
            </div>
            <div style="display:flex; gap:8px;">
                <input
                    class="text-input"
                    style="margin-bottom:0; flex:1;"
                    r#type="text"
                    placeholder="add a tag"
                    prop:value=move || new_tag.get()
                    on:input=move |ev| new_tag.set(input_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { add(()) }
                />
                <button class="btn btn-primary" on:click=move |_| add(())>"Add"</button>
            </div>
        </div>
    }
}

// ---- photos ------------------------------------------------------------------

#[component]
fn PhotosSection(inst: Instance) -> impl IntoView {
    let state = AppState::expect();
    let id = inst.id;
    let photos = inst.photos.clone();

    // File input -> read bytes -> upload.
    let on_file = move |ev: leptos::ev::Event| {
        let Some(input) = ev
            .target()
            .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        else {
            return;
        };
        let Some(files) = input.files() else { return };
        let Some(file) = files.get(0) else { return };
        let name = file.name();
        let mime = {
            let t = file.type_();
            if t.is_empty() {
                "application/octet-stream".to_string()
            } else {
                t
            }
        };
        let blob = gloo_file::Blob::from(file);
        // The `FileReader` guard aborts the read when dropped, so keep it alive
        // until the callback fires, then release it from inside the callback.
        let guard: Rc<RefCell<Option<gloo_file::callbacks::FileReader>>> =
            Rc::new(RefCell::new(None));
        let guard_for_cb = guard.clone();
        let reader = gloo_file::callbacks::read_as_bytes(&blob, move |result| {
            match result {
                Ok(bytes) => state.upload_photo(id, bytes, mime, name),
                Err(e) => state.error(format!("Could not read file: {e}")),
            }
            // Drop our self-reference now that the read is complete.
            guard_for_cb.borrow_mut().take();
        });
        *guard.borrow_mut() = Some(reader);
        // Reset so selecting the same file again re-fires `change`.
        input.set_value("");
    };

    view! {
        <div class="detail-section">
            <h3 class="card-title" style="margin-bottom:8px;">"Photos"</h3>
            <Show
                when={
                    let empty = photos.is_empty();
                    move || !empty
                }
                fallback=|| view! { <p class="muted">"No photos attached."</p> }
            >
                <div style="display:flex; gap:10px; flex-wrap:wrap; margin-bottom:8px;">
                    {photos
                        .clone()
                        .into_iter()
                        .map(|photo| view! { <PhotoThumb key=photo.key name=photo.name/> })
                        .collect_view()}
                </div>
            </Show>
            <input
                r#type="file"
                accept="image/*"
                on:change=on_file
            />
        </div>
    }
}

/// A single stored photo, fetched lazily into an object URL for the `<img>`.
#[component]
fn PhotoThumb(key: String, name: String) -> impl IntoView {
    let state = AppState::expect();
    let src = RwSignal::new(String::new());
    let key_for_load = key.clone();
    spawn_local(async move {
        let url = state.photo_object_url(key_for_load).await;
        src.set(url);
    });

    view! {
        <figure style="margin:0; width:96px; text-align:center;">
            {move || {
                let url = src.get();
                if url.is_empty() {
                    view! {
                        <div
                            class="count-badge"
                            style="width:96px; height:96px; display:flex; \
                                   align-items:center; justify-content:center;"
                        >"…"</div>
                    }
                    .into_any()
                } else {
                    view! {
                        <img
                            src=url
                            alt=name.clone()
                            style="width:96px; height:96px; object-fit:cover; \
                                   border-radius:8px; border:1px solid var(--line);"
                        />
                    }
                    .into_any()
                }
            }}
            <figcaption
                class="muted"
                style="font-size:11px; overflow:hidden; text-overflow:ellipsis; \
                       white-space:nowrap;"
            >{key}</figcaption>
        </figure>
    }
}

// ---- relationships -----------------------------------------------------------

#[component]
fn RelationshipsSection(inst: Instance) -> impl IntoView {
    let state = AppState::expect();
    let id = inst.id;
    let rels = inst.relationships.clone();

    let kind = RwSignal::new(String::new());
    let query = RwSignal::new(String::new());
    let picked = RwSignal::new(Option::<i64>::None);

    // Candidate targets matching the search box, excluding self. Capped for sanity.
    let candidates = move || {
        let q = query.get();
        let q = q.trim();
        if q.is_empty() {
            return Vec::new();
        }
        let ids = state.search(SearchQuery {
            text: Some(q.to_string()),
            ..Default::default()
        });
        state.inventory.with(|inv| {
            ids.into_iter()
                .filter(|cid| *cid != id)
                .take(8)
                .map(|cid| {
                    let label = inv
                        .instances
                        .get(&cid)
                        .map(|i| format!("{} ({} #{})", i.name, i.class, cid))
                        .unwrap_or_else(|| format!("#{cid}"));
                    (cid, label)
                })
                .collect::<Vec<_>>()
        })
    };

    let add = move |_| {
        let kind_val = kind.get();
        let kind_val = kind_val.trim().to_string();
        if kind_val.is_empty() {
            state.error("Enter a link type");
            return;
        }
        let Some(target) = picked.get() else {
            state.error("Pick a target item");
            return;
        };
        state.add_relationship(id, kind_val, target);
        kind.set(String::new());
        query.set(String::new());
        picked.set(None);
    };

    view! {
        <div class="detail-section">
            <h3 class="card-title" style="margin-bottom:8px;">"Relationships"</h3>
            <Show
                when={
                    let empty = rels.is_empty();
                    move || !empty
                }
                fallback=|| view! { <p class="muted">"No links."</p> }
            >
                <div style="display:flex; flex-direction:column; gap:6px; margin-bottom:10px;">
                    {rels
                        .clone()
                        .into_iter()
                        .map(|rel| {
                            let target = rel.target;
                            let kind_s = rel.kind.clone();
                            let kind_for_remove = rel.kind.clone();
                            let target_name = state.inventory.with(|inv| {
                                inv.instances
                                    .get(&target)
                                    .map(|i| i.name.clone())
                                    .unwrap_or_else(|| format!("#{target}"))
                            });
                            let remove = move |_| {
                                state.remove_relationship(id, kind_for_remove.clone(), target)
                            };
                            view! {
                                <div style="display:flex; gap:8px; align-items:center;">
                                    <span class="count-badge">{kind_s}</span>
                                    <span style="flex:1;">{target_name}" #"{target}</span>
                                    <button
                                        class="btn btn-ghost"
                                        title="Remove link"
                                        on:click=remove
                                    >"x"</button>
                                </div>
                            }
                        })
                        .collect_view()}
                </div>
            </Show>

            <div style="display:flex; flex-direction:column; gap:8px;">
                <div style="display:flex; gap:8px;">
                    <input
                        class="text-input"
                        style="margin-bottom:0; width:120px;"
                        r#type="text"
                        placeholder="link type"
                        prop:value=move || kind.get()
                        on:input=move |ev| kind.set(input_value(&ev))
                    />
                    <input
                        class="text-input"
                        style="margin-bottom:0; flex:1;"
                        r#type="text"
                        placeholder="search target item"
                        prop:value=move || query.get()
                        on:input=move |ev| {
                            query.set(input_value(&ev));
                            picked.set(None);
                        }
                    />
                    <button class="btn btn-primary" on:click=add>"Link"</button>
                </div>
                {move || {
                    let chosen = picked.get();
                    let list = candidates();
                    if list.is_empty() {
                        ().into_any()
                    } else {
                        view! {
                            <div
                                style="display:flex; flex-direction:column; gap:4px; \
                                       border:1px solid var(--line); border-radius:8px; \
                                       padding:6px;"
                            >
                                {list
                                    .into_iter()
                                    .map(|(cid, label)| {
                                        let selected = chosen == Some(cid);
                                        let cls = if selected {
                                            "btn btn-primary"
                                        } else {
                                            "btn btn-ghost"
                                        };
                                        view! {
                                            <button
                                                class=cls
                                                style="justify-content:flex-start;"
                                                on:click=move |_| picked.set(Some(cid))
                                            >{label}</button>
                                        }
                                    })
                                    .collect_view()}
                            </div>
                        }
                        .into_any()
                    }
                }}
            </div>
        </div>
    }
}

// ---- actions: duplicate / remove --------------------------------------------

#[component]
fn Actions(id: i64) -> impl IntoView {
    let state = AppState::expect();
    let deep = RwSignal::new(false);
    let remove_mode = RwSignal::new(RemoveModeWire::Cascade);

    let duplicate = move |_| state.duplicate_instance(id, deep.get());

    let remove = move |_| state.remove_instance(id, remove_mode.get());

    view! {
        <div
            class="detail-section"
            style="display:flex; flex-direction:column; gap:10px; \
                   border-top:1px solid var(--line); padding-top:14px;"
        >
            <div style="display:flex; gap:8px; align-items:center; flex-wrap:wrap;">
                <button class="btn btn-ghost" on:click=duplicate>"Duplicate"</button>
                <label style="display:flex; align-items:center; gap:6px;">
                    <input
                        r#type="checkbox"
                        prop:checked=move || deep.get()
                        on:change=move |ev| deep.set(checkbox_checked(&ev))
                    />
                    <span class="muted">"deep (copy subtree)"</span>
                </label>
            </div>
            <div style="display:flex; gap:8px; align-items:center; flex-wrap:wrap;">
                <button
                    class="btn btn-ghost"
                    style="color:var(--error); border-color:#f3c2bd;"
                    on:click=remove
                >"Remove"</button>
                <select
                    class="text-input"
                    style="margin-bottom:0; width:auto;"
                    on:change=move |ev| {
                        let mode = if select_value(&ev) == "reparent" {
                            RemoveModeWire::Reparent
                        } else {
                            RemoveModeWire::Cascade
                        };
                        remove_mode.set(mode);
                    }
                >
                    <option value="cascade">"cascade (delete subtree)"</option>
                    <option value="reparent">"reparent children"</option>
                </select>
            </div>
        </div>
    }
}

// ---- meta: id + timestamps ---------------------------------------------------

#[component]
fn Meta(inst: Instance) -> impl IntoView {
    let id = inst.id;
    let created = format_millis(inst.created_at);
    let updated = format_millis(inst.updated_at);
    view! {
        <div class="muted" style="font-size:12px; border-top:1px solid var(--line); padding-top:12px;">
            "id "{id}" · created "{created}" · updated "{updated}
        </div>
    }
}

// ---- helpers -----------------------------------------------------------------

/// Read the current `value` of the `<input>` that fired an event.
fn input_value(ev: &leptos::ev::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

/// Read the current `value` of the `<select>` that fired an event.
fn select_value(ev: &leptos::ev::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<HtmlSelectElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

/// Read the `checked` state of a checkbox `<input>` that fired an event.
fn checkbox_checked(ev: &leptos::ev::Event) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|el| el.checked())
        .unwrap_or(false)
}

/// Render a [`FieldValue`] as the string an `<input>` should show.
fn value_to_input_string(v: &FieldValue) -> String {
    match v {
        FieldValue::Text(s) => s.clone(),
        FieldValue::Number(n) => format_number(*n),
        FieldValue::Bool(b) => b.to_string(),
        FieldValue::Date(ms) => millis_to_date_string(*ms),
        FieldValue::Empty => String::new(),
    }
}

/// Parse `raw` into the same variant as `like`, surfacing a user-facing message on
/// failure. (Bool is handled separately via a checkbox.)
fn parse_like(like: &FieldValue, raw: &str) -> Result<FieldValue, String> {
    match like {
        FieldValue::Number(_) => raw
            .trim()
            .parse::<f64>()
            .map(FieldValue::Number)
            .map_err(|_| "Enter a valid number".to_string()),
        FieldValue::Date(_) => date_string_to_millis(raw)
            .map(FieldValue::Date)
            .ok_or_else(|| "Enter a valid date".to_string()),
        FieldValue::Empty => {
            if raw.is_empty() {
                Ok(FieldValue::Empty)
            } else {
                Ok(FieldValue::Text(raw.to_string()))
            }
        }
        // Text and Bool(fallback): treat as text.
        _ => Ok(FieldValue::Text(raw.to_string())),
    }
}

/// Format an `f64` without a trailing `.0` for whole numbers.
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.is_finite() {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Convert unix-millis to a `YYYY-MM-DD` string for an `<input type="date">`.
fn millis_to_date_string(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let iso = d.to_iso_string().as_string().unwrap_or_default();
    // ISO is `YYYY-MM-DDTHH:MM:SS.sssZ`; keep the date part.
    iso.split('T').next().unwrap_or("").to_string()
}

/// Convert a `YYYY-MM-DD` string into unix-millis (UTC midnight), or `None`.
fn date_string_to_millis(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // `Date.parse` understands `YYYY-MM-DD` as UTC midnight.
    let ms = js_sys::Date::parse(s);
    if ms.is_nan() {
        None
    } else {
        Some(ms as i64)
    }
}

/// Human-readable local timestamp for the meta line.
fn format_millis(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let s = d.to_locale_string("en-US", &wasm_bindgen::JsValue::UNDEFINED);
    s.as_string().unwrap_or_default()
}
