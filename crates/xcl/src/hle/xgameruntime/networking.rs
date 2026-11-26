//! XNetworking – GDK networking helpers (`XNetworking.h`, public GDK contract).
//!
//! Covers UDP multiplayer port queries, per-URL HTTP security information (UTF-8 and UTF-16),
//! server certificate verification (real Windows CryptoAPI SSL chain + SHA-1 thumbprint pin
//! checking, with plain-HTTP loopback allowance for local servers), and host connectivity
//! hints via `iphlpapi.dll`.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use super::taskqueue::PORT_COMPLETION;
use super::xasync::{begin_with, submit_callback, XAsyncBlock, XAsyncGetResult, XAsyncGetResultSize};
use crate::util::{hresult_from_win32, wide, E_INVALIDARG, HRESULT, S_OK};

/// Default Xbox Live multiplayer UDP port.
pub const DEFAULT_UDP_PORT: u16 = 3074;

/// `WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3`.
pub const DEFAULT_TLS_FLAGS: u32 = 0x0000_0800 | 0x0000_2000;

const CERT_E_UNTRUSTEDROOT: HRESULT = 0x800B_0109u32 as i32;
const WINHTTP_OPTION_URL: u32 = 34;
const WINHTTP_OPTION_SERVER_CERT_CONTEXT: u32 = 78;
const CERT_SHA1_HASH_PROP_ID: u32 = 3;
const CERT_CHAIN_POLICY_SSL: usize = 4;
const AUTHTYPE_SERVER: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct XNetworkingThumbprint {
    pub thumbprint_type: u32,
    pub thumbprint_buffer_byte_count: usize,
    pub thumbprint_buffer: *const u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct XNetworkingSecurityInformation {
    pub enabled_http_security_protocol_flags: u32,
    pub thumbprint_count: usize,
    pub thumbprints: *const XNetworkingThumbprint,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XNetworkingConnectivityHint {
    pub connectivity_level: u32,
    pub connectivity_cost: u32,
    pub iana_interface_type: u32,
    pub network_initialized: bool,
    pub approaching_data_limit: bool,
    pub over_data_limit: bool,
    pub roaming: bool,
}

static UDP_PORT_ID: u8 = 0;
static SEC_INFO_ID: u8 = 0;
static SEC_INFO_UTF16_ID: u8 = 0;
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// The one notification a register function sends right away, with the current value.
struct InitialNotification<T> {
    cb: usize,
    ctx: usize,
    value: T,
}

/// Queue `call(record)` on `queue`'s completion port; the record is freed by `call`, or here
/// when the queue does not take it.
unsafe fn notify_initial<T>(queue: *mut c_void, ctx: *mut c_void, cb: usize, value: T, call: usize, what: &str) {
    let rec = Box::into_raw(Box::new(InitialNotification { cb, ctx: ctx as usize, value }));
    let hr = submit_callback(queue, PORT_COMPLETION, rec as *mut c_void, call);
    if hr < 0 {
        drop(Box::from_raw(rec));
        crate::xlog_warn!("XNetworking: the initial {what} callback could not be queued ({:#x})", hr as u32);
    }
}

// ---------------------------------------------------------------------------------------
// UDP multiplayer port
// ---------------------------------------------------------------------------------------

pub unsafe extern "system" fn XNetworkingQueryPreferredLocalUdpMultiplayerPort(port: *mut u16) -> HRESULT {
    if port.is_null() {
        return E_INVALIDARG;
    }
    *port = DEFAULT_UDP_PORT;
    S_OK
}

pub unsafe extern "system" fn XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync(b: *mut XAsyncBlock) -> HRESULT {
    begin_with(b, &UDP_PORT_ID as *const u8 as usize, || Ok(DEFAULT_UDP_PORT.to_le_bytes().to_vec()))
}

pub unsafe extern "system" fn XNetworkingQueryPreferredLocalUdpMultiplayerPortAsyncResult(
    b: *mut XAsyncBlock,
    port: *mut u16,
) -> HRESULT {
    if port.is_null() {
        return E_INVALIDARG;
    }
    *port = 0;
    XAsyncGetResult(b, &UDP_PORT_ID as *const u8 as *const c_void, std::mem::size_of::<u16>(), port as *mut c_void, std::ptr::null_mut())
}

unsafe extern "system" fn udp_port_changed_call(rec: *mut c_void, canceled: bool) {
    let r = Box::from_raw(rec as *mut InitialNotification<u16>);
    if !canceled {
        let f: unsafe extern "system" fn(*mut c_void, u16) = std::mem::transmute(r.cb);
        f(r.ctx as *mut c_void, r.value);
    }
}

/// The port never changes, but registering "always fires an initial notification callback"
/// (GDK docs), and a title may wait for it.
pub unsafe extern "system" fn XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged(
    queue: *mut c_void,
    ctx: *mut c_void,
    cb: usize,
    token: *mut u64,
) -> HRESULT {
    if token.is_null() || cb == 0 {
        return E_INVALIDARG;
    }
    *token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    notify_initial(queue, ctx, cb, DEFAULT_UDP_PORT, udp_port_changed_call as usize, "UDP port");
    S_OK
}

pub unsafe extern "system" fn XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged(_token: u64, _wait: bool) -> bool {
    true
}

// ---------------------------------------------------------------------------------------
// Per-URL HTTP security information (TLS 1.2 / 1.3, no certificate pinning)
// ---------------------------------------------------------------------------------------

/// Serialize the default `XNetworkingSecurityInformation` into bytes without reading
/// uninitialized `#[repr(C)]` struct padding.
///
/// On x86_64 Windows, all-zero bytes represent both `thumbprint_count == 0` and a null
/// `thumbprints` pointer, as well as deterministic zero padding between fields.
fn default_security_info_bytes() -> Vec<u8> {
    let mut out = vec![0u8; std::mem::size_of::<XNetworkingSecurityInformation>()];
    let off = std::mem::offset_of!(XNetworkingSecurityInformation, enabled_http_security_protocol_flags);
    out[off..off + std::mem::size_of::<u32>()].copy_from_slice(&DEFAULT_TLS_FLAGS.to_le_bytes());
    out
}

pub unsafe extern "system" fn XNetworkingQuerySecurityInformationForUrlAsync(
    url: *const u8,
    b: *mut XAsyncBlock,
) -> HRESULT {
    if url.is_null() {
        return E_INVALIDARG;
    }
    begin_with(b, &SEC_INFO_ID as *const u8 as usize, || Ok(default_security_info_bytes()))
}

pub unsafe extern "system" fn XNetworkingQuerySecurityInformationForUrlAsyncResultSize(
    b: *mut XAsyncBlock,
    size: *mut usize,
) -> HRESULT {
    XAsyncGetResultSize(b, size)
}

/// Note the GDK parameter order: `buffer_used` precedes `buffer` and `out_ptr`.
pub unsafe extern "system" fn XNetworkingQuerySecurityInformationForUrlAsyncResult(
    b: *mut XAsyncBlock,
    buffer_size: usize,
    buffer_used: *mut usize,
    buffer: *mut u8,
    out_ptr: *mut *mut XNetworkingSecurityInformation,
) -> HRESULT {
    security_info_result(&SEC_INFO_ID as *const u8 as *const c_void, b, buffer_size, buffer_used, buffer, out_ptr)
}

pub unsafe extern "system" fn XNetworkingQuerySecurityInformationForUrlUtf16Async(
    url: *const u16,
    b: *mut XAsyncBlock,
) -> HRESULT {
    if url.is_null() {
        return E_INVALIDARG;
    }
    begin_with(b, &SEC_INFO_UTF16_ID as *const u8 as usize, || Ok(default_security_info_bytes()))
}

pub unsafe extern "system" fn XNetworkingQuerySecurityInformationForUrlUtf16AsyncResultSize(
    b: *mut XAsyncBlock,
    size: *mut usize,
) -> HRESULT {
    XAsyncGetResultSize(b, size)
}

pub unsafe extern "system" fn XNetworkingQuerySecurityInformationForUrlUtf16AsyncResult(
    b: *mut XAsyncBlock,
    buffer_size: usize,
    buffer_used: *mut usize,
    buffer: *mut u8,
    out_ptr: *mut *mut XNetworkingSecurityInformation,
) -> HRESULT {
    security_info_result(&SEC_INFO_UTF16_ID as *const u8 as *const c_void, b, buffer_size, buffer_used, buffer, out_ptr)
}

unsafe fn security_info_result(
    identity: *const c_void,
    b: *mut XAsyncBlock,
    buffer_size: usize,
    buffer_used: *mut usize,
    buffer: *mut u8,
    out_ptr: *mut *mut XNetworkingSecurityInformation,
) -> HRESULT {
    if !out_ptr.is_null() {
        *out_ptr = std::ptr::null_mut();
    }
    if !buffer_used.is_null() {
        *buffer_used = 0;
    }
    if out_ptr.is_null() || buffer.is_null() {
        return E_INVALIDARG;
    }
    let mut used = 0usize;
    let hr = XAsyncGetResult(b, identity, buffer_size, buffer as *mut c_void, &mut used);
    if !buffer_used.is_null() {
        *buffer_used = used;
    }
    if hr >= 0 {
        *out_ptr = buffer as *mut XNetworkingSecurityInformation;
    }
    hr
}

// ---------------------------------------------------------------------------------------
// Server certificate verification (WinHTTP + CryptoAPI)
// ---------------------------------------------------------------------------------------

#[repr(C)]
struct CertContext {
    encoding_type: u32,
    encoded: *const u8,
    encoded_len: u32,
    cert_info: *const c_void,
    cert_store: *mut c_void,
}

#[repr(C)]
struct CertTrustStatus {
    error_status: u32,
    info_status: u32,
}

/// `CERT_CHAIN_CONTEXT` up to the fields read here.
#[repr(C)]
struct CertChainContext {
    cb_size: u32,
    trust_status: CertTrustStatus,
    chain_count: u32,
    chains: *const *const CertSimpleChain,
}

/// `CERT_SIMPLE_CHAIN` up to the fields read here.
#[repr(C)]
struct CertSimpleChain {
    cb_size: u32,
    trust_status: CertTrustStatus,
    element_count: u32,
    elements: *const *const CertChainElement,
}

/// `CERT_CHAIN_ELEMENT` up to the fields read here.
#[repr(C)]
struct CertChainElement {
    cb_size: u32,
    cert_context: *const CertContext,
}

#[repr(C)]
struct CertChainPara {
    cb_size: u32,
    usage_match_type: u32,
    usage_identifier_count: u32,
    usage_identifiers: *const *const u8,
}

#[repr(C)]
struct SslExtraCertChainPolicyPara {
    cb_size: u32,
    auth_type: u32,
    fdw_checks: u32,
    server_name: *const u16,
}

#[repr(C)]
struct CertChainPolicyPara {
    cb_size: u32,
    flags: u32,
    extra_policy_para: *const c_void,
}

#[repr(C)]
struct CertChainPolicyStatus {
    cb_size: u32,
    error: u32,
    chain_index: i32,
    element_index: i32,
    extra_policy_status: *mut c_void,
}

type WinHttpQueryOptionFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut u32) -> i32;
type CertGetChainFn = unsafe extern "system" fn(
    *mut c_void,
    *const CertContext,
    *const c_void,
    *mut c_void,
    *const CertChainPara,
    u32,
    *mut c_void,
    *mut *const c_void,
) -> i32;
type CertVerifyPolicyFn =
    unsafe extern "system" fn(usize, *const c_void, *const CertChainPolicyPara, *mut CertChainPolicyStatus) -> i32;
type CertFreeChainFn = unsafe extern "system" fn(*const c_void);
type CertFreeContextFn = unsafe extern "system" fn(*const CertContext) -> i32;
type CertGetPropFn = unsafe extern "system" fn(*const CertContext, u32, *mut c_void, *mut u32) -> i32;

struct CryptoProcs {
    winhttp_query_option: WinHttpQueryOptionFn,
    cert_get_chain: CertGetChainFn,
    cert_verify_policy: CertVerifyPolicyFn,
    cert_free_chain: CertFreeChainFn,
    cert_free_context: CertFreeContextFn,
    cert_get_prop: CertGetPropFn,
}

fn crypto_procs() -> Option<&'static CryptoProcs> {
    static PROCS: OnceLock<Option<CryptoProcs>> = OnceLock::new();
    PROCS
        .get_or_init(|| unsafe {
            use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
            let wh = LoadLibraryExW(wide("winhttp.dll").as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
            let cr = LoadLibraryExW(wide("crypt32.dll").as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
            if wh.is_null() || cr.is_null() {
                return None;
            }
            let sym = |h, name: &[u8]| GetProcAddress(h, name.as_ptr()).map(|f| f as usize);
            Some(CryptoProcs {
                winhttp_query_option: std::mem::transmute(sym(wh, b"WinHttpQueryOption\0")?),
                cert_get_chain: std::mem::transmute(sym(cr, b"CertGetCertificateChain\0")?),
                cert_verify_policy: std::mem::transmute(sym(cr, b"CertVerifyCertificateChainPolicy\0")?),
                cert_free_chain: std::mem::transmute(sym(cr, b"CertFreeCertificateChain\0")?),
                cert_free_context: std::mem::transmute(sym(cr, b"CertFreeCertificateContext\0")?),
                cert_get_prop: std::mem::transmute(sym(cr, b"CertGetCertificateContextProperty\0")?),
            })
        })
        .as_ref()
}

fn win32_failure_hr(err: u32) -> HRESULT {
    if err != 0 {
        hresult_from_win32(err)
    } else {
        crate::util::E_FAIL
    }
}

fn policy_status_hr(error: u32) -> HRESULT {
    let signed = error as HRESULT;
    if signed < 0 {
        signed
    } else {
        win32_failure_hr(error)
    }
}

fn url_host_and_is_plain_loopback(url: &str) -> (String, bool) {
    let lower = url.to_ascii_lowercase();
    let is_http = lower.starts_with("http://");
    let after_scheme = match url.split_once("://") {
        Some((_, r)) => r,
        None => url,
    };
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, _)) => h,
            None => "",
        }
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    let is_loopback_ip = host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    let loopback = host.eq_ignore_ascii_case("localhost") || is_loopback_ip;
    (host.to_string(), is_http && loopback)
}

