#![cfg(windows)]
// Windows-only: `xcl::hle` is cfg(windows)-only on this branch, so the
// `xcl::hle::xgameruntime::xsign` import below only resolves on Windows.
//! Black-box contract tests for the XSTS v1 offline-signing module.
//!
//! These assert the public API of `xcl::hle::xgameruntime::xsign` end-to-end:
//! the offline token format, the FILETIME shape of `current_filetime`,
//! base64 encoding through the public API and the XSTS v1 header layout that
//! `sign_request[_at]` writes for `XUserGetTokenAndSignature*` thunks.

use std::time::{SystemTime, UNIX_EPOCH};

use xcl::hle::xgameruntime::xsign;

const UNIX_TO_FILETIME_SECS: u64 = 11_644_473_600;
/// One hour in FILETIME ticks (100-ns intervals): 3_600 secs * 10_000_000 ticks/sec.
const ONE_HOUR_TICKS: u64 = 36_000_000_000;

/// True when `ft` is within one hour of `now`.
fn within_one_hour(ft: u64, now: u64) -> bool {
    ft.abs_diff(now) <= ONE_HOUR_TICKS
}

/// Independent RFC 4648 base64 decoder (test-only, not the module's own).
fn base64_decode(input: &str) -> Vec<u8> {
    fn val(b: u8) -> u32 {
        match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("invalid base64 byte: {:?}", b as char),
        }
    }
    let bytes = input.as_bytes();
    assert_eq!(bytes.len() % 4, 0, "base64 length must be a multiple of 4");
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks_exact(4) {
        let a = val(chunk[0]);
        let b = val(chunk[1]);
        out.push(((a << 2) | (b >> 4)) as u8);
        if chunk[2] != b'=' {
            let c = val(chunk[2]);
            out.push((((b & 0x0F) << 4) | (c >> 2)) as u8);
            if chunk[3] != b'=' {
                let d = val(chunk[3]);
                out.push((((c & 0x03) << 6) | d) as u8);
            }
        }
    }
    out
}

#[test]
fn offline_token_has_xbl3_xcl_prefix_and_hex_user_id() {
    let token = xsign::offline_token();
    let prefix = "XBL3.0 x=xcl;local-";
    assert!(token.starts_with(prefix), "token {token:?} must start with {prefix:?}");
    let hex = &token[prefix.len()..];
    assert_eq!(hex.len(), 16, "token {token:?} must end with 16 hex chars");
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "token {token:?} hex suffix must be hex");
    // The hex suffix must round-trip the configured user id (default: 0x0009_0000_0000_0001).
    let configured = xcl::state::config().user_id;
    let decoded = u64::from_str_radix(hex, 16).expect("hex suffix must parse");
    assert_eq!(decoded, configured, "token hex suffix must equal config().user_id");
}

#[test]
fn current_filetime_is_plausible_now() {
    let ft = xsign::current_filetime();
    // FILETIME is 100-ns ticks since 1601-01-01. 2024-01-01T00:00:00Z is
    // (UNIX_TO_FILETIME_SECS + 1_704_067_200) * 10_000_000.
    // Use a loose lower bound so the test stays stable across years and timezones.
    let utc_now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let filetime_min = (UNIX_TO_FILETIME_SECS + 1_600_000_000) * 10_000_000; // ~2020-09-06
    assert!(ft >= filetime_min, "filetime {ft} too small (sanity floor {filetime_min})");
    // Must not claim a time more than an hour in the future or more than an hour in the past.
    let now_filetime = (UNIX_TO_FILETIME_SECS + utc_now_secs) * 10_000_000;
    assert!(
        within_one_hour(ft, now_filetime),
        "filetime {ft} deviates from now {now_filetime} by more than 1h"
    );
    // As a second monotonic-sanity check, a second reading is not less than the first.
    let ft2 = xsign::current_filetime();
    assert!(ft2 >= ft, "filetime went backwards: {ft} -> {ft2}");
    // Guard the bound itself: a timestamp more than an hour off must fail the check.
    assert!(!within_one_hour(now_filetime + ONE_HOUR_TICKS + 1, now_filetime));
    assert!(!within_one_hour(now_filetime - (ONE_HOUR_TICKS + 1), now_filetime));
}

#[test]
fn base64_encode_matches_rfc4648_and_decodes_roundtrip() {
    // RFC 4648 test vectors.
    let vectors: [(&[u8], &str); 7] = [
        (b"", ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg=="),
        (b"fooba", "Zm9vYmE="),
        (b"foobar", "Zm9vYmFy"),
    ];
    for (plain, encoded) in vectors {
        assert_eq!(xsign::base64_encode(plain), encoded, "encode {plain:?}");
        assert_eq!(base64_decode(encoded), plain, "round trip {plain:?}");
    }
    // One extra round-trip case: a body longer than one 3-byte group.
    let body = b"{\"ok\":true}";
    let encoded = xsign::base64_encode(body);
    assert_eq!(base64_decode(&encoded), body);
}

#[test]
fn sign_request_at_produces_parseable_xsts_v1_header() {
    let fixed_filetime: u64 = 0x01DB_1234_5678_9A_BC;
    let encoded = xsign::sign_request_at(fixed_filetime, "POST", "https://social.xboxlive.com/users/me/people", "XBL3.0 x=xcl;local-0000000000000001", b"{\"ok\":true}")
        .expect("sign_request_at must succeed");
    let raw = base64_decode(&encoded);
    // 4 (version) + 8 (filetime) + 64 (raw ECDSA P-256 signature) = 76 bytes.
    assert_eq!(raw.len(), 76, "XSTS v1 header must be 76 bytes, got {}", raw.len());
    assert_eq!(&raw[0..4], &1u32.to_be_bytes(), "version must be u32 BE == 1");
    assert_eq!(&raw[4..12], &fixed_filetime.to_be_bytes(), "filetime must be u64 BE == the fixed value");
    let sig = &raw[12..];
    assert!(sig.iter().any(|&b| b != 0), "signature must not be all zeros");
}

#[test]
fn sign_request_returns_ok_and_has_xsts_v1_shape() {
    let encoded = xsign::sign_request("POST", "https://userstats.xboxlive.com/batch", "XBL3.0 x=xcl;local-0000000000000001", b"{\"ok\":true}")
        .expect("sign_request must succeed offline");
    let raw = base64_decode(&encoded);
    assert_eq!(raw.len(), 76, "XSTS v1 header must be 76 bytes, got {}", raw.len());
    assert_eq!(&raw[0..4], &1u32.to_be_bytes(), "version must be u32 BE == 1");
    let filetime = u64::from_be_bytes(raw[4..12].try_into().unwrap());
    let now_filetime = (UNIX_TO_FILETIME_SECS + SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()) * 10_000_000;
    assert!(
        within_one_hour(filetime, now_filetime),
        "signed filetime {filetime} must be within 1h of now {now_filetime}"
    );
    // Guard the bound itself: a timestamp more than an hour off must fail the check.
    assert!(!within_one_hour(now_filetime + ONE_HOUR_TICKS + 1, now_filetime));
    assert!(!within_one_hour(now_filetime - (ONE_HOUR_TICKS + 1), now_filetime));
}
