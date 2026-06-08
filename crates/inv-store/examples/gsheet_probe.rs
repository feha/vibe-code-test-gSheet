//! PHASE-A probe: prove the anonymous (no-OAuth) read-WRITE protocol for Google
//! Sheets from Rust, end to end, against the live test sheet.
//!
//! This replicates the Sheets web-editor's private save protocol (reverse
//! engineered from a live anonymous browser session) using `ureq` with an
//! automatic cookie jar.
//!
//! ## The protocol as actually proven (differs from the original capture notes)
//!
//! The original notes described a 3-step GET/bind/save handshake with a
//! *client-chosen* sid. Live re-capture from the browser showed two corrections:
//!
//!   * The `sid` is **NOT** client-chosen. The server assigns it in the `/edit`
//!     HTML as `"sid":"<16 hex>"`; only that sid is accepted by `/save`. A random
//!     client sid yields HTTP 400/550.
//!   * The `/bind` step is **NOT required** for a one-shot write. `/bind` actually
//!     returns HTTP 400 for a fresh page session, yet `/save` still succeeds. Bind
//!     only matters for the live collaboration channel, which a batch writer does
//!     not need.
//!
//! So the minimal proven protocol is just TWO steps:
//!
//!   1. GET  /edit  -> capture COMPASS/NID cookies + parse `"revision":N` and
//!      `"sid":"<hex>"` from the HTML.
//!   2. POST /save  -> multipart {rev, bundles}; revision advances; success.
//!
//! It then VERIFIES the write landed by reading the gviz CSV export.
//!
//! Run:  cargo run -p inv-store --example gsheet_probe
//!
//! The test sheet is shared "anyone with link can edit", so no credentials are
//! needed. It writes a junk value to cell B2.

use std::time::{SystemTime, UNIX_EPOCH};

/// The live anonymous-editable test sheet (shared: anyone-with-link can edit).
const SHEET_ID: &str = "1mAoX1uy2xE263M4uYzC5sM6LyC-eNDMnNUEKkb9EFuQ";

/// Browser-like UA; Google rejects/limits some non-browser agents on these
/// private editor endpoints.
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                  AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

// --- Magic opcodes, build-version-tied (editors.spreadsheets-frontend_20260601).
// If Google ships a new editor build these may need re-capture from the browser.
const OP_BUNDLE: i64 = 21299578; // outer "set of cell mutations" command tag
const OP_SET_CELL: i64 = 132274236; // inner "set one cell" mutation tag

fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

/// Parse the current revision from the editor HTML (`"revision":<N>`).
fn parse_revision(html: &str) -> Option<i64> {
    let needle = "\"revision\":";
    let i = html.find(needle)? + needle.len();
    let digits: String = html[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Parse the server-assigned session id from the editor HTML (`"sid":"<hex>"`).
/// This is the ONLY sid `/save` accepts; a client-chosen sid is rejected.
fn parse_sid(html: &str) -> Option<String> {
    let needle = "\"sid\":\"";
    let i = html.find(needle)? + needle.len();
    let val: String = html[i..].chars().take_while(|&c| c != '"').collect();
    if val.is_empty() { None } else { Some(val) }
}

fn null_v() -> serde_json::Value {
    serde_json::Value::Null
}

/// Build the INNER (double-encoded) set-cell command string for a 0-based
/// (row, col) on grid `gid` set to string `v`.
fn inner_set_cell(gid: &str, row: i64, col: i64, v: &str) -> String {
    serde_json::json!([
        [gid, row, row + 1, col, col + 1],
        [OP_SET_CELL, 3, [2, v], null_v(), null_v(), 0],
        [null_v(), [[null_v(), 513, [0], null_v(), null_v(), null_v(), null_v(), null_v(), null_v(), null_v(), null_v(), 0]]]
    ])
    .to_string()
}

/// Build the `bundles` JSON string for a set of (gid,row,col,value) cells.
fn build_bundles(sid: &str, req_id: i64, cells: &[(&str, i64, i64, &str)]) -> String {
    let commands: Vec<serde_json::Value> = cells
        .iter()
        .map(|(gid, r, c, v)| serde_json::json!([OP_BUNDLE, inner_set_cell(gid, *r, *c, v)]))
        .collect();
    serde_json::json!([{ "commands": commands, "sid": sid, "reqId": req_id }]).to_string()
}

/// Encode a multipart/form-data body from string fields. Returns (content_type, body).
fn multipart(fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    let boundary = format!("----rustprobe{:x}", now_nanos());
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn main() {
    let agent = ureq::AgentBuilder::new().user_agent(UA).redirects(5).build();

    // --- STEP 1: GET /edit -> cookies (auto-jarred) + revision + sid ---------
    let edit_url = format!("https://docs.google.com/spreadsheets/d/{SHEET_ID}/edit");
    let html = agent
        .get(&edit_url)
        .call()
        .expect("GET /edit")
        .into_string()
        .expect("read /edit body");
    let rev = parse_revision(&html).expect("parse revision from /edit HTML");
    let sid = parse_sid(&html).expect("parse server sid from /edit HTML");
    println!("STEP 1 OK: revision={rev} sid={sid}");

    // --- STEP 2: POST /save with a set-cell bundle (B2 = unique value) -------
    let probe_value = "rust-probe-B2-claudeprobe777";
    // B2 is 0-based (row=1, col=1) on the first grid gid="0".
    let bundles = build_bundles(&sid, 0, &[("0", 1, 1, probe_value)]);
    let (ct, body) = multipart(&[("rev", &rev.to_string()), ("bundles", &bundles)]);

    let save_url = format!(
        "https://docs.google.com/spreadsheets/d/{SHEET_ID}/save?\
         id={SHEET_ID}&sid={sid}&vc=1&c=1&w=1&flr=0&smv=2147483647&smb=%5B2147483647%2C%20APxr%5D\
         &includes_info_params=true&cros_files=false&nded=false"
    );
    match agent
        .post(&save_url)
        .set("content-type", &ct)
        .set("x-same-domain", "1")
        .send_bytes(&body)
    {
        Ok(r) => {
            let status = r.status();
            let text = r.into_string().unwrap_or_default();
            println!("STEP 2 OK: save status={status}");
            println!("save body: {text}");
        }
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            panic!("save HTTP {code}: {text}");
        }
        Err(e) => panic!("save transport error: {e}"),
    }

    // --- VERIFY: read back via gviz CSV --------------------------------------
    let csv_url = format!(
        "https://docs.google.com/spreadsheets/d/{SHEET_ID}/gviz/tq?tqx=out:csv&sheet=Sheet1"
    );
    let mut found = false;
    for attempt in 0..6 {
        let csv = agent
            .get(&csv_url)
            .call()
            .expect("GET gviz csv")
            .into_string()
            .expect("read csv");
        if csv.contains(probe_value) {
            println!("VERIFY OK (attempt {attempt}): probe value found in gviz CSV");
            found = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(900));
    }
    if !found {
        panic!("VERIFY FAILED: probe value {probe_value:?} not found in gviz CSV after retries");
    }

    println!("\nALL STEPS PASSED: anonymous write landed on the live sheet.");
}
