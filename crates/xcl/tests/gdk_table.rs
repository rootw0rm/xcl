#![cfg(windows)]
// Windows-only: `xcl::hle` is cfg(windows)-only on this branch, so the
// `xcl::hle::xgameruntime::gdk_table` import below only resolves on Windows.
// The Linux twin (`hle/xgameruntime_api_linux.rs`, same generated table via
// `#[path = "xgameruntime/gdk_table.rs"]`) mirrors these invariants in its
// `gdk_table_invariants_match_the_generated_table` unit tests.
//! Pin the invariants of `gdk_table::API` — the generated GDK thunk table that
//! api.rs maps onto (class, interface, slot). A bad regeneration (duplicate
//! rows, bad sort, out-of-range slots, empty names) must fail loudly here.

use xcl::hle::xgameruntime::gdk_table::API;

const EXPECTED_ROWS: usize = 396;
const EXPECTED_CLASSES: usize = 24;
const EXPECTED_INTERFACES: usize = 34;
const MAX_SLOT: u8 = 83;

/// The table is sorted by (class, interface, slot) — the generator's sort key.
/// The u128 class/interface ids are all 32 hex chars, so a plain string compare
/// on the rendered hex equals a numeric compare.
#[test]
fn rows_are_sorted_by_class_interface_slot() {
    let keys: Vec<(String, String, u8)> = API
        .iter()
        .map(|&(_, c, i, s)| (hex32(c), hex32(i), s))
        .collect();
    let mut expected = keys.clone();
    expected.sort_by(|a, b| (a.0.as_str(), a.1.as_str(), a.2).cmp(&(b.0.as_str(), b.1.as_str(), b.2)));
    assert_eq!(keys, expected, "gdk_table::API is not sorted by (class, interface, slot)");
}

#[test]
fn function_names_are_unique_and_non_empty() {
    let mut seen = std::collections::HashSet::new();
    for &(name, _, _, _) in API {
        assert!(!name.is_empty(), "empty function name");
        assert!(seen.insert(name), "duplicate function name {name}");
    }
}

#[test]
fn no_duplicate_class_interface_slot_triples() {
    let mut seen = std::collections::HashSet::new();
    for &(_, class, interface, slot) in API {
        assert!(seen.insert((class, interface, slot)), "duplicate (class, interface, slot) {class:#x} {interface:#x} {slot}");
    }
}

#[test]
fn row_and_id_counts_are_stable() {
    assert_eq!(API.len(), EXPECTED_ROWS, "expected {EXPECTED_ROWS} rows");
    let classes: std::collections::HashSet<u128> = API.iter().map(|&(_, c, _, _)| c).collect();
    let interfaces: std::collections::HashSet<u128> = API.iter().map(|&(_, _, i, _)| i).collect();
    assert_eq!(classes.len(), EXPECTED_CLASSES, "distinct classes");
    assert_eq!(interfaces.len(), EXPECTED_INTERFACES, "distinct interfaces");
}

#[test]
fn every_slot_is_within_u8_and_the_known_range() {
    for &(_, _, _, slot) in API {
        assert!(slot <= MAX_SLOT, "slot {slot} exceeds known maximum {MAX_SLOT}");
    }
}

#[test]
fn every_class_and_interface_is_non_zero() {
    for &(_, class, interface, _) in API {
        assert!(class != 0, "zero class id");
        assert!(interface != 0, "zero interface id");
    }
}

/// The generated file only ever holds lowercase 32-hex-char ids between
/// `0x` and the tuple — anything else (leading zeros, uppercase, non-hex)
/// would mean a hand-edited or badly regenerated table.
#[test]
fn source_format_is_32_lowercase_hex_chars() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/hle/xgameruntime/gdk_table.rs");
    let src = std::fs::read_to_string(&path).unwrap();
    let mut rows_seen = 0usize;
    for line in src.lines() {
        let line = line.trim();
        if !line.starts_with("(\"X") {
            continue;
        }
        rows_seen += 1;
        let inner = line.strip_prefix('(').and_then(|l| l.strip_suffix(',')).and_then(|l| l.strip_suffix(')'))
            .unwrap_or_else(|| panic!("unparseable row line: {line}"));
        let mut parts = inner.split(',');
        parts.next().unwrap();
        let class = parts.next().unwrap().trim();
        let interface = parts.next().unwrap().trim();
        let slot = parts.next().unwrap().trim();
        assert!(class.starts_with("0x") && class.len() == 34 && class[2..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "bad class id {class}");
        assert!(interface.starts_with("0x") && interface.len() == 34 && interface[2..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "bad interface id {interface}");
        assert!(slot.chars().all(|c| c.is_ascii_digit()), "bad slot {slot}");
    }
    assert_eq!(rows_seen, EXPECTED_ROWS, "source row count drifted from {EXPECTED_ROWS}");
}

fn hex32(v: u128) -> String {
    format!("{v:032x}")
}
