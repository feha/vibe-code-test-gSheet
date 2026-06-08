//! `<AddForm/>`: a prominent quick-add panel for creating a new instance in the
//! open store.
//!
//! Captures a name (required), a class (combobox that both suggests known class
//! names and accepts a brand-new typed name), an optional set of typed custom
//! fields, and free-form tags (comma/space separated). The target container
//! defaults to the navigator's `current_container` (shown by name, or
//! "Top level"). On submit it calls [`AppState::add_instance`], clears the form,
//! and selects the freshly created instance.
//!
//! The new id is not returned by the fire-and-forget mutator, so we predict it
//! from `inventory.next_id` (the single-user gateway hands ids out sequentially,
//! exactly as `inv_core::add_instance` does) and select it optimistically.

use std::collections::BTreeMap;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{HtmlInputElement, HtmlSelectElement};

use inv_model::FieldValue;

use crate::state::AppState;

/// The kind of value a custom field row produces.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Text,
    Number,
    Bool,
    Date,
}

impl FieldKind {
    fn from_str(s: &str) -> Self {
        match s {
            "number" => FieldKind::Number,
            "bool" => FieldKind::Bool,
            "date" => FieldKind::Date,
            _ => FieldKind::Text,
        }
    }
}

/// One editable custom-field row: a stable key (for the `<For>`), the field
/// name, the chosen value kind, and the raw text the user typed.
#[derive(Clone)]
struct FieldRow {
    key: usize,
    name: RwSignal<String>,
    kind: RwSignal<FieldKind>,
    value: RwSignal<String>,
}

#[component]
pub fn AddForm() -> impl IntoView {
    let state = AppState::expect();

    let name = RwSignal::new(String::new());
    let class = RwSignal::new(String::new());
    let tags = RwSignal::new(String::new());
    let rows = RwSignal::new(Vec::<FieldRow>::new());
    let next_row_key = RwSignal::new(0usize);

    // The container we'll drop the new item into, and a friendly label for it.
    let container_label = move || match state.current_container.get() {
        Some(id) => state
            .inventory
            .with(|inv| inv.instances.get(&id).map(|i| i.name.clone()))
            .unwrap_or_else(|| format!("#{id}")),
        None => "Top level".to_string(),
    };

    let add_row = move |_| {
        let key = next_row_key.get_untracked();
        next_row_key.set(key + 1);
        rows.update(|r| {
            r.push(FieldRow {
                key,
                name: RwSignal::new(String::new()),
                kind: RwSignal::new(FieldKind::Text),
                value: RwSignal::new(String::new()),
            });
        });
    };

    // The actual submit: validate, build fields/tags, fire the op, select the
    // predicted new id, and reset the form.
    let submit = move || {
        let item_name = name.get_untracked().trim().to_string();
        if item_name.is_empty() {
            state.error("Name is required");
            return;
        }
        // A blank class falls back to a sensible default so the user can add an
        // item with one keystroke.
        let class_name = {
            let c = class.get_untracked().trim().to_string();
            if c.is_empty() { "Item".to_string() } else { c }
        };

        // Collect custom fields, skipping rows with an empty key. Later rows with
        // the same name win (BTreeMap insert).
        let mut fields: BTreeMap<String, FieldValue> = BTreeMap::new();
        for row in rows.get_untracked() {
            let fname = row.name.get_untracked().trim().to_string();
            if fname.is_empty() {
                continue;
            }
            let raw = row.value.get_untracked();
            let value = parse_field_value(row.kind.get_untracked(), &raw);
            fields.insert(fname, value);
        }

        // Tags: split on commas and whitespace, dedupe, drop blanks.
        let mut tag_list: Vec<String> = Vec::new();
        for t in tags
            .get_untracked()
            .split([',', ' ', '\n', '\t'])
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let t = t.to_string();
            if !tag_list.contains(&t) {
                tag_list.push(t);
            }
        }

        let parent = state.current_container.get_untracked();

        // Predict the id the gateway will assign (sequential, single-user).
        let predicted_id = state.inventory.with_untracked(|inv| inv.next_id);

        state.add_instance(class_name, item_name, fields, parent, tag_list);
        state.selected.set(Some(predicted_id));

        // Reset the form for the next entry.
        name.set(String::new());
        class.set(String::new());
        tags.set(String::new());
        rows.set(Vec::new());
    };

    // Enter anywhere in the form submits (without inserting a newline).
    let on_keydown = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Enter" {
            ev.prevent_default();
            submit();
        }
    };

    let class_options = move || state.class_names();

    view! {
        <section class="card add-form">
            <h2 class="card-title">"Add item"</h2>

            <div on:keydown=on_keydown>
                <input
                    class="text-input"
                    r#type="text"
                    placeholder="Name (required)"
                    aria-label="Item name"
                    prop:value=move || name.get()
                    on:input=move |ev| name.set(input_value(&ev))
                />

                <input
                    class="text-input"
                    r#type="text"
                    list="add-form-classes"
                    placeholder="Class (e.g. Box, Tool) — or pick one"
                    aria-label="Class name"
                    prop:value=move || class.get()
                    on:input=move |ev| class.set(input_value(&ev))
                />
                <datalist id="add-form-classes">
                    <For
                        each=class_options
                        key=|c| c.clone()
                        let:c
                    >
                        <option value=c></option>
                    </For>
                </datalist>

                <p
                    class="muted"
                    style="margin:0 0 10px; font-size:12px;"
                >
                    "Into: "
                    <strong style="color:var(--ink); font-weight:600;">
                        {container_label}
                    </strong>
                </p>

                <input
                    class="text-input"
                    r#type="text"
                    placeholder="Tags (comma or space separated)"
                    aria-label="Tags"
                    prop:value=move || tags.get()
                    on:input=move |ev| tags.set(input_value(&ev))
                />

                <Show when=move || !rows.get().is_empty()>
                    <div style="display:flex; flex-direction:column; gap:8px; margin-bottom:10px;">
                        <For
                            each=move || rows.get()
                            key=|row| row.key
                            let:row
                        >
                            <FieldRowView row=row rows=rows/>
                        </For>
                    </div>
                </Show>

                <div style="display:flex; gap:8px; align-items:center;">
                    <button
                        class="btn btn-ghost"
                        r#type="button"
                        on:click=add_row
                    >
                        "+ Field"
                    </button>
                    <button
                        class="btn btn-primary"
                        r#type="button"
                        style="margin-left:auto;"
                        on:click=move |_| submit()
                    >
                        "Add item"
                    </button>
                </div>
            </div>
        </section>
    }
}

