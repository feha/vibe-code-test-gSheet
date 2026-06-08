//! LIVE integration test for the anonymous (no-OAuth) read-WRITE Google Sheets
//! backend, against a real link-shared ("anyone with link can edit") sheet.
//!
//! This is `#[ignore]` by default so normal `cargo test` / CI runs skip it (it
//! needs the network and mutates a shared live sheet). Run it explicitly with:
//!
//!   cargo test -p inv-store --test gsheet_anon_live -- --ignored --nocapture
//!
//! The default target is the project's public test sheet; override with the env
//! var `INV_GSHEET_ANON_TEST_URL` to point at your own anonymous-editable sheet.
//!
//! The test writes a couple of classes + instances through the store's anonymous
//! transact, reads them back through the store, and asserts equality.

use std::collections::BTreeMap;

use inv_core::InventoryExt;
use inv_model::{FieldValue, Inventory};
use inv_store::{GSheetMode, StoreDescriptor, StoreError, StoreExt};

/// The live anonymous-editable test sheet (shared: anyone-with-link can edit).
const DEFAULT_URL: &str =
    "https://docs.google.com/spreadsheets/d/1mAoX1uy2xE263M4uYzC5sM6LyC-eNDMnNUEKkb9EFuQ/edit";

fn test_url() -> String {
    std::env::var("INV_GSHEET_ANON_TEST_URL").unwrap_or_else(|_| DEFAULT_URL.to_string())
}

#[test]
#[ignore = "live: hits the real Google Sheets anonymous /edit + /save endpoints and mutates a shared sheet"]
fn anon_inventory_roundtrip_against_live_sheet() {
    let desc = StoreDescriptor::GSheet {
        mode: GSheetMode::PublicUrl { url: test_url() },
    };
    let store = inv_store::open(&desc).expect("open public_url store");

    // Build a small, fully-overwriting inventory. We REPLACE the whole sheet so
    // the test is idempotent regardless of prior content.
    let mut want = Inventory::new();
    let mut f = BTreeMap::new();
    f.insert("color".to_string(), FieldValue::Text("red".to_string()));
    f.insert("qty".to_string(), FieldValue::Number(7.0));
    f.insert("active".to_string(), FieldValue::Bool(true));
    let a = want
        .add_instance("Widget", "live-thing", f, None, 1_000)
        .unwrap();
    let _b = want
        .add_instance("Gadget", "live-child", BTreeMap::new(), Some(a), 1_001)
        .unwrap();
    want.add_tag(a, "fresh", 1_002).unwrap();
    want.ensure_class("EmptyClass", 1_003);

    // Write it via the anonymous transact (full overwrite).
    let target = want.clone();
    store
        .transact(&mut |cur: &mut Inventory| -> Result<(), StoreError> {
            *cur = target.clone();
            Ok(())
        })
        .expect("anonymous transact should commit to the live sheet");

    // Read it back through the store and assert equality.
    let got = store.load().expect("load back from the live sheet");
    assert_eq!(got, want, "inventory round-trips through the anonymous protocol");

    // Sanity: the specific values survived.
    let inst = got.get(a).expect("instance a present");
    assert_eq!(inst.name, "live-thing");
    assert_eq!(inst.class, "Widget");
    assert_eq!(
        inst.fields.get("color"),
        Some(&FieldValue::Text("red".to_string()))
    );
    assert!(inst.tags.contains("fresh"));

    eprintln!("LIVE anon round-trip OK: {} instances, {} classes", got.instances.len(), got.classes.len());
}
