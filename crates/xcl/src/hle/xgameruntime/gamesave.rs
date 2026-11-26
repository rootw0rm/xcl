//! XGameSave – containers of named blobs (public GDK `XGameSave.h`), stored as files:
//! `<data>/gamesave/<configuration id>/<container>/b/<blob>` + `display` (display name).
//! Offline: no cloud sync. Async variants use XAsync.

use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::xasync::*;
use crate::util::{from_cstr_ptr, E_FAIL, E_INVALIDARG, HRESULT, S_OK};

/// Prefix for atomic-write temporary files placed alongside blobs/display files.
/// `enc()` always percent-encodes `~` as `%7E`, so no encoded blob name can ever start with this.
const TMP_PREFIX: &str = "~xcltmp.";
static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

pub const E_GS_INVALID_CONTAINER_NAME: HRESULT = 0x8083_0001u32 as i32;
pub const E_GS_OUT_OF_LOCAL_STORAGE: HRESULT = 0x8083_0003u32 as i32;
pub const E_GS_UPDATE_TOO_BIG: HRESULT = 0x8083_0005u32 as i32;
pub const E_GS_QUOTA_EXCEEDED: HRESULT = 0x8083_0006u32 as i32;
pub const E_GS_PROVIDED_BUFFER_TOO_SMALL: HRESULT = 0x8083_0007u32 as i32;
pub const E_GS_BLOB_NOT_FOUND: HRESULT = 0x8083_0008u32 as i32;

pub const GS_MAX_BLOB_NAME_SIZE: usize = 1024;
pub const GS_MAX_BLOB_SIZE: usize = 16 << 20; // 16 MiB
/// quota reported to the title (the GDK default)
pub const QUOTA: i64 = 256 << 20;

const MAGIC_PROVIDER: u32 = 0x5053_4758; // "XGSP"
const MAGIC_CONTAINER: u32 = 0x4353_4758;
const MAGIC_UPDATE: u32 = 0x5553_4758;

pub struct Provider {
    magic: u32,
    root: PathBuf,
}

pub struct Container {
    magic: u32,
    name: String,
    dir: PathBuf,
}

pub struct Update {
    magic: u32,
    dir: PathBuf,
    name: String,
    display: Option<String>,
    /// blob name → new contents (None: delete)
    ops: Vec<(String, Option<Vec<u8>>)>,
}

#[repr(C)]
pub struct ContainerInfo {
    pub name: *const u8,
    pub display_name: *const u8,
    pub blob_count: u32,
    pub total_size: u64,
    pub last_modified_time: i64,
    pub needs_sync: bool,
}

#[repr(C)]
pub struct BlobInfo {
    pub name: *const u8,
    pub size: u32,
}

#[repr(C)]
pub struct Blob {
    pub info: BlobInfo,
    pub data: *mut u8,
}

type ContainerCb = unsafe extern "system" fn(*const ContainerInfo, *mut c_void) -> bool;
type BlobCb = unsafe extern "system" fn(*const BlobInfo, *mut c_void) -> bool;

fn is_reserved_device_name(base: &str) -> bool {
    matches!(base, "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit())
}

/// File-name form of a container / blob name: unusual characters as %XX.
fn enc(name: &str) -> String {
    let mut out: String = name
        .bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"._- ".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect();
    // Names Windows does not keep as given get one more character escaped: a device name (with
    // any extension) opens the device, "." and ".." are the folder itself and its parent, and a
    // trailing dot or space is dropped.
    let base = name.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    if !name.is_empty() && (is_reserved_device_name(&base) || name.bytes().all(|b| b == b'.')) {
        out.replace_range(..1, &format!("%{:02X}", name.as_bytes()[0]));
    }
    if out.ends_with('.') || out.ends_with(' ') {
        let last = out.pop().unwrap();
        out.push_str(&format!("%{:02X}", last as u8));
    }
    out
}

/// `enc()` of a container name longer than this cannot be a path component on Windows
/// (255 chars) once the provider root is counted, so it is stored under a hash folder
/// instead. Names that fit keep their `enc()` folder, so existing saves are untouched.
const MAX_CONTAINER_ENC: usize = 200;
/// Folder-name prefix for hashed container folders. `enc()` percent-encodes `~`, so no
/// plain encoded name can start with this.
const LONG_PREFIX: &str = "~xcllong.";

/// Folder name for a container: `enc(name)` when it fits, else `~xcllong.<fnv1a>` with the
/// real name kept in a `name` file inside the folder (see `apply` / `each_container`).
/// An `enc()` folder that already exists on disk keeps being used, so saves written
/// before hashed folders stay reachable. The hash is over the lowercased name so lookups
/// stay case-insensitive like every other container folder on NTFS.
fn container_dir_name(root: &Path, name: &str) -> String {
    let e = enc(name);
    if e.len() <= MAX_CONTAINER_ENC || root.join(&e).is_dir() {
        return e;
    }
    format!("{LONG_PREFIX}{:016x}", crate::gpu::shader::fnv64(name.to_ascii_lowercase().as_bytes()))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn dec(file: &str) -> String {
    let b = file.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h1), Some(h2)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push((h1 << 4) | h2);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn try_blobs_of(dir: &Path) -> std::io::Result<Vec<(String, PathBuf, u64)>> {
    let rd = match std::fs::read_dir(dir.join("b")) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut v = Vec::new();
    for entry in rd {
        let e = entry?;
        if e.file_type().is_ok_and(|t| t.is_file())
            && !e.file_name().to_string_lossy().starts_with(TMP_PREFIX)
        {
            v.push((dec(&e.file_name().to_string_lossy()), e.path(), e.metadata().map(|m| m.len()).unwrap_or(0)));
        }
    }
    v.sort();
    Ok(v)
}

fn blobs_of(dir: &Path) -> Vec<(String, PathBuf, u64)> {
    try_blobs_of(dir).unwrap_or_default()
}

fn mtime_secs(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|t| t.as_secs() as i64)
        .unwrap_or(0)
}

/// The container folder's own mtime only moves when `b\` or `display` first appear;
/// titles read `lastModifiedTime` as the last submitted update (#1041), so report
/// the newest mtime among the container, `b\`, `display` and the blobs.
fn container_modified(dir: &Path, blobs: &[(String, PathBuf, u64)]) -> i64 {
    let mut newest = mtime_secs(dir);
    for p in [dir.join("b"), dir.join("display")] {
        newest = newest.max(mtime_secs(&p));
    }
    for (_, p, _) in blobs {
        newest = newest.max(mtime_secs(p));
    }
    newest
}

fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

#[cfg(windows)]
fn move_path(path: &Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    // Resolve normal Win32 path components before adding an extended-length prefix.
    let absolute = std::path::absolute(path)?;
    let (prefix, skip) = match absolute.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(_) => (r"\\?\", 0),
            Prefix::UNC(_, _) => (r"\\?\UNC\", 2),
            _ => ("", 0),
        },
        _ => ("", 0),
    };
    Ok(prefix
        .encode_utf16()
        .chain(absolute.as_os_str().encode_wide().skip(skip))
        .chain(std::iter::once(0))
        .collect())
}

fn replace_file(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
        let src = move_path(tmp)?;
        let dst = move_path(dest)?;
        // SAFETY: `src` and `dst` are valid NUL-terminated UTF-16 paths that outlive the call.
        let ok = unsafe { MoveFileExW(src.as_ptr(), dst.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
        if ok != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(tmp, dest)
    }
}

/// A unique sibling temp file of `dest` (`blobs_of` skips what a crash leaves behind).
fn temp_path(dest: &Path) -> std::io::Result<PathBuf> {
    let parent = dest
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing parent directory"))?;
    let id = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!("{TMP_PREFIX}{}.{id}", std::process::id())))
}

/// Write `data` to a unique sibling temp file of `dest`, flushed to disk, for the caller to
/// move into place (`replace_file`); nothing is left behind when the write fails.
fn write_temp(dest: &Path, data: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let tmp = temp_path(dest)?;
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(tmp)
}

unsafe fn provider<'a>(p: *mut Provider) -> Option<&'a Provider> {
    p.as_ref().filter(|p| p.magic == MAGIC_PROVIDER)
}

unsafe fn container<'a>(c: *mut Container) -> Option<&'a Container> {
    c.as_ref().filter(|c| c.magic == MAGIC_CONTAINER)
}

fn open_provider(config: Option<String>) -> *mut Provider {
    let id: String = enc(&config.unwrap_or_else(|| "default".into()));
    let root = crate::state::data_dir().join("gamesave").join(if id.is_empty() { "default".into() } else { id });
    let _ = std::fs::create_dir_all(&root);
    Box::into_raw(Box::new(Provider { magic: MAGIC_PROVIDER, root }))
}

pub unsafe extern "system" fn XGameSaveInitializeProvider(_user: *mut c_void, config: *const u8, _sync_on_demand: bool, out: *mut *mut Provider) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = open_provider(from_cstr_ptr(config));
    S_OK
}

static INIT_ID: u8 = 0;

pub unsafe extern "system" fn XGameSaveInitializeProviderAsync(_user: *mut c_void, config: *const u8, _sync: bool, b: *mut XAsyncBlock) -> HRESULT {
    let config = from_cstr_ptr(config);
    begin_with(b, &INIT_ID as *const u8 as usize, move || Ok((open_provider(config) as usize).to_le_bytes().to_vec()))
}

pub unsafe extern "system" fn XGameSaveInitializeProviderResult(b: *mut XAsyncBlock, out: *mut *mut Provider) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    let mut p = 0usize;
    let hr = XAsyncGetResult(b, &INIT_ID as *const u8 as *const c_void, 8, &mut p as *mut usize as *mut c_void, std::ptr::null_mut());
    if hr >= 0 {
        *out = p as *mut Provider;
    }
    hr
}

pub unsafe extern "system" fn XGameSaveCloseProvider(p: *mut Provider) {
    if provider(p).is_some() {
        (*p).magic = 0;
        drop(Box::from_raw(p));
    }
}

