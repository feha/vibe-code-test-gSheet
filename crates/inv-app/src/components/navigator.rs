//! `<Navigator/>`: the containment-tree browser.
//!
//! Shows a breadcrumb trail from `path_of(current_container)` (with a "Top level"
//! root), then the children of the current container (or the roots when at the
//! top). Each row shows the instance's name, class, tags, child-count and a photo
//! thumbnail if one exists. Clicking a row selects it; an "open" affordance
//! navigates into it (sets `current_container`). Each row also offers Move (a
//! searchable parent picker, surfacing `WouldCycle` as a toast via the shared
//! op path) and Delete (with a Cascade / Reparent choice).

use leptos::prelude::*;
use web_sys::HtmlInputElement;

use inv_core::InventoryExt;

use crate::api::RemoveModeWire;
use crate::state::AppState;

/// Which inline action panel (if any) is open for a given row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowMode {
    None,
    Move,
    Delete,
}

#[component]
pub fn Navigator() -> impl IntoView {
    let state = AppState::expect();

    // Breadcrumb: root..=current_container, resolved to (id, name) pairs.
    let crumbs = move || {
        let Some(container) = state.current_container.get() else {
            return Vec::<(i64, String)>::new();
        };
        state.inventory.with(|inv| {
            inv.path_of(container)
                .into_iter()
                .map(|id| {
                    let name = inv
                        .get(id)
                        .map(|i| i.name.clone())
                        .unwrap_or_else(|| format!("#{id}"));
                    (id, name)
                })
                .collect::<Vec<_>>()
        })
    };

    // The ids to list: children of the current container, or the roots at top.
    let rows = move || {
        state.inventory.with(|inv| match state.current_container.get() {
            Some(container) => inv.children_of(container),
            None => inv.roots(),
        })
    };

    let at_top = move || state.current_container.get().is_none();

    view! {
        <section class="card navigator">
            <style>{NAVIGATOR_CSS}</style>
            <h2 class="card-title">"Navigator"</h2>

            <nav class="nav-crumbs" aria-label="Containment path">
                <button
                    class="crumb crumb-root"
                    class:crumb-current=at_top
                    on:click=move |_| state.current_container.set(None)
                    title="Go to the top level"
                >
                    "Top level"
                </button>
                <For
                    each=crumbs
                    key=|(id, name)| (*id, name.clone())
                    let:crumb
                >
                    {
                        let (id, name) = crumb;
                        let is_current = move || state.current_container.get() == Some(id);
                        view! {
                            <span class="crumb-sep" aria-hidden="true">"/"</span>
                            <button
                                class="crumb"
                                class:crumb-current=is_current
                                on:click=move |_| state.current_container.set(Some(id))
                            >
                                {name}
                            </button>
                        }
                    }
                </For>
            </nav>

            <div class="nav-list">
                <For each=rows key=|id| *id let:id>
                    <NavRow id=id/>
                </For>
            </div>

            <Show when=move || rows().is_empty()>
                <p class="nav-empty muted">
                    {move || {
                        if at_top() {
                            "No items yet. Use \u{201c}Add item\u{201d} to create your first one."
                        } else {
                            "This container is empty. Add items, or move existing ones in here."
                        }
                    }}
                </p>
            </Show>
        </section>
    }
}