/// One custom-field editor row: name, type, value, and a remove button.
#[component]
fn FieldRowView(row: FieldRow, rows: RwSignal<Vec<FieldRow>>) -> impl IntoView {
    let FieldRow {
        key,
        name,
        kind,
        value,
    } = row;

    let remove = move |_| {
        rows.update(|r| r.retain(|x| x.key != key));
    };

    // The value editor depends on the chosen kind: a checkbox for Bool, a
    // date picker for Date, otherwise a text/number-ish input.
    let value_editor = move || {
        let placeholder = match kind.get() {
            FieldKind::Number => "0",
            FieldKind::Date => "",
            _ => "value",
        };
        match kind.get() {
            FieldKind::Bool => {
                let checked = matches!(value.get().as_str(), "true");
                view! {
                    <label style="display:flex; align-items:center; gap:6px; flex:1; font-size:13px;">
                        <input
                            r#type="checkbox"
                            prop:checked=checked
                            on:change=move |ev| {
                                value.set(if checkbox_checked(&ev) { "true".into() } else { "false".into() });
                            }
                        />
                        {move || if matches!(value.get().as_str(), "true") { "true" } else { "false" }}
                    </label>
                }
                .into_any()
            }
            FieldKind::Date => view! {
                <input
                    class="text-input"
                    r#type="date"
                    style="flex:1; margin-bottom:0;"
                    prop:value=move || value.get()
                    on:input=move |ev| value.set(input_value(&ev))
                />
            }
            .into_any(),
            _ => {
                let input_type = if matches!(kind.get(), FieldKind::Number) { "number" } else { "text" };
                view! {
                    <input
                        class="text-input"
                        r#type=input_type
                        style="flex:1; margin-bottom:0;"
                        placeholder=placeholder
                        prop:value=move || value.get()
                        on:input=move |ev| value.set(input_value(&ev))
                    />
                }
                .into_any()
            }
        }
    };

    view! {
        <div style="display:flex; gap:6px; align-items:center;">
            <input
                class="text-input"
                r#type="text"
                style="flex:1; margin-bottom:0;"
                placeholder="field name"
                aria-label="Field name"
                prop:value=move || name.get()
                on:input=move |ev| name.set(input_value(&ev))
            />
            <select
                class="text-input"
                style="width:auto; margin-bottom:0;"
                aria-label="Field type"
                on:change=move |ev| kind.set(FieldKind::from_str(&select_value(&ev)))
            >
                <option value="text" selected=move || matches!(kind.get(), FieldKind::Text)>"Text"</option>
                <option value="number" selected=move || matches!(kind.get(), FieldKind::Number)>"Number"</option>
                <option value="bool" selected=move || matches!(kind.get(), FieldKind::Bool)>"Bool"</option>
                <option value="date" selected=move || matches!(kind.get(), FieldKind::Date)>"Date"</option>
            </select>
            {value_editor}
            <button
                class="btn btn-ghost"
                r#type="button"
                style="padding:9px 11px;"
                title="Remove field"
                aria-label="Remove field"
                on:click=remove
            >
                "x"
            </button>
        </div>
    }
}

/// Convert a row's raw text + chosen kind into a typed [`FieldValue`].
///
/// Parsing is forgiving: an unparseable number/date and an empty value become
/// [`FieldValue::Empty`] so a half-filled row never blocks the add.
fn parse_field_value(kind: FieldKind, raw: &str) -> FieldValue {
    let raw = raw.trim();
    match kind {
        FieldKind::Text => {
            if raw.is_empty() {
                FieldValue::Empty
            } else {
                FieldValue::Text(raw.to_string())
            }
        }
        FieldKind::Number => match raw.parse::<f64>() {
            Ok(n) => FieldValue::Number(n),
            Err(_) => FieldValue::Empty,
        },
        FieldKind::Bool => FieldValue::Bool(raw == "true"),
        FieldKind::Date => match parse_date_to_millis(raw) {
            Some(ms) => FieldValue::Date(ms),
            None => FieldValue::Empty,
        },
    }
}

/// Parse an `<input type="date">` value (`YYYY-MM-DD`) into unix milliseconds
/// (UTC midnight). Returns `None` for anything that doesn't parse.
fn parse_date_to_millis(s: &str) -> Option<i64> {
    let mut parts = s.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400_000)
}

/// Days since the unix epoch for a civil (proleptic Gregorian) date.
/// Howard Hinnant's well-known `days_from_civil` algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Read the `value` of the `<input>` that fired an event.
fn input_value(ev: &leptos::ev::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

/// Read the `checked` state of the checkbox that fired an event.
fn checkbox_checked(ev: &leptos::ev::Event) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|el| el.checked())
        .unwrap_or(false)
}

/// Read the `value` of the `<select>` that fired an event.
fn select_value(ev: &leptos::ev::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<HtmlSelectElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}
