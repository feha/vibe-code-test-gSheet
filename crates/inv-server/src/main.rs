//! Binary entry point for the inventory storage gateway.
//!
//! Env config:
//! - `INV_ADDR`: listen address (default `127.0.0.1:8080`).
//! - `INV_STATIC_DIR`: directory of the built SPA to serve. If unset, `./dist`
//!   is used when it exists.

use inv_server::{app, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::var("INV_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    // Static dir: explicit env wins; otherwise serve ./dist if it exists.
    let static_dir = std::env::var("INV_STATIC_DIR").ok().or_else(|| {
        if std::path::Path::new("./dist").is_dir() {
            Some("./dist".to_string())
        } else {
            None
        }
    });

    let router = app(AppState::new(), static_dir.as_deref());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("inv-server listening on http://{addr}");
    if let Some(dir) = &static_dir {
        println!("serving static SPA from {dir}");
    }
    axum::serve(listener, router).await?;
    Ok(())
}
