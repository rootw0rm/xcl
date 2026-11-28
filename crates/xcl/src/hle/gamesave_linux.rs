//! Linux twin of the XGameSave provider (`hle/xgameruntime/gamesave.rs`).
//!
//! The provider logic – containers of named blobs under
//! `<data>/gamesave/<configuration id>/`, atomic updates, the 256 MiB quota – is
//! platform-neutral and already tested, so this module *includes* the shared file
//! instead of copying it. Two things need a Linux shim:
//!
//! - the shared entry points are `extern "system"`, which is the Windows x64 ABI only
//!   on Windows; on Linux `"system"` is SysV, while the guest calls every vtable slot
//!   with the Windows ABI. The `win64` wrappers below are what the GDK class table
//!   exports.
//! - the provider's async variants run on the XAsync/XTaskQueue engine, and the
//!   shared module names its `super::xasync`/`super::taskqueue` subsystems. The
//!   shim modules below point those names at the Linux engine, so the provider uses
//!   the same `XAsyncBlock` state the title's own `XAsyncGetStatus`/`GetResult`
//!   calls use – one engine, not a second one.
//!
//! Enumeration callbacks are the guest's code too: the wrappers take the ABI-correct
//! `win64` function pointer, and bridge it for the shared module's `system`-ABI
//! callback type.

#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::util::HRESULT;
use xasync::XAsyncBlock;

#[path = "xgameruntime/gamesave.rs"]
mod core;

pub use core::{Blob, BlobInfo, Container, ContainerInfo, Provider, Update, QUOTA};

pub mod xasync {
    pub use crate::hle::xuser_linux::{
        begin_with, XAsyncBlock, XAsyncGetResult, XAsyncGetResultSize, XAsyncGetStatus,
    };

