//! Linux `ws2_32.dll` name resolution (Step 85): `gethostname`, the `getaddrinfo`
//! family and `freeaddrinfo`.
//!
//! The title's LAN probe (`game_scarlett_master.exe+0x41d3170`) calls
//! `gethostname(buf, 0x100)`, builds `ADDRINFOA{ai_family = AF_INET}` and resolves the
//! returned name; the auto-stub returned 0 with an empty name, so every later socket
//! path stayed dead. This shim answers both honestly through the same local-name policy
//! as the Windows `XCL_NETWORK=0` route (`hle::net_names`): the machine's own name,
//! `localhost`, the loopback literals and numeric addresses resolve through libc;
//! every other name is `WSAHOST_NOT_FOUND` (11001), never a fabricated address.
//!
//! The libc answer is translated into the Windows `ADDRINFOA`/`ADDRINFOEXW` layout
//! before it is handed out: on this host `ai_addr`/`ai_canonname` are swapped and
//! `ai_addrlen` is 32-bit, while the title reads `ai_addr` at offset 0x20, as the
//! disassembly of its consumer (`game_scarlett_master.exe+0x19f1a00`) shows.
//!
//! Only name resolution: `WSAStartup` (ordinal 115) and every socket ordinal stay
//! stubs. No socket is created, connected, or written.

#![allow(non_snake_case)]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::ptr;
use std::sync::{LazyLock, Mutex};

use crate::hle::net_names;

const SOCKET_ERROR: i32 = -1;
const WSAEFAULT: i32 = 10014;
const WSAENETDOWN: i32 = 10050;
const WSAEAFNOSUPPORT: i32 = 10047;
const WSAHOST_NOT_FOUND: i32 = 11001;

// Winsock keeps its own per-thread last error (`WSAGetLastError`), apart from
// `GetLastError`. `gethostname` reports `WSAEFAULT` here rather than in its return.
thread_local! {
    static LAST_ERROR: Cell<i32> = const { Cell::new(0) };
}

fn set_last_error(e: i32) {
    LAST_ERROR.with(|c| c.set(e));
}

pub unsafe extern "win64" fn WSAGetLastError() -> i32 {
    LAST_ERROR.with(Cell::get)
}

pub unsafe extern "win64" fn WSASetLastError(e: i32) {
    set_last_error(e);
}

/// `int gethostname(char *name, int namelen)` — the real host node name,
/// NUL-terminated and truncated to `namelen - 1` bytes. A null buffer or a
/// non-positive length is `SOCKET_ERROR` with `WSAEFAULT`, with nothing written.
pub unsafe extern "win64" fn gethostname(name: *mut u8, namelen: i32) -> i32 {
    if name.is_null() || namelen <= 0 {
        set_last_error(WSAEFAULT);
        return SOCKET_ERROR;
    }
    let Some(host) = net_names::hostname() else {
        set_last_error(WSAENETDOWN);
        return SOCKET_ERROR;
    };
    let n = host.len().min(namelen as usize - 1);
    ptr::copy_nonoverlapping(host.as_ptr(), name, n);
    *name.add(n) = 0;
    set_last_error(0);
    0
}

/// `ADDRINFOA` / `ADDRINFOW` (the wide twin has the same layout). The title reads
/// `ai_addr` at offset 0x20 and `ai_next` at 0x28.
#[repr(C)]
pub struct AddrInfoA {
    ai_flags: i32,
    ai_family: i32,
    ai_socktype: i32,
    ai_protocol: i32,
    ai_addrlen: usize,
    ai_canonname: *mut u8,
    ai_addr: *mut c_void,
    ai_next: *mut AddrInfoA,
}

/// `ADDRINFOEXW` extends the first eight fields with zeroed blob/provider fields.
/// Every node keeps a zeroed 128-byte header, so both shapes read the same at
/// offsets 0x20/0x28 and an `ExW` consumer sees nulls after `ai_next`.
const NODE_HEADER: usize = 128;
const NODE_BYTES: usize = NODE_HEADER + 128;

#[repr(C, align(8))]
struct NodeStore {
    bytes: [u8; NODE_BYTES],
}

/// Lists handed out by this module, keyed by their head node. `freeaddrinfo` only
/// frees a head it issued (a foreign or already-freed pointer is a no-op).
static LISTS: LazyLock<Mutex<HashMap<usize, Vec<Box<NodeStore>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn offline_name(name: &str) -> bool {
    net_names::local_name(name) || name.parse::<std::net::IpAddr>().is_ok()
}

