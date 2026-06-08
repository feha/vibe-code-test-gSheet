//! `<SearchBar/>`: filter instances by text / tag / class (client-side via
//! `inv_core::InventoryExt::search`).
//!
//! Three inputs (free-text name substring, exact tag, class dropdown) feed a
//! [`SearchQuery`] that is applied with a light debounce so typing doesn't run a
//! scan on every keystroke. Results show name + class + a breadcrumb built from
//! `path_of`; clicking a result selects it and points `current_container` at its
//! parent so the navigator reveals it in place.

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{HtmlInputElement, HtmlSelectElement};

use inv_core::{InventoryExt, SearchQuery};

use crate::state::AppState;

/// Debounce window for applying the text/tag inputs (milliseconds).
const DEBOUNCE_MS: u32 = 180;

#[component]
pub fn SearchBar() -> impl IntoView {
    let state = AppState::expect();

    // Live input signals (update on every keystroke / selection).
    let text = RwSignal::new(String::new());
    let tag = RwSignal::new(String::new());
    let class = RwSignal::new(String::new());

    // The debounced, "applied" mirrors of text/tag that actually drive the query.
    // Class applies immediately (a dropdown choice is a deliberate action).
    let applied_text = RwSignal::new(String::new());
    let applied_tag = RwSignal::new(String::new());

    // A single pending debounce timer, cancelled/replaced on each keystroke.
    // `Timeout` is `!Send`, so use thread-local storage.
    let pending = StoredValue::new_local(None::<gloo_timers::callback::Timeout>);

    let schedule_apply = move || {
        // Drop (cancel) any in-flight timer, then arm a fresh one.
        pending.set_value(None);
        let timer = gloo_timers::callback::Timeout::new(DEBOUNCE_MS, move || {
            applied_text.set(text.get_untracked());
            applied_tag.set(tag.get_untracked());
        });
        pending.set_value(Some(timer));
    };

    // Whether any filter is active (drives the "clear" affordance + empty copy).
    let has_filters = move || {
        !applied_text.get().trim().is_empty()
            || !applied_tag.get().trim().is_empty()
            || !class.get().is_empty()
    };

    // Reactively recompute the matching ids whenever an applied input changes OR
    // the inventory itself changes. `state.search` reads the inventory signal, so
    // this memo tracks both the filters and the underlying data.
    let results = Memo::new(move |_| {
        let q = SearchQuery {
            text: non_empty(applied_text.get().trim()),
            tag: non_empty(applied_tag.get().trim()),
            class: non_empty(class.get().trim()),
        };
        state.search(q)
    });

    let result_count = Memo::new(move |_| results.get().len());

    let clear = move |_| {
        text.set(String::new());
        tag.set(String::new());
        class.set(String::new());
        applied_text.set(String::new());
        applied_tag.set(String::new());
        pending.set_value(None);
    };

    view! {
        <section class="card search-bar">
            <h2 class="card-title">"Search"</h2>

            <input
                class="text-input"
                r#type="search"
                placeholder="Name contains…"
                prop:value=move || text.get()
                on:input=move |ev| {
                    text.set(input_value(&ev));
                    schedule_apply();
                }
            />

            <div class="search-filters">
                <input
                    class="text-input"
                    r#type="text"
                    placeholder="Tag"
                    prop:value=move || tag.get()
                    on:input=move |ev| {
                        tag.set(input_value(&ev));
                        schedule_apply();
                    }
                />
                <select
                    class="text-input"
                    prop:value=move || class.get()
                    on:change=move |ev| class.set(select_value(&ev))
                >
                    <option value="">"All classes"</option>
                    <For
                        each=move || state.class_names()
                        key=|name| name.clone()
                        let:name
                    >
                        <option value=name.clone()>{name.clone()}</option>
                    </For>
                </select>
            </div>

            <div class="search-meta">
                <span class="count-badge">
                    {move || {
                        let n = result_count.get();
                        if n == 1 { "1 match".to_string() } else { format!("{n} matches") }
                    }}
                </span>
                <Show when=has_filters fallback=|| ()>
                    <button class="btn btn-ghost search-clear" on:click=clear>
                        "Clear"
                    </button>
                </Show>
            </div>

            <Show
                when=move || result_count.get() != 0
                fallback=move || {
                    let msg = if has_filters() {
                        "No items match these filters."
                    } else {
                        "Type to search, or filter by tag or class."
                    };
                    view! { <p class="muted search-empty">{msg}</p> }
                }
            >
                <ul class="search-results">
                    <For
                        each=move || results.get()
                        key=|id| *id
                        let:id
                    >
                        <SearchResult id=id/>
                    </For>
                </ul>
            </Show>
        </section>
    }
}

/// A single search hit: name, class chip, and a containment breadcrumb. Clicking
/// selects the instance and points the navigator at its parent container.
#[component]
fn SearchResult(id: i64) -> impl IntoView {
    let state = AppState::expect();

    // Pull display data reactively so renames / moves elsewhere stay in sync.
    let name = move || {
        state
            .inventory
            .with(|inv| inv.get(id).map(|i| i.name.clone()))
            .unwrap_or_default()
    };
    let class = move || {
        state
            .inventory
            .with(|inv| inv.get(id).map(|i| i.class.clone()))
            .unwrap_or_default()
    };
    // Breadcrumb = ancestor names along the containment path, excluding `id`
    // itself (its own name is already the title).
    let breadcrumb = move || {
        state.inventory.with(|inv| {
            let path = inv.path_of(id);
            let names: Vec<String> = path
                .iter()
                .filter(|&&p| p != id)
                .filter_map(|&p| inv.get(p).map(|i| i.name.clone()))
                .collect();
            names.join(" / ")
        })
    };

    let select = move |_| {
        // Selecting reveals the item: open its parent container in the navigator,
        // then mark it selected for the detail panel.
        let parent = state.inventory.with(|inv| inv.get(id).and_then(|i| i.parent));
        state.current_container.set(parent);
        state.selected.set(Some(id));
    };

    let is_selected = move || state.selected.get() == Some(id);

    view! {
        <li>
            <button
                class="search-result"
                class:is-selected=is_selected
                on:click=select
            >
                <span class="search-result-main">
                    <span class="search-result-name">{name}</span>
                    <span class="search-result-class">{class}</span>
                </span>
                <Show when=move || !breadcrumb().is_empty() fallback=|| ()>
                    <span class="search-result-path">{breadcrumb}</span>
                </Show>
            </button>
        </li>
    }
}

/// `Some(trimmed-owned)` when non-empty, else `None` — keeps `SearchQuery` fields
/// `None` when a filter is unused so the AND-semantics ignore them.
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Read the current `value` from the `<input>` that fired an event.
fn input_value(ev: &leptos::ev::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

/// Read the current `value` from the `<select>` that fired an event.
fn select_value(ev: &leptos::ev::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<HtmlSelectElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}