unsafe fn sha1_thumbprint(procs: &CryptoProcs, cert: *const CertContext) -> Vec<u8> {
    let mut buf = [0u8; 20];
    let mut len = buf.len() as u32;
    if (procs.cert_get_prop)(cert, CERT_SHA1_HASH_PROP_ID, buf.as_mut_ptr() as *mut c_void, &mut len) == 0 {
        return Vec::new();
    }
    buf[..len as usize].to_vec()
}

/// Extract leaf-to-root certificate contexts from the primary simple chain in `CERT_CHAIN_CONTEXT`.
unsafe fn extract_chain_certs(chain: *const c_void) -> Vec<*const CertContext> {
    let chain = chain as *const CertChainContext;
    if chain.is_null() || (*chain).chain_count == 0 || (*chain).chains.is_null() {
        return Vec::new();
    }
    let first = *(*chain).chains;
    if first.is_null() || (*first).elements.is_null() {
        return Vec::new();
    }
    let elements = std::slice::from_raw_parts((*first).elements, (*first).element_count as usize);
    elements.iter().filter(|e| !e.is_null()).map(|&e| (*e).cert_context).filter(|c| !c.is_null()).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PinCheck {
    NoPins,
    Matched,
    Mismatch,
    InvalidMetadata,
}

const SHA1_LEN: usize = 20;

unsafe fn validate_thumbprint_metadata(security_info: *const XNetworkingSecurityInformation) -> Result<usize, ()> {
    if security_info.is_null() {
        return Ok(0);
    }
    let info = std::ptr::read_unaligned(security_info);
    if info.thumbprint_count == 0 {
        return Ok(0);
    }
    if info.thumbprints.is_null() {
        return Err(());
    }
    let Some(table_bytes) = info.thumbprint_count.checked_mul(std::mem::size_of::<XNetworkingThumbprint>()) else {
        return Err(());
    };
    if table_bytes > isize::MAX as usize {
        return Err(());
    }
    for i in 0..info.thumbprint_count {
        let tp = std::ptr::read_unaligned(info.thumbprints.add(i));
        if tp.thumbprint_buffer.is_null() || tp.thumbprint_buffer_byte_count != SHA1_LEN || tp.thumbprint_type > 2 {
            return Err(());
        }
    }
    Ok(info.thumbprint_count)
}

unsafe fn check_thumbprints(security_info: *const XNetworkingSecurityInformation, chain_sha1s: &[Vec<u8>]) -> PinCheck {
    let count = match validate_thumbprint_metadata(security_info) {
        Ok(0) => return PinCheck::NoPins,
        Ok(n) => n,
        Err(()) => return PinCheck::InvalidMetadata,
    };
    let info = std::ptr::read_unaligned(security_info);
    for i in 0..count {
        let tp = std::ptr::read_unaligned(info.thumbprints.add(i));
        let expected = std::slice::from_raw_parts(tp.thumbprint_buffer, SHA1_LEN);
        let matched = match tp.thumbprint_type {
            0 => chain_sha1s.first().is_some_and(|c| c.as_slice() == expected),
            1 => chain_sha1s.iter().skip(1).take(chain_sha1s.len().saturating_sub(2)).any(|c| c.as_slice() == expected),
            2 => chain_sha1s.last().is_some_and(|c| c.as_slice() == expected),
            _ => return PinCheck::InvalidMetadata,
        };
        if matched {
            return PinCheck::Matched;
        }
    }
    PinCheck::Mismatch
}

/// Verify optional SHA-1 thumbprint pins against the leaf-to-root certificate chain.
///
/// Rejects malformed pin metadata (nonzero count with null array, overflowing count,
/// null or non-20-byte SHA-1 pin buffers, or unknown `XNetworkingThumbprintType` values).
pub unsafe fn verify_thumbprints(security_info: *const XNetworkingSecurityInformation, chain_sha1s: &[Vec<u8>]) -> bool {
    matches!(check_thumbprints(security_info, chain_sha1s), PinCheck::NoPins | PinCheck::Matched)
}

/// The request's URL (`WINHTTP_OPTION_URL`) and the bytes WinHTTP reported, or its Win32
/// error. A URL longer than the first buffer leaves the bytes it needs in the length: query
/// again with that much.
unsafe fn request_url(query: WinHttpQueryOptionFn, request: *mut c_void) -> Result<(Vec<u16>, usize), u32> {
    let mut url_buf = vec![0u16; 2048];
    let mut url_bytes = (url_buf.len() * 2) as u32;
    if query(request, WINHTTP_OPTION_URL, url_buf.as_mut_ptr() as *mut c_void, &mut url_bytes) == 0 {
        let err = windows_sys::Win32::Foundation::GetLastError();
        if err != windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER || url_bytes as usize <= url_buf.len() * 2 {
            return Err(err);
        }
        url_buf.resize((url_bytes as usize).div_ceil(2), 0);
        url_bytes = (url_buf.len() * 2) as u32;
        if query(request, WINHTTP_OPTION_URL, url_buf.as_mut_ptr() as *mut c_void, &mut url_bytes) == 0 {
            return Err(windows_sys::Win32::Foundation::GetLastError());
        }
    }
    Ok((url_buf, url_bytes as usize))
}

pub unsafe extern "system" fn XNetworkingVerifyServerCertificate(
    request_handle: *mut c_void,
    security_info: *const XNetworkingSecurityInformation,
) -> HRESULT {
    if request_handle.is_null() {
        return E_INVALIDARG;
    }
    let Ok(pin_count) = validate_thumbprint_metadata(security_info) else {
        return E_INVALIDARG;
    };
    let Some(procs) = crypto_procs() else {
        return crate::util::E_FAIL;
    };

    let (url_buf, url_bytes) = match request_url(procs.winhttp_query_option, request_handle) {
        Ok(u) => u,
        Err(err) => return win32_failure_hr(err),
    };
    if url_bytes == 0 || url_bytes > url_buf.len() * 2 || (url_bytes & 1) != 0 {
        return crate::util::E_FAIL;
    }
    let chars = url_bytes / 2;
    let trimmed = url_buf[..chars].iter().position(|&c| c == 0).unwrap_or(chars);
    let url = String::from_utf16_lossy(&url_buf[..trimmed]);
    let (host, is_plain_loopback) = url_host_and_is_plain_loopback(&url);
    if host.is_empty() {
        return crate::util::E_FAIL;
    }
    if is_plain_loopback {
        // Plain HTTP has no server certificate and must fail if the caller requires pinning.
        return if pin_count > 0 { CERT_E_UNTRUSTEDROOT } else { S_OK };
    }

    let mut cert: *const CertContext = std::ptr::null();
    let mut cert_size = std::mem::size_of::<*const CertContext>() as u32;
    if (procs.winhttp_query_option)(
        request_handle,
        WINHTTP_OPTION_SERVER_CERT_CONTEXT,
        &mut cert as *mut *const CertContext as *mut c_void,
        &mut cert_size,
    ) == 0
        || cert.is_null()
    {
        return win32_failure_hr(windows_sys::Win32::Foundation::GetLastError());
    }

    let para = CertChainPara {
        cb_size: std::mem::size_of::<CertChainPara>() as u32,
        usage_match_type: 0,
        usage_identifier_count: 0,
        usage_identifiers: std::ptr::null(),
    };
    let mut chain: *const c_void = std::ptr::null();
    let got_chain = (procs.cert_get_chain)(
        std::ptr::null_mut(),
        cert,
        std::ptr::null(),
        (*cert).cert_store,
        &para,
        0,
        std::ptr::null_mut(),
        &mut chain,
    ) != 0
        && !chain.is_null();

    let mut hr = if got_chain {
        S_OK
    } else {
        win32_failure_hr(windows_sys::Win32::Foundation::GetLastError())
    };
    if got_chain {
        let host_w = wide(&host);
        let ssl = SslExtraCertChainPolicyPara {
            cb_size: std::mem::size_of::<SslExtraCertChainPolicyPara>() as u32,
            auth_type: AUTHTYPE_SERVER,
            fdw_checks: 0,
            server_name: host_w.as_ptr(),
        };
        let policy_para = CertChainPolicyPara {
            cb_size: std::mem::size_of::<CertChainPolicyPara>() as u32,
            flags: 0,
            extra_policy_para: &ssl as *const SslExtraCertChainPolicyPara as *const c_void,
        };
        let mut status = CertChainPolicyStatus {
            cb_size: std::mem::size_of::<CertChainPolicyStatus>() as u32,
            error: 0,
            chain_index: -1,
            element_index: -1,
            extra_policy_status: std::ptr::null_mut(),
        };
        if (procs.cert_verify_policy)(CERT_CHAIN_POLICY_SSL, chain, &policy_para, &mut status) == 0 {
            hr = win32_failure_hr(windows_sys::Win32::Foundation::GetLastError());
        } else if status.error != 0 {
            hr = policy_status_hr(status.error);
        } else {
            let prints: Vec<Vec<u8>> = extract_chain_certs(chain).into_iter().map(|c| sha1_thumbprint(procs, c)).collect();
            hr = match check_thumbprints(security_info, &prints) {
                PinCheck::NoPins | PinCheck::Matched => S_OK,
                PinCheck::Mismatch => CERT_E_UNTRUSTEDROOT,
                PinCheck::InvalidMetadata => E_INVALIDARG,
            };
        }
        (procs.cert_free_chain)(chain);
    }
    (procs.cert_free_context)(cert);
    hr
}

// ---------------------------------------------------------------------------------------
// Connectivity hint (iphlpapi.dll)
// ---------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NlNetworkConnectivityHint {
    connectivity_level: u32,
    connectivity_cost: u32,
    approaching_data_limit: u8,
    over_data_limit: u8,
    roaming: u8,
}

