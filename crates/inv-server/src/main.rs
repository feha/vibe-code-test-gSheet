//! Binary entrypoint for `inv-server`.
//!
//! Configuration via environment variables:
//! - `INV_ADDR`       — socket address to bind (default `127.0.0.1:8080`).
//! - `INV_DB_PATH`    — redb database path (default `./inventory.redb`).
//! - `INV_STATIC_DIR` — directory to serve as the SPA. If unset, `./dist` is used
//!   when it exists, otherwise no static files are served.

use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr: SocketAddr = std::env::var("INV_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()?;

    let db_path =
        std::env::var("INV_DB_PATH").unwrap_or_else(|_| "./inventory.redb".to_string());
    let db = inv_server::open_db(db_path)?;

    let static_dir: Option<PathBuf> = match std::env::var("INV_STATIC_DIR") {
        Ok(d) => Some(PathBuf::from(d)),
        Err(_) => {
            let dist = PathBuf::from("./dist");
            dist.is_dir().then_some(dist)
        }
    };

    inv_server::run(addr, db, static_dir).await
}