/// A single row in the navigator list: summary + select/open + inline actions.
#[component]
fn NavRow(id: i64) -> impl IntoView {
    let state = AppState::expect();
    let mode = RwSignal::new(RowMode::None);

    // Reactive snapshot of this instance's display data. `None` once it is gone.
    let name = move || state.inventory.with(|inv| inv.get(id).map(|i| i.name.clone()));
    let class = move || state.inventory.with(|inv| inv.get(id).map(|i| i.class.clone()));
    let tags = move || {
        state
            .inventory
            .with(|inv| inv.get(id).map(|i| i.tags.iter().cloned().collect::<Vec<_>>()))
            .unwrap_or_default()
    };
    let child_count = move || state.inventory.with(|inv| inv.children_of(id).len());
    let first_photo_key = move || {
        state
            .inventory
            .with(|inv| inv.get(id).and_then(|i| i.photos.first().map(|p| p.key.clone())))
    };
    let is_selected = move || state.selected.get() == Some(id);

    let select = move |_| state.selected.set(Some(id));
    let open = move |ev: leptos::ev::MouseEvent| {
        ev.stop_propagation();
        state.current_container.set(Some(id));
        state.selected.set(Some(id));
    };

    view! {
        <Show when=move || name().is_some()>
            <div class="nav-row" class:selected=is_selected>
                <div
                    class="nav-row-main"
                    role="button"
                    tabindex="0"
                    on:click=select
                >
                    <PhotoThumb key=Signal::derive(first_photo_key)/>
                    <div class="nav-row-text">
                        <div class="nav-row-line">
                            <span class="nav-name">{move || name().unwrap_or_default()}</span>
                            <span class="nav-class">{move || class().unwrap_or_default()}</span>
                        </div>
                        <div class="nav-row-meta">
                            <For
                                each=tags
                                key=|t| t.clone()
                                let:tag
                            >
                                <span class="nav-tag">{tag}</span>
                            </For>
                            <Show when=move || { child_count() > 0 }>
                                <span class="nav-children" title="Items contained inside">
                                    {move || child_count()}
                                    " inside"
                                </span>
                            </Show>
                        </div>
                    </div>
                </div>

                <div class="nav-row-actions">
                    <button
                        class="icon-btn"
                        title="Open this container"
                        on:click=open
                    >
                        "Open \u{203a}"
                    </button>
                    <button
                        class="icon-btn"
                        title="Move to another container"
                        class:active=move || mode.get() == RowMode::Move
                        on:click=move |ev: leptos::ev::MouseEvent| {
                            ev.stop_propagation();
                            mode.update(|m| {
                                *m = if *m == RowMode::Move { RowMode::None } else { RowMode::Move };
                            });
                        }
                    >
                        "Move"
                    </button>
                    <button
                        class="icon-btn icon-danger"
                        title="Delete this item"
                        class:active=move || mode.get() == RowMode::Delete
                        on:click=move |ev: leptos::ev::MouseEvent| {
                            ev.stop_propagation();
                            mode.update(|m| {
                                *m = if *m == RowMode::Delete { RowMode::None } else { RowMode::Delete };
                            });
                        }
                    >
                        "Delete"
                    </button>
                </div>

                <Show when=move || mode.get() == RowMode::Move>
                    <MovePanel id=id on_done=Callback::new(move |_| mode.set(RowMode::None))/>
                </Show>
                <Show when=move || mode.get() == RowMode::Delete>
                    <DeletePanel id=id on_done=Callback::new(move |_| mode.set(RowMode::None))/>
                </Show>
            </div>
        </Show>
    }
}

/// A photo thumbnail that lazily resolves the stored bytes to an object URL.
#[component]
fn PhotoThumb(key: Signal<Option<String>>) -> impl IntoView {
    let state = AppState::expect();
    let url = RwSignal::new(String::new());

    // Whenever the key changes, fetch a fresh object URL (or clear it).
    Effect::new(move |_| {
        url.set(String::new());
        if let Some(k) = key.get() {
            leptos::task::spawn_local(async move {
                let resolved = state.photo_object_url(k).await;
                url.set(resolved);
            });
        }
    });

    move || {
        if key.get().is_none() {
            view! { <div class="nav-thumb nav-thumb-empty" aria-hidden="true">"\u{1F4E6}"</div> }
                .into_any()
        } else {
            view! {
                <img
                    class="nav-thumb"
                    alt="photo"
                    prop:src=move || url.get()
                />
            }
            .into_any()
        }
    }
}