type GetConnectivityHintFn = unsafe extern "system" fn(*mut NlNetworkConnectivityHint) -> u32;

fn host_connectivity_hint() -> Option<NlNetworkConnectivityHint> {
    static FN: OnceLock<Option<GetConnectivityHintFn>> = OnceLock::new();
    let f = (*FN.get_or_init(|| unsafe {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
        let h = LoadLibraryExW(wide("iphlpapi.dll").as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if h.is_null() {
            return None;
        }
        GetProcAddress(h, b"GetNetworkConnectivityHint\0".as_ptr()).map(|p| std::mem::transmute(p))
    }))?;
    let mut hint = NlNetworkConnectivityHint::default();
    if unsafe { f(&mut hint) } == 0 { Some(hint) } else { None }
}

/// The hint's level from the host's (`None`: not known). With `XCL_NETWORK=0` it is None (1), as
/// on a console without a network: answered with the host's InternetAccess, a retail game took
/// itself to be online and kept asking its online service for a ticket that never came.
fn connectivity_level(host: Option<u32>, network_off: bool) -> u32 {
    match host {
        _ if network_off => 1,
        Some(l @ 1..=4) => l,
        Some(5) => 2, // LocalAccess
        _ => 3,       // InternetAccess
    }
}

pub unsafe extern "system" fn XNetworkingGetConnectivityHint(out: *mut XNetworkingConnectivityHint) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let host = host_connectivity_hint();
    let level = connectivity_level(host.map(|h| h.connectivity_level), crate::hle::netoff::enabled());
    let cost = match host.map(|h| h.connectivity_cost) {
        Some(c @ 1..=3) => c,
        _ => 1, // Unrestricted
    };
    *out = XNetworkingConnectivityHint {
        connectivity_level: level,
        connectivity_cost: cost,
        iana_interface_type: 6, // IF_TYPE_ETHERNET_CSMACD
        network_initialized: true,
        approaching_data_limit: host.is_some_and(|h| h.approaching_data_limit != 0),
        over_data_limit: host.is_some_and(|h| h.over_data_limit != 0),
        roaming: host.is_some_and(|h| h.roaming != 0),
    };
    S_OK
}