pub unsafe extern "system" fn XGameSaveGetRemainingQuota(p: *mut Provider, quota: *mut i64) -> HRESULT {
    let Some(pr) = provider(p) else { return E_INVALIDARG };
    if quota.is_null() {
        return E_INVALIDARG;
    }
    let _mutation = SAVE_MUTATION.lock().unwrap_or_else(|e| e.into_inner());
    let used: u64 = std::fs::read_dir(&pr.root).into_iter().flatten().flatten().flat_map(|e| blobs_of(&e.path())).map(|b| b.2).sum();
    *quota = (QUOTA - used as i64).max(0);
    static LAST: std::sync::Mutex<Option<i64>> = std::sync::Mutex::new(None);
    if let Ok(mut last) = LAST.lock() {
        if *last != Some(*quota) {
            *last = Some(*quota);
            crate::xlog_info!("XGameSaveGetRemainingQuota: {} KiB left of {} MiB", *quota >> 10, QUOTA >> 20);
        }
    }
    S_OK
}

static QUOTA_ID: u8 = 0;

pub unsafe extern "system" fn XGameSaveGetRemainingQuotaAsync(p: *mut Provider, b: *mut XAsyncBlock) -> HRESULT {
    let mut q = 0i64;
    let hr = XGameSaveGetRemainingQuota(p, &mut q);
    begin_with(b, &QUOTA_ID as *const u8 as usize, move || if hr < 0 { Err(hr) } else { Ok(q.to_le_bytes().to_vec()) })
}

pub unsafe extern "system" fn XGameSaveGetRemainingQuotaResult(b: *mut XAsyncBlock, quota: *mut i64) -> HRESULT {
    if quota.is_null() {
        return E_INVALIDARG;
    }
    XAsyncGetResult(b, &QUOTA_ID as *const u8 as *const c_void, 8, quota as *mut c_void, std::ptr::null_mut())
}

fn valid_container_name_length(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256
}

unsafe fn delete_path(p: *mut Provider, name: *const u8) -> Result<PathBuf, HRESULT> {
    let pr = provider(p).ok_or(E_INVALIDARG)?;
    let name = from_cstr_ptr(name).ok_or(E_INVALIDARG)?;
    if !valid_container_name_length(&name) {
        return Err(E_GS_INVALID_CONTAINER_NAME);
    }
    std::path::absolute(pr.root.join(container_dir_name(&pr.root, &name))).map_err(|_| E_FAIL)
}

// A queued delete must not remove a container partway through a blob update, and a read must not
// see part of an update or keep open a file it replaces.
static SAVE_MUTATION: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn delete_container(dir: &Path) {
    let _mutation = SAVE_MUTATION.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::remove_dir_all(dir);
}

pub unsafe extern "system" fn XGameSaveDeleteContainer(p: *mut Provider, name: *const u8) -> HRESULT {
    match delete_path(p, name) {
        Ok(dir) => {
            delete_container(&dir);
            S_OK
        }
        Err(hr) => hr,
    }
}

static DELETE_ID: u8 = 0;

pub unsafe extern "system" fn XGameSaveDeleteContainerAsync(p: *mut Provider, name: *const u8, b: *mut XAsyncBlock) -> HRESULT {
    if b.is_null() {
        return E_INVALIDARG;
    }
    let dir = delete_path(p, name);
    begin_with(b, &DELETE_ID as *const u8 as usize, move || {
        delete_container(&dir?);
        Ok(Vec::new())
    })
}

pub unsafe extern "system" fn XGameSaveDeleteContainerResult(b: *mut XAsyncBlock) -> HRESULT {
    XAsyncGetStatus(b, false)
}

pub unsafe extern "system" fn XGameSaveCreateContainer(p: *mut Provider, name: *const u8, out: *mut *mut Container) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    let (Some(pr), Some(n)) = (provider(p), from_cstr_ptr(name)) else { return E_INVALIDARG };
    if !valid_container_name_length(&n) {
        return E_GS_INVALID_CONTAINER_NAME;
    }
    *out = Box::into_raw(Box::new(Container { magic: MAGIC_CONTAINER, dir: pr.root.join(container_dir_name(&pr.root, &n)), name: n }));
    S_OK
}

pub unsafe extern "system" fn XGameSaveCloseContainer(c: *mut Container) {
    if container(c).is_some() {
        (*c).magic = 0;
        drop(Box::from_raw(c));
    }
}

/// Call `cb` for every stored container whose name starts with `prefix`.
unsafe fn each_container(pr: &Provider, prefix: &str, exact: bool, ctx: *mut c_void, cb: ContainerCb) -> Result<usize, HRESULT> {
    let rd = match std::fs::read_dir(&pr.root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(_) => return Err(E_FAIL),
    };
    let mut dirs = Vec::new();
    for entry in rd {
        let e = entry.map_err(|_| E_FAIL)?;
        if e.path().is_dir() {
            dirs.push(e.path());
        }
    }
    dirs.sort();
    let mut n = 0;
    for d in dirs {
        let file_name = d.file_name().unwrap_or_default().to_string_lossy();
        let name = if let Some(hash) = file_name.strip_prefix(LONG_PREFIX) {
            if hash.len() != 16 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            // Hashed folder: the real container name lives in the `name` file.
            match std::fs::read_to_string(d.join("name")) {
                Ok(n) => n,
                Err(_) => continue,
            }
        } else {
            dec(&file_name)
        };
        if exact {
            if !name.eq_ignore_ascii_case(prefix) {
                continue;
            }
        } else if !starts_with_ignore_ascii_case(&name, prefix) {
            continue;
        }
        let blobs = try_blobs_of(&d).map_err(|_| E_FAIL)?;
        if blobs.is_empty() && !d.join("display").exists() {
            continue; // created but never written
        }
        let display = std::fs::read_to_string(d.join("display")).unwrap_or_else(|_| name.clone());
        let modified = container_modified(&d, &blobs);
        let (cn, cd) = (CString::new(name).unwrap_or_default(), CString::new(display).unwrap_or_default());
        let info = ContainerInfo {
            name: cn.as_ptr() as *const u8,
            display_name: cd.as_ptr() as *const u8,
            blob_count: blobs.len() as u32,
            total_size: blobs.iter().map(|b| b.2).sum(),
            last_modified_time: modified,
            needs_sync: false,
        };
        n += 1;
        if !cb(&info, ctx) {
            break;
        }
    }
    Ok(n)
}

pub unsafe extern "system" fn XGameSaveGetContainerInfo(p: *mut Provider, name: *const u8, ctx: *mut c_void, cb: Option<ContainerCb>) -> HRESULT {
    let (Some(pr), Some(n), Some(cb)) = (provider(p), from_cstr_ptr(name), cb) else { return E_INVALIDARG };
    if !valid_container_name_length(&n) {
        return E_GS_INVALID_CONTAINER_NAME;
    }
    // A container that is not stored yet is not an error: the callback is just not called.
    each_container(pr, &n, true, ctx, cb).map(|_| S_OK).unwrap_or_else(|hr| hr)
}

pub unsafe extern "system" fn XGameSaveEnumerateContainerInfo(p: *mut Provider, ctx: *mut c_void, cb: Option<ContainerCb>) -> HRESULT {
    let (Some(pr), Some(cb)) = (provider(p), cb) else { return E_INVALIDARG };
    each_container(pr, "", false, ctx, cb).map(|_| S_OK).unwrap_or_else(|hr| hr)
}

pub unsafe extern "system" fn XGameSaveEnumerateContainerInfoByName(p: *mut Provider, prefix: *const u8, ctx: *mut c_void, cb: Option<ContainerCb>) -> HRESULT {
    let (Some(pr), Some(cb)) = (provider(p), cb) else { return E_INVALIDARG };
    each_container(pr, &from_cstr_ptr(prefix).unwrap_or_default(), false, ctx, cb).map(|_| S_OK).unwrap_or_else(|hr| hr)
}

unsafe fn each_blob(c: &Container, prefix: &str, ctx: *mut c_void, cb: BlobCb) {
    let blobs = {
        let _mutation = SAVE_MUTATION.lock().unwrap_or_else(|e| e.into_inner());
        blobs_of(&c.dir)
    };
    for (name, _, size) in blobs {
        if !starts_with_ignore_ascii_case(&name, prefix) {
            continue;
        }
        let cn = CString::new(name).unwrap_or_default();
        if !cb(&BlobInfo { name: cn.as_ptr() as *const u8, size: size as u32 }, ctx) {
            break;
        }
    }
}

pub unsafe extern "system" fn XGameSaveEnumerateBlobInfo(c: *mut Container, ctx: *mut c_void, cb: Option<BlobCb>) -> HRESULT {
    let (Some(c), Some(cb)) = (container(c), cb) else { return E_INVALIDARG };
    each_blob(c, "", ctx, cb);
    S_OK
}

pub unsafe extern "system" fn XGameSaveEnumerateBlobInfoByName(c: *mut Container, prefix: *const u8, ctx: *mut c_void, cb: Option<BlobCb>) -> HRESULT {
    let (Some(c), Some(cb)) = (container(c), cb) else { return E_INVALIDARG };
    each_blob(c, &from_cstr_ptr(prefix).unwrap_or_default(), ctx, cb);
    S_OK
}

/// Blobs `names` (all when empty) laid out as the API returns them: `Blob` array, then the
/// names and data; pointers stored as offsets from the start (fixed up by the caller).
fn read_layout(dir: &Path, names: &[String]) -> Result<(Vec<u8>, u32), HRESULT> {
    let _mutation = SAVE_MUTATION.lock().unwrap_or_else(|e| e.into_inner());
    let all = blobs_of(dir);
    let picked: Vec<(String, PathBuf)> = if names.is_empty() {
        all.into_iter().map(|b| (b.0, b.1)).collect()
    } else {
        names
            .iter()
            .map(|n| {
                all.iter()
                    .find(|b| &b.0 == n)
                    .or_else(|| all.iter().find(|b| b.0.eq_ignore_ascii_case(n)))
                    .map(|b| (b.0.clone(), b.1.clone()))
                    .ok_or(E_GS_BLOB_NOT_FOUND)
            })
            .collect::<Result<_, _>>()?
    };
    let head = picked.len() * std::mem::size_of::<Blob>();
    let mut out = vec![0u8; head];
    for (k, (name, path)) in picked.iter().enumerate() {
        let data = std::fs::read(path).map_err(|_| E_GS_BLOB_NOT_FOUND)?;
        let name_at = out.len();
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        let data_at = out.len();
        out.extend_from_slice(&data);
        let e = k * std::mem::size_of::<Blob>();
        out[e..e + 8].copy_from_slice(&(name_at as u64).to_le_bytes());
        out[e + 8..e + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        out[e + 16..e + 24].copy_from_slice(&(data_at as u64).to_le_bytes());
    }
    Ok((out, picked.len() as u32))
}

fn valid_blob_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= GS_MAX_BLOB_NAME_SIZE
}