    /// Serialises the shared provider tests with each other, exactly like the Windows
    /// module's static (the tests reach it through `use super::*`).
    pub static ASYNC_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

pub mod taskqueue {
    pub use crate::hle::xtaskqueue_linux::{
        XTaskQueueCloseHandle, XTaskQueueCreate, XTaskQueueDispatch, XTaskQueueTerminate,
        MODE_MANUAL, PORT_WORK,
    };
}

// ---------------------------------------------------------------------------------------
// entry points (guest ABI)
// ---------------------------------------------------------------------------------------

pub unsafe extern "win64" fn XGameSaveInitializeProvider(
    user: *mut c_void,
    config: *const u8,
    sync_on_demand: bool,
    out: *mut *mut Provider,
) -> HRESULT {
    core::XGameSaveInitializeProvider(user, config, sync_on_demand, out)
}

pub unsafe extern "win64" fn XGameSaveInitializeProviderAsync(
    user: *mut c_void,
    config: *const u8,
    sync: bool,
    b: *mut XAsyncBlock,
) -> HRESULT {
    core::XGameSaveInitializeProviderAsync(user, config, sync, b)
}

pub unsafe extern "win64" fn XGameSaveInitializeProviderResult(
    b: *mut XAsyncBlock,
    out: *mut *mut Provider,
) -> HRESULT {
    core::XGameSaveInitializeProviderResult(b, out)
}

pub unsafe extern "win64" fn XGameSaveCloseProvider(p: *mut Provider) {
    core::XGameSaveCloseProvider(p)
}

pub unsafe extern "win64" fn XGameSaveGetRemainingQuota(
    p: *mut Provider,
    quota: *mut i64,
) -> HRESULT {
    core::XGameSaveGetRemainingQuota(p, quota)
}

pub unsafe extern "win64" fn XGameSaveGetRemainingQuotaAsync(
    p: *mut Provider,
    b: *mut XAsyncBlock,
) -> HRESULT {
    core::XGameSaveGetRemainingQuotaAsync(p, b)
}

pub unsafe extern "win64" fn XGameSaveGetRemainingQuotaResult(
    b: *mut XAsyncBlock,
    quota: *mut i64,
) -> HRESULT {
    core::XGameSaveGetRemainingQuotaResult(b, quota)
}

pub unsafe extern "win64" fn XGameSaveDeleteContainer(
    p: *mut Provider,
    name: *const u8,
) -> HRESULT {
    core::XGameSaveDeleteContainer(p, name)
}

pub unsafe extern "win64" fn XGameSaveDeleteContainerAsync(
    p: *mut Provider,
    name: *const u8,
    b: *mut XAsyncBlock,
) -> HRESULT {
    core::XGameSaveDeleteContainerAsync(p, name, b)
}

pub unsafe extern "win64" fn XGameSaveDeleteContainerResult(b: *mut XAsyncBlock) -> HRESULT {
    core::XGameSaveDeleteContainerResult(b)
}

pub unsafe extern "win64" fn XGameSaveCreateContainer(
    p: *mut Provider,
    name: *const u8,
    out: *mut *mut Container,
) -> HRESULT {
    core::XGameSaveCreateContainer(p, name, out)
}

pub unsafe extern "win64" fn XGameSaveCloseContainer(c: *mut Container) {
    core::XGameSaveCloseContainer(c)
}

pub unsafe extern "win64" fn XGameSaveEnumerateBlobInfo(
    c: *mut Container,
    ctx: *mut c_void,
    cb: Option<BlobCallback>,
) -> HRESULT {
    let Some(guest) = cb else {
        return core::XGameSaveEnumerateBlobInfo(c, ctx, None);
    };
    let bridge = BlobBridge { guest, ctx };
    core::XGameSaveEnumerateBlobInfo(
        c,
        (&bridge as *const BlobBridge).cast_mut().cast(),
        Some(blob_bridge as BlobCb),
    )
}

pub unsafe extern "win64" fn XGameSaveEnumerateBlobInfoByName(
    c: *mut Container,
    prefix: *const u8,
    ctx: *mut c_void,
    cb: Option<BlobCallback>,
) -> HRESULT {
    let Some(guest) = cb else {
        return core::XGameSaveEnumerateBlobInfoByName(c, prefix, ctx, None);
    };
    let bridge = BlobBridge { guest, ctx };
    core::XGameSaveEnumerateBlobInfoByName(
        c,
        prefix,
        (&bridge as *const BlobBridge).cast_mut().cast(),
        Some(blob_bridge as BlobCb),
    )
}

pub unsafe extern "win64" fn XGameSaveReadBlobData(
    c: *mut Container,
    names: *const *const u8,
    count: *mut u32,
    size: usize,
    out: *mut Blob,
) -> HRESULT {
    core::XGameSaveReadBlobData(c, names, count, size, out)
}

pub unsafe extern "win64" fn XGameSaveReadBlobDataAsync(
    c: *mut Container,
    names: *const *const u8,
    count: u32,
    b: *mut XAsyncBlock,
) -> HRESULT {
    core::XGameSaveReadBlobDataAsync(c, names, count, b)
}

pub unsafe extern "win64" fn XGameSaveReadBlobDataResult(
    b: *mut XAsyncBlock,
    size: usize,
    out: *mut Blob,
    count: *mut u32,
) -> HRESULT {
    core::XGameSaveReadBlobDataResult(b, size, out, count)
}

pub unsafe extern "win64" fn XGameSaveCreateUpdate(
    c: *mut Container,
    display: *const u8,
    out: *mut *mut Update,
) -> HRESULT {
    core::XGameSaveCreateUpdate(c, display, out)
}

pub unsafe extern "win64" fn XGameSaveCloseUpdate(u: *mut Update) {
    core::XGameSaveCloseUpdate(u)
}

pub unsafe extern "win64" fn XGameSaveSubmitBlobWrite(
    u: *mut Update,
    name: *const u8,
    data: *const u8,
    size: usize,
) -> HRESULT {
    core::XGameSaveSubmitBlobWrite(u, name, data, size)
}

pub unsafe extern "win64" fn XGameSaveSubmitBlobDelete(u: *mut Update, name: *const u8) -> HRESULT {
    core::XGameSaveSubmitBlobDelete(u, name)
}

pub unsafe extern "win64" fn XGameSaveSubmitUpdate(u: *mut Update) -> HRESULT {
    core::XGameSaveSubmitUpdate(u)
}

pub unsafe extern "win64" fn XGameSaveSubmitUpdateAsync(
    u: *mut Update,
    b: *mut XAsyncBlock,
) -> HRESULT {
    core::XGameSaveSubmitUpdateAsync(u, b)
}

pub unsafe extern "win64" fn XGameSaveSubmitUpdateResult(b: *mut XAsyncBlock) -> HRESULT {
    core::XGameSaveSubmitUpdateResult(b)
}

pub unsafe extern "win64" fn XGameSaveGetContainerInfo(
    p: *mut Provider,
    name: *const u8,
    ctx: *mut c_void,
    cb: Option<ContainerCallback>,
) -> HRESULT {
    let Some(guest) = cb else {
        return core::XGameSaveGetContainerInfo(p, name, ctx, None);
    };
    let bridge = ContainerBridge { guest, ctx };
    core::XGameSaveGetContainerInfo(
        p,
        name,
        (&bridge as *const ContainerBridge).cast_mut().cast(),
        Some(container_bridge as ContainerCb),
    )
}

pub unsafe extern "win64" fn XGameSaveEnumerateContainerInfo(
    p: *mut Provider,
    ctx: *mut c_void,
    cb: Option<ContainerCallback>,
) -> HRESULT {
    let Some(guest) = cb else {
        return core::XGameSaveEnumerateContainerInfo(p, ctx, None);
    };
    let bridge = ContainerBridge { guest, ctx };
    core::XGameSaveEnumerateContainerInfo(
        p,
        (&bridge as *const ContainerBridge).cast_mut().cast(),
        Some(container_bridge as ContainerCb),
    )
}

pub unsafe extern "win64" fn XGameSaveEnumerateContainerInfoByName(
    p: *mut Provider,
    prefix: *const u8,
    ctx: *mut c_void,
    cb: Option<ContainerCallback>,
) -> HRESULT {
    let Some(guest) = cb else {
        return core::XGameSaveEnumerateContainerInfoByName(p, prefix, ctx, None);
    };
    let bridge = ContainerBridge { guest, ctx };
    core::XGameSaveEnumerateContainerInfoByName(
        p,
        prefix,
        (&bridge as *const ContainerBridge).cast_mut().cast(),
        Some(container_bridge as ContainerCb),
    )
}

// ---------------------------------------------------------------------------------------
// guest callbacks: win64 function pointers, bridged to the shared module's system type
// ---------------------------------------------------------------------------------------

type ContainerCallback = unsafe extern "win64" fn(*const ContainerInfo, *mut c_void) -> bool;
type BlobCallback = unsafe extern "win64" fn(*const BlobInfo, *mut c_void) -> bool;
type ContainerCb = unsafe extern "system" fn(*const ContainerInfo, *mut c_void) -> bool;
type BlobCb = unsafe extern "system" fn(*const BlobInfo, *mut c_void) -> bool;

struct ContainerBridge {
    guest: ContainerCallback,
    ctx: *mut c_void,
}

struct BlobBridge {
    guest: BlobCallback,
    ctx: *mut c_void,
}

unsafe extern "system" fn container_bridge(info: *const ContainerInfo, ctx: *mut c_void) -> bool {
    let b = &*(ctx as *const ContainerBridge);
    (b.guest)(info, b.ctx)
}

unsafe extern "system" fn blob_bridge(info: *const BlobInfo, ctx: *mut c_void) -> bool {
    let b = &*(ctx as *const BlobBridge);
    (b.guest)(info, b.ctx)
}

// ---------------------------------------------------------------------------------------
// exports
// ---------------------------------------------------------------------------------------

/// The 27 provider functions the GDK table puts in the XGameSave class (slots 3..=29).
pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        (
            "XGameSaveInitializeProvider",
            XGameSaveInitializeProvider as usize,
        ),
        (
            "XGameSaveInitializeProviderAsync",
            XGameSaveInitializeProviderAsync as usize,
        ),
        (
            "XGameSaveInitializeProviderResult",
            XGameSaveInitializeProviderResult as usize,
        ),
        ("XGameSaveCloseProvider", XGameSaveCloseProvider as usize),
        (
            "XGameSaveGetRemainingQuota",
            XGameSaveGetRemainingQuota as usize,
        ),
        (
            "XGameSaveGetRemainingQuotaAsync",
            XGameSaveGetRemainingQuotaAsync as usize,
        ),
        (
            "XGameSaveGetRemainingQuotaResult",
            XGameSaveGetRemainingQuotaResult as usize,
        ),
        (
            "XGameSaveDeleteContainer",
            XGameSaveDeleteContainer as usize,
        ),
        (
            "XGameSaveDeleteContainerAsync",
            XGameSaveDeleteContainerAsync as usize,
        ),
        (
            "XGameSaveDeleteContainerResult",
            XGameSaveDeleteContainerResult as usize,
        ),
        (
            "XGameSaveGetContainerInfo",
            XGameSaveGetContainerInfo as usize,
        ),
        (
            "XGameSaveEnumerateContainerInfo",
            XGameSaveEnumerateContainerInfo as usize,
        ),
        (
            "XGameSaveEnumerateContainerInfoByName",
            XGameSaveEnumerateContainerInfoByName as usize,
        ),
        (
            "XGameSaveCreateContainer",
            XGameSaveCreateContainer as usize,
        ),
        ("XGameSaveCloseContainer", XGameSaveCloseContainer as usize),
        (
            "XGameSaveEnumerateBlobInfo",
            XGameSaveEnumerateBlobInfo as usize,
        ),
        (
            "XGameSaveEnumerateBlobInfoByName",
            XGameSaveEnumerateBlobInfoByName as usize,
        ),
        ("XGameSaveReadBlobData", XGameSaveReadBlobData as usize),
        (
            "XGameSaveReadBlobDataAsync",
            XGameSaveReadBlobDataAsync as usize,
        ),
        (
            "XGameSaveReadBlobDataResult",
            XGameSaveReadBlobDataResult as usize,
        ),
        ("XGameSaveCreateUpdate", XGameSaveCreateUpdate as usize),
        ("XGameSaveCloseUpdate", XGameSaveCloseUpdate as usize),
        (
            "XGameSaveSubmitBlobWrite",
            XGameSaveSubmitBlobWrite as usize,
        ),
        (
            "XGameSaveSubmitBlobDelete",
            XGameSaveSubmitBlobDelete as usize,
        ),
        ("XGameSaveSubmitUpdate", XGameSaveSubmitUpdate as usize),
        (
            "XGameSaveSubmitUpdateAsync",
            XGameSaveSubmitUpdateAsync as usize,
        ),
        (
            "XGameSaveSubmitUpdateResult",
            XGameSaveSubmitUpdateResult as usize,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The shim exports exactly the GDK provider rows (3..=29), all as distinct code.
    #[test]
    fn exports_cover_the_gdk_provider_rows() {
        let names: Vec<&str> = [
            "XGameSaveInitializeProvider",
            "XGameSaveInitializeProviderAsync",
            "XGameSaveInitializeProviderResult",
            "XGameSaveCloseProvider",
            "XGameSaveGetRemainingQuota",
            "XGameSaveGetRemainingQuotaAsync",
            "XGameSaveGetRemainingQuotaResult",
            "XGameSaveDeleteContainer",
            "XGameSaveDeleteContainerAsync",
            "XGameSaveDeleteContainerResult",
            "XGameSaveGetContainerInfo",
            "XGameSaveEnumerateContainerInfo",
            "XGameSaveEnumerateContainerInfoByName",
            "XGameSaveCreateContainer",
            "XGameSaveCloseContainer",
            "XGameSaveEnumerateBlobInfo",
            "XGameSaveEnumerateBlobInfoByName",
            "XGameSaveReadBlobData",
            "XGameSaveReadBlobDataAsync",
            "XGameSaveReadBlobDataResult",
            "XGameSaveCreateUpdate",
            "XGameSaveCloseUpdate",
            "XGameSaveSubmitBlobWrite",
            "XGameSaveSubmitBlobDelete",
            "XGameSaveSubmitUpdate",
            "XGameSaveSubmitUpdateAsync",
            "XGameSaveSubmitUpdateResult",
        ]
        .to_vec();
        let e = exports();
        assert_eq!(e.len(), 27);
        assert_eq!(e.iter().map(|(n, _)| *n).collect::<Vec<_>>(), names);
        for (name, addr) in &e {
            assert_ne!(*addr, 0, "{name} must be real code");
        }
        let mut addrs: Vec<usize> = e.iter().map(|(_, a)| *a).collect();
        addrs.sort_unstable();
        addrs.dedup();
        assert_eq!(addrs.len(), 27, "two provider names share one address");
    }

    static CALLS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "win64" fn stop_at_first(info: *const ContainerInfo, ctx: *mut c_void) -> bool {
        let seen = &mut *(ctx as *mut Vec<String>);
        seen.push(crate::util::from_cstr_ptr((*info).name).unwrap_or_default());
        CALLS.fetch_add(1, Ordering::SeqCst);
        false
    }

    /// The `win64` guest callback reaches the shared enumerator with the caller's
    /// context, and its `false` stops the enumeration.
    #[test]
    fn win64_container_callback_is_bridged_with_its_context() {
        let _g = xasync::ASYNC_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let cfg = format!("xcl-gs-linux-{}-{nonce}\0", std::process::id());
            let cfg_name = cfg.trim_end_matches('\0').to_string();
            let mut p = std::ptr::null_mut();
            assert_eq!(
                core::XGameSaveInitializeProvider(
                    std::ptr::null_mut(),
                    cfg.as_ptr(),
                    false,
                    &mut p
                ),
                crate::util::S_OK
            );
            for name in [b"a\0".as_ptr(), b"b\0".as_ptr()] {
                let mut c = std::ptr::null_mut();
                assert_eq!(
                    core::XGameSaveCreateContainer(p, name, &mut c),
                    crate::util::S_OK
                );
                let mut u = std::ptr::null_mut();
                assert_eq!(
                    core::XGameSaveCreateUpdate(c, std::ptr::null(), &mut u),
                    crate::util::S_OK
                );
                assert_eq!(
                    core::XGameSaveSubmitBlobWrite(u, b"b1\0".as_ptr(), b"x".as_ptr(), 1),
                    crate::util::S_OK
                );
                assert_eq!(core::XGameSaveSubmitUpdate(u), crate::util::S_OK);
                core::XGameSaveCloseUpdate(u);
                core::XGameSaveCloseContainer(c);
            }

            let before = CALLS.load(Ordering::SeqCst);
            let mut seen: Vec<String> = Vec::new();
            assert_eq!(
                XGameSaveEnumerateContainerInfo(
                    p,
                    &mut seen as *mut _ as *mut c_void,
                    Some(stop_at_first)
                ),
                crate::util::S_OK
            );
            assert_eq!(
                seen.len(),
                1,
                "false must stop after the first container: {seen:?}"
            );
            assert_eq!(CALLS.load(Ordering::SeqCst), before + 1);
            // no callback: the shared contract's E_INVALIDARG
            assert_eq!(
                XGameSaveEnumerateContainerInfo(p, std::ptr::null_mut(), None),
                crate::util::E_INVALIDARG
            );
            // a blob callback uses the same bridge
            let mut c = std::ptr::null_mut();
            assert_eq!(
                core::XGameSaveCreateContainer(p, b"a\0".as_ptr(), &mut c),
                crate::util::S_OK
            );
            let mut blobs: Vec<String> = Vec::new();
            unsafe extern "win64" fn one_blob(info: *const BlobInfo, ctx: *mut c_void) -> bool {
                (*(ctx as *mut Vec<String>))
                    .push(crate::util::from_cstr_ptr((*info).name).unwrap_or_default());
                false
            }
            assert_eq!(
                XGameSaveEnumerateBlobInfo(c, &mut blobs as *mut _ as *mut c_void, Some(one_blob)),
                crate::util::S_OK
            );
            assert_eq!(blobs, vec!["b1".to_string()]);
            core::XGameSaveCloseContainer(c);
            core::XGameSaveCloseProvider(p);
            let _ =
                std::fs::remove_dir_all(crate::state::data_dir().join("gamesave").join(&cfg_name));
        }
    }
}