/// Inline panel to move an instance under a new parent (or to the top level),
/// with a searchable list of candidate containers. `WouldCycle` and other
/// errors surface as toasts via the shared op path in `AppState`.
#[component]
fn MovePanel(id: i64, on_done: Callback<()>) -> impl IntoView {
    let state = AppState::expect();
    let filter = RwSignal::new(String::new());

    // Candidate parents: every instance except `id` itself and its descendants
    // (those would form a cycle), matched against the case-insensitive filter.
    let candidates = move || {
        let needle = filter.get().to_lowercase();
        state.inventory.with(|inv| {
            let mut forbidden = inv.descendants_of(id);
            forbidden.push(id);
            let forbidden: std::collections::BTreeSet<i64> = forbidden.into_iter().collect();

            let mut out: Vec<(i64, String, String)> = inv
                .instances
                .values()
                .filter(|i| !forbidden.contains(&i.id))
                .filter(|i| {
                    needle.is_empty()
                        || i.name.to_lowercase().contains(&needle)
                        || i.class.to_lowercase().contains(&needle)
                })
                .map(|i| (i.id, i.name.clone(), i.class.clone()))
                .collect();
            out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            out.truncate(50);
            out
        })
    };

    // Where does this instance currently live? (used to disable a no-op move).
    let current_parent = move || state.inventory.with(|inv| inv.get(id).and_then(|i| i.parent));

    view! {
        <div class="nav-panel" on:click=|ev: leptos::ev::MouseEvent| ev.stop_propagation()>
            <div class="nav-panel-head">
                <span class="nav-panel-title">"Move into\u{2026}"</span>
                <button class="icon-btn" on:click=move |_| on_done.run(())>"Cancel"</button>
            </div>

            <input
                class="text-input nav-panel-search"
                r#type="text"
                placeholder="Search containers by name or class"
                prop:value=move || filter.get()
                on:click=|ev: leptos::ev::MouseEvent| ev.stop_propagation()
                on:input=move |ev| filter.set(input_value(&ev))
            />

            <div class="nav-pick-list">
                <button
                    class="nav-pick"
                    disabled=move || current_parent().is_none()
                    on:click=move |_| {
                        state.move_instance(id, None);
                        on_done.run(());
                    }
                >
                    "Top level (no container)"
                </button>
                <For
                    each=candidates
                    key=|(cid, name, _)| (*cid, name.clone())
                    let:cand
                >
                    {
                        let (cid, name, klass) = cand;
                        let is_current = move || current_parent() == Some(cid);
                        view! {
                            <button
                                class="nav-pick"
                                disabled=is_current
                                on:click=move |_| {
                                    state.move_instance(id, Some(cid));
                                    on_done.run(());
                                }
                            >
                                <span class="nav-pick-name">{name}</span>
                                <span class="nav-pick-class">{klass}</span>
                            </button>
                        }
                    }
                </For>
                <Show when=move || candidates().is_empty()>
                    <p class="nav-empty muted">"No eligible containers match."</p>
                </Show>
            </div>
        </div>
    }
}

/// Inline panel to delete an instance, choosing how to treat its children.
#[component]
fn DeletePanel(id: i64, on_done: Callback<()>) -> impl IntoView {
    let state = AppState::expect();
    let child_count = move || state.inventory.with(|inv| inv.children_of(id).len());

    view! {
        <div class="nav-panel" on:click=|ev: leptos::ev::MouseEvent| ev.stop_propagation()>
            <div class="nav-panel-head">
                <span class="nav-panel-title">"Delete this item?"</span>
                <button class="icon-btn" on:click=move |_| on_done.run(())>"Cancel"</button>
            </div>

            <Show
                when=move || { child_count() > 0 }
                fallback=move || view! {
                    <p class="muted nav-panel-note">"This item has no contained items."</p>
                    <div class="nav-panel-row">
                        <button
                            class="btn btn-danger"
                            on:click=move |_| {
                                state.remove_instance(id, RemoveModeWire::Cascade);
                                on_done.run(());
                            }
                        >
                            "Delete"
                        </button>
                    </div>
                }
            >
                <p class="muted nav-panel-note">
                    {move || format!(
                        "This item contains {} item(s). Choose what happens to them:",
                        child_count(),
                    )}
                </p>
                <div class="nav-panel-row">
                    <button
                        class="btn btn-danger"
                        title="Delete this item and everything inside it"
                        on:click=move |_| {
                            state.remove_instance(id, RemoveModeWire::Cascade);
                            on_done.run(());
                        }
                    >
                        "Delete subtree"
                    </button>
                    <button
                        class="btn btn-ghost"
                        title="Delete only this item; move its children up to its parent"
                        on:click=move |_| {
                            state.remove_instance(id, RemoveModeWire::Reparent);
                            on_done.run(());
                        }
                    >
                        "Delete, keep children"
                    </button>
                </div>
            </Show>
        </div>
    }
}

