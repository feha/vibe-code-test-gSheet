//! The root `<App/>` component and the two top-level screens:
//! - [`OpenDatabase`]: shown when no store is open (three connection options).
//! - [`Workspace`]: the sidebar + main-panel layout shown once a store is open.

use leptos::prelude::*;
use web_sys::HtmlInputElement;

use crate::api::StoreDescriptor;
use crate::components::{AddForm, DetailPanel, Navigator, SearchBar};
use crate::state::{AppState, ToastKind};

/// Root component. Provides [`AppState`] and switches between the open-database
/// screen and the workspace depending on whether a store is open.
#[component]
pub fn App() -> impl IntoView {
    let state = AppState::provide();

    view! {
        <div class="app">
            <ToastBar/>
            <Show
                when=move || state.descriptor.get().is_some()
                fallback=|| view! { <OpenDatabase/> }
            >
                <Workspace/>
            </Show>
        </div>
    }
}

/// The status toast (info / error), dismissable.
#[component]
fn ToastBar() -> impl IntoView {
    let state = AppState::expect();
    move || {
        state.status.get().map(|toast| {
            let cls = match toast.kind {
                ToastKind::Info => "toast toast-info",
                ToastKind::Error => "toast toast-error",
            };
            view! {
                <div class=cls role="status">
                    <span class="toast-msg">{toast.message}</span>
                    <button class="toast-close" on:click=move |_| state.clear_toast()>"x"</button>
                </div>
            }
        })
    }
}

/// The "open database" screen: three bring-your-own-database options.
#[component]
fn OpenDatabase() -> impl IntoView {
    let state = AppState::expect();

    // Local file
    let file_path = RwSignal::new(String::new());
    let open_file = move |_| {
        let path = file_path.get();
        if path.trim().is_empty() {
            state.error("Enter a file path");
            return;
        }
        state.open(StoreDescriptor::File { path });
    };

    // Postgres
    let pg_url = RwSignal::new(String::new());
    let open_pg = move |_| {
        let url = pg_url.get();
        if url.trim().is_empty() {
            state.error("Enter a connection URL");
            return;
        }
        state.open(StoreDescriptor::Postgres { url });
    };

    // Google Sheet
    let gs_id = RwSignal::new(String::new());
    let gs_token = RwSignal::new(String::new());
    let open_gs = move |_| {
        let spreadsheet_id = gs_id.get();
        let token = gs_token.get();
        if spreadsheet_id.trim().is_empty() || token.trim().is_empty() {
            state.error("Enter a spreadsheet id and token");
            return;
        }
        state.open(StoreDescriptor::GSheet {
            spreadsheet_id,
            token,
        });
    };

    view! {
        <main class="open-screen">
            <div class="open-hero">
                <h1 class="open-title">"Inventory"</h1>
                <p class="open-sub">"Bring your own database. Pick where your data lives."</p>
            </div>

            <div class="open-grid">
                <div class="card open-option">
                    <h2 class="card-title">"Local file"</h2>
                    <p class="muted">"A JSON document on the server's filesystem."</p>
                    <input
                        class="text-input"
                        r#type="text"
                        placeholder="/path/to/inventory.json"
                        prop:value=move || file_path.get()
                        on:input=move |ev| file_path.set(event_value(&ev))
                    />
                    <button class="btn btn-primary" on:click=open_file>"Open"</button>
                </div>

                <div class="card open-option">
                    <h2 class="card-title">"Postgres"</h2>
                    <p class="muted">"Connect to a PostgreSQL database."</p>
                    <input
                        class="text-input"
                        r#type="text"
                        placeholder="postgres://user:pass@host/db"
                        prop:value=move || pg_url.get()
                        on:input=move |ev| pg_url.set(event_value(&ev))
                    />
                    <button class="btn btn-primary" on:click=open_pg>"Open"</button>
                </div>

                <div class="card open-option">
                    <h2 class="card-title">"Google Sheet"</h2>
                    <p class="muted">"Use a Google Sheet as the backing store."</p>
                    <input
                        class="text-input"
                        r#type="text"
                        placeholder="spreadsheet id"
                        prop:value=move || gs_id.get()
                        on:input=move |ev| gs_id.set(event_value(&ev))
                    />
                    <input
                        class="text-input"
                        r#type="text"
                        placeholder="OAuth access token"
                        prop:value=move || gs_token.get()
                        on:input=move |ev| gs_token.set(event_value(&ev))
                    />
                    <button class="btn btn-primary" on:click=open_gs>"Open"</button>
                </div>
            </div>
        </main>
    }
}

/// The open-store workspace: header + sidebar + main panel.
#[component]
fn Workspace() -> impl IntoView {
    let state = AppState::expect();
    let label = move || {
        state
            .descriptor
            .get()
            .map(|d| d.label())
            .unwrap_or_default()
    };
    let count = move || state.inventory.with(|inv| inv.instances.len());

    view! {
        <div class="workspace">
            <header class="topbar">
                <div class="topbar-left">
                    <span class="brand">"Inventory"</span>
                    <span class="store-label">{label}</span>
                </div>
                <div class="topbar-right">
                    <span class="count-badge">{count}" items"</span>
                    <button class="btn btn-ghost" on:click=move |_| state.close()>"Close"</button>
                </div>
            </header>

            <div class="layout">
                <aside class="sidebar">
                    <AddForm/>
                    <SearchBar/>
                    <Navigator/>
                </aside>
                <main class="main-panel">
                    <DetailPanel/>
                </main>
            </div>
        </div>
    }
}

/// Read the current `value` of the input that fired an event.
fn event_value(ev: &leptos::ev::Event) -> String {
    use wasm_bindgen::JsCast;
    ev.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}
