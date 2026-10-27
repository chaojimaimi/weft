//! PLAN_v1153 §2.2 防线 1 — `occ_mutation_sites`: source-scan tripwire.
//!
//! Scans every `grid/` + `vt/` source file for `Row.cells` mutation shapes
//! (`cells[..] =`, `cells.push/insert/truncate/resize/drain/fill/extend/
//! swap/copy_within/split_off`, `cells.iter_mut()`, `&mut ..cells`,
//! whole-Row `viewport[..] =` assignments, `mark_dirty(` calls) and asserts
//! every hit sits in the classified inventory below. A NEW mutation site
//! that was not classified turns this test red — the developer must either
//! hang `cell_written` / `recompute_tail` on it or document why occ is
//! untouched (the five categories mirror PLAN_v1153 §2.2).
//!
//! Inventory key = (file, trimmed line text): resilient to line-number
//! drift, sensitive to new code shapes. `row_extras.rs` is excluded — its
//! `cells` field is the `RowExtras` BTreeMap, not the row cell vector.
//!
//! Shape coverage (v1.15.3 reviewer P2-1 rewrite — the earlier "all
//! production writes go through &mut funnels" claim was false for the
//! indexed forms): scanned shapes are direct indexed assignment, indexed
//! method mutations (`.reset()`/`.set_bg()`/`.set_fg()`), indexed compound
//! field writes (`.flags |= X`), Vec bulk ops, `iter_mut`, `&mut` borrows,
//! viewport row swaps, and `mark_dirty` calls. Remaining blind spot: plain
//! field writes like `cells[i].character = c` with no operator — no live
//! production site uses that form today; the property tests + byte-goldens
//! (defense lines 2/3) are the backstop if one ever appears.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::occ_sites_inventory::CLASSIFIED;

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).expect("scan root must exist");
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_row_cells_mutation_site_is_classified() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src.join("grid"), &mut files);
    collect_rs_files(&src.join("vt"), &mut files);
    assert!(
        files.len() >= 40,
        "scan scope collapsed — grid/+vt/ source tree moved?"
    );

    let patterns: Vec<regex::Regex> = [
        r"cells\[[^\]]*\]\s*=[^=]",
        // v1.15.3 reviewer P2-1: indexed-method mutations (`cells[c].reset()`)
        // and indexed compound field writes (`cells[c].flags |= X`) are live
        // production shapes — a mutant injected in this form survived the
        // funnel-only pattern set, so they are scanned explicitly now.
        r"cells\[[^\]]*\]\s*\.\s*(reset|set_bg|set_fg)\s*\(",
        r"cells\[[^\]]*\]\s*\.\s*[a-z_]+\s*(\|=|&=)",
        r"cells\.(push|insert|truncate|resize|drain|fill|extend|swap|copy_within|split_off)\(",
        r"cells\.iter_mut\(",
        r"&mut\s+[^;]*cells",
        r"viewport\[[^\]]*\]\s*=",
        r"mark_dirty\(",
    ]
    .iter()
    .map(|p| regex::Regex::new(p).expect("pattern compiles"))
    .collect();

    // row_extras.rs: its `cells` field is the RowExtras BTreeMap, not the
    // row cell vector. This file + the inventory module: their contents ARE
    // the scan patterns / classification strings themselves.
    let excluded_files: HashSet<String> = [
        "grid/row_extras.rs".to_string(),
        "grid/occ_mutation_sites_tests.rs".to_string(),
        "grid/occ_sites_inventory.rs".to_string(),
    ]
    .into_iter()
    .collect();
    let excluded_hints = ["fn mark_dirty", "fn cell_written", "fn recompute"];

    let mut unclassified: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    for file in &files {
        let rel = file
            .strip_prefix(&src)
            .expect("under src")
            .to_string_lossy()
            .replace('\\', "/");
        if excluded_files.contains(&rel) {
            continue;
        }
        let content = fs::read_to_string(file).expect("source readable");
        for raw in content.lines() {
            let trimmed = raw.trim();
            if trimmed.is_empty() || excluded_hints.iter().any(|h| trimmed.contains(h)) {
                continue;
            }
            if !patterns.iter().any(|p| p.is_match(trimmed)) {
                continue;
            }
            scanned += 1;
            if !CLASSIFIED
                .iter()
                .any(|(f, line, _)| *f == rel && *line == trimmed)
            {
                unclassified.push(format!("{rel}: {trimmed}"));
            }
        }
    }

    assert!(
        scanned >= CLASSIFIED.len(),
        "scan saw fewer sites than classified entries ({scanned} < {}) — pattern set drifted",
        CLASSIFIED.len()
    );
    assert!(
        unclassified.is_empty(),
        "unclassified Row.cells mutation site(s) — hang cell_written / recompute_tail on them, \
         or add them to the CLASSIFIED inventory with the right category:\n  {}",
        unclassified.join("\n  ")
    );
}