unsafe fn names_of(names: *const *const u8, count: u32) -> Result<Vec<String>, HRESULT> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if names.is_null() {
        return Err(E_INVALIDARG);
    }
    (0..count as usize)
        .map(|i| {
            let n = from_cstr_ptr(*names.add(i)).ok_or(E_INVALIDARG)?;
            if !valid_blob_name(&n) {
                return Err(E_INVALIDARG);
            }
            Ok(n)
        })
        .collect()
}

/// Turn the offsets of a copied layout into pointers.
unsafe fn fix_up(buf: *mut Blob, count: u32) {
    for k in 0..count as usize {
        let b = &mut *buf.add(k);
        b.info.name = (buf as *const u8).add(b.info.name as usize);
        b.data = (buf as *mut u8).add(b.data as usize);
    }
}

pub unsafe extern "system" fn XGameSaveReadBlobData(c: *mut Container, names: *const *const u8, count: *mut u32, size: usize, out: *mut Blob) -> HRESULT {
    let Some(c) = container(c) else { return E_INVALIDARG };
    if count.is_null() {
        return E_INVALIDARG;
    }
    let requested = match names_of(names, *count) {
        Ok(names) => names,
        Err(e) => return e,
    };
    let (bytes, n) = match read_layout(&c.dir, &requested) {
        Ok(x) => x,
        Err(e) => return e,
    };
    if out.is_null() || size < bytes.len() {
        return E_GS_PROVIDED_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len());
    fix_up(out, n);
    *count = n;
    S_OK
}

static READ_ID: u8 = 0;

pub unsafe extern "system" fn XGameSaveReadBlobDataAsync(c: *mut Container, names: *const *const u8, count: u32, b: *mut XAsyncBlock) -> HRESULT {
    let Some(c) = container(c) else { return E_INVALIDARG };
    let Ok(list) = names_of(names, count) else { return E_INVALIDARG };
    let dir = c.dir.clone();
    begin_with(b, &READ_ID as *const u8 as usize, move || {
        let (mut bytes, n) = read_layout(&dir, &list)?;
        bytes.splice(0..0, n.to_le_bytes()); // count travels in front, dropped by the Result
        Ok(bytes)
    })
}

pub unsafe extern "system" fn XGameSaveReadBlobDataResult(b: *mut XAsyncBlock, size: usize, out: *mut Blob, count: *mut u32) -> HRESULT {
    let mut need = 0usize;
    let hr = XAsyncGetResultSize(b, &mut need);
    if hr < 0 {
        return hr;
    }
    if need < std::mem::size_of::<u32>() {
        return E_FAIL;
    }
    let body_size = need - std::mem::size_of::<u32>();
    if out.is_null() || size < body_size {
        // Do not consume the one-shot XAsync result until the caller can
        // receive it; the documented insufficient-buffer retry must succeed.
        return E_GS_PROVIDED_BUFFER_TOO_SMALL;
    }
    let mut tmp = vec![0u8; need];
    let hr = XAsyncGetResult(b, &READ_ID as *const u8 as *const c_void, need, tmp.as_mut_ptr() as *mut c_void, std::ptr::null_mut());
    if hr < 0 {
        return hr;
    }
    let n = u32::from_le_bytes(tmp[..4].try_into().unwrap());
    let body = &tmp[4..];
    debug_assert_eq!(body.len(), body_size);
    std::ptr::copy_nonoverlapping(body.as_ptr(), out as *mut u8, body.len());
    fix_up(out, n);
    if !count.is_null() {
        *count = n;
    }
    S_OK
}

pub unsafe extern "system" fn XGameSaveCreateUpdate(c: *mut Container, display: *const u8, out: *mut *mut Update) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    let Some(c) = container(c) else { return E_INVALIDARG };
    let _ = &c.name;
    *out = Box::into_raw(Box::new(Update { magic: MAGIC_UPDATE, dir: c.dir.clone(), name: c.name.clone(), display: from_cstr_ptr(display), ops: Vec::new() }));
    S_OK
}

pub unsafe extern "system" fn XGameSaveCloseUpdate(u: *mut Update) {
    if u.as_ref().is_some_and(|u| u.magic == MAGIC_UPDATE) {
        (*u).magic = 0;
        drop(Box::from_raw(u));
    }
}

unsafe fn update<'a>(u: *mut Update) -> Option<&'a mut Update> {
    u.as_mut().filter(|u| u.magic == MAGIC_UPDATE)
}

pub unsafe extern "system" fn XGameSaveSubmitBlobWrite(u: *mut Update, name: *const u8, data: *const u8, size: usize) -> HRESULT {
    let (Some(u), Some(n)) = (update(u), from_cstr_ptr(name)) else { return E_INVALIDARG };
    if data.is_null() && size > 0 {
        return E_INVALIDARG;
    }
    if !valid_blob_name(&n) {
        return E_INVALIDARG;
    }
    if size > GS_MAX_BLOB_SIZE {
        return E_GS_UPDATE_TOO_BIG;
    }
    let bytes = if size == 0 { Vec::new() } else { std::slice::from_raw_parts(data, size).to_vec() };
    u.ops.push((n, Some(bytes)));
    S_OK
}

pub unsafe extern "system" fn XGameSaveSubmitBlobDelete(u: *mut Update, name: *const u8) -> HRESULT {
    let (Some(u), Some(n)) = (update(u), from_cstr_ptr(name)) else { return E_INVALIDARG };
    if !valid_blob_name(&n) {
        return E_INVALIDARG;
    }
    u.ops.push((n, None));
    S_OK
}

fn calculate_projected_quota(root: &Path, dir: &Path, ops: &[(String, Option<Vec<u8>>)]) -> Result<u64, HRESULT> {
    let total_used: u64 = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|e| blobs_of(&e.path()))
        .map(|b| b.2)
        .sum();

    let current_blobs = blobs_of(dir);

    let mut final_ops: Vec<(&str, Option<u64>)> = Vec::new();
    for (name, data) in ops {
        let size = data.as_ref().map(|d| d.len() as u64);
        if let Some(existing) = final_ops.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
            *existing = (name.as_str(), size);
        } else {
            final_ops.push((name.as_str(), size));
        }
    }

    let mut net_delta: i128 = 0;
    for (name, new_size) in final_ops {
        let old_size = current_blobs
            .iter()
            .find(|b| b.0.eq_ignore_ascii_case(name))
            .map(|b| b.2)
            .unwrap_or(0);
        match new_size {
            Some(s) => {
                net_delta += (s as i128) - (old_size as i128);
            }
            None => {
                net_delta -= old_size as i128;
            }
        }
    }

    let projected = (total_used as i128) + net_delta;
    if projected > QUOTA as i128 {
        return Err(E_GS_QUOTA_EXCEEDED);
    }

    Ok(projected.max(0) as u64)
}

fn apply(dir: &Path, name: &str, display: Option<&str>, ops: &[(String, Option<Vec<u8>>)]) -> HRESULT {
    let _mutation = SAVE_MUTATION.lock().unwrap_or_else(|e| e.into_inner());
    let Some(root) = dir.parent() else { return crate::util::E_FAIL };

    if let Err(hr) = calculate_projected_quota(root, dir, ops) {
        return hr;
    }

    let blobs = dir.join("b");
    if std::fs::create_dir_all(&blobs).is_err() {
        return crate::util::E_FAIL;
    }
    // A hashed folder cannot be decoded back to the container name; keep it in `name`.
    if dir
        .file_name()
        .is_some_and(|f| f.to_string_lossy().starts_with(LONG_PREFIX))
        && write_atomic(&dir.join("name"), name.as_bytes()).is_err()
    {
        return crate::util::E_FAIL;
    }
    // All or nothing (#324): every write goes to a temp file first; then the files the update
    // replaces or deletes are moved aside and the temps moved into place. A failure anywhere
    // puts the files moved aside back and removes the temps, so the container stays as it was.
    let mut writes: Vec<(PathBuf, PathBuf)> = Vec::new(); // (temp, destination)
    let mut deletes: Vec<PathBuf> = Vec::new();
    let mut aside: Vec<(PathBuf, PathBuf)> = Vec::new(); // (where it went, where it was)
    let mut placed = 0;
    let done = (|| -> std::io::Result<()> {
        if let Some(d) = display {
            let dest = dir.join("display");
            writes.push((write_temp(&dest, d.as_bytes())?, dest));
        }
        // the last op on a blob is the one that counts, as when they ran in order (names
        // compare without case: one file on NTFS)
        let mut seen: Vec<&str> = Vec::new();
        let last: Vec<&(String, Option<Vec<u8>>)> =
            ops.iter().rev().filter(|(name, _)| !seen.iter().any(|s| s.eq_ignore_ascii_case(name)) && { seen.push(name); true }).collect();
        for (name, data) in last.into_iter().rev() {
            let path = blobs.join(enc(name));
            match data {
                Some(d) => writes.push((write_temp(&path, d)?, path)),
                None => deletes.push(path),
            }
        }
        for dest in writes.iter().map(|(_, dest)| dest).chain(&deletes) {
            if dest.is_file() {
                let away = temp_path(dest)?;
                replace_file(dest, &away)?;
                aside.push((away, dest.clone()));
            }
        }
        for (tmp, dest) in &writes {
            replace_file(tmp, dest)?;
            placed += 1;
        }
        Ok(())
    })();
    if done.is_err() {
        for (tmp, dest) in &writes {
            let _ = std::fs::remove_file(tmp);
            if placed > 0 {
                let _ = std::fs::remove_file(dest);
                placed -= 1;
            }
        }
        for (away, dest) in &aside {
            if let Err(e) = replace_file(away, dest) {
                crate::xlog_warn!("XGameSave: a failed update could not put {} back from {}: {e}", dest.display(), away.display());
            }
        }
        return crate::util::E_FAIL;
    }
    for (away, _) in &aside {
        let _ = std::fs::remove_file(away);
    }
    S_OK
}