unsafe extern "system" fn connectivity_hint_changed_call(rec: *mut c_void, canceled: bool) {
    let r = Box::from_raw(rec as *mut InitialNotification<XNetworkingConnectivityHint>);
    if !canceled {
        let f: unsafe extern "system" fn(*mut c_void, *const XNetworkingConnectivityHint) = std::mem::transmute(r.cb);
        f(r.ctx as *mut c_void, &r.value);
    }
}

/// Registering "sends an initial notification callback" (GDK docs) with the current hint: a
/// title that waits for it to see `network_initialized` would otherwise wait forever.
pub unsafe extern "system" fn XNetworkingRegisterConnectivityHintChanged(
    queue: *mut c_void,
    ctx: *mut c_void,
    cb: usize,
    token: *mut u64,
) -> HRESULT {
    if token.is_null() || cb == 0 {
        return E_INVALIDARG;
    }
    *token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let mut hint: XNetworkingConnectivityHint = std::mem::zeroed();
    XNetworkingGetConnectivityHint(&mut hint);
    notify_initial(queue, ctx, cb, hint, connectivity_hint_changed_call as usize, "connectivity hint");
    S_OK
}

pub unsafe extern "system" fn XNetworkingUnregisterConnectivityHintChanged(_token: u64, _wait: bool) -> bool {
    true
}

// ---------------------------------------------------------------------------------------
// Configuration settings and statistics (TCP queued receive buffers)
// ---------------------------------------------------------------------------------------

/// `XNetworkingConfigurationSetting` values: the most memory the title's, the system's and
/// the tools' TCP sockets may queue for received payloads no buffer was posted for yet.
pub const MAX_TITLE_TCP_QUEUED_RECEIVE_BUFFER_SIZE: u32 = 0;
pub const MAX_SYSTEM_TCP_QUEUED_RECEIVE_BUFFER_SIZE: u32 = 1;
pub const MAX_TOOLS_TCP_QUEUED_RECEIVE_BUFFER_SIZE: u32 = 2;

/// The console's defaults (XCL reports a console, not a PC): 32 MiB for the title, 4 MiB
/// for the system, no limit for the tools.
const CONFIG_DEFAULTS: [u64; 3] = [32 << 20, 4 << 20, u64::MAX];

static CONFIG: [AtomicU64; 3] = [AtomicU64::new(CONFIG_DEFAULTS[0]), AtomicU64::new(CONFIG_DEFAULTS[1]), AtomicU64::new(CONFIG_DEFAULTS[2])];

/// `XNetworkingQueryConfigurationSetting(setting, value)`: the value last set, else the
/// console default. A setting the runtime does not know is E_INVALIDARG.
pub unsafe extern "system" fn XNetworkingQueryConfigurationSetting(setting: u32, value: *mut u64) -> HRESULT {
    let Some(v) = CONFIG.get(setting as usize) else { return E_INVALIDARG };
    if value.is_null() {
        return E_INVALIDARG;
    }
    *value = v.load(Ordering::Acquire);
    S_OK
}

/// `XNetworkingSetConfigurationSetting(setting, value)`: every value from 0 to UINT64_MAX
/// is valid. XCL's sockets are the host's, so the limit is kept and reported, not enforced.
pub unsafe extern "system" fn XNetworkingSetConfigurationSetting(setting: u32, value: u64) -> HRESULT {
    let Some(v) = CONFIG.get(setting as usize) else { return E_INVALIDARG };
    if v.swap(value, Ordering::AcqRel) != value {
        crate::xlog_info!("XNetworkingSetConfigurationSetting({setting}) = {value:#x}");
    }
    S_OK
}

