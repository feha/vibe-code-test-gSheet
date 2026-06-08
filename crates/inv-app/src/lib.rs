//! `inv-app`: a Leptos CSR (client-side rendered) WASM single-page app for the
//! bring-your-own-database inventory tool.
//!
//! The app talks to the gateway over the relative `/api` surface (see [`api`]).
//! All shared reactive state lives in [`state::AppState`], published as Leptos
//! context at the app root. UI is split into the open-database screen and the
//! workspace ([`app`]) plus leaf [`components`].

pub mod api;
pub mod app;
pub mod components;
pub mod state;

pub use app::App;
pub use state::AppState;