pub unsafe extern "system" fn XGameSaveSubmitUpdate(u: *mut Update) -> HRESULT {
    let Some(u) = update(u) else { return E_INVALIDARG };
    apply(&u.dir, &u.name, u.display.as_deref(), &u.ops)
}

static SUBMIT_ID: u8 = 0;

pub unsafe extern "system" fn XGameSaveSubmitUpdateAsync(u: *mut Update, b: *mut XAsyncBlock) -> HRESULT {
    let Some(u) = update(u) else { return E_INVALIDARG };
    let (dir, name, display, ops) = (u.dir.clone(), u.name.clone(), u.display.clone(), u.ops.clone());
    begin_with(b, &SUBMIT_ID as *const u8 as usize, move || match apply(&dir, &name, display.as_deref(), &ops) {
        S_OK => Ok(Vec::new()),
        e => Err(e),
    })
}

pub unsafe extern "system" fn XGameSaveSubmitUpdateResult(b: *mut XAsyncBlock) -> HRESULT {
    XAsyncGetResult(b, &SUBMIT_ID as *const u8 as *const c_void, 0, std::ptr::null_mut(), std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::taskqueue::{
        XTaskQueueCloseHandle, XTaskQueueCreate, XTaskQueueDispatch, XTaskQueueTerminate,
        MODE_MANUAL, PORT_WORK,
    };
    use crate::util::{E_ABORT, E_PENDING};

    const DELETE_TEST_NAME: &str = "slot/1";

    struct DeleteFixture {
        root: PathBuf,
        provider: *mut Provider,
        block: Box<XAsyncBlock>,
    }

    impl DeleteFixture {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("xcl-gamesave-delete-{}-{nonce}", std::process::id()));
            std::fs::create_dir(&root).unwrap();
            let mut fixture = Self {
                provider: Box::into_raw(Box::new(Provider { magic: MAGIC_PROVIDER, root: root.clone() })),
                root,
                block: Box::new(unsafe { std::mem::zeroed() }),
            };
            unsafe {
                assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut fixture.block.queue), S_OK);
            }
            std::fs::create_dir_all(fixture.container_dir().join("b")).unwrap();
            std::fs::write(fixture.blob_path(), b"synthetic save").unwrap();
            fixture
        }

        fn container_dir(&self) -> PathBuf {
            self.root.join(enc(DELETE_TEST_NAME))
        }

        fn blob_path(&self) -> PathBuf {
            self.container_dir().join("b").join("data")
        }
    }

    impl Drop for DeleteFixture {
        fn drop(&mut self) {
            unsafe {
                let queue = self.block.queue;
                if !queue.is_null() {
                    // Drain pending work before freeing the block, including after an assertion fails.
                    let _ = XTaskQueueTerminate(queue, true, std::ptr::null_mut(), 0);
                    XTaskQueueCloseHandle(queue);
                }
                XGameSaveCloseProvider(self.provider);
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn delete_container_async_runs_on_work_dispatch_with_owned_inputs() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut fixture = DeleteFixture::new();
        unsafe {
            let name = CString::new(DELETE_TEST_NAME).unwrap();
            let sibling = fixture.root.join("another-slot");
            std::fs::create_dir(&sibling).unwrap();
            assert_eq!(XGameSaveDeleteContainerAsync(fixture.provider, name.as_ptr().cast(), &mut *fixture.block), S_OK);
            drop(name);
            XGameSaveCloseProvider(fixture.provider);
            fixture.provider = std::ptr::null_mut();

            assert_eq!(XAsyncGetStatus(&mut *fixture.block, false), E_PENDING);
            assert_eq!(XGameSaveDeleteContainerResult(&mut *fixture.block), E_PENDING);
            assert_eq!(std::fs::read(fixture.blob_path()).unwrap().as_slice(), b"synthetic save");
            assert!(XTaskQueueDispatch(fixture.block.queue, PORT_WORK, 0));
            assert_eq!(XAsyncGetStatus(&mut *fixture.block, false), S_OK);
            assert_eq!(XGameSaveDeleteContainerResult(&mut *fixture.block), S_OK);
            assert!(sibling.is_dir());
        }
        assert!(!fixture.container_dir().exists());
        assert!(fixture.root.is_dir());
    }

    #[test]
    fn delete_container_async_terminated_queue_preserves_save() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut fixture = DeleteFixture::new();
        unsafe {
            let name = CString::new(DELETE_TEST_NAME).unwrap();
            let queue = fixture.block.queue;
            assert_eq!(XGameSaveDeleteContainerAsync(fixture.provider, name.as_ptr().cast(), &mut *fixture.block), S_OK);
            assert_eq!(XAsyncGetStatus(&mut *fixture.block, false), E_PENDING);
            assert_eq!(XTaskQueueTerminate(queue, true, std::ptr::null_mut(), 0), S_OK);
            assert_eq!(XAsyncGetStatus(&mut *fixture.block, false), E_ABORT);
            assert_eq!(XGameSaveDeleteContainerResult(&mut *fixture.block), E_ABORT);
            assert_eq!(std::fs::read(fixture.blob_path()).unwrap().as_slice(), b"synthetic save");

            // A new request on the same queue fails at scheduling.
            assert_eq!(XGameSaveDeleteContainerAsync(fixture.provider, name.as_ptr().cast(), &mut *fixture.block), S_OK);
            assert_eq!(XAsyncGetStatus(&mut *fixture.block, false), E_ABORT);
            assert_eq!(XGameSaveDeleteContainerResult(&mut *fixture.block), E_ABORT);
            assert!(!XTaskQueueDispatch(queue, PORT_WORK, 0));
        }
        assert_eq!(std::fs::read(fixture.blob_path()).unwrap().as_slice(), b"synthetic save");
    }

    #[test]
    fn delete_container_invalid_arguments_preserve_provider_root() {
        let _g = ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut fixture = DeleteFixture::new();
        unsafe {
            let name = CString::new(DELETE_TEST_NAME).unwrap();
            let too_long = CString::new("a".repeat(257)).unwrap();
            assert_eq!(XGameSaveDeleteContainerAsync(fixture.provider, name.as_ptr().cast(), std::ptr::null_mut()), E_INVALIDARG);
            for (provider, name, expected) in [
                (std::ptr::null_mut(), name.as_ptr().cast(), E_INVALIDARG),
                (fixture.provider, std::ptr::null(), E_INVALIDARG),
                (fixture.provider, b"\0".as_ptr(), E_GS_INVALID_CONTAINER_NAME),
                (fixture.provider, too_long.as_ptr().cast(), E_GS_INVALID_CONTAINER_NAME),
            ] {
                assert_eq!(XGameSaveDeleteContainer(provider, name), expected);
                assert_eq!(XGameSaveDeleteContainerAsync(provider, name, &mut *fixture.block), S_OK);
                assert_eq!(XAsyncGetStatus(&mut *fixture.block, false), E_PENDING);
                assert!(XTaskQueueDispatch(fixture.block.queue, PORT_WORK, 0));
                assert_eq!(XAsyncGetStatus(&mut *fixture.block, false), expected);
                assert_eq!(XGameSaveDeleteContainerResult(&mut *fixture.block), expected);
                assert_eq!(std::fs::read(fixture.blob_path()).unwrap().as_slice(), b"synthetic save");
            }
        }
        assert!(fixture.root.is_dir());
    }

    unsafe extern "system" fn count_containers(i: *const ContainerInfo, ctx: *mut c_void) -> bool {
        let v = &mut *(ctx as *mut Vec<(String, String, u32, u64)>);
        let i = &*i;
        v.push((from_cstr_ptr(i.name).unwrap(), from_cstr_ptr(i.display_name).unwrap(), i.blob_count, i.total_size));
        true
    }

    #[test]
    fn read_blob_names_require_a_complete_counted_array() {
        // Another test temporarily routes the process-global XAsync entry points
        // through the system runtime. Keep this invalid-block status check on the
        // local implementation, like every other test that exercises XAsync.
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut provider = std::ptr::null_mut();
            assert_eq!(
                XGameSaveInitializeProvider(
                    std::ptr::null_mut(),
                    b"xcl-read-name-validation\0".as_ptr(),
                    false,
                    &mut provider,
                ),
                S_OK
            );
            let _ = XGameSaveDeleteContainer(provider, b"slot\0".as_ptr());
            let mut container = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(provider, b"slot\0".as_ptr(), &mut container), S_OK);
            let mut update = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(container, std::ptr::null(), &mut update), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(update, b"data\0".as_ptr(), b"secret".as_ptr(), 6), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(update), S_OK);
            XGameSaveCloseUpdate(update);

            let mut count = 1u32;
            let mut buffer = vec![0u64; 32];
            assert_eq!(
                XGameSaveReadBlobData(container, std::ptr::null(), &mut count, 256, buffer.as_mut_ptr() as *mut Blob),
                E_INVALIDARG
            );
            assert_eq!(count, 1, "invalid filter changed the caller's count");
            let null_name = [std::ptr::null()];
            assert_eq!(
                XGameSaveReadBlobData(container, null_name.as_ptr(), &mut count, 256, buffer.as_mut_ptr() as *mut Blob),
                E_INVALIDARG
            );

            let mut async_block: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveReadBlobDataAsync(container, std::ptr::null(), 1, &mut async_block), E_INVALIDARG);
            assert_eq!(XAsyncGetStatus(&mut async_block, false), E_INVALIDARG, "invalid async filter started an operation");
            assert_eq!(XGameSaveReadBlobDataAsync(container, null_name.as_ptr(), 1, &mut async_block), E_INVALIDARG);

            // The documented empty filter remains the read-all form.
            count = 0;
            assert_eq!(
                XGameSaveReadBlobData(container, std::ptr::null(), &mut count, 256, buffer.as_mut_ptr() as *mut Blob),
                S_OK
            );
            assert_eq!(count, 1);

            XGameSaveCloseContainer(container);
            assert_eq!(XGameSaveDeleteContainer(provider, b"slot\0".as_ptr()), S_OK);
            XGameSaveCloseProvider(provider);
        }
    }

    unsafe extern "system" fn blob_names(i: *const BlobInfo, ctx: *mut c_void) -> bool {
        (*(ctx as *mut Vec<(String, u32)>)).push((from_cstr_ptr((*i).name).unwrap(), (*i).size));
        true
    }

    /// Names Windows does not keep as file names (device names, a trailing dot or space,
    /// "." / "..") come back as they were written.
    /// #325: '%' followed by a multi-byte character stays literal instead of panicking, a sign is
    /// not a hex digit, and every name `enc` writes still decodes to itself.
    #[test]
    fn dec_keeps_bad_escapes_literal() {
        assert_eq!(dec("%a\u{e9}"), "%a\u{e9}");
        assert_eq!(dec("%\u{e9}a"), "%\u{e9}a");
        assert_eq!(dec("%+1"), "%+1");
        assert_eq!(dec("%-1x"), "%-1x");
        assert_eq!(dec("a%4"), "a%4");
        assert_eq!(dec("%41%42c"), "ABc");
        assert_eq!(dec("%c3%A9"), "\u{e9}");
        for name in ["%a\u{e9}", "sauvegarde \u{e9}t\u{e9}", "\u{4fdd}\u{5b58}1", "100%", "%+1", "save.", "aux"] {
            assert_eq!(dec(&enc(name)), name);
        }
    }

    /// #325: a container folder named "%a\u{e9}" (made outside XCL) is enumerated, not a panic across
    /// the `extern "system"` call that aborts the process.
    #[test]
    fn enumerating_a_container_with_a_bad_escape_does_not_abort() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-325-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let dir = (*p).root.join("%a\u{e9}");
            std::fs::create_dir_all(dir.join("b")).unwrap();
            std::fs::write(dir.join("b").join("data"), b"x").unwrap();
            let mut list: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(XGameSaveEnumerateContainerInfo(p, &mut list as *mut _ as *mut c_void, Some(count_containers)), S_OK);
            assert!(list.iter().any(|c| c.0 == "%a\u{e9}"), "{list:?}");
            std::fs::remove_dir_all(&dir).unwrap();
            XGameSaveCloseProvider(p);
        }
    }

    #[test]
    fn names_windows_would_change_round_trip() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-names-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            for container_name in ["aux", "..", "slot."] {
                let cname = format!("{container_name}\0");
                let mut c = std::ptr::null_mut();
                assert_eq!(XGameSaveCreateContainer(p, cname.as_ptr(), &mut c), S_OK);
                let mut u = std::ptr::null_mut();
                assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u), S_OK);
                let blobs: [&[u8]; 5] = [b"aux\0", b"CON.dat\0", b"save.\0", b"save \0", b"nul .txt\0"];
                for (i, name) in blobs.iter().enumerate() {
                    assert_eq!(XGameSaveSubmitBlobWrite(u, name.as_ptr(), b"0123".as_ptr(), i + 1), S_OK);
                }
                assert_eq!(XGameSaveSubmitUpdate(u), S_OK, "{container_name}");
                XGameSaveCloseUpdate(u);
                let mut names: Vec<(String, u32)> = Vec::new();
                assert_eq!(XGameSaveEnumerateBlobInfo(c, &mut names as *mut _ as *mut c_void, Some(blob_names)), S_OK);
                names.sort();
                let want: Vec<(String, u32)> = vec![("CON.dat".into(), 2), ("aux".into(), 1), ("nul .txt".into(), 5), ("save ".into(), 4), ("save.".into(), 3)];
                assert_eq!(names, want, "{container_name}");
                XGameSaveCloseContainer(c);
            }
            let mut list: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(XGameSaveEnumerateContainerInfo(p, &mut list as *mut _ as *mut c_void, Some(count_containers)), S_OK);
            let mut got: Vec<String> = list.into_iter().map(|c| c.0).collect();
            got.sort();
            assert_eq!(got, ["..", "aux", "slot."]);
            for name in [&b"aux\0"[..], b"..\0", b"slot.\0", b".\0"] {
                assert_eq!(XGameSaveDeleteContainer(p, name.as_ptr()), S_OK);
            }
            // deleting "." / ".." deleted only those containers, not the provider's folder or its parent
            assert!((*p).root.is_dir());
            XGameSaveCloseProvider(p);
        }
    }

    /// A container name whose `enc()` exceeds the path-component budget is stored under a
    /// hashed folder; the real name round-trips through enumeration, lookup and delete.
    #[test]
    fn long_container_names_round_trip_through_a_hashed_folder() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-long-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let names = ["a".repeat(256), format!("{}x", "a/".repeat(85))];
            for name in &names {
                let cname = format!("{name}\0");
                let mut c = std::ptr::null_mut();
                assert_eq!(XGameSaveCreateContainer(p, cname.as_ptr(), &mut c), S_OK);
                let mut u = std::ptr::null_mut();
                assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u), S_OK);
                assert_eq!(XGameSaveSubmitBlobWrite(u, b"data\0".as_ptr(), b"v\0".as_ptr(), 2), S_OK);
                assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
                XGameSaveCloseUpdate(u);
                let dir = (*c).dir.clone();
                assert!(dir.file_name().unwrap().to_string_lossy().starts_with(LONG_PREFIX), "{name}");
                assert_eq!(std::fs::read_to_string(dir.join("name")).unwrap(), *name);
                XGameSaveCloseContainer(c);
            }
            let mut list: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(XGameSaveEnumerateContainerInfo(p, &mut list as *mut _ as *mut c_void, Some(count_containers)), S_OK);
            let mut got: Vec<String> = list.into_iter().map(|c| c.0).collect();
            got.sort();
            let mut want = names.to_vec();
            want.sort();
            assert_eq!(got, want);
            for name in &names {
                let cname = format!("{name}\0");
                let mut one: Vec<(String, String, u32, u64)> = Vec::new();
                assert_eq!(
                    XGameSaveGetContainerInfo(p, cname.as_ptr(), &mut one as *mut _ as *mut c_void, Some(count_containers)),
                    S_OK
                );
                assert_eq!(one.len(), 1);
                assert_eq!(one[0].0, *name);
                assert_eq!(XGameSaveDeleteContainer(p, cname.as_ptr()), S_OK);
            }
            let mut left: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(XGameSaveEnumerateContainerInfo(p, &mut left as *mut _ as *mut c_void, Some(count_containers)), S_OK);
            assert!(left.is_empty());
            // Hashed folders hash the lowercased name, so delete/lookup with another
            // case still finds the same folder — like every other name on NTFS.
            let upper = "A".repeat(256);
            let cname = format!("{upper}\0");
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, cname.as_ptr(), &mut c), S_OK);
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"data\0".as_ptr(), b"v\0".as_ptr(), 2), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
            XGameSaveCloseUpdate(u);
            XGameSaveCloseContainer(c);
            let lower = format!("{}\0", "a".repeat(256));
            let mut one: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(
                XGameSaveGetContainerInfo(p, lower.as_ptr(), &mut one as *mut _ as *mut c_void, Some(count_containers)),
                S_OK
            );
            assert_eq!(one.len(), 1);
            assert_eq!(XGameSaveDeleteContainer(p, lower.as_ptr()), S_OK);
            let mut left2: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(XGameSaveEnumerateContainerInfo(p, &mut left2 as *mut _ as *mut c_void, Some(count_containers)), S_OK);
            assert!(left2.is_empty(), "{left2:?}");

            // A save written before hashed folders existed keeps its long enc() folder
            // (201 chars: over the hash threshold, still a valid path component).
            let legacy = format!("{}x", "a/".repeat(50)); // name 101 chars, enc() 201
            assert!(enc(&legacy).len() > MAX_CONTAINER_ENC);
            let legacy_dir = (*p).root.join(enc(&legacy));
            std::fs::create_dir_all(legacy_dir.join("b")).unwrap();
            std::fs::write(legacy_dir.join("b").join(enc("old")), b"v").unwrap();
            let cname = format!("{legacy}\0");
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, cname.as_ptr(), &mut c), S_OK);
            assert_eq!((*c).dir, legacy_dir);
            XGameSaveCloseContainer(c);
            let mut one: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(
                XGameSaveGetContainerInfo(p, cname.as_ptr(), &mut one as *mut _ as *mut c_void, Some(count_containers)),
                S_OK
            );
            assert_eq!(one[0].0, legacy);
            assert_eq!(XGameSaveDeleteContainer(p, cname.as_ptr()), S_OK);

            XGameSaveCloseProvider(p);
        }
    }

    #[test]
    fn containers_blobs_round_trip() {
    let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"slot/1\0".as_ptr(), &mut c), S_OK);
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, b"Save 1\0".as_ptr(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"data\0".as_ptr(), b"hello".as_ptr(), 5), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"meta\0".as_ptr(), b"xy".as_ptr(), 2), S_OK);
            // async submit
            let mut blk: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveSubmitUpdateAsync(u, &mut blk), S_OK);
            assert_eq!(XAsyncGetStatus(&mut blk, true), S_OK);
            assert_eq!(XGameSaveSubmitUpdateResult(&mut blk), S_OK);
            XGameSaveCloseUpdate(u);
            let mut list: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(XGameSaveEnumerateContainerInfo(p, &mut list as *mut _ as *mut c_void, Some(count_containers)), S_OK);
            assert_eq!(list, vec![("slot/1".to_string(), "Save 1".to_string(), 2, 7)]);
            // read everything (sync): too small first
            let mut n = 0u32;
            let mut buf = vec![0u64; 32];
            assert_eq!(XGameSaveReadBlobData(c, std::ptr::null(), &mut n, 8, buf.as_mut_ptr() as *mut Blob), E_GS_PROVIDED_BUFFER_TOO_SMALL);
            n = 0;
            assert_eq!(XGameSaveReadBlobData(c, std::ptr::null(), &mut n, 256, buf.as_mut_ptr() as *mut Blob), S_OK);
            assert_eq!(n, 2);
            let b0 = &*(buf.as_ptr() as *const Blob);
            assert_eq!((from_cstr_ptr(b0.info.name).unwrap(), b0.info.size), ("data".to_string(), 5));
            assert_eq!(std::slice::from_raw_parts(b0.data, 5), b"hello");
            // read one by name (async), then delete it
            let names = [b"meta\0".as_ptr()];
            let mut blk: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveReadBlobDataAsync(c, names.as_ptr(), 1, &mut blk), S_OK);
            assert_eq!(XAsyncGetStatus(&mut blk, true), S_OK);
            let mut buf2 = vec![0u64; 16];
            let mut n2 = u32::MAX;
            assert_eq!(
                XGameSaveReadBlobDataResult(&mut blk, 1, buf2.as_mut_ptr() as *mut Blob, &mut n2),
                E_GS_PROVIDED_BUFFER_TOO_SMALL
            );
            assert_eq!(n2, u32::MAX, "failed result changed the output count");
            assert_eq!(XGameSaveReadBlobDataResult(&mut blk, 128, buf2.as_mut_ptr() as *mut Blob, &mut n2), S_OK);
            let b1 = &*(buf2.as_ptr() as *const Blob);
            assert_eq!((n2, std::slice::from_raw_parts(b1.data, 2)), (1, &b"xy"[..]));
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobDelete(u, b"meta\0".as_ptr()), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
            XGameSaveCloseUpdate(u);
            let names = [b"meta\0".as_ptr()];
            let mut n3 = 1u32;
            assert_eq!(XGameSaveReadBlobData(c, names.as_ptr(), &mut n3, 256, buf.as_mut_ptr() as *mut Blob), E_GS_BLOB_NOT_FOUND);
            let mut q = 0i64;
            assert_eq!(XGameSaveGetRemainingQuota(p, &mut q), S_OK);
            assert_eq!(q, QUOTA - 5);
            XGameSaveCloseContainer(c);
            assert_eq!(XGameSaveDeleteContainer(p, b"slot/1\0".as_ptr()), S_OK);
            let mut gone: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(XGameSaveGetContainerInfo(p, b"slot/1\0".as_ptr(), &mut gone as *mut _ as *mut c_void, Some(count_containers)), S_OK);
            assert!(gone.is_empty(), "{gone:?}");
            let root = (*p).root.clone();
            XGameSaveCloseProvider(p);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    /// An update is all or nothing (#324): when one of its blobs cannot be written, the blobs
    /// and the display name it wrote before stay as they were, and no temp file is left.
    #[test]
    fn a_failed_update_leaves_the_container_as_it_was() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-324-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"slot/324\0".as_ptr(), &mut c), S_OK);
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, b"Old\0".as_ptr(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"a\0".as_ptr(), b"old".as_ptr(), 3), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
            XGameSaveCloseUpdate(u);
            // the second blob of the next update cannot be written: a folder is in its place
            let dir = (*p).root.join(enc("slot/324"));
            std::fs::create_dir_all(dir.join("b").join(enc("b"))).unwrap();
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, b"New\0".as_ptr(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"a\0".as_ptr(), b"new".as_ptr(), 3), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"b\0".as_ptr(), b"x".as_ptr(), 1), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), crate::util::E_FAIL);
            XGameSaveCloseUpdate(u);
            assert_eq!(std::fs::read(dir.join("b").join(enc("a"))).unwrap(), b"old");
            assert_eq!(std::fs::read_to_string(dir.join("display")).unwrap(), "Old");
            let temps: Vec<String> = [dir.clone(), dir.join("b")]
                .iter()
                .flat_map(|d| std::fs::read_dir(d).unwrap().flatten())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(TMP_PREFIX))
                .collect();
            assert!(temps.is_empty(), "temp files left behind: {temps:?}");
            // ops on one blob count in their order: write then delete, delete then write
            std::fs::remove_dir(dir.join("b").join(enc("b"))).unwrap();
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"b\0".as_ptr(), b"x".as_ptr(), 1), S_OK);
            assert_eq!(XGameSaveSubmitBlobDelete(u, b"b\0".as_ptr()), S_OK);
            assert_eq!(XGameSaveSubmitBlobDelete(u, b"a\0".as_ptr()), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"a\0".as_ptr(), b"z".as_ptr(), 1), S_OK);
            // one file on NTFS whatever the case, so the delete of META is the last op on Meta
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"Meta\0".as_ptr(), b"m".as_ptr(), 1), S_OK);
            assert_eq!(XGameSaveSubmitBlobDelete(u, b"META\0".as_ptr()), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
            XGameSaveCloseUpdate(u);
            assert!(!dir.join("b").join(enc("b")).exists());
            assert!(!dir.join("b").join(enc("Meta")).exists());
            assert_eq!(std::fs::read(dir.join("b").join(enc("a"))).unwrap(), b"z");
            assert_eq!(std::fs::read_to_string(dir.join("display")).unwrap(), "Old");
            XGameSaveCloseContainer(c);
            let root = (*p).root.clone();
            XGameSaveCloseProvider(p);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn atomic_writes_preserve_xcltmp_blobs_and_skip_leftover_temps() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-atomic-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let root = (*p).root.clone();
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"slot\0".as_ptr(), &mut c), S_OK);

            // Write a `.xcltmp` blob, a `~xcltmp.*` blob, and a sibling `.dat` blob.
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, b"Atomic\0".as_ptr(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"save.xcltmp\0".as_ptr(), b"orig-xcltmp".as_ptr(), 11), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"~xcltmp.1.0\0".as_ptr(), b"orig-tilde".as_ptr(), 10), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"save.dat\0".as_ptr(), b"dat-v1".as_ptr(), 6), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
            XGameSaveCloseUpdate(u);

            // Overwriting `save.dat` must never clobber `save.xcltmp`, and overwriting `save.xcltmp` must succeed.
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"save.dat\0".as_ptr(), b"dat-v2".as_ptr(), 6), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"save.xcltmp\0".as_ptr(), b"next-xcltmp".as_ptr(), 11), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
            XGameSaveCloseUpdate(u);

            // A simulated crash-leftover temp file in `b/` must never be enumerated as a blob.
            let blobs_dir = (*c).dir.join("b");
            let leftover = blobs_dir.join(format!("{TMP_PREFIX}99999.7"));
            std::fs::write(&leftover, b"partial").unwrap();

            let mut listed: Vec<(String, u32)> = Vec::new();
            assert_eq!(XGameSaveEnumerateBlobInfo(c, &mut listed as *mut _ as *mut c_void, Some(blob_names)), S_OK);
            assert_eq!(
                listed,
                vec![
                    ("save.dat".to_string(), 6),
                    ("save.xcltmp".to_string(), 11),
                    ("~xcltmp.1.0".to_string(), 10),
                ]
            );

            // Verify all three blobs retained their exact contents.
            let names = [b"save.dat\0".as_ptr(), b"save.xcltmp\0".as_ptr(), b"~xcltmp.1.0\0".as_ptr()];
            let mut count = 3u32;
            let mut buf = vec![0u64; 64];
            assert_eq!(XGameSaveReadBlobData(c, names.as_ptr(), &mut count, 512, buf.as_mut_ptr() as *mut Blob), S_OK);
            assert_eq!(count, 3);
            let b0 = &*(buf.as_ptr() as *const Blob);
            let b1 = &*(buf.as_ptr() as *const Blob).add(1);
            let b2 = &*(buf.as_ptr() as *const Blob).add(2);
            assert_eq!(std::slice::from_raw_parts(b0.data, b0.info.size as usize), b"dat-v2");
            assert_eq!(std::slice::from_raw_parts(b1.data, b1.info.size as usize), b"next-xcltmp");
            assert_eq!(std::slice::from_raw_parts(b2.data, b2.info.size as usize), b"orig-tilde");

            // Failed atomic replace (dest is an existing directory) removes its temp file.
            let _ = std::fs::remove_file(&leftover);
            let blocker = blobs_dir.join("dir_blocker");
            std::fs::create_dir_all(&blocker).unwrap();
            let ops = [("dir_blocker".to_string(), Some(b"cannot-replace-dir".to_vec()))];
            assert_eq!(apply(blobs_dir.parent().unwrap(), None, &ops), crate::util::E_FAIL);
            let remaining_tmps: Vec<_> = std::fs::read_dir(&blobs_dir)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(TMP_PREFIX))
                .collect();
            assert!(remaining_tmps.is_empty(), "temp file leaked on failed write_atomic: {remaining_tmps:?}");

            XGameSaveCloseContainer(c);
            XGameSaveCloseProvider(p);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn case_insensitive_blob_and_container_matching_on_ntfs() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-case-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let root = (*p).root.clone();

            // Write container "SlotMixed" and blob "Meta"
            let mut c1 = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"SlotMixed\0".as_ptr(), &mut c1), S_OK);
            let mut u1 = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c1, b"Display 1\0".as_ptr(), &mut u1), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u1, b"Meta\0".as_ptr(), b"old".as_ptr(), 3), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u1), S_OK);
            XGameSaveCloseUpdate(u1);
            XGameSaveCloseContainer(c1);

            // Re-open with lowercase name, overwrite "meta", then query/read using different casings.
            let mut c2 = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"slotmixed\0".as_ptr(), &mut c2), S_OK);
            let mut u2 = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c2, b"Display 2\0".as_ptr(), &mut u2), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u2, b"meta\0".as_ptr(), b"fresh".as_ptr(), 5), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u2), S_OK);
            XGameSaveCloseUpdate(u2);

            for query_name in [b"meta\0".as_ptr(), b"Meta\0".as_ptr(), b"META\0".as_ptr()] {
                let names = [query_name];
                let mut count = 1u32;
                let mut buf = vec![0u64; 32];
                assert_eq!(XGameSaveReadBlobData(c2, names.as_ptr(), &mut count, 256, buf.as_mut_ptr() as *mut Blob), S_OK);
                assert_eq!(count, 1);
                let b0 = &*(buf.as_ptr() as *const Blob);
                assert_eq!(std::slice::from_raw_parts(b0.data, b0.info.size as usize), b"fresh");
            }

            let mut prefixed_blobs: Vec<(String, u32)> = Vec::new();
            assert_eq!(
                XGameSaveEnumerateBlobInfoByName(c2, b"me\0".as_ptr(), &mut prefixed_blobs as *mut _ as *mut c_void, Some(blob_names)),
                S_OK
            );
            assert_eq!(prefixed_blobs.len(), 1);
            assert!(prefixed_blobs[0].0.eq_ignore_ascii_case("meta"));

            let mut exact_c: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(
                XGameSaveGetContainerInfo(p, b"slotmixed\0".as_ptr(), &mut exact_c as *mut _ as *mut c_void, Some(count_containers)),
                S_OK
            );
            assert_eq!(exact_c.len(), 1);
            assert_eq!(exact_c[0].1, "Display 2");

            let mut prefix_c: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(
                XGameSaveEnumerateContainerInfoByName(p, b"slot\0".as_ptr(), &mut prefix_c as *mut _ as *mut c_void, Some(count_containers)),
                S_OK
            );
            assert_eq!(prefix_c.len(), 1);

            XGameSaveCloseContainer(c2);
            XGameSaveCloseProvider(p);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn initialize_provider_result_rejects_null_out_without_consuming_async() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-null-out-{}\0", std::process::id());
            let mut blk: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveInitializeProviderAsync(std::ptr::null_mut(), cfg.as_ptr(), false, &mut blk), S_OK);
            assert_eq!(XAsyncGetStatus(&mut blk, true), S_OK);

            // Passing null `out` must fail with E_INVALIDARG and preserve the completed async result.
            assert_eq!(XGameSaveInitializeProviderResult(&mut blk, std::ptr::null_mut()), E_INVALIDARG);

            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProviderResult(&mut blk, &mut p), S_OK);
            assert!(!p.is_null());
            let root = (*p).root.clone();
            XGameSaveCloseProvider(p);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn delete_container_rejects_empty_name_without_wiping_provider_root() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-empty-del-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let root = (*p).root.clone();
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"slot1\0".as_ptr(), &mut c), S_OK);
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, b"Save 1\0".as_ptr(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"data\0".as_ptr(), b"keepme".as_ptr(), 6), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
            XGameSaveCloseUpdate(u);

            // Empty and overlong names must be rejected without touching the provider root or any container.
            assert_eq!(XGameSaveDeleteContainer(p, b"\0".as_ptr()), E_GS_INVALID_CONTAINER_NAME);
            let overlong = format!("{}\0", "a".repeat(257));
            assert_eq!(XGameSaveDeleteContainer(p, overlong.as_ptr()), E_GS_INVALID_CONTAINER_NAME);

            let mut blk: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveDeleteContainerAsync(p, b"\0".as_ptr(), &mut blk), S_OK);
            assert_eq!(XAsyncGetStatus(&mut blk, true), E_GS_INVALID_CONTAINER_NAME);
            assert_eq!(XGameSaveDeleteContainerResult(&mut blk), E_GS_INVALID_CONTAINER_NAME);

            assert!(root.is_dir(), "provider root must still exist");
            let mut list: Vec<(String, String, u32, u64)> = Vec::new();
            assert_eq!(XGameSaveEnumerateContainerInfo(p, &mut list as *mut _ as *mut c_void, Some(count_containers)), S_OK);
            assert_eq!(list, vec![("slot1".to_string(), "Save 1".to_string(), 1, 6)]);

            let names = [b"data\0".as_ptr()];
            let mut n = 1u32;
            let mut buf = vec![0u64; 32];
            assert_eq!(XGameSaveReadBlobData(c, names.as_ptr(), &mut n, 256, buf.as_mut_ptr() as *mut Blob), S_OK);
            assert_eq!(n, 1);
            let b0 = &*(buf.as_ptr() as *const Blob);
            assert_eq!(std::slice::from_raw_parts(b0.data, 6), b"keepme");

            XGameSaveCloseContainer(c);
            XGameSaveCloseProvider(p);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn blob_enumeration_sees_one_submitted_update() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        struct Temp(PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
        }
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = Temp(std::env::temp_dir().join(format!("xcl-blob-enum-{}-{nonce}", std::process::id())));
        let dir = root.0.join("slot");
        std::fs::create_dir_all(&dir).unwrap();
        let names: Vec<String> = (0..16).map(|n| format!("blob{n:02}")).collect();
        let ops = |size| names.iter().map(|name| (name.clone(), Some(vec![0u8; size]))).collect::<Vec<_>>();
        assert_eq!(apply(&dir, "slot", None, &ops(8)), S_OK);

        let mut container = Container { magic: MAGIC_CONTAINER, name: "slot".into(), dir: dir.clone() };
        let address = &mut container as *mut Container as usize;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let start = std::sync::Arc::new(std::sync::Barrier::new(2));
        let reader_done = done.clone();
        let reader_start = start.clone();
        let reader = std::thread::spawn(move || {
            reader_start.wait();
            let mut reads = 0usize;
            let mut mixed = 0usize;
            while !reader_done.load(std::sync::atomic::Ordering::Relaxed) {
                let mut rows: Vec<(String, u32)> = Vec::new();
                let hr = unsafe { XGameSaveEnumerateBlobInfo(address as *mut Container, &mut rows as *mut _ as *mut c_void, Some(blob_names)) };
                assert_eq!(hr, S_OK);
                reads += 1;
                if rows.len() != 16 || rows.iter().any(|(_, size)| *size != rows[0].1) {
                    mixed += 1;
                }
            }
            (reads, mixed)
        });
        start.wait();
        for round in 0..30 {
            assert_eq!(apply(&dir, "slot", None, &ops(if round % 2 == 0 { 16 } else { 8 })), S_OK);
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let (reads, mixed) = reader.join().unwrap();
        assert!(reads > 0, "the reader never enumerated blobs");
        assert_eq!(mixed, 0, "{mixed} of {reads} enumerations mixed submitted updates");
    }

    /// An update replaces its blobs one file at a time: a read in between must not return some of
    /// them from before the update and some from after it, nor make the update fail by having one
    /// of the files open.
    #[test]
    fn reads_during_an_update_see_all_of_it_or_none() {
        let root = std::env::temp_dir().join(format!("xcl-gamesave-read-update-{}", std::process::id()));
        let dir = root.join("slot");
        let names = ["head".to_string(), "tail".to_string()];
        let write = |v: u8| {
            let ops: Vec<_> = names.iter().map(|n| (n.clone(), Some(vec![v; 4096]))).collect();
            apply(&dir, "slot", None, &ops)
        };
        assert_eq!(write(0), S_OK);
        let done = std::sync::atomic::AtomicBool::new(false);
        let (failed_updates, (reads, mixed, failed_reads)) = std::thread::scope(|s| {
            let reader = s.spawn(|| {
                let (mut reads, mut mixed, mut failed) = (0u32, 0u32, 0u32);
                loop {
                    reads += 1;
                    match read_layout(&dir, &names) {
                        Ok((bytes, 2)) => {
                            let data_at = |k: usize| k * std::mem::size_of::<Blob>() + std::mem::offset_of!(Blob, data);
                            let first = |k: usize| bytes[u64::from_le_bytes(bytes[data_at(k)..data_at(k) + 8].try_into().unwrap()) as usize];
                            mixed += (first(0) != first(1)) as u32;
                        }
                        _ => failed += 1,
                    }
                    if done.load(Ordering::Relaxed) {
                        return (reads, mixed, failed);
                    }
                }
            });
            let failed = (1..=30u8).filter(|&v| write(v) != S_OK).count();
            done.store(true, Ordering::Relaxed);
            (failed, reader.join().unwrap())
        });
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!((failed_updates, mixed, failed_reads), (0, 0, 0), "failed updates, mixed reads, failed reads of {reads}");
    }

    #[test]
    fn gamesave_functions_null_outptrs_on_failure() {
        unsafe {
            let mut p_out = 0xDEADBEEF as *mut Provider;
            let mut b_bad: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveInitializeProviderResult(&mut b_bad, &mut p_out), E_INVALIDARG);
            assert_eq!(p_out, std::ptr::null_mut());

            let mut c_out = 0xDEADBEEF as *mut Container;
            assert_eq!(XGameSaveCreateContainer(std::ptr::null_mut(), b"slot\0".as_ptr(), &mut c_out), E_INVALIDARG);
            assert_eq!(c_out, std::ptr::null_mut());

            let mut u_out = 0xDEADBEEF as *mut Update;
            assert_eq!(XGameSaveCreateUpdate(std::ptr::null_mut(), std::ptr::null(), &mut u_out), E_INVALIDARG);
            assert_eq!(u_out, std::ptr::null_mut());
        }
    }

    #[test]
    fn url_encoding_safety_dec_non_ascii_and_malformed_percent_does_not_panic() {
        // Must never panic on non-ASCII characters or malformed percent escapes
        assert_eq!(dec("test%€"), "test%€");
        assert_eq!(dec("test%ü"), "test%ü");
        assert_eq!(dec("%"), "%");
        assert_eq!(dec("%2"), "%2");
        assert_eq!(dec("%2G"), "%2G");
        assert_eq!(dec("%+1"), "%+1");
        assert_eq!(dec("%20"), " ");
        assert_eq!(dec("%C3%A9"), "é");
        assert_eq!(dec("prefix%2Fmiddle%20suffix"), "prefix/middle suffix");
    }

    #[test]
    fn url_encoding_device_names_and_windows_specials() {
        for special in ["CONIN$", "CONOUT$", "CLOCK$", "CONIN$.txt", "CONOUT$.dat", "aux", "PRN", "..", "."] {
            let encoded = enc(special);
            assert_ne!(encoded, special);
            assert_eq!(dec(&encoded), special, "failed to roundtrip: {special}");
        }
    }

    #[test]
    fn blob_name_bounds_and_empty_validation() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-blob-bounds-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let root = (*p).root.clone();
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"slot\0".as_ptr(), &mut c), S_OK);
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u), S_OK);

            // Empty blob name is rejected
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"\0".as_ptr(), b"data".as_ptr(), 4), E_INVALIDARG);
            assert_eq!(XGameSaveSubmitBlobDelete(u, b"\0".as_ptr()), E_INVALIDARG);

            // Overlong blob name (> 1024) is rejected
            let long_name = format!("{}\0", "a".repeat(1025));
            assert_eq!(XGameSaveSubmitBlobWrite(u, long_name.as_ptr(), b"data".as_ptr(), 4), E_INVALIDARG);
            assert_eq!(XGameSaveSubmitBlobDelete(u, long_name.as_ptr()), E_INVALIDARG);

            // Blob size > 16 MiB returns E_GS_UPDATE_TOO_BIG
            assert_eq!(
                XGameSaveSubmitBlobWrite(u, b"big\0".as_ptr(), b"a".as_ptr(), GS_MAX_BLOB_SIZE + 1),
                E_GS_UPDATE_TOO_BIG
            );

            // Empty blob name in read is rejected
            let empty_name = [b"\0".as_ptr()];
            let mut count = 1u32;
            let mut buf = vec![0u64; 16];
            assert_eq!(
                XGameSaveReadBlobData(c, empty_name.as_ptr(), &mut count, 128, buf.as_mut_ptr() as *mut Blob),
                E_INVALIDARG
            );

            XGameSaveCloseUpdate(u);
            XGameSaveCloseContainer(c);
            XGameSaveCloseProvider(p);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn container_quota_validation_enforces_limit_and_atomicity() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-quota-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let root = (*p).root.clone();
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"slot\0".as_ptr(), &mut c), S_OK);

            // Write 1 MiB save
            let data_1mb = vec![0x42u8; 1 << 20];
            let mut u = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, b"Display\0".as_ptr(), &mut u), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u, b"save1\0".as_ptr(), data_1mb.as_ptr(), data_1mb.len()), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u), S_OK);
            XGameSaveCloseUpdate(u);

            let mut remaining = 0i64;
            assert_eq!(XGameSaveGetRemainingQuota(p, &mut remaining), S_OK);
            assert_eq!(remaining, QUOTA - (1 << 20));

            // Exceed quota by queueing multiple max-size blobs whose sum > remaining
            let mut u_over = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u_over), S_OK);
            // 17 blobs * 16 MiB = 272 MiB > 256 MiB
            let chunk = vec![0x55u8; GS_MAX_BLOB_SIZE];
            for i in 0..17 {
                let bname = format!("big_{i}\0");
                assert_eq!(XGameSaveSubmitBlobWrite(u_over, bname.as_ptr(), chunk.as_ptr(), chunk.len()), S_OK);
            }
            // Update must fail with E_GS_QUOTA_EXCEEDED
            assert_eq!(XGameSaveSubmitUpdate(u_over), E_GS_QUOTA_EXCEEDED);
            XGameSaveCloseUpdate(u_over);

            // Verify atomicity: container still only has original 1 MiB save
            let mut list: Vec<(String, u32)> = Vec::new();
            assert_eq!(XGameSaveEnumerateBlobInfo(c, &mut list as *mut _ as *mut c_void, Some(blob_names)), S_OK);
            assert_eq!(list, vec![("save1".to_string(), 1 << 20)]);

            // Overwrite original blob with 512 KiB: net reduction in used space
            let mut u_shrink = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, std::ptr::null(), &mut u_shrink), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(u_shrink, b"save1\0".as_ptr(), data_1mb[..512 << 10].as_ptr(), 512 << 10), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(u_shrink), S_OK);
            XGameSaveCloseUpdate(u_shrink);

            assert_eq!(XGameSaveGetRemainingQuota(p, &mut remaining), S_OK);
            assert_eq!(remaining, QUOTA - (512 << 10));

            // Quota query with null pointer returns E_INVALIDARG
            assert_eq!(XGameSaveGetRemainingQuota(p, std::ptr::null_mut()), E_INVALIDARG);

            XGameSaveCloseContainer(c);
            XGameSaveCloseProvider(p);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    /// A container with no saved data is not an error: the call succeeds and the callback
    /// is not called, as on the console (first launch).
    #[test]
    fn get_container_info_of_a_container_without_saved_data_succeeds_without_a_result() {
        let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let cfg = format!("xcl-test-info-missing-{}\0", std::process::id());
            let mut p = std::ptr::null_mut();
            assert_eq!(XGameSaveInitializeProvider(std::ptr::null_mut(), cfg.as_ptr(), false, &mut p), S_OK);
            let root = (*p).root.clone();
            let mut info: Vec<(String, String, u32, u64)> = Vec::new();
            let ctx = &mut info as *mut _ as *mut c_void;

            // Never created.
            assert_eq!(XGameSaveGetContainerInfo(p, b"missing\0".as_ptr(), ctx, Some(count_containers)), S_OK);
            let longest = format!("{}\0", "a".repeat(256));
            assert_eq!(XGameSaveGetContainerInfo(p, longest.as_ptr(), ctx, Some(count_containers)), S_OK);
            assert!(info.is_empty(), "{info:?}");

            // Created, but no update was ever submitted.
            let mut c = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateContainer(p, b"fresh\0".as_ptr(), &mut c), S_OK);
            assert_eq!(XGameSaveGetContainerInfo(p, b"fresh\0".as_ptr(), ctx, Some(count_containers)), S_OK);
            assert!(info.is_empty(), "{info:?}");
            XGameSaveCloseContainer(c);

            // A name XGameSaveCreateContainer rejects is rejected here with the same error.
            let overlong = format!("{}\0", "a".repeat(257));
            assert_eq!(XGameSaveGetContainerInfo(p, b"\0".as_ptr(), ctx, Some(count_containers)), E_GS_INVALID_CONTAINER_NAME);
            assert_eq!(XGameSaveGetContainerInfo(p, overlong.as_ptr(), ctx, Some(count_containers)), E_GS_INVALID_CONTAINER_NAME);
            assert!(info.is_empty(), "{info:?}");

            XGameSaveCloseProvider(p);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn get_container_info_reports_storage_errors() {
        unsafe {
            let mut info: Vec<(String, String, u32, u64)> = Vec::new();
            let ctx = &mut info as *mut _ as *mut c_void;

            // Provider root is a file, not a directory.
            let mut pr = Provider { magic: MAGIC_PROVIDER, root: std::env::current_exe().unwrap() };
            let r = XGameSaveGetContainerInfo(&mut pr, b"slot\0".as_ptr(), ctx, Some(count_containers));
            assert_eq!(r, E_FAIL);
            assert!(info.is_empty(), "{info:?}");

            // Container's `b` entry is a file instead of a directory.
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("xcl-gamesave-err-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(root.join("slot")).unwrap();
            std::fs::write(root.join("slot").join("b"), b"not a directory").unwrap();
            let mut pr = Provider { magic: MAGIC_PROVIDER, root: root.clone() };
            let r = XGameSaveGetContainerInfo(&mut pr, b"slot\0".as_ptr(), ctx, Some(count_containers));
            let _ = std::fs::remove_dir_all(&root);
            assert_eq!(r, E_FAIL);
            assert!(info.is_empty(), "{info:?}");
        }
    }

    #[test]
    fn container_modified_reports_the_newest_blob_not_the_folder() {
        // The container folder's own mtime stops at the first save (#1041): titles
        // read lastModifiedTime as the last update, so a blob or display write must
        // move the reported time even when the folder's did not.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("xcl-gamesave-mtime-{}-{nonce}", std::process::id()));
        let dir = root.join("slot1");
        std::fs::create_dir_all(dir.join("b")).unwrap();
        let blob = dir.join("b").join("data");
        std::fs::write(&blob, b"v1").unwrap();
        let display = dir.join("display");
        std::fs::write(&display, b"Save 1").unwrap();

        // Future times: the freshly made folders' own mtimes are "now", so the
        // artificial ones must be later to prove they win the max.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let t = |secs: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        std::fs::File::options().write(true).open(&blob).unwrap().set_modified(t(now + 100)).unwrap();
        std::fs::File::options().write(true).open(&display).unwrap().set_modified(t(now + 200)).unwrap();
        assert_eq!(container_modified(&dir, &blobs_of(&dir)), (now + 200) as i64);

        std::fs::File::options().write(true).open(&blob).unwrap().set_modified(t(now + 300)).unwrap();
        assert_eq!(container_modified(&dir, &blobs_of(&dir)), (now + 300) as i64);

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn move_paths_preserve_windows_path_forms() {
        for (path, expected) in [
            (r"C:\saves/backup\..\slot.dat", r"\\?\C:\saves\slot.dat"),
            (r"\\server\share\saves\slot.dat", r"\\?\UNC\server\share\saves\slot.dat"),
            (r"\\?\UNC\server\share\saves\slot.dat", r"\\?\UNC\server\share\saves\slot.dat"),
        ] {
            let expected: Vec<_> = expected.encode_utf16().chain(std::iter::once(0)).collect();
            assert_eq!(move_path(Path::new(path)).unwrap(), expected);
        }
    }

    #[cfg(windows)]
    #[test]
    fn atomic_save_creates_and_replaces_a_blob_past_max_path() {
        use std::os::windows::ffi::OsStrExt;

        let root = std::env::temp_dir().join(format!("xcl-gamesave-long-path-{}", std::process::id()));
        let dir = root.join("a".repeat(96)).join("b".repeat(96)).join("c".repeat(96));
        let blob = dir.join("b").join("save.dat");
        assert!(blob.as_os_str().encode_wide().count() >= 260);
        std::fs::create_dir(&root).unwrap();

        let create = apply(&dir, "slot", None, &[("save.dat".to_string(), Some(b"original".to_vec()))]);
        let original = std::fs::read(&blob);
        let replace = apply(&dir, "slot", None, &[("save.dat".to_string(), Some(b"updated".to_vec()))]);
        let updated = std::fs::read(&blob);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(create, S_OK);
        assert_eq!(original.unwrap(), b"original");
        assert_eq!(replace, S_OK);
        assert_eq!(updated.unwrap(), b"updated");
    }

}