/// Translate a libc list into Windows-layout nodes. Returns the head and the owned
/// storage; the caller frees the libc list and keeps the storage in [`LISTS`].
unsafe fn windows_list(mut src: *mut libc::addrinfo) -> (*mut AddrInfoA, Vec<Box<NodeStore>>) {
    let mut stores = Vec::new();
    let mut head: *mut AddrInfoA = ptr::null_mut();
    let mut tail: *mut AddrInfoA = ptr::null_mut();
    while !src.is_null() {
        let a = &*src;
        let (family, addr_len) = match a.ai_family {
            libc::AF_INET if !a.ai_addr.is_null() => (2i32, 16usize),
            libc::AF_INET6 if !a.ai_addr.is_null() => (23i32, 28usize),
            _ => {
                src = a.ai_next;
                continue;
            }
        };
        let mut store = Box::new(NodeStore { bytes: [0u8; NODE_BYTES] });
        let base = store.bytes.as_mut_ptr();
        let node = base as *mut AddrInfoA;
        let addr = base.add(NODE_HEADER);
        ptr::copy_nonoverlapping(a.ai_addr as *const u8, addr, addr_len);
        // Windows sockaddr family numbers: AF_INET 2 is shared, AF_INET6 is 23.
        *(addr as *mut u16) = family as u16;
        (*node).ai_flags = a.ai_flags;
        (*node).ai_family = family;
        (*node).ai_socktype = a.ai_socktype;
        (*node).ai_protocol = a.ai_protocol;
        (*node).ai_addrlen = addr_len;
        (*node).ai_canonname = ptr::null_mut();
        (*node).ai_addr = addr as *mut c_void;
        (*node).ai_next = ptr::null_mut();
        if head.is_null() {
            head = node;
        } else {
            (*tail).ai_next = node;
        }
        tail = node;
        stores.push(store);
        src = a.ai_next;
    }
    (head, stores)
}

/// Resolve `name` (None: the wildcard node / local addresses) through libc and
/// translate the result. `service` and `hints` are the caller's Windows forms.
unsafe fn resolve(
    name: Option<&str>,
    service: Option<&str>,
    hints: *const AddrInfoA,
    out: *mut *mut AddrInfoA,
) -> i32 {
    if !out.is_null() {
        *out = ptr::null_mut();
    }
    if let Some(n) = name {
        if !offline_name(n) {
            set_last_error(WSAHOST_NOT_FOUND);
            return WSAHOST_NOT_FOUND;
        }
    }

    let mut libc_hints = std::mem::zeroed::<libc::addrinfo>();
    let hints_ptr = if hints.is_null() {
        ptr::null()
    } else {
        let h = &*hints;
        let family = match h.ai_family {
            0 => libc::AF_UNSPEC,
            2 => libc::AF_INET,
            23 => libc::AF_INET6,
            _ => {
                set_last_error(WSAEAFNOSUPPORT);
                return WSAEAFNOSUPPORT;
            }
        };
        // AI_PASSIVE/AI_CANONNAME/AI_NUMERICHOST share the same low bits; the
        // Windows-only flags do not take part in a local lookup.
        libc_hints.ai_flags = h.ai_flags & 0x7;
        libc_hints.ai_family = family;
        libc_hints.ai_socktype = h.ai_socktype;
        libc_hints.ai_protocol = h.ai_protocol;
        &libc_hints as *const libc::addrinfo
    };

    let node_c = match name {
        None => None,
        Some(n) => match CString::new(n.as_bytes()) {
            Ok(c) => Some(c),
            Err(_) => {
                set_last_error(WSAHOST_NOT_FOUND);
                return WSAHOST_NOT_FOUND;
            }
        },
    };
    let service_c = match service {
        None => None,
        Some(s) => match CString::new(s.as_bytes()) {
            Ok(c) => Some(c),
            Err(_) => {
                set_last_error(WSAHOST_NOT_FOUND);
                return WSAHOST_NOT_FOUND;
            }
        },
    };
    let node_ptr = node_c.as_ref().map_or(ptr::null(), |c| c.as_ptr());
    let service_ptr = service_c.as_ref().map_or(ptr::null(), |c| c.as_ptr());

    let mut found: *mut libc::addrinfo = ptr::null_mut();
    let rc = libc::getaddrinfo(node_ptr, service_ptr, hints_ptr, &mut found);
    if rc != 0 {
        set_last_error(WSAHOST_NOT_FOUND);
        return WSAHOST_NOT_FOUND;
    }
    let (head, stores) = windows_list(found);
    libc::freeaddrinfo(found);
    if head.is_null() {
        set_last_error(WSAHOST_NOT_FOUND);
        return WSAHOST_NOT_FOUND;
    }
    if out.is_null() {
        set_last_error(WSAEFAULT);
        return WSAEFAULT;
    }
    LISTS.lock().unwrap().insert(head as usize, stores);
    *out = head;
    set_last_error(0);
    0
}

