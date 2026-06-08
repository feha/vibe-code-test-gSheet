//! PHASE-A2 probe: prove the anonymous (no-OAuth) ADD-SHEET command works from
//! Rust against the live test sheet, then write a cell into the freshly-created
//! tab in the SAME /save bundle.
//!
//! This builds on `gsheet_probe.rs` (which proved the set-cell command). It adds
//! the newly-captured compound "add sheet" command:
//!
//!   [OP_ADD_SHEET, [[OP_ADD_INNER, "<ADDINNER>"], [OP_ADD_INDEX, "<IDXINNER>"]]]
//!
//! where ADDINNER creates a tab with a client-chosen gid + name and IDXINNER
//! positions it. We then prove a set-cell command can target that new gid in the
//! same bundle.
//!
//! Run:  cargo run -p inv-store --example gsheet_addsheet_probe
//!
//! VERIFY: gviz CSV of the new tab returns a (possibly empty) doc + our cell,
//! NOT an error doc — proving the tab now exists.

use std::time::{SystemTime, UNIX_EPOCH};

const SHEET_ID: &str = "1mAoX1uy2xE263M4uYzC5sM6LyC-eNDMnNUEKkb9EFuQ";

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                  AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

// Magic opcodes, build-version-tied (editors.spreadsheets-frontend_20260601).
const OP_BUNDLE: i64 = 21299578; // outer command tag for a set-cell
const OP_SET_CELL: i64 = 132274236; // inner "set one cell" mutation tag
const OP_ADD_SHEET: i64 = 4444216; // outer "add sheet" compound command tag
const OP_ADD_INNER: i64 = 21350203; // inner add-sheet (gid + name + dims)
const OP_ADD_INDEX: i64 = 28950036; // inner sheet-index positioner

fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

fn parse_revision(html: &str) -> Option<i64> {
    let needle = "\"revision\":";
    let i = html.find(needle)? + needle.len();
    let digits: String = html[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn parse_sid(html: &str) -> Option<String> {
    let needle = "\"sid\":\"";
    let i = html.find(needle)? + needle.len();
    let val: String = html[i..].chars().take_while(|&c| c != '"').collect();
    if val.is_empty() { None } else { Some(val) }
}

fn null_v() -> serde_json::Value {
    serde_json::Value::Null
}

fn inner_set_cell(gid: &str, row: i64, col: i64, v: &str) -> String {
    serde_json::json!([
        [gid, row, row + 1, col, col + 1],
        [OP_SET_CELL, 3, [2, v], null_v(), null_v(), 0],
        [null_v(), [[null_v(), 513, [0], null_v(), null_v(), null_v(), null_v(), null_v(), null_v(), null_v(), null_v(), 0]]]
    ])
    .to_string()
}

/// Build the inner add-sheet command string (gid + name + default 1000x26 dims).
fn inner_add_sheet(gid: &str, name: &str) -> String {
    serde_json::json!([
        1, 0, gid,
        [[
            [0, 0, name],
            [2, 0, null_v(), null_v(), 0],
            [3, 0, null_v(), null_v(), null_v(), 0],
            [4, 0, null_v(), null_v(), null_v(), null_v(), 0],
            [5, 0, null_v(), null_v(), null_v(), null_v(), null_v(), 0],
            [6, 0, null_v(), null_v(), null_v(), null_v(), null_v(), null_v(), 0]
        ]],
        1000, 26
    ])
    .to_string()
}

/// Build the inner sheet-index positioner command string.
fn inner_add_index(index: i64) -> String {
    serde_json::json!([[[[4, 0, null_v(), null_v(), index]]]]).to_string()
}

/// Build the outer compound add-sheet command value.
fn add_sheet_command(gid: &str, name: &str, index: i64) -> serde_json::Value {
    serde_json::json!([
        OP_ADD_SHEET,
        [
            [OP_ADD_INNER, inner_add_sheet(gid, name)],
            [OP_ADD_INDEX, inner_add_index(index)]
        ]
    ])
}

/// Build the outer set-cell command value.
fn set_cell_command(gid: &str, row: i64, col: i64, v: &str) -> serde_json::Value {
    serde_json::json!([OP_BUNDLE, inner_set_cell(gid, row, col, v)])
}

fn build_bundles(sid: &str, req_id: i64, commands: Vec<serde_json::Value>) -> String {
    serde_json::json!([{ "commands": commands, "sid": sid, "reqId": req_id }]).to_string()
}

fn multipart(fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    let boundary = format!("----rustaddsheet{:x}", now_nanos());
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

    // Use a unique-ish gid + name so re-running the probe is idempotent-ish; if
    // the tab already exists the add-sheet command fails, so probe a fresh name.
    let unique = now_nanos() % 1_000_000;
    let new_gid = format!("{}", 970000 + unique as i64 % 20000);
    let new_name = format!("Probe{}", unique);

    // --- STEP 1: GET /edit -> cookies + revision + sid -----------------------
    let edit_url = format!("https://docs.google.com/spreadsheets/d/{SHEET_ID}/edit");
    let html = agent
        .get(&edit_url)
        .call()
        .expect("GET /edit")
        .into_string()
        .expect("read /edit body");
    let rev = parse_revision(&html).expect("parse revision");
    let sid = parse_sid(&html).expect("parse sid");
    println!("STEP 1 OK: revision={rev} sid={sid}");
    println!("Adding tab name={new_name} gid={new_gid}");

    // --- STEP 2: POST /save: add-sheet + set-cell into the new tab, one bundle.
    let probe_value = format!("addsheet-cell-{unique}");
    let commands = vec![
        add_sheet_command(&new_gid, &new_name, 3),
        set_cell_command(&new_gid, 0, 0, &probe_value),
    ];
    let bundles = build_bundles(&sid, 0, commands);
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

    // --- VERIFY: gviz CSV of the NEW tab by name should NOT be an error doc. --
    let csv_url = format!(
        "https://docs.google.com/spreadsheets/d/{SHEET_ID}/gviz/tq?tqx=out:csv&sheet={new_name}"
    );
    let mut found = false;
    for attempt in 0..8 {
        let resp = agent.get(&csv_url).call();
        match resp {
            Ok(r) => {
                let status = r.status();
                let csv = r.into_string().unwrap_or_default();
                println!("VERIFY attempt {attempt}: status={status} body={csv:?}");
                if csv.contains(&probe_value) {
                    println!("VERIFY OK: new tab exists AND carries our cell");
                    found = true;
                    break;
                }
                // Even an empty body (no error doc) means the tab exists.
                if status == 200 && !csv.contains("error") && !csv.contains("Invalid") {
                    // Keep polling for the cell to propagate, but note tab existence.
                }
            }
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                println!("VERIFY attempt {attempt}: HTTP {code} body={text:?}");
            }
            Err(e) => println!("VERIFY attempt {attempt}: transport error {e}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
    if !found {
        panic!("VERIFY FAILED: cell {probe_value:?} not found in new tab CSV after retries");
    }
    println!("\nALL STEPS PASSED: anonymous add-sheet + set-cell landed on the live sheet.");
    println!("(New tab '{new_name}' gid={new_gid} created — leftover, like Sheet2.)");
}
