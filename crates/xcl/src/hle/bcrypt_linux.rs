//! Linux CNG randomness shim (Step 82): the `bcrypt.dll` RNG triple.
//!
//! The title's nonce helper (`game_scarlett_master.exe+0x216b40b`) calls
//! `BCryptOpenAlgorithmProvider(&h, L"RNG", NULL, 0)`, `BCryptGenRandom(h, buf, 52, 0)`
//! and `BCryptCloseAlgorithmProvider(h, 0)`, five times per run. `bcrypt.dll` is a
//! Windows system DLL with no Linux twin, so the auto-stub returned success without
//! writing the handle or the buffer — the "random" nonce stayed stale caller memory.
//!
//! The shim serves the RNG algorithm only and sources bytes from the same OS entropy
//! call as `std::_Random_device`/`CoCreateGuid` (`crt_linux::fill_os_entropy`), never
//! from a constant or a clock alone. Other CNG algorithms are genuinely unsupported:
//! `STATUS_NOT_SUPPORTED`, not a zero-filled buffer.

#![allow(non_snake_case)]

use crate::hle::crt_linux::fill_os_entropy;

const STATUS_SUCCESS: i32 = 0;
const STATUS_INVALID_HANDLE: i32 = 0xC000_0008u32 as i32;
const STATUS_INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;
const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BBu32 as i32;

/// `BCRYPT_USE_SYSTEM_PREFERRED_RNG`: `BCryptGenRandom` accepts a NULL algorithm
/// handle when this flag is set.
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

/// Opaque provider cookie. The shim keeps no per-algorithm state, so every `Open`
/// hands back the same non-null value and `Close` recognises exactly that value.
const PROVIDER_HANDLE: usize = 0x4243_5259; // "BCRY" little-endian

/// Compare a UTF-16 algorithm identifier against ASCII `s`, case-insensitively.
/// Reads at most 64 units and treats a missing terminator as "unknown", so a
/// malformed identifier can never trigger an unbounded read.
fn utf16_eq_ascii_ci(p: *const u16, s: &str) -> bool {
    let mut bytes = s.bytes();
    for i in 0..64 {
        let unit = unsafe { *p.add(i) };
        if unit == 0 {
            return bytes.next().is_none();
        }
        if unit > 0x7f {
            return false;
        }
        match bytes.next() {
            Some(b) if (unit as u8).eq_ignore_ascii_case(&b) => {}
            _ => return false,
        }
    }
    false
}

/// `NTSTATUS BCryptOpenAlgorithmProvider(BCRYPT_ALG_HANDLE *phAlgorithm,
/// LPCWSTR pszAlgId, LPCWSTR pszImplementation, ULONG dwFlags)` — accept the random
/// number generator (`RNG` / `System preferred RNG`, case-insensitive) and hand back
/// the opaque provider cookie. `pszImplementation`/`dwFlags` are ignored: the shim
/// serves one provider regardless of the requested implementation name.
pub unsafe extern "win64" fn BCryptOpenAlgorithmProvider(
    ph_algorithm: *mut usize,
    alg_id: *const u16,
    _implementation: *const u16,
    _flags: u32,
) -> i32 {
    if ph_algorithm.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    *ph_algorithm = 0;
    if alg_id.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    if utf16_eq_ascii_ci(alg_id, "RNG") || utf16_eq_ascii_ci(alg_id, "System preferred RNG") {
        *ph_algorithm = PROVIDER_HANDLE;
        STATUS_SUCCESS
    } else {
        STATUS_NOT_SUPPORTED
    }
}

/// `NTSTATUS BCryptGenRandom(BCRYPT_ALG_HANDLE hAlgorithm, PUCHAR pbBuffer,
/// ULONG cbBuffer, ULONG dwFlags)` — fill the caller's buffer from the OS entropy
/// source and report success. A NULL handle is accepted only with
/// `BCRYPT_USE_SYSTEM_PREFERRED_RNG`; handles this shim did not issue and NULL
/// buffers with a non-zero length are `STATUS_INVALID_HANDLE` /
/// `STATUS_INVALID_PARAMETER` respectively.
pub unsafe extern "win64" fn BCryptGenRandom(
    h_algorithm: usize,
    pb_buffer: *mut u8,
    cb_buffer: u32,
    dw_flags: u32,
) -> i32 {
    let handle_ok = if h_algorithm == 0 {
        dw_flags & BCRYPT_USE_SYSTEM_PREFERRED_RNG != 0
    } else {
        h_algorithm == PROVIDER_HANDLE
    };
    if !handle_ok {
        return STATUS_INVALID_HANDLE;
    }
    if cb_buffer == 0 {
        return STATUS_SUCCESS;
    }
    if pb_buffer.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    // `fill_os_entropy` always writes the buffer (clock mix last), so the nonce can
    // never stay stale caller memory even when getrandom and /dev/urandom both fail.
    fill_os_entropy(std::slice::from_raw_parts_mut(pb_buffer, cb_buffer as usize));
    STATUS_SUCCESS
}