/// `int getaddrinfo(PCSTR node, PCSTR service, const ADDRINFOA *hints, PADDRINFOA *res)`
pub unsafe extern "win64" fn getaddrinfo(
    node: *const u8,
    service: *const u8,
    hints: *const AddrInfoA,
    out: *mut *mut AddrInfoA,
) -> i32 {
    let name = if node.is_null() { None } else { crate::util::from_ansi_ptr(node) };
    let service = if service.is_null() { None } else { crate::util::from_ansi_ptr(service) };
    resolve(name.as_deref(), service.as_deref(), hints, out)
}

/// `INT GetAddrInfoW(PCWSTR node, PCWSTR service, const ADDRINFOW *hints, PADDRINFOW *res)`
pub unsafe extern "win64" fn GetAddrInfoW(
    node: *const u16,
    service: *const u16,
    hints: *const AddrInfoA,
    out: *mut *mut AddrInfoA,
) -> i32 {
    let name = crate::util::from_wide_ptr(node);
    let service = crate::util::from_wide_ptr(service);
    resolve(name.as_deref(), service.as_deref(), hints, out)
}

/// `INT GetAddrInfoExW(...)` — the synchronous form. The title does not call it; it
/// mirrors `GetAddrInfoW` so an uncalled import is not left as a logging stub.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "win64" fn GetAddrInfoExW(
    node: *const u16,
    service: *const u16,
    _namespace: u32,
    _nsp_id: *const c_void,
    hints: *const c_void,
    out: *mut *mut c_void,
    _timeout: *const c_void,
    _overlapped: *mut c_void,
    _completion: *const c_void,
    _handle: *mut c_void,
) -> i32 {
    let name = crate::util::from_wide_ptr(node);
    let service = crate::util::from_wide_ptr(service);
    resolve(
        name.as_deref(),
        service.as_deref(),
        hints as *const AddrInfoA,
        out as *mut *mut AddrInfoA,
    )
}

/// `void freeaddrinfo(PADDRINFOA res)` — releases a list this module handed out;
/// a null, foreign or already-freed pointer is a no-op.
pub unsafe extern "win64" fn freeaddrinfo(head: *mut AddrInfoA) {
    if head.is_null() {
        return;
    }
    LISTS.lock().unwrap().remove(&(head as usize));
}

pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("gethostname", gethostname as usize),
        ("getaddrinfo", getaddrinfo as usize),
        ("GetAddrInfoW", GetAddrInfoW as usize),
        ("GetAddrInfoExW", GetAddrInfoExW as usize),
        ("freeaddrinfo", freeaddrinfo as usize),
        ("WSAGetLastError", WSAGetLastError as usize),
        ("WSASetLastError", WSASetLastError as usize),
    ]
}