/// Read the current `value` of the input that fired an event.
fn input_value(ev: &leptos::ev::Event) -> String {
    use wasm_bindgen::JsCast;
    ev.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

/// Navigator-scoped styling, kept in this file so no shared asset is touched.
/// Builds on the shared design tokens (`--accent`, `--line`, etc.) from styles.css.
const NAVIGATOR_CSS: &str = r#"
.navigator { display: flex; flex-direction: column; gap: 12px; }

.nav-crumbs {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px;
  font-size: 13px;
}
.crumb {
  border: none;
  background: transparent;
  color: var(--accent);
  cursor: pointer;
  padding: 2px 6px;
  border-radius: 6px;
  font-size: 13px;
  max-width: 160px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.crumb:hover { background: var(--bg); }
.crumb-current {
  color: var(--ink);
  font-weight: 600;
  cursor: default;
}
.crumb-current:hover { background: transparent; }
.crumb-sep { color: var(--muted); opacity: 0.6; }

.nav-list { display: flex; flex-direction: column; gap: 8px; }

.nav-row {
  display: grid;
  grid-template-columns: 1fr auto;
  grid-template-rows: auto;
  gap: 8px;
  align-items: center;
  border: 1px solid var(--line);
  border-radius: 10px;
  padding: 8px 10px;
  background: #fff;
  transition: border-color 0.12s ease, box-shadow 0.12s ease;
}
.nav-row:hover { border-color: #cdd3dc; }
.nav-row.selected {
  border-color: var(--accent);
  box-shadow: 0 0 0 2px rgba(59, 108, 255, 0.15);
}

.nav-row-main {
  display: flex;
  align-items: center;
  gap: 10px;
  min-width: 0;
  cursor: pointer;
}

.nav-thumb {
  width: 38px;
  height: 38px;
  border-radius: 8px;
  object-fit: cover;
  background: var(--bg);
  border: 1px solid var(--line);
  flex: none;
}
.nav-thumb-empty {
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 18px;
  opacity: 0.55;
}

.nav-row-text { min-width: 0; }
.nav-row-line {
  display: flex;
  align-items: baseline;
  gap: 8px;
  min-width: 0;
}
.nav-name {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.nav-class {
  font-size: 11px;
  color: var(--muted);
  background: var(--bg);
  border-radius: 999px;
  padding: 1px 8px;
  flex: none;
}
.nav-row-meta {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
  margin-top: 3px;
}
.nav-tag {
  font-size: 11px;
  color: var(--accent);
  background: rgba(59, 108, 255, 0.10);
  border-radius: 999px;
  padding: 1px 8px;
}
.nav-children { font-size: 11px; color: var(--muted); }

.nav-row-actions {
  display: flex;
  align-items: center;
  gap: 4px;
  justify-self: end;
}
.icon-btn {
  border: 1px solid var(--line);
  background: #fff;
  color: var(--ink);
  border-radius: 7px;
  padding: 4px 9px;
  font-size: 12px;
  font-weight: 600;
  cursor: pointer;
  white-space: nowrap;
}
.icon-btn:hover { background: var(--bg); }
.icon-btn.active { border-color: var(--accent); color: var(--accent); }
.icon-danger:hover { background: var(--error-bg); color: var(--error); border-color: #f3c2bd; }

.nav-panel {
  grid-column: 1 / -1;
  border-top: 1px dashed var(--line);
  margin-top: 6px;
  padding-top: 8px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.nav-panel-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.nav-panel-title { font-size: 12px; font-weight: 600; color: var(--muted); }
.nav-panel-note { font-size: 12px; }
.nav-panel-search { margin-bottom: 0; }
.nav-panel-row { display: flex; flex-wrap: wrap; gap: 8px; }

.btn-danger { background: var(--error); color: #fff; }
.btn-danger:hover { filter: brightness(1.05); }

.nav-pick-list {
  display: flex;
  flex-direction: column;
  gap: 4px;
  max-height: 220px;
  overflow: auto;
}
.nav-pick {
  display: flex;
  align-items: baseline;
  gap: 8px;
  text-align: left;
  border: 1px solid var(--line);
  background: #fff;
  border-radius: 8px;
  padding: 7px 10px;
  cursor: pointer;
  font-size: 13px;
  color: var(--ink);
}
.nav-pick:hover:not(:disabled) { background: var(--bg); border-color: #cdd3dc; }
.nav-pick:disabled { opacity: 0.5; cursor: default; }
.nav-pick-name { font-weight: 600; }
.nav-pick-class {
  font-size: 11px;
  color: var(--muted);
  background: var(--bg);
  border-radius: 999px;
  padding: 1px 8px;
}

.nav-empty { font-size: 13px; padding: 6px 2px; }
"#;