/// `XNetworkingTcpQueuedReceivedBufferUsageStatistics` (`XNetworking.h`, 40 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XNetworkingTcpQueuedReceivedBufferUsageStatistics {
    pub num_bytes_currently_queued: u64,
    pub peak_num_bytes_ever_queued: u64,
    pub total_num_bytes_queued: u64,
    pub num_bytes_dropped_for_exceeding_configured_max: u64,
    pub num_bytes_dropped_due_to_any_failure: u64,
}

/// `XNetworkingQueryStatistics(type, buffer)`: the three `XNetworkingStatisticsType`s (title,
/// system, tools TCP queued receive buffer usage) all write `tcpQueuedReceiveBufferUsage`,
/// and only it (the rest of the caller's union stays as it was). The host keeps no such
/// queues per partition, so nothing is queued or dropped: all zero, as a PC reports.
pub unsafe extern "system" fn XNetworkingQueryStatistics(kind: u32, buffer: *mut XNetworkingTcpQueuedReceivedBufferUsageStatistics) -> HRESULT {
    if kind > 2 || buffer.is_null() {
        return E_INVALIDARG;
    }
    std::ptr::write_unaligned(buffer, XNetworkingTcpQueuedReceivedBufferUsageStatistics::default());
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hle::xgameruntime::xasync::XAsyncGetStatus;

    #[test]
    fn struct_layouts_match_gdk_abi() {
        assert_eq!(std::mem::size_of::<XNetworkingThumbprint>(), 24);
        assert_eq!(std::mem::size_of::<XNetworkingSecurityInformation>(), 24);
        assert_eq!(std::mem::size_of::<XNetworkingConnectivityHint>(), 16);
        assert_eq!(std::mem::size_of::<XNetworkingTcpQueuedReceivedBufferUsageStatistics>(), 40);
    }

    /// Configuration settings start at the console defaults, keep what the title sets (any
    /// u64, 0 and UINT64_MAX included) and refuse a setting the runtime does not know.
    #[test]
    fn configuration_settings_keep_what_the_title_sets() {
        unsafe {
            let mut v = 0u64;
            assert_eq!(XNetworkingQueryConfigurationSetting(MAX_SYSTEM_TCP_QUEUED_RECEIVE_BUFFER_SIZE, &mut v), S_OK);
            assert_eq!(v, 4 << 20);
            assert_eq!(XNetworkingQueryConfigurationSetting(MAX_TOOLS_TCP_QUEUED_RECEIVE_BUFFER_SIZE, &mut v), S_OK);
            assert_eq!(v, u64::MAX);
            for want in [0, 1 << 20, u64::MAX, 32 << 20] {
                assert_eq!(XNetworkingSetConfigurationSetting(MAX_TITLE_TCP_QUEUED_RECEIVE_BUFFER_SIZE, want), S_OK);
                assert_eq!(XNetworkingQueryConfigurationSetting(MAX_TITLE_TCP_QUEUED_RECEIVE_BUFFER_SIZE, &mut v), S_OK);
                assert_eq!(v, want);
            }
            assert_eq!(XNetworkingQueryConfigurationSetting(3, &mut v), E_INVALIDARG);
            assert_eq!(XNetworkingSetConfigurationSetting(3, 0), E_INVALIDARG);
            assert_eq!(XNetworkingQueryConfigurationSetting(MAX_TITLE_TCP_QUEUED_RECEIVE_BUFFER_SIZE, std::ptr::null_mut()), E_INVALIDARG);
        }
    }

    /// Statistics write the 40-byte substructure only: the bytes after it in the caller's
    /// buffer stay as they were.
    #[test]
    fn statistics_write_only_the_substructure() {
        unsafe {
            let mut buf = [0xAAu8; 48];
            let p = buf.as_mut_ptr() as *mut XNetworkingTcpQueuedReceivedBufferUsageStatistics;
            for kind in 0..3 {
                buf = [0xAA; 48];
                assert_eq!(XNetworkingQueryStatistics(kind, p), S_OK);
                assert!(buf[..40].iter().all(|&b| b == 0), "type {kind}");
                assert!(buf[40..].iter().all(|&b| b == 0xAA), "type {kind}");
            }
            assert_eq!(XNetworkingQueryStatistics(3, p), E_INVALIDARG);
            assert_eq!(XNetworkingQueryStatistics(0, std::ptr::null_mut()), E_INVALIDARG);
        }
    }

    /// A chain laid out byte by byte as `wincrypt.h` places it on x64, independent of the
    /// structs above: `cChain` @ +12, `rgpChain` @ +16, `cLowerQualityChainContext` @ +24;
    /// `cElement` @ +12, `rgpElement` @ +16; `pCertContext` @ +8.
    #[test]
    fn chain_certs_read_at_the_sdk_offsets() {
        #[repr(C, align(8))]
        struct Raw([u8; 64]);
        fn put(raw: &mut Raw, at: usize, bytes: &[u8]) {
            raw.0[at..at + bytes.len()].copy_from_slice(bytes);
        }
        let leaf: CertContext = unsafe { std::mem::zeroed() };
        let root: CertContext = unsafe { std::mem::zeroed() };
        let (leaf, root) = (&leaf as *const CertContext, &root as *const CertContext);

        let mut elements = [Raw([0; 64]), Raw([0; 64])];
        for (e, cert) in elements.iter_mut().zip([leaf, root]) {
            put(e, 0, &24u32.to_le_bytes());
            put(e, 8, &(cert as usize).to_le_bytes());
        }
        let element_ptrs = [&elements[0] as *const Raw, &elements[1] as *const Raw];
        let mut simple = Raw([0; 64]);
        put(&mut simple, 12, &2u32.to_le_bytes());
        put(&mut simple, 16, &(element_ptrs.as_ptr() as usize).to_le_bytes());
        let simple_ptrs = [&simple as *const Raw];
        let mut context = Raw([0; 64]);
        put(&mut context, 12, &1u32.to_le_bytes());
        put(&mut context, 16, &(simple_ptrs.as_ptr() as usize).to_le_bytes());

        unsafe {
            assert_eq!(extract_chain_certs(&context as *const Raw as *const c_void), vec![leaf, root]);
            assert!(extract_chain_certs(std::ptr::null()).is_empty());
            put(&mut context, 12, &0u32.to_le_bytes());
            assert!(extract_chain_certs(&context as *const Raw as *const c_void).is_empty());
        }
    }

    #[test]
    fn udp_port_sync_and_async() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut port = 0u16;
            assert_eq!(XNetworkingQueryPreferredLocalUdpMultiplayerPort(&mut port), S_OK);
            assert_eq!(port, DEFAULT_UDP_PORT);

            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync(&mut b), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            let mut async_port = 0u16;
            assert_eq!(XNetworkingQueryPreferredLocalUdpMultiplayerPortAsyncResult(&mut b, &mut async_port), S_OK);
            assert_eq!(async_port, DEFAULT_UDP_PORT);
        }
    }

    /// Registering for the UDP port or connectivity hint changes fires one initial callback
    /// with the current value, on the queue's completion port.
    #[test]
    fn register_fires_an_initial_callback() {
        use crate::hle::xgameruntime::taskqueue::{XTaskQueueCloseHandle, XTaskQueueCreate, MODE_IMMEDIATE};
        use std::sync::atomic::AtomicU32;
        struct Seen {
            calls: AtomicU32,
            port: AtomicU32,
            hint: std::sync::Mutex<Option<XNetworkingConnectivityHint>>,
        }
        unsafe extern "system" fn on_port(ctx: *mut c_void, port: u16) {
            let seen = &*(ctx as *const Seen);
            seen.port.store(port as u32, Ordering::SeqCst);
            seen.calls.fetch_add(1, Ordering::SeqCst);
        }
        unsafe extern "system" fn on_hint(ctx: *mut c_void, hint: *const XNetworkingConnectivityHint) {
            let seen = &*(ctx as *const Seen);
            *seen.hint.lock().unwrap() = Some(*hint);
            seen.calls.fetch_add(1, Ordering::SeqCst);
        }

        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut q = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_IMMEDIATE, MODE_IMMEDIATE, &mut q), S_OK);

            let seen = Seen { calls: AtomicU32::new(0), port: AtomicU32::new(0), hint: std::sync::Mutex::new(None) };
            let ctx = &seen as *const Seen as *mut c_void;
            let mut token = 0u64;
            assert_eq!(XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged(q, ctx, on_port as usize, &mut token), S_OK);
            assert_eq!(seen.calls.load(Ordering::SeqCst), 1);
            assert_eq!(seen.port.load(Ordering::SeqCst), DEFAULT_UDP_PORT as u32);
            assert!(XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged(token, true));

            seen.calls.store(0, Ordering::SeqCst);
            assert_eq!(XNetworkingRegisterConnectivityHintChanged(q, ctx, on_hint as usize, &mut token), S_OK);
            assert_eq!(seen.calls.load(Ordering::SeqCst), 1);
            let hint = seen.hint.lock().unwrap().expect("no connectivity hint callback");
            assert!(hint.network_initialized);
            assert!((1..=4).contains(&hint.connectivity_level));
            assert!(XNetworkingUnregisterConnectivityHintChanged(token, true));

            // the callback is required (`_In_`)
            assert_eq!(XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged(q, ctx, 0, &mut token), E_INVALIDARG);
            assert_eq!(XNetworkingRegisterConnectivityHintChanged(q, ctx, 0, &mut token), E_INVALIDARG);
            assert_eq!(seen.calls.load(Ordering::SeqCst), 1);

            XTaskQueueCloseHandle(q);
        }
    }

    #[test]
    fn default_security_info_bytes_zeroes_padding_and_fields() {
        let bytes = default_security_info_bytes();
        assert_eq!(bytes.len(), std::mem::size_of::<XNetworkingSecurityInformation>());
        let off = std::mem::offset_of!(XNetworkingSecurityInformation, enabled_http_security_protocol_flags);
        assert_eq!(&bytes[off..off + 4], &DEFAULT_TLS_FLAGS.to_le_bytes());
        assert!(bytes[..off].iter().all(|&b| b == 0));
        assert!(bytes[off + 4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn security_info_utf8_and_utf16_follow_gdk_arg_order() {
        #[repr(C)]
        struct AlignedBuf {
            _align: [XNetworkingSecurityInformation; 0],
            bytes: [u8; 32],
        }

        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            // UTF-8
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XNetworkingQuerySecurityInformationForUrlAsync(b"https://example.invalid/\0".as_ptr(), &mut b), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            let mut sz = 0usize;
            assert_eq!(XNetworkingQuerySecurityInformationForUrlAsyncResultSize(&mut b, &mut sz), S_OK);
            assert_eq!(sz, std::mem::size_of::<XNetworkingSecurityInformation>());
            let mut buf = AlignedBuf { _align: [], bytes: [0xAA; 32] };
            let mut used = 0usize;
            let mut ptr: *mut XNetworkingSecurityInformation = std::ptr::null_mut();
            assert_eq!(
                XNetworkingQuerySecurityInformationForUrlAsyncResult(&mut b, buf.bytes.len(), &mut used, buf.bytes.as_mut_ptr(), &mut ptr),
                S_OK
            );
            assert_eq!(used, sz);
            assert_eq!(ptr as *const u8, buf.bytes.as_ptr());
            assert_eq!((*ptr).enabled_http_security_protocol_flags, DEFAULT_TLS_FLAGS);
            assert_eq!((*ptr).thumbprint_count, 0);
            assert!((*ptr).thumbprints.is_null());

            // UTF-16
            let url16 = wide("https://example.invalid/hotfix");
            let mut b2: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XNetworkingQuerySecurityInformationForUrlUtf16Async(url16.as_ptr(), &mut b2), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b2, true), S_OK);
            let mut sz2 = 0usize;
            assert_eq!(XNetworkingQuerySecurityInformationForUrlUtf16AsyncResultSize(&mut b2, &mut sz2), S_OK);
            assert_eq!(sz2, std::mem::size_of::<XNetworkingSecurityInformation>());
            buf.bytes.fill(0x55);
            let mut ptr2: *mut XNetworkingSecurityInformation = std::ptr::null_mut();
            assert_eq!(
                XNetworkingQuerySecurityInformationForUrlUtf16AsyncResult(&mut b2, buf.bytes.len(), &mut used, buf.bytes.as_mut_ptr(), &mut ptr2),
                S_OK
            );
            assert_eq!(used, sz2);
            assert_eq!(ptr2 as *const u8, buf.bytes.as_ptr());
            assert_eq!((*ptr2).enabled_http_security_protocol_flags, DEFAULT_TLS_FLAGS);
            assert_eq!((*ptr2).thumbprint_count, 0);
            assert!((*ptr2).thumbprints.is_null());
        }
    }

    /// `XCL_NETWORK=0`: no network whatever the host has; otherwise the host's level.
    #[test]
    fn connectivity_level_without_network_is_none() {
        for host in [None, Some(1), Some(2), Some(3), Some(4), Some(5)] {
            assert_eq!(connectivity_level(host, true), 1, "host {host:?}, network off");
        }
        assert_eq!(connectivity_level(Some(3), false), 3);
        assert_eq!(connectivity_level(Some(5), false), 2);
        assert_eq!(connectivity_level(None, false), 3);
    }

    #[test]
    fn connectivity_hint_and_pin_verification() {
        unsafe {
            let mut hint: XNetworkingConnectivityHint = std::mem::zeroed();
            assert_eq!(XNetworkingGetConnectivityHint(&mut hint), S_OK);
            assert!((1..=4).contains(&hint.connectivity_level));
            assert!((1..=3).contains(&hint.connectivity_cost));
            assert_eq!(hint.iana_interface_type, 6);
            assert!(hint.network_initialized);

            assert_eq!(XNetworkingVerifyServerCertificate(std::ptr::null_mut(), std::ptr::null()), E_INVALIDARG);

            let leaf = vec![0xAAu8; 20];
            let root = vec![0xBBu8; 20];
            let chain = vec![leaf.clone(), root.clone()];
            let pin_ok = XNetworkingThumbprint { thumbprint_type: 2, thumbprint_buffer_byte_count: root.len(), thumbprint_buffer: root.as_ptr() };
            let info_ok = XNetworkingSecurityInformation { enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS, thumbprint_count: 1, thumbprints: &pin_ok };
            assert!(verify_thumbprints(&info_ok, &chain));

            let bad = vec![0xCCu8; 20];
            let pin_bad = XNetworkingThumbprint { thumbprint_type: 0, thumbprint_buffer_byte_count: bad.len(), thumbprint_buffer: bad.as_ptr() };
            let info_bad = XNetworkingSecurityInformation { enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS, thumbprint_count: 1, thumbprints: &pin_bad };
            assert!(!verify_thumbprints(&info_bad, &chain));
        }

        assert_eq!(url_host_and_is_plain_loopback("http://127.0.0.1:3551/account/api/oauth/token"), ("127.0.0.1".into(), true));
        assert_eq!(url_host_and_is_plain_loopback("http://127.0.0.2:8080/test"), ("127.0.0.2".into(), true));
        assert_eq!(url_host_and_is_plain_loopback("http://127.1.2.3:8080/test"), ("127.1.2.3".into(), true));
        assert_eq!(url_host_and_is_plain_loopback("https://127.0.0.1:3551/secure"), ("127.0.0.1".into(), false));
        assert_eq!(url_host_and_is_plain_loopback("http://[::1]:8080/test"), ("::1".into(), true));
        assert_eq!(url_host_and_is_plain_loopback("https://social.xboxlive.com/users"), ("social.xboxlive.com".into(), false));
        assert_eq!(url_host_and_is_plain_loopback("http://user:pass@127.0.0.1:8080/test"), ("127.0.0.1".into(), true));
        assert_eq!(url_host_and_is_plain_loopback("http://[::1/bad"), ("".into(), false));
        assert_eq!(url_host_and_is_plain_loopback(""), ("".into(), false));
    }

    #[test]
    fn cert_verification_fails_closed_on_malformed_pins_and_zero_win32_errors() {
        assert_eq!(win32_failure_hr(0), crate::util::E_FAIL);
        assert!(win32_failure_hr(87) < 0);
        assert_eq!(win32_failure_hr(CERT_E_UNTRUSTEDROOT as u32), CERT_E_UNTRUSTEDROOT);
        assert_eq!(policy_status_hr(CERT_E_UNTRUSTEDROOT as u32), CERT_E_UNTRUSTEDROOT);
        assert_eq!(policy_status_hr(87), hresult_from_win32(87));

        let leaf = vec![0xAAu8; 20];
        let issuer = vec![0xBBu8; 20];
        let root = vec![0xCCu8; 20];
        let chain = vec![leaf.clone(), issuer.clone(), root.clone()];

        unsafe {
            // Nonzero thumbprint_count with null pointer must fail closed.
            let null_array = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: std::ptr::null(),
            };
            assert_eq!(check_thumbprints(&null_array, &chain), PinCheck::InvalidMetadata);
            assert!(!verify_thumbprints(&null_array, &chain));
            assert_eq!(XNetworkingVerifyServerCertificate(1usize as *mut c_void, &null_array), E_INVALIDARG);

            // Empty pin buffer must fail closed even when paired with a matching pin.
            let mixed = [
                XNetworkingThumbprint { thumbprint_type: 0, thumbprint_buffer_byte_count: leaf.len(), thumbprint_buffer: leaf.as_ptr() },
                XNetworkingThumbprint { thumbprint_type: 0, thumbprint_buffer_byte_count: 0, thumbprint_buffer: leaf.as_ptr() },
            ];
            let info_mixed = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: mixed.len(),
                thumbprints: mixed.as_ptr(),
            };
            assert_eq!(check_thumbprints(&info_mixed, &chain), PinCheck::InvalidMetadata);
            assert!(!verify_thumbprints(&info_mixed, &chain));

            // Null pin buffer must fail closed.
            let null_buf = XNetworkingThumbprint { thumbprint_type: 0, thumbprint_buffer_byte_count: 20, thumbprint_buffer: std::ptr::null() };
            let info_null_buf = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: &null_buf,
            };
            assert_eq!(check_thumbprints(&info_null_buf, &chain), PinCheck::InvalidMetadata);
            assert!(!verify_thumbprints(&info_null_buf, &chain));

            // Unknown thumbprint_type must fail closed even if bytes match the leaf cert.
            let unknown_type = XNetworkingThumbprint { thumbprint_type: 3, thumbprint_buffer_byte_count: leaf.len(), thumbprint_buffer: leaf.as_ptr() };
            let info_unknown = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: &unknown_type,
            };
            assert_eq!(check_thumbprints(&info_unknown, &chain), PinCheck::InvalidMetadata);
            assert!(!verify_thumbprints(&info_unknown, &chain));
            assert_eq!(XNetworkingVerifyServerCertificate(1usize as *mut c_void, &info_unknown), E_INVALIDARG);

            // Valid issuer pin (type 1) matches chain[1].
            let pin_issuer = XNetworkingThumbprint { thumbprint_type: 1, thumbprint_buffer_byte_count: issuer.len(), thumbprint_buffer: issuer.as_ptr() };
            let info_issuer = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: &pin_issuer,
            };
            assert_eq!(check_thumbprints(&info_issuer, &chain), PinCheck::Matched);
            assert!(verify_thumbprints(&info_issuer, &chain));

            // Overflowing thumbprint_count must fail closed without dereferencing.
            let huge_count = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: usize::MAX,
                thumbprints: &pin_issuer,
            };
            assert_eq!(check_thumbprints(&huge_count, &chain), PinCheck::InvalidMetadata);
            assert!(!verify_thumbprints(&huge_count, &chain));

            // Non-20-byte SHA-1 pin length must fail closed even when paired with a valid match.
            let wrong_len = [
                XNetworkingThumbprint { thumbprint_type: 0, thumbprint_buffer_byte_count: leaf.len(), thumbprint_buffer: leaf.as_ptr() },
                XNetworkingThumbprint { thumbprint_type: 0, thumbprint_buffer_byte_count: 16, thumbprint_buffer: leaf.as_ptr() },
            ];
            let info_wrong_len = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: wrong_len.len(),
                thumbprints: wrong_len.as_ptr(),
            };
            assert_eq!(check_thumbprints(&info_wrong_len, &chain), PinCheck::InvalidMetadata);
            assert!(!verify_thumbprints(&info_wrong_len, &chain));
        }
    }

    /// The URL of a request is read whatever its length. The query stands in for
    /// WinHttpQueryOption(WINHTTP_OPTION_URL) as Windows answers it (measured): a buffer too
    /// small fails with ERROR_INSUFFICIENT_BUFFER and the bytes needed; a big enough one gets the
    /// URL and its bytes without the terminator. (Not through WinHTTP itself: the XCL_NETWORK=0
    /// test hooks WinHttpConnect for the whole test process.)
    #[test]
    fn request_url_reads_a_url_of_any_length() {
        use windows_sys::Win32::Foundation::{SetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_HANDLE};
        static URL: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
        unsafe extern "system" fn query(_h: *mut c_void, option: u32, buf: *mut c_void, len: *mut u32) -> i32 {
            assert_eq!(option, WINHTTP_OPTION_URL);
            let url = URL.lock().unwrap();
            if (*len as usize) < (url.len() + 1) * 2 {
                *len = ((url.len() + 1) * 2) as u32;
                SetLastError(ERROR_INSUFFICIENT_BUFFER);
                return 0;
            }
            std::ptr::copy_nonoverlapping(url.as_ptr(), buf as *mut u16, url.len());
            *(buf as *mut u16).add(url.len()) = 0;
            *len = (url.len() * 2) as u32;
            1
        }
        unsafe extern "system" fn invalid(_h: *mut c_void, _option: u32, _buf: *mut c_void, _len: *mut u32) -> i32 {
            SetLastError(ERROR_INVALID_HANDLE);
            0
        }
        for url in ["http://localhost/short".to_string(), format!("http://localhost/{}", "a".repeat(3000))] {
            *URL.lock().unwrap() = url.encode_utf16().collect();
            let (buf, bytes) = unsafe { request_url(query, std::ptr::null_mut()) }.expect("URL read");
            assert_eq!(bytes, url.len() * 2);
            assert_eq!(String::from_utf16_lossy(&buf[..bytes / 2]), url);
        }
        assert_eq!(unsafe { request_url(invalid, std::ptr::null_mut()) }.unwrap_err(), ERROR_INVALID_HANDLE);
    }

    #[test]
    fn security_info_result_validates_pointers_and_nulls_on_error() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XNetworkingQuerySecurityInformationForUrlAsync(b"https://example.invalid/\0".as_ptr(), &mut b), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);

            let mut used = 123usize;
            let mut ptr: *mut XNetworkingSecurityInformation = 0xDEAD_BEEF as *mut _;
            let mut buf = [0u8; 32];

            // Null out_ptr is rejected with E_INVALIDARG and clears buffer_used.
            assert_eq!(
                XNetworkingQuerySecurityInformationForUrlAsyncResult(&mut b, buf.len(), &mut used, buf.as_mut_ptr(), std::ptr::null_mut()),
                E_INVALIDARG
            );
            assert_eq!(used, 0);

            // Null buffer is rejected with E_INVALIDARG and nulls out_ptr and buffer_used.
            assert_eq!(
                XNetworkingQuerySecurityInformationForUrlAsyncResult(&mut b, buf.len(), &mut used, std::ptr::null_mut(), &mut ptr),
                E_INVALIDARG
            );
            assert!(ptr.is_null());
            assert_eq!(used, 0);

            // Successfully retrieve result into valid buffer and out_ptr.
            assert_eq!(
                XNetworkingQuerySecurityInformationForUrlAsyncResult(&mut b, buf.len(), &mut used, buf.as_mut_ptr(), &mut ptr),
                S_OK
            );
            assert_eq!(used, std::mem::size_of::<XNetworkingSecurityInformation>());
            assert_eq!(ptr, buf.as_mut_ptr() as *mut _);

            // An invalid or already consumed block zeroes out_ptr and buffer_used.
            let mut ptr2: *mut XNetworkingSecurityInformation = 0xDEAD_BEEF as *mut _;
            let mut used2 = 456usize;
            let hr = XNetworkingQuerySecurityInformationForUrlAsyncResult(&mut b, buf.len(), &mut used2, buf.as_mut_ptr(), &mut ptr2);
            assert!(hr < 0);
            assert!(ptr2.is_null());
            assert_eq!(used2, 0);

            // UDP multiplayer port async result zeroes port on error.
            let mut uncompleted_b: XAsyncBlock = std::mem::zeroed();
            let mut port = 0x1234u16;
            assert_eq!(XNetworkingQueryPreferredLocalUdpMultiplayerPortAsyncResult(&mut uncompleted_b, std::ptr::null_mut()), E_INVALIDARG);
            let hr_port = XNetworkingQueryPreferredLocalUdpMultiplayerPortAsyncResult(&mut uncompleted_b, &mut port);
            assert!(hr_port < 0);
            assert_eq!(port, 0);
        }
    }

    #[test]
    fn issuer_pin_matches_any_intermediate_or_issuing_cert_in_chain() {
        let leaf = vec![0x11u8; 20];
        let sub1 = vec![0x22u8; 20];
        let sub2 = vec![0x33u8; 20];
        let root = vec![0x44u8; 20];
        let unrelated = vec![0x55u8; 20];
        let chain = vec![leaf.clone(), sub1.clone(), sub2.clone(), root.clone()];

        unsafe {
            // Issuer pin (type 1) matching first intermediate (sub1)
            let pin_sub1 = XNetworkingThumbprint {
                thumbprint_type: 1,
                thumbprint_buffer_byte_count: sub1.len(),
                thumbprint_buffer: sub1.as_ptr(),
            };
            let info_sub1 = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: &pin_sub1,
            };
            assert_eq!(check_thumbprints(&info_sub1, &chain), PinCheck::Matched);
            assert!(verify_thumbprints(&info_sub1, &chain));

            // Issuer pin (type 1) matching secondary intermediate (sub2)
            let pin_sub2 = XNetworkingThumbprint {
                thumbprint_type: 1,
                thumbprint_buffer_byte_count: sub2.len(),
                thumbprint_buffer: sub2.as_ptr(),
            };
            let info_sub2 = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: &pin_sub2,
            };
            assert_eq!(check_thumbprints(&info_sub2, &chain), PinCheck::Matched);
            assert!(verify_thumbprints(&info_sub2, &chain));

            // A root must only match a root pin, not an issuer pin
            let pin_root_as_issuer = XNetworkingThumbprint {
                thumbprint_type: 1,
                thumbprint_buffer_byte_count: root.len(),
                thumbprint_buffer: root.as_ptr(),
            };
            let info_root_as_issuer = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: &pin_root_as_issuer,
            };
            assert_eq!(check_thumbprints(&info_root_as_issuer, &chain), PinCheck::Mismatch);
            assert!(!verify_thumbprints(&info_root_as_issuer, &chain));
            assert_eq!(check_thumbprints(&info_root_as_issuer, &[leaf.clone(), root.clone()]), PinCheck::Mismatch);
            assert_eq!(check_thumbprints(&info_root_as_issuer, &[root.clone()]), PinCheck::Mismatch);
            assert_eq!(check_thumbprints(&info_root_as_issuer, &[]), PinCheck::Mismatch);

            // Issuer pin (type 1) matching unrelated cert must reject with Mismatch
            let pin_unrelated = XNetworkingThumbprint {
                thumbprint_type: 1,
                thumbprint_buffer_byte_count: unrelated.len(),
                thumbprint_buffer: unrelated.as_ptr(),
            };
            let info_unrelated = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: &pin_unrelated,
            };
            assert_eq!(check_thumbprints(&info_unrelated, &chain), PinCheck::Mismatch);
            assert!(!verify_thumbprints(&info_unrelated, &chain));

            // Issuer pin (type 1) against leaf cert must reject with Mismatch
            let pin_leaf_as_issuer = XNetworkingThumbprint {
                thumbprint_type: 1,
                thumbprint_buffer_byte_count: leaf.len(),
                thumbprint_buffer: leaf.as_ptr(),
            };
            let info_leaf_as_issuer = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: &pin_leaf_as_issuer,
            };
            assert_eq!(check_thumbprints(&info_leaf_as_issuer, &chain), PinCheck::Mismatch);
            assert!(!verify_thumbprints(&info_leaf_as_issuer, &chain));

            // Leaf pin (type 0) must only match leaf
            let pin_leaf = XNetworkingThumbprint {
                thumbprint_type: 0,
                thumbprint_buffer_byte_count: leaf.len(),
                thumbprint_buffer: leaf.as_ptr(),
            };
            let info_leaf = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: &pin_leaf,
            };
            assert_eq!(check_thumbprints(&info_leaf, &chain), PinCheck::Matched);
            assert!(verify_thumbprints(&info_leaf, &chain));

            // Root pin (type 2) must only match root
            let pin_root = XNetworkingThumbprint {
                thumbprint_type: 2,
                thumbprint_buffer_byte_count: root.len(),
                thumbprint_buffer: root.as_ptr(),
            };
            let info_root = XNetworkingSecurityInformation {
                enabled_http_security_protocol_flags: DEFAULT_TLS_FLAGS,
                thumbprint_count: 1,
                thumbprints: &pin_root,
            };
            assert_eq!(check_thumbprints(&info_root, &chain), PinCheck::Matched);
            assert!(verify_thumbprints(&info_root, &chain));
        }
    }
}