/// The title imports `gethostname` as ws2_32 ordinal 57.
pub fn ordinals() -> Vec<(u16, &'static str)> {
    vec![(57, "gethostname")]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hints(family: i32) -> AddrInfoA {
        AddrInfoA {
            ai_flags: 0,
            ai_family: family,
            ai_socktype: 0,
            ai_protocol: 0,
            ai_addrlen: 0,
            ai_canonname: ptr::null_mut(),
            ai_addr: ptr::null_mut(),
            ai_next: ptr::null_mut(),
        }
    }

    #[test]
    fn gethostname_fills_and_truncates_without_overrun() {
        unsafe {
            // The full buffer gets a non-empty NUL-terminated node name.
            let mut full = [0xA5u8; 256];
            assert_eq!(gethostname(full.as_mut_ptr(), full.len() as i32), 0);
            let end = full.iter().position(|&b| b == 0).expect("NUL terminator");
            assert!(end > 0, "the host name must not be empty");

            // A truncated buffer keeps at most len-1 bytes and never writes past it.
            let mut small = [0xA5u8; 32];
            assert_eq!(gethostname(small.as_mut_ptr(), 2), 0);
            assert_eq!(small[1], 0, "truncation leaves room for the terminator");
            assert!(small[2..].iter().all(|&b| b == 0xA5), "no write past len");

            // null / non-positive length: SOCKET_ERROR with WSAEFAULT, no write.
            let mut untouched = [0xA5u8; 8];
            assert_eq!(gethostname(ptr::null_mut(), 8), SOCKET_ERROR);
            assert_eq!(WSAGetLastError(), WSAEFAULT);
            assert_eq!(gethostname(untouched.as_mut_ptr(), 0), SOCKET_ERROR);
            assert_eq!(WSAGetLastError(), WSAEFAULT);
            assert!(untouched.iter().all(|&b| b == 0xA5));
        }
    }

    #[test]
    fn getaddrinfo_resolves_localhost_into_the_windows_layout() {
        unsafe {
            let h = hints(2); // AF_INET
            let mut out: *mut AddrInfoA = ptr::null_mut();
            assert_eq!(getaddrinfo(b"localhost\0".as_ptr(), ptr::null(), &h, &mut out), 0);
            assert!(!out.is_null());
            let head = &*out;
            assert_eq!(head.ai_family, 2, "the caller asked for AF_INET");
            assert_eq!(head.ai_addrlen, 16, "sizeof(sockaddr_in)");
            assert_eq!(head.ai_flags, 0);
            assert!(!head.ai_addr.is_null());
            let sa = head.ai_addr as *const u8;
            assert_eq!((sa as *const u16).read_unaligned(), 2, "sockaddr family");
            assert_eq!(*sa.add(4), 127, "localhost is a loopback address");
            // Every node is walkable in the Windows layout.
            let mut n = head.ai_next;
            while !n.is_null() {
                assert_eq!((*n).ai_family, 2);
                assert_eq!((*n).ai_addrlen, 16);
                assert!(!(*n).ai_addr.is_null());
                n = (*n).ai_next;
            }
            freeaddrinfo(out);
            freeaddrinfo(out); // already freed: no-op, not a double free
            freeaddrinfo(ptr::null_mut());
        }
    }

    #[test]
    fn getaddrinfo_refuses_unknown_names_and_writes_only_the_result_slot() {
        unsafe {
            let h = hints(2);
            let mut slot = [0xdead_usize; 3];
            let out = slot.as_mut_ptr().add(1) as *mut *mut AddrInfoA;
            assert_eq!(
                getaddrinfo(b"no-such-host.invalid\0".as_ptr(), ptr::null(), &h, out),
                WSAHOST_NOT_FOUND
            );
            assert!((*out).is_null(), "*res must be null after a failed lookup");
            assert_eq!(slot[0], 0xdead, "memory before the slot is untouched");
            assert_eq!(slot[2], 0xdead, "memory after the slot is untouched");

            // The wide twins answer the same way; an unknown name is never resolved.
            let mut wide_out: *mut AddrInfoA = ptr::null_mut();
            let unknown = crate::util::wide("no-such-host.invalid");
            assert_eq!(GetAddrInfoW(unknown.as_ptr(), ptr::null(), &h, &mut wide_out), WSAHOST_NOT_FOUND);
            assert!(wide_out.is_null());
            let local = crate::util::wide("localhost");
            assert_eq!(GetAddrInfoW(local.as_ptr(), ptr::null(), &h, &mut wide_out), 0);
            assert!(!wide_out.is_null());
            freeaddrinfo(wide_out);
        }
    }

    #[test]
    fn numeric_names_resolve_without_a_lookup() {
        let h = hints(2);
        unsafe {
            let mut out: *mut AddrInfoA = ptr::null_mut();
            assert_eq!(getaddrinfo(b"127.0.0.1\0".as_ptr(), ptr::null(), &h, &mut out), 0);
            assert!(!out.is_null());
            assert_eq!((*out).ai_family, 2);
            assert_eq!(*((*out).ai_addr as *const u8).add(4), 127);
            freeaddrinfo(out);
        }
    }
}
