//! `<ClassList/>`: a compact list of the inventory's class names, each with a
//! delete (×) affordance.
//!
//! Class names are read client-side via [`AppState::class_names`]. Deleting a
//! class routes through [`AppState::delete_class`] (the gateway's `delete_class`
//! op). A class still referenced by an instance is rejected by the gateway
//! (`ClassInUse` -> HTTP 409); that surfaces as an error toast and is expected.

use leptos::prelude::*;

use crate::state::AppState;

#[component]
pub fn ClassList() -> impl IntoView {
    let state = AppState::expect();

    // Reactive over the inventory signal: class_names() reads it, so the memo
    // tracks both the class set and any committed changes.
    let names = Memo::new(move |_| state.class_names());

    view! {
        <section class="card class-list">
            <style>{CLASS_LIST_CSS}</style>
            <h2 class="card-title">"Classes"</h2>
            <Show
                when=move || !names.get().is_empty()
                fallback=|| view! { <p class="muted">"No classes yet."</p> }
            >
                <ul class="class-items">
                    <For
                        each=move || names.get()
                        key=|name| name.clone()
                        let:name
                    >
                        <ClassRow name=name/>
                    </For>
                </ul>
            </Show>
        </section>
    }
}

/// A single class chip with a delete button. The button removes the class via the
/// shared op path; a class in use surfaces "class is in use" as a toast (409).
#[component]
fn ClassRow(name: String) -> impl IntoView {
    let state = AppState::expect();
    let label = name.clone();
    let delete = move |_| state.delete_class(name.clone());

    view! {
        <li class="class-item">
            <span class="class-name">{label}</span>
            <button
                class="class-del"
                title="Delete this class"
                on:click=delete
            >"\u{00d7}"</button>
        </li>
    }
}

/// Class-list styling, kept in this file so no shared asset is touched.
/// Builds on the shared design tokens (`--accent`, `--line`, etc.) from styles.css.
const CLASS_LIST_CSS: &str = r#"
.class-list { display: flex; flex-direction: column; gap: 10px; }
.class-items {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.class-item {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--ink);
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: 999px;
  padding: 2px 6px 2px 10px;
}
.class-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 140px;
}
.class-del {
  border: none;
  background: transparent;
  color: var(--muted);
  cursor: pointer;
  font-size: 14px;
  line-height: 1;
  padding: 0 2px;
  border-radius: 999px;
}
.class-del:hover { color: var(--error); background: var(--error-bg); }
"#;
