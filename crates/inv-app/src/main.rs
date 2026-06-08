//! Trunk entry point for the bring-your-own-database inventory SPA.
//!
//! Mounts the Leptos CSR app onto `<body>` and installs the panic hook so Rust
//! panics surface as readable messages in the browser console.

use inv_app::App;
use leptos::prelude::*;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}
