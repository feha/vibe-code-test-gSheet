//! LIVE integration test for the anonymous (no-OAuth) read-WRITE Google Sheets
//! backend, against a real link-shared ("anyone with link can edit") sheet, using
//! the MULTI-TAB layout (separate `Meta`/`Classes`/`Instances` tabs).
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
//! transact, reads them back through the store, asserts equality, AND verifies via
//! gviz that the `Classes` and `Instances` tables landed on their OWN separate
//! tabs (not a single flat Sheet1).

use std::collections::BTreeMap;

use inv_core::InventoryExt;
use inv_model::{FieldValue, Inventory};
use inv_store::{GSheetMode, StoreDescriptor, StoreError, StoreExt};

/// The live anonymous-editable test sheet (shared: anyone-with-link can edit).
const SHEET_ID: &str = "1mAoX1uy2xE263M4uYzC5sM6LyC-eNDMnNUEKkb9EFuQ";
const DEFAULT_URL: &str =
    "https://docs.google.com/spreadsheets/d/1mAoX1uy2xE263M4uYzC5sM6LyC-eNDMnNUEKkb9EFuQ/edit";

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                  AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

fn test_url() -> String {
    std::env::var("INV_GSHEET_ANON_TEST_URL").unwrap_or_else(|_| DEFAULT_URL.to_string())
}

/// Fetch a tab's gviz CSV export (anonymous, no credential).
fn gviz_csv(tab: &str) -> String {
    let agent = ureq::AgentBuilder::new().user_agent(UA).redirects(5).build();
    let url = format!(
        "https://docs.google.com/spreadsheets/d/{SHEET_ID}/gviz/tq?tqx=out:csv&sheet={tab}"
    );
    agent
        .get(&url)
        .call()
        .expect("gviz GET")
        .into_string()
        .expect("gviz body")
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

    // Write it via the anonymous transact (full overwrite). On a sheet that does
    // not yet have the Meta/Classes/Instances tabs this also CREATES them.
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

    // VERIFY the SEPARATE-TABS layout via gviz: the Classes tab carries class
    // rows and the Instances tab carries instance rows — each on its OWN tab.
    let classes_csv = gviz_csv("Classes");
    let instances_csv = gviz_csv("Instances");
    eprintln!("Classes tab CSV:\n{classes_csv}");
    eprintln!("Instances tab CSV:\n{instances_csv}");

    // Classes tab: the class names appear; instance-only data does NOT.
    assert!(classes_csv.contains("Widget"), "Classes tab lists Widget");
    assert!(classes_csv.contains("Gadget"), "Classes tab lists Gadget");
    assert!(classes_csv.contains("EmptyClass"), "Classes tab lists EmptyClass");
    assert!(
        !classes_csv.contains("live-thing"),
        "instance names must NOT be on the Classes tab"
    );

    // Instances tab: the instance names appear; it carries its own header.
    assert!(instances_csv.contains("live-thing"), "Instances tab has live-thing");
    assert!(instances_csv.contains("live-child"), "Instances tab has live-child");
    assert!(
        instances_csv.contains("relationships_json"),
        "Instances tab carries its own header columns"
    );

    eprintln!(
        "LIVE anon multi-tab round-trip OK: {} instances, {} classes on separate tabs",
        got.instances.len(),
        got.classes.len()
    );
}