/// `NTSTATUS BCryptCloseAlgorithmProvider(BCRYPT_ALG_HANDLE hAlgorithm, ULONG
/// dwFlags)` — succeed for the cookie `Open` issued; every other handle is
/// `STATUS_INVALID_HANDLE`.
pub unsafe extern "win64" fn BCryptCloseAlgorithmProvider(h_algorithm: usize, _flags: u32) -> i32 {
    if h_algorithm == PROVIDER_HANDLE {
        STATUS_SUCCESS
    } else {
        STATUS_INVALID_HANDLE
    }
}

pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("BCryptOpenAlgorithmProvider", BCryptOpenAlgorithmProvider as usize),
        ("BCryptGenRandom", BCryptGenRandom as usize),
        ("BCryptCloseAlgorithmProvider", BCryptCloseAlgorithmProvider as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[test]
    fn open_accepts_rng_names_and_sets_a_handle() {
        for alg in ["RNG", "rng", "System preferred RNG", "SYSTEM PREFERRED RNG"] {
            let name = wide(alg);
            let mut h = 0usize;
            unsafe {
                assert_eq!(
                    BCryptOpenAlgorithmProvider(&mut h, name.as_ptr(), std::ptr::null(), 0),
                    STATUS_SUCCESS,
                    "{alg}"
                );
                assert_ne!(h, 0, "{alg} must hand back a non-null handle");
                assert_eq!(BCryptCloseAlgorithmProvider(h, 0), STATUS_SUCCESS, "{alg}");
            }
        }
    }

    #[test]
    fn open_refuses_unknown_algorithm_and_bad_pointers() {
        let sha = wide("SHA256");
        let mut h = 0xdead_usize;
        unsafe {
            assert_eq!(
                BCryptOpenAlgorithmProvider(&mut h, sha.as_ptr(), std::ptr::null(), 0),
                STATUS_NOT_SUPPORTED
            );
            assert_eq!(h, 0, "a refused open must not leave a handle");
            assert_eq!(
                BCryptOpenAlgorithmProvider(std::ptr::null_mut(), sha.as_ptr(), std::ptr::null(), 0),
                STATUS_INVALID_PARAMETER
            );
            assert_eq!(
                BCryptOpenAlgorithmProvider(&mut h, std::ptr::null(), std::ptr::null(), 0),
                STATUS_INVALID_PARAMETER
            );
            // No terminator inside the comparison bound: unknown, and no over-read.
            let unterminated = vec![b'X' as u16; 96];
            assert_eq!(
                BCryptOpenAlgorithmProvider(&mut h, unterminated.as_ptr(), std::ptr::null(), 0),
                STATUS_NOT_SUPPORTED
            );
        }
    }

    #[test]
    fn gen_random_fills_the_callers_buffer() {
        let name = wide("RNG");
        let mut h = 0usize;
        unsafe {
            assert_eq!(
                BCryptOpenAlgorithmProvider(&mut h, name.as_ptr(), std::ptr::null(), 0),
                STATUS_SUCCESS
            );
            let mut a = [0u8; 52];
            let mut b = [0u8; 52];
            assert_eq!(BCryptGenRandom(h, a.as_mut_ptr(), a.len() as u32, 0), STATUS_SUCCESS);
            assert_eq!(BCryptGenRandom(h, b.as_mut_ptr(), b.len() as u32, 0), STATUS_SUCCESS);
            assert_ne!(a, b, "two draws must not return the same bytes");
            assert!(a.iter().any(|&x| x != 0), "the buffer must be written");
            assert!(b.iter().any(|&x| x != 0), "the buffer must be written");
            assert_eq!(BCryptCloseAlgorithmProvider(h, 0), STATUS_SUCCESS);

            // NULL handle + BCRYPT_USE_SYSTEM_PREFERRED_RNG is the documented form.
            let mut c = [0u8; 16];
            assert_eq!(
                BCryptGenRandom(0, c.as_mut_ptr(), c.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG),
                STATUS_SUCCESS
            );
            assert!(c.iter().any(|&x| x != 0));
        }
    }

    #[test]
    fn gen_random_rejects_null_buffer_and_foreign_handles() {
        let name = wide("RNG");
        let mut h = 0usize;
        let mut buf = [0u8; 8];
        unsafe {
            assert_eq!(
                BCryptOpenAlgorithmProvider(&mut h, name.as_ptr(), std::ptr::null(), 0),
                STATUS_SUCCESS
            );
            assert_eq!(
                BCryptGenRandom(h, std::ptr::null_mut(), 52, 0),
                STATUS_INVALID_PARAMETER
            );
            // A zero-length fill needs no buffer.
            assert_eq!(BCryptGenRandom(h, std::ptr::null_mut(), 0, 0), STATUS_SUCCESS);
            // NULL without the system-preferred flag, and handles we never issued.
            assert_eq!(BCryptGenRandom(0, buf.as_mut_ptr(), buf.len() as u32, 0), STATUS_INVALID_HANDLE);
            assert_eq!(
                BCryptGenRandom(0xdead_beef, buf.as_mut_ptr(), buf.len() as u32, 0),
                STATUS_INVALID_HANDLE
            );
            assert_eq!(BCryptCloseAlgorithmProvider(0, 0), STATUS_INVALID_HANDLE);
            assert_eq!(BCryptCloseAlgorithmProvider(0xdead_beef, 0), STATUS_INVALID_HANDLE);
        }
    }
}
