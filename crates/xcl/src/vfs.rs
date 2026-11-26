//! Virtual file system (design §5.4).
//!
//! * Console path prefixes (`G:\`, `GAME:`, `T:\` …) are translated via mounts.
//! * The game folder is **read-only**. A write to a game file first copies that single
//!   file into `<data>\overlay\` and the write goes to the copy. Reads prefer the
//!   overlay. Deleting a game file records a "whiteout" instead of touching the original.
//! * This module is pure logic + std::fs; the Win32 hooks that feed it live in
//!   `vfs_hooks.rs`.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Not ours – use the path unchanged.
    Pass,
    /// A console path translated to a real, writable location (e.g. T:\ → data\temp).
    Translated(String),
    /// Inside the (read-only) game folder; `rel` has no leading separator ("" = root).
    Game { rel: String },
    /// A console mount was unsafe (for example, traversal escaped its host root).
    #[cfg(not(windows))]
    Reject(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Pass,
    Use(String),
    /// Use this path with a rewritten Win32 creation disposition (e.g. `TRUNCATE_EXISTING`
    /// on an un-overlaid game file becomes `CREATE_ALWAYS` in the overlay so no placeholder
    /// file is published before the native open succeeds).
    UseWithDisposition(String, u32),
    /// CREATE_ALWAYS creates an overlay file over an existing original.
    CreateOverOriginal(String),
    /// Fail with this Win32 error code.
    Fail(u32),
}

/// Pending commit for a staged game-file open: `rel` to finalise after the host
/// open succeeds, and whether the stage created a new overlay entry. Only a new
/// entry may clear a whiteout — reopening an existing entry must keep the
/// whiteout that hides its original after delete-on-close (upstream #1038).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCommit {
    pub rel: String,
    pub is_new: bool,
}

pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PATH_NOT_FOUND: u32 = 3;
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_FILE_EXISTS: u32 = 80;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const ERROR_DIR_NOT_EMPTY: u32 = 145;
pub const ERROR_DIRECTORY: u32 = 267;

pub(crate) const CREATE_NEW: u32 = 1;
pub(crate) const CREATE_ALWAYS: u32 = 2;
pub(crate) const OPEN_EXISTING: u32 = 3;
pub(crate) const OPEN_ALWAYS: u32 = 4;
pub(crate) const TRUNCATE_EXISTING: u32 = 5;

#[cfg(not(windows))]
pub(crate) const GENERIC_READ: u32 = 0x8000_0000;
const FILE_READ_DATA: u32 = 0x0000_0001;
const FILE_WRITE_DATA: u32 = 0x0000_0002;
const FILE_APPEND_DATA: u32 = 0x0000_0004;
pub(crate) const GENERIC_WRITE: u32 = 0x4000_0000;
const WRITE_ACCESS: u32 = GENERIC_WRITE
    | 0x1000_0000 // GENERIC_ALL
    | FILE_WRITE_DATA // FILE_WRITE_DATA / ADD_FILE
    | FILE_APPEND_DATA // FILE_APPEND_DATA / ADD_SUBDIRECTORY
    | 0x0000_0010 // FILE_WRITE_EA
    | 0x0000_0100 // FILE_WRITE_ATTRIBUTES
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000; // WRITE_OWNER
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
pub(crate) const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;

pub struct Vfs {
    game_root: String,
    game_root_lc: String,
    /// XCL's data folder for the title, absolute and lower-case.
    #[cfg_attr(not(windows), allow(dead_code))]
    data_root_lc: String,
    // The Linux PLS quota path is used by the kernelx file APIs (phase C step-10).
    #[cfg_attr(not(windows), allow(dead_code))]
    pls_root: PathBuf,
    #[cfg_attr(not(windows), allow(dead_code))]
    pls_root_lc: String,
    /// Other folders XCL itself reads and writes (`own_folder`), absolute and lower-case.
    own_roots_lc: Vec<String>,
    overlay: String,
    /// The overlay holds (or may hold) something besides the whiteout list: from an earlier run,
    /// or written by this one. While it holds nothing, no game file has an overlay copy and
    /// `visible_in_overlay` needs no file system call (a title that never writes its game folder
    /// opens every file without one).
    overlay_used: AtomicBool,
    mounts: Vec<(String, String)>,
    whiteouts: Mutex<HashSet<String>>,
    whiteout_file: PathBuf,
    #[cfg(windows)]
    existing_drives: u32,
    unmapped: Mutex<BTreeSet<String>>,
    writes: Mutex<BTreeSet<String>>,
    /// Serializes write bookkeeping across a staged host open, so rollback cannot replace
    /// whiteout changes made by another VFS operation while that open was in flight.
    write_lock: Mutex<()>,
}

static VFS: OnceLock<Vfs> = OnceLock::new();

pub fn init(v: Vfs) -> &'static Vfs {
    let _ = VFS.set(v);
    VFS.get().unwrap()
}

pub fn get() -> Option<&'static Vfs> {
    VFS.get()
}

/// Host path separators: Windows paths are backslash-separated, Linux ones slash-separated.
/// Console paths from the title are always backslash-separated and go through `logical`.
#[cfg(windows)]
fn norm(s: &str) -> String {
    s.replace('/', "\\")
}

#[cfg(not(windows))]
fn norm(s: &str) -> String {
    s.replace('\\', "/")
}

/// A console path (`G:\dir/file`) in the one form the mount and whiteout tables use.
fn logical(s: &str) -> String {
    s.replace('/', "\\")
}

fn trim_sep(s: &str) -> String {
    s.trim_end_matches(['\\', '/']).to_string()
}

/// Canonicalise a relative path within the game / overlay directory (upstream
/// #1560 audit):
/// 1. Replaces forward slashes '/' with backslashes '\'.
/// 2. Strips leading and trailing backslashes.
/// 3. Normalises consecutive backslashes into a single backslash.
/// 4. Resolves '.' (current directory) and '..' (parent directory).
/// 5. Boundary check: returns `None` if '..' attempts to escape above the root
///    (when the component stack is empty), or if the path contains a drive letter
///    (e.g. "C:") or device prefix.
/// A `None` rel must never touch the host: callers fail with the Win32 code for
/// the operation (PATH_NOT_FOUND / FILE_NOT_FOUND) instead.
pub fn clean_rel(rel: &str) -> Option<String> {
    if rel.is_empty() {
        return Some(String::new());
    }
    let norm_str = rel.replace('/', "\\");
    if norm_str.starts_with(r"\\") {
        return None;
    }
    let trimmed = norm_str.trim_matches('\\');
    if trimmed.is_empty() {
        return Some(String::new());
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return None;
    }

    let mut parts: Vec<&str> = Vec::new();
    for seg in trimmed.split('\\') {
        match seg {
            "" | "." => continue,
            ".." => {
                if parts.pop().is_none() {
                    return None;
                }
            }
            s => {
                if s.contains(':') {
                    return None;
                }
                parts.push(s);
            }
        }
    }
    Some(parts.join("\\"))
}

/// A `..` in a console path stops at its mount's root: resolve `.`/`..` inside
/// the mount-relative rest, with `..` past the root clamped instead of escaping
/// to the PC folder next to the game (upstream #1170).
fn below_mount(target: &str, rest: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for s in rest.split('\\') {
        match s {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        target.to_string()
    } else {
        format!("{target}\\{}", parts.join("\\"))
    }
}

/// Join a root folder and a VFS-relative path (`\` separated).
fn join_rel(root: &str, rel: &str) -> String {
    if rel.is_empty() {
        return root.to_string();
    }
    #[cfg(windows)]
    {
        format!("{root}\\{rel}")
    }
    #[cfg(not(windows))]
    {
        format!("{root}/{}", rel.replace('\\', "/"))
    }
}

/// The `rel` part of a `Target::Game` (no leading separator, `\` separated).
fn rel_from_host(s: &str) -> String {
    #[cfg(windows)]
    {
        s.to_string()
    }
    #[cfg(not(windows))]
    {
        s.replace('/', "\\")
    }
}

/// A host folder path (game/data/overlay/mount target) in platform form.
#[cfg(windows)]
fn host_path_string(s: &str) -> String {
    norm(s)
}

#[cfg(not(windows))]
fn host_path_string(s: &str) -> String {
    // Strip the verbatim prefix before replacing separators: the prefixes are
    // backslash-spelled (`\\?\`, `\??\`), so replacing first would hide them
    // behind forward slashes (upstream #316).
    let s = strip_verbatim(s);
    let s = s.replace('\\', "/");
    let path = Path::new(&s);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => return s,
        }
    };
    normalize_host(&abs)
}

/// Absolutise and lexically normalise a host path (like `GetFullPathNameW`, no file access).
#[cfg(not(windows))]
fn normalize_host(path: &Path) -> String {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    let s = out.to_string_lossy().into_owned();
    if s.is_empty() {
        "/".to_string()
    } else {
        s
    }
}

#[cfg(not(windows))]
fn canonical_existing_ancestor(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(resolved) => return Ok(resolved),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling final symlink would be followed by O_CREAT. Reject it instead of
                // treating it as a missing regular file whose parent is safe.
                if std::fs::symlink_metadata(&current)
                    .is_ok_and(|meta| meta.file_type().is_symlink())
                {
                    return Err(error);
                }
                if !current.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// Keep a translated path inside its mount even when an existing component is a symlink.
/// Missing files are checked through their nearest existing ancestor, so a symlinked parent
/// cannot redirect a later create outside the mount either.
#[cfg(not(windows))]
fn ensure_mount_contained(root: &Path, path: &Path) -> Result<(), u32> {
    if !path.starts_with(root) {
        return Err(ERROR_ACCESS_DENIED);
    }
    let root = canonical_existing_ancestor(root).map_err(|_| ERROR_ACCESS_DENIED)?;
    let path = canonical_existing_ancestor(path).map_err(|_| ERROR_ACCESS_DENIED)?;
    if !path.starts_with(root) {
        return Err(ERROR_ACCESS_DENIED);
    }
    Ok(())
}

#[cfg(not(windows))]
fn checked_mount_path(root: &str, tail: &str) -> Result<String, u32> {
    // Clamp `..` at the mount root first, so an above-mount `..` stays in the
    // mount (`Game`) while a genuine symlink escape still fails below.
    let joined = below_mount(root, tail).replace('\\', "/");
    let path = normalize_host(Path::new(&joined));
    ensure_mount_contained(Path::new(root), Path::new(&path))?;
    Ok(path)
}

fn strip_verbatim(p: &str) -> String {
    // Upstream #1560: `UNC` matches case-insensitively and either separator
    // may follow it (`\\?\UNC\` vs `\\?\unc/...`).
    let bytes = p.as_bytes();
    if bytes.len() >= 8
        && (p.starts_with(r"\\?\") || p.starts_with(r"\??\"))
        && p[4..7].eq_ignore_ascii_case("unc")
        && (bytes[7] == b'\\' || bytes[7] == b'/')
    {
        return format!(r"\\{}", &p[8..]);
    }
    if p.starts_with(r"\\?\") || p.starts_with(r"\??\") {
        return p[4..].to_string();
    }
    p.to_string()
}

/// Match `root_lc` (lowercased, no trailing separator) against the leading
/// separator-separated components of `abs` (normalized, no trailing
/// separator), returning the relative tail in `abs`'s original casing.
/// Matching per component avoids slicing `abs` with a byte offset from
/// `.to_lowercase()`, which changes UTF-8 byte lengths for characters like
/// `İ`, `ẞ`, `K`. Either separator matches either side: Linux host paths use
/// `/` while console spellings use `\`.
fn strip_path_prefix_ci<'a>(abs: &'a str, root_lc: &str) -> Option<&'a str> {
    if root_lc.is_empty() {
        return None;
    }
    let mut pos = 0usize;
    let mut first = true;
    for root_comp in root_lc.split(|c| c == '\\' || c == '/') {
        if !first {
            if abs.as_bytes().get(pos).is_some_and(|b| *b == b'\\' || *b == b'/') {
                pos += 1;
            } else {
                return None;
            }
        }
        first = false;
        let rest = &abs[pos..];
        let len = rest.find(|c| c == '\\' || c == '/').unwrap_or(rest.len());
        if rest[..len].to_lowercase() != root_comp {
            return None;
        }
        pos += len;
    }
    if pos == abs.len() {
        Some("")
    } else if abs.as_bytes().get(pos).is_some_and(|b| *b == b'\\' || *b == b'/') {
        Some(abs[pos + 1..].trim_start_matches(|c| c == '\\' || c == '/'))
    } else {
        None
    }
}

fn is_device(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    if l.starts_with("\\\\.\\") || l.starts_with("\\\\?\\globalroot") {
        return true;
    }
    // "\\?\" / "\??\" names that are not a drive path or UNC: device interfaces
    // ("\\?\hid#vid_045e&pid_…#{guid}" – XInput opens the controller this way, SetupAPI
    // hands them out), volumes ("\\?\Volume{…}") – never files of the title
    if let Some(r) = l.strip_prefix("\\\\?\\").or_else(|| l.strip_prefix("\\??\\")) {
        let b = r.as_bytes();
        let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
        if !drive && !r.starts_with("unc\\") {
            return true;
        }
    }
    let base = l.trim_end_matches(':');
    matches!(base, "con" | "nul" | "prn" | "aux" | "conin$" | "conout$" | "clock$")
        || (base.len() == 4 && (base.starts_with("com") || base.starts_with("lpt")) && base.as_bytes()[3].is_ascii_digit())
}

/// Full path (no file system access).
#[cfg(windows)]
pub fn full_path(p: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;
    let w = crate::util::wide(p);
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetFullPathNameW(w.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    if n == 0 {
        return None;
    }
    if n > buf.len() {
        buf.resize(n + 1, 0);
        let n2 = unsafe { GetFullPathNameW(w.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut()) } as usize;
        return (n2 > 0).then(|| String::from_utf16_lossy(&buf[..n2]));
    }
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// The host has no Windows drive letters; a console path that a mount does not translate is
/// reported as unmapped instead of being looked up on a host drive.
#[cfg(not(windows))]
pub fn full_path(p: &str) -> Option<String> {
    let p = strip_verbatim(p);
    let path = Path::new(&p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    Some(normalize_host(&abs))
}

#[cfg(windows)]
fn logical_drives() -> u32 {
    unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() }
}

impl Vfs {
    pub fn new(game_root: &Path, data_dir: &Path, mounts: Vec<(String, String)>) -> Vfs {
        // Upstream #1560: roots go through `full_path` + `strip_verbatim` + `norm`
        // and mount prefixes lowercase with `/` normalised, so `MNT/DIR:` matches
        // `mnt/dir:/...`. Linux keeps `host_path_string` (absolutise + lexically
        // normalise to `/`) for host folders — plain `norm` would fold host `/`
        // to `\` — and `logical` (`/` -> `\`) for console-spelled prefixes.
        #[cfg(windows)]
        let game_root = {
            let game_root_s = game_root.to_string_lossy();
            let game_root_norm = trim_sep(&strip_verbatim(&norm(&game_root_s)));
            let game_root = full_path(&game_root_norm).unwrap_or(game_root_norm);
            trim_sep(&strip_verbatim(&norm(&game_root)))
        };
        #[cfg(not(windows))]
        let game_root = trim_sep(&strip_verbatim(&host_path_string(&game_root.to_string_lossy())));
        let overlay_dir = data_dir.join("overlay");
        let whiteout_file = overlay_dir.join(".xcl_whiteouts");
        let whiteouts = std::fs::read_to_string(&whiteout_file)
            .map(|s| {
                s.lines()
                    .filter_map(|l| Self::wkey(l.trim()))
                    .filter(|k| !k.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let overlay_used = std::fs::read_dir(&overlay_dir)
            .map(|d| d.flatten().any(|e| e.file_name() != ".xcl_whiteouts"))
            .unwrap_or(false);
        let mut mounts: Vec<(String, String)> = mounts
            .into_iter()
            .map(|(p, t)| {
                let p_clean = logical(&p).to_ascii_lowercase();
                let p_clean = p_clean.trim_end_matches('\\').to_string();
                #[cfg(windows)]
                let t_clean = {
                    let t_norm = trim_sep(&strip_verbatim(&norm(&t)));
                    let t_clean = full_path(&t_norm).unwrap_or(t_norm);
                    trim_sep(&strip_verbatim(&norm(&t_clean)))
                };
                #[cfg(not(windows))]
                let t_clean = trim_sep(&host_path_string(&t));
                (p_clean, t_clean)
            })
            .collect();
        // longest prefix first ("GAME:" before "G:")
        mounts.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        let data_root = data_dir.to_string_lossy();
        #[cfg(windows)]
        let data_root = {
            let data_root_norm = trim_sep(&strip_verbatim(&norm(&data_root)));
            let data_root = full_path(&data_root_norm).unwrap_or(data_root_norm);
            trim_sep(&strip_verbatim(&norm(&data_root)))
        };
        #[cfg(not(windows))]
        let data_root = {
            let data_root = full_path(&data_root).unwrap_or_else(|| data_root.into_owned());
            trim_sep(&strip_verbatim(&norm(&data_root)))
        };
        let pls_root = Path::new(&data_root).join("pls");
        let pls_root_lc = trim_sep(&strip_verbatim(&norm(&pls_root.to_string_lossy()))).to_lowercase();
        Vfs {
            game_root_lc: game_root.to_lowercase(),
            data_root_lc: trim_sep(&strip_verbatim(&norm(&data_root))).to_lowercase(),
            pls_root,
            pls_root_lc,
            own_roots_lc: Vec::new(),
            game_root,
            overlay: trim_sep(&norm(&overlay_dir.to_string_lossy())),
            overlay_used: AtomicBool::new(overlay_used),
            mounts,
            whiteouts: Mutex::new(whiteouts),
            whiteout_file,
            #[cfg(windows)]
            existing_drives: logical_drives(),
            unmapped: Mutex::new(BTreeSet::new()),
            writes: Mutex::new(BTreeSet::new()),
            write_lock: Mutex::new(()),
        }
    }

    /// `dir` is a folder of XCL's own outside the title's data folder – the shader cache,
    /// `xcl.exe`'s directory, or configured host tool DLL folders (`XCL_DXC_DIR`, …). A console
    /// drive mount must not translate its paths either: with the cache or DXC on the PC's D: drive
    /// ("D:" is the console's game drive) the title's process wrote its DXIL into the overlay and
    /// looked for it or `dxcompiler.dll` in the game folder. Bare drive roots are ignored so an
    /// empty or root path cannot disable a whole console drive mount.
    pub fn own_folder(mut self, dir: &Path) -> Vfs {
        let dir = dir.to_string_lossy();
        let dir = full_path(&dir).unwrap_or_else(|| dir.into_owned());
        let dir = trim_sep(&strip_verbatim(&norm(&dir))).to_lowercase();
        if !dir.is_empty() && !(dir.len() == 2 && dir.as_bytes()[1] == b':') {
            self.own_roots_lc.push(dir);
        }
        self
    }

    pub fn game_root(&self) -> &str {
        &self.game_root
    }

    /// Find the quota root for a path already translated by this VFS.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn pls_root_for(&self, translated: &str) -> Option<&Path> {
        let path = translated.to_lowercase();
        path.strip_prefix(&self.pls_root_lc)
            .filter(|rest| rest.is_empty() || rest.starts_with(std::path::MAIN_SEPARATOR))
            .map(|_| self.pls_root.as_path())
    }

    fn mount_match<'a>(&'a self, p: &'a str) -> Option<(&'a str, &'a str)> {
        // to_ascii_lowercase keeps byte offsets, so slicing `p` with prefix.len() is safe
        let l = p.to_ascii_lowercase();
        // Embedded "...\GAME:\rest": some runtimes treat "GAME:\x" as a relative path and
        // prepend the current directory. A component ending in ':' is never a valid file
        // name, so it can only be a console mount – checked first, so that a drive mount
        // ("D:") does not take the path when the current directory is on that drive.
        for (prefix, target) in &self.mounts {
            if !prefix.ends_with(':') {
                continue;
            }
            let needle = format!("\\{prefix}");
            let mut from = 0;
            while let Some(i) = l[from..].find(&needle) {
                let at = from + i;
                let after = at + needle.len();
                let rest = &p[after..];
                if rest.is_empty() || rest.starts_with('\\') {
                    return Some((target, rest.trim_start_matches('\\')));
                }
                from = after;
            }
        }
        for (prefix, target) in &self.mounts {
            if !l.starts_with(prefix.as_str()) {
                continue;
            }
            let rest = &p[prefix.len()..];
            // "G:" / "GAME:" accept anything after the colon ("G:foo" == "G:\foo");
            // directory-style prefixes need a separator boundary.
            if !(prefix.ends_with(':') || rest.is_empty() || rest.starts_with('\\')) {
                continue;
            }
            return Some((target, rest.trim_start_matches('\\')));
        }
        None
    }

    #[cfg(windows)]
    fn apply_mounts(&self, p: &str) -> Option<String> {
        self.mount_match(p)
            .map(|(target, tail)| below_mount(target, tail))
    }

    #[cfg(not(windows))]
    fn apply_mounts_checked(&self, p: &str) -> Option<Result<String, u32>> {
        self.mount_match(p).map(|(target, tail)| {
            let path = checked_mount_path(target, tail)?;
            // Configured title-data mounts (the default S:/T:) may not use a symlinked
            // child root to leave the title data directory, especially not to reach the
            // read-only game layer through a writable mount.
            if let Some(data_root) = self.pls_root.parent() {
                if Path::new(target).starts_with(data_root) {
                    ensure_mount_contained(data_root, Path::new(&path))?;
                }
            }
            Ok(path)
        })
    }

    /// Classify a path using `full` to absolutise (injected for unit tests).
    pub fn classify_with(&self, path: &str, full: impl Fn(&str) -> Option<String>) -> Target {
        if path.is_empty() {
            return Target::Pass;
        }
        #[cfg(not(windows))]
        if let Some(t) = self.classify_host(path, &full) {
            return t;
        }
        let p = logical(path);
        if is_device(&p) {
            return Target::Pass;
        }
        let p = strip_verbatim(&p);
        if p.starts_with("\\\\") {
            // A UNC game root stays copy-on-write: UNC paths inside the game
            // root or XCL's own folders classify through the VFS instead of
            // passing straight to the host (upstream #1077). Linux host roots
            // use `/` while UNC input uses `\`, and host normalization folds a
            // leading `\\` to `/`, so fold both sides to one separator first;
            // on Windows this is the same comparison (roots already use `\`).
            let lc = p.to_lowercase();
            let path = lc.trim_start_matches('\\');
            let fold = |root: &str| root.replace('/', "\\");
            let game = fold(&self.game_root_lc);
            let game = game.trim_start_matches('\\');
            let data = fold(&self.data_root_lc);
            let data = data.trim_start_matches('\\');
            let under = |root: &str| {
                path == root
                    || path
                        .strip_prefix(root)
                        .is_some_and(|r| r.starts_with('\\'))
            };
            let matches_own = self
                .own_roots_lc
                .iter()
                .map(|r| fold(r))
                .any(|r| under(r.trim_start_matches('\\')));
            if !under(game) && !under(data) && !matches_own {
                return Target::Pass; // UNC outside game and XCL folders
            }
            #[cfg(not(windows))]
            {
                // Linux rejects untranslated console paths below, but a UNC
                // path naming a known root is already resolved, so finish it
                // here: the game root returns `Game` (copy-on-write, as
                // `classify_abs` yields on Windows), data/own folders `Pass`
                // (what `classify_abs` yields there for untranslated paths).
                // The remainder keeps the title's case; match per component
                // like `classify_abs` (upstream #316) — slicing `trimmed` at
                // the lowercased `game` length would split fold-expanding
                // chars such as ẞ, whose lowercase is shorter in bytes.
                if under(game) {
                    let folded = p.replace('/', "\\");
                    let trimmed = folded.trim_start_matches('\\');
                    if let Some(rel) = strip_path_prefix_ci(trimmed, game) {
                        return Target::Game { rel: rel.to_string() };
                    }
                    return Target::Pass;
                }
                return Target::Pass;
            }
        }
        #[cfg(windows)]
        let (p, translated) = match self.apply_mounts(&p) {
            Some(t) if !self.in_own_folders(&p, &full) => (t, true),
            _ => (p, false),
        };
        #[cfg(not(windows))]
        let (p, translated) = match self.apply_mounts_checked(&p) {
            Some(Ok(t)) if !self.translated_in_own_folders(&t) => (t, true),
            // an own folder of XCL's under the mount target stays where it is (upstream
            // #2101: the console drive must not translate the host's tool folders)
            Some(Ok(_)) => return Target::Pass,
            Some(Err(error)) => return Target::Reject(error),
            None => (p, false),
        };
        if !translated {
            #[cfg(windows)]
            {
                let b = p.as_bytes();
                if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
                    let bit = (b[0].to_ascii_uppercase() - b'A') as u32;
                    if self.existing_drives & (1 << bit) == 0 {
                        self.log_unmapped(&p);
                        return Target::Pass;
                    }
                }
            }
            // Linux has no Windows drives: an untranslated console path is not a host file.
            #[cfg(not(windows))]
            {
                let b = p.as_bytes();
                if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
                    self.log_unmapped(&p);
                }
                return Target::Reject(ERROR_PATH_NOT_FOUND);
            }
        }
        let Some(abs) = full(&p) else {
            return Target::Pass;
        };
        self.classify_abs(&abs, translated)
    }

    fn log_unmapped(&self, p: &str) {
        let mut u = self.unmapped.lock().unwrap();
        if u.len() < 1000 && u.insert(p.to_string()) {
            crate::xlog_warn!("[VFS] unmapped console path: {p}");
        }
    }

    /// A host path (Linux: the game folder itself, or an absolute path the title passed after
    /// a runtime absolved it). Relative paths are host-relative, as `GetFullPathNameW` treats
    /// them on Windows. `None` when `path` is a console path (drive/UNC/device/rooted).
    #[cfg(not(windows))]
    fn classify_host(&self, path: &str, full: &impl Fn(&str) -> Option<String>) -> Option<Target> {
        let stripped = strip_verbatim(path);
        let b = stripped.as_bytes();
        let relative = !stripped.starts_with('/')
            && !stripped.starts_with('\\')
            && !(b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic());
        if !stripped.starts_with('/') && !relative {
            return None;
        }
        // A runtime may prepend its current directory to "GAME:\x"; an embedded mount still
        // wins, exactly as in the Windows parsing below.
        if let Some(t) = self.apply_mounts_checked(&logical(&stripped)) {
            return Some(match t {
                Ok(t) => {
                    let Some(abs) = full(&t) else {
                        return Some(Target::Pass);
                    };
                    self.classify_abs(&abs, true)
                }
                Err(error) => Target::Reject(error),
            });
        }
        let host = if relative {
            stripped.replace('\\', "/")
        } else {
            stripped
        };
        let Some(abs) = full(&host) else {
            return Some(Target::Pass);
        };
        Some(self.classify_abs(&abs, false))
    }

    fn classify_abs(&self, abs: &str, translated: bool) -> Target {
        let abs = trim_sep(&strip_verbatim(&norm(abs)));
        // Match the game root per component (upstream #316): slicing `abs`
        // at a lowercased byte offset would split fold-expanding chars such
        // as ẞ/İ/K, whose lowercase forms differ in byte length, and corrupt
        // the rel (or panic on the slice).
        if let Some(rel) = strip_path_prefix_ci(&abs, &self.game_root_lc) {
            #[cfg(not(windows))]
            if ensure_mount_contained(Path::new(&self.game_root), Path::new(&abs)).is_err() {
                return Target::Reject(ERROR_ACCESS_DENIED);
            }
            // Upstream #1560: canonicalise the game rel; a `None` rel (drive,
            // colon, UNC, or `..` past the root) becomes the `..` sentinel the
            // open path fails with PATH_NOT_FOUND. Linux keeps the #1170 clamp
            // in `apply_mounts_checked`, so genuine mount escapes never reach
            // this sentinel — only hostile rel spellings do.
            if let Some(clean) = clean_rel(&rel_from_host(rel)) {
                return Target::Game { rel: clean };
            }
            return Target::Game { rel: "..".to_string() };
        }
        if translated {
            Target::Translated(abs)
        } else {
            Target::Pass
        }
    }

    /// A mount-translated host path inside one of XCL's own folders (the Linux twin of the
    /// Windows `in_own_folders` exemption, on the translated side): a console drive mount
    /// whose target contains a tool folder must not route the title into it. Only
    /// `own_folder` roots count – the game root still classifies `Game` and data mounts
    /// keep their `Translated` semantics in `classify_abs`.
    /// Only a translated host path counts: Linux has no drive-letter resolution, so the
    /// spelled console path itself can never name a host folder the way `in_own_folders`
    /// resolves it on Windows.
    #[cfg(not(windows))]
    fn translated_in_own_folders(&self, translated: &str) -> bool {
        let lc = trim_sep(&strip_verbatim(&norm(translated))).to_lowercase();
        self.own_roots_lc.iter().any(|root| {
            lc == *root
                || lc
                    .strip_prefix(root.as_str())
                    .is_some_and(|rest| rest.starts_with(std::path::MAIN_SEPARATOR))
        })
    }

    /// A path that already names the game folder or one of XCL's own folders on the PC (the
    /// title's data, `own_folder`). A console drive mount ("D:") must not translate it when the
    /// PC has its folders on that drive.
    /// Only a plain drive path counts: "<game>\GAME:\x" still names a console mount.
    #[cfg(windows)]
    fn in_own_folders(&self, p: &str, full: impl Fn(&str) -> Option<String>) -> bool {
        let b = p.as_bytes();
        if !(b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) || p[2..].contains(':') {
            return false;
        }
        let Some(abs) = full(p) else { return false };
        let lc = trim_sep(&strip_verbatim(&norm(&abs))).to_lowercase();
        [&self.game_root_lc, &self.data_root_lc].into_iter().chain(&self.own_roots_lc).any(|root| {
            lc.strip_prefix(root.as_str()).is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
        })
    }

    pub fn classify(&self, path: &str) -> Target {
        self.classify_with(path, full_path)
    }

    pub fn is_game_path(&self, p: &Path) -> bool {
        matches!(self.classify(&p.to_string_lossy()), Target::Game { .. })
    }

    pub fn overlay_path(&self, rel: &str) -> String {
        // Upstream #1560: build from the canonical rel so every spelling names
        // the same file; `join_rel` keeps the platform join (Linux `/`).
        let clean =
            clean_rel(rel).unwrap_or_else(|| rel.replace('/', "\\").trim_matches('\\').to_string());
        join_rel(&self.overlay, &clean)
    }

    pub fn original_path(&self, rel: &str) -> String {
        // Same canonicalisation as `overlay_path` (upstream #1560).
        let clean =
            clean_rel(rel).unwrap_or_else(|| rel.replace('/', "\\").trim_matches('\\').to_string());
        join_rel(&self.game_root, &clean)
    }

    #[cfg(not(windows))]
    fn checked_rel(rel: &str) -> Result<(), u32> {
        use std::path::Component;
        let rel = rel.replace('\\', "/");
        let path = Path::new(&rel);
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(component, Component::RootDir | Component::Prefix(_) | Component::ParentDir)
            })
        {
            return Err(ERROR_ACCESS_DENIED);
        }
        Ok(())
    }

    #[cfg(not(windows))]
    fn checked_overlay_path(&self, rel: &str) -> Result<String, u32> {
        Self::checked_rel(rel)?;
        // The overlay root is owned by XCL. Unlike a configured mount root it must not be a
        // symlink, otherwise every "contained" child would share the outside boundary.
        if std::fs::symlink_metadata(&self.overlay)
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(ERROR_ACCESS_DENIED);
        }
        let path = self.overlay_path(rel);
        ensure_mount_contained(Path::new(&self.overlay), Path::new(&path))?;
        Ok(path)
    }

    #[cfg(not(windows))]
    fn checked_original_path(&self, rel: &str) -> Result<String, u32> {
        Self::checked_rel(rel)?;
        let path = self.original_path(rel);
        ensure_mount_contained(Path::new(&self.game_root), Path::new(&path))?;
        Ok(path)
    }

    fn wkey(rel: &str) -> Option<String> {
        // Upstream #1560: whiteout keys are the canonical rel, ASCII-folded;
        // a `None` rel (traversal/drive/UNC) never names a whiteout.
        clean_rel(rel).map(|s| s.to_ascii_lowercase())
    }

    pub fn is_whiteout(&self, rel: &str) -> bool {
        let Some(k) = Self::wkey(rel) else {
            return false;
        };
        let g = self.whiteouts.lock().unwrap();
        if g.is_empty() {
            return false;
        }
        // a whiteout on a parent directory hides everything below it
        if g.contains(&k) {
            return true;
        }
        let mut cur = k.as_str();
        while let Some((parent, _)) = cur.rsplit_once('\\') {
            if g.contains(parent) {
                return true;
            }
            cur = parent;
        }
        false
    }

    pub fn has_whiteouts_under(&self, rel_dir: &str) -> bool {
        let Some(k) = Self::wkey(rel_dir) else {
            return false;
        };
        let g = self.whiteouts.lock().unwrap();
        if k.is_empty() {
            return !g.is_empty();
        }
        let pre = format!("{k}\\");
        g.iter().any(|w| w.starts_with(&pre))
    }

    fn persist_whiteouts(&self, g: &HashSet<String>) {
        #[cfg(not(windows))]
        if self.checked_overlay_path("").is_err() {
            return;
        }
        let _ = std::fs::create_dir_all(&self.overlay);
        let mut v: Vec<&String> = g.iter().collect();
        v.sort();
        let body: String = v.into_iter().map(|s| format!("{s}\n")).collect();
        let _ = std::fs::write(&self.whiteout_file, body);
    }

    fn add_whiteout(&self, rel: &str) {
        if let Some(k) = Self::wkey(rel) {
            let mut g = self.whiteouts.lock().unwrap();
            if g.insert(k) {
                self.persist_whiteouts(&g);
            }
        }
    }

    fn remove_whiteout(&self, rel: &str) {
        let Some(k) = Self::wkey(rel) else {
            return;
        };
        if k.is_empty() {
            return;
        }
        let mut g = self.whiteouts.lock().unwrap();
        // Walk top-down through every ancestor prefix and `k` itself.
        // If a whited-out directory is un-hidden (recreated, or a child is created under it),
        // push whiteouts down to its immediate original children so none of them resurrect.
        let mut changed = false;
        let mut end = 0;
        loop {
            let next = k[end..].find('\\').map(|i| end + i);
            let prefix = match next {
                Some(pos) => &k[..pos],
                None => k.as_str(),
            };
            if g.remove(prefix) {
                changed = true;
                #[cfg(windows)]
                let entries = std::fs::read_dir(self.original_path(prefix)).ok();
                #[cfg(not(windows))]
                let entries = self
                    .checked_original_path(prefix)
                    .ok()
                    .and_then(|path| std::fs::read_dir(path).ok());
                if let Some(entries) = entries {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                        g.insert(format!("{prefix}\\{name}"));
                    }
                }
            }
            match next {
                Some(pos) => end = pos + 1,
                None => break,
            }
        }
        if changed {
            self.persist_whiteouts(&g);
        }
    }

    fn record_write(&self, rel: &str) {
        // Upstream #1560: log the canonical spelling.
        let clean = clean_rel(rel).unwrap_or_else(|| rel.to_string());
        let mut w = self.writes.lock().unwrap();
        if w.len() < 10_000 && w.insert(clean.clone()) {
            crate::xlog_info!("[VFS] write to a game file redirected to the overlay: {clean}");
        }
    }

    pub fn visible_in_overlay(&self, rel: &str) -> bool {
        let Some(clean) = clean_rel(rel) else {
            return false;
        };
        if !self.overlay_used.load(Ordering::Acquire) {
            return false;
        }
        #[cfg(windows)]
        let path = self.overlay_path(&clean);
        #[cfg(not(windows))]
        let Ok(path) = self.checked_overlay_path(&clean) else {
            return false;
        };
        Path::new(&path).exists()
    }

    /// Something is about to be created in the overlay (set before it exists, so a reader that
    /// sees the entry also sees the flag). `pub(crate)` for the Linux twin tests that plant
    /// an overlay folder behind the VFS's back to model a run that found one at start
    /// (same touch upstream #1106 makes in its own `remove_merged_dir` fixture).
    pub(crate) fn mark_overlay_used(&self) {
        self.overlay_used.store(true, Ordering::Release);
    }

    pub fn visible_in_original(&self, rel: &str) -> bool {
        let Some(clean) = clean_rel(rel) else {
            return false;
        };
        if self.is_whiteout(&clean) {
            return false;
        }
        #[cfg(windows)]
        let path = self.original_path(&clean);
        #[cfg(not(windows))]
        let Ok(path) = self.checked_original_path(&clean) else {
            return false;
        };
        Path::new(&path).exists()
    }

    pub fn exists(&self, rel: &str) -> bool {
        clean_rel(rel).is_some_and(|c| self.visible_in_overlay(&c) || self.visible_in_original(&c))
    }

    pub fn is_dir(&self, rel: &str) -> bool {
        let Some(clean) = clean_rel(rel) else {
            return false;
        };
        if self.overlay_used.load(Ordering::Acquire) {
            #[cfg(windows)]
            let overlay = self.overlay_path(&clean);
            #[cfg(not(windows))]
            let overlay = match self.checked_overlay_path(&clean) {
                Ok(overlay) => overlay,
                Err(_) => {
                    return self.visible_in_original(&clean)
                        && self
                            .checked_original_path(&clean)
                            .is_ok_and(|path| Path::new(&path).is_dir());
                }
            };
            if let Ok(m) = std::fs::metadata(overlay) {
                return m.is_dir();
            }
        }
        #[cfg(windows)]
        let original_is_dir = Path::new(&self.original_path(&clean)).is_dir();
        #[cfg(not(windows))]
        let original_is_dir = self
            .checked_original_path(&clean)
            .is_ok_and(|path| Path::new(&path).is_dir());
        self.visible_in_original(&clean) && original_is_dir
    }

    /// Real path to read `path` from. None = hidden by a whiteout.
    pub fn real_path_for_read(&self, path: &str) -> Option<String> {
        match self.classify(path) {
            Target::Pass => Some(path.to_string()),
            Target::Translated(p) => Some(p),
            Target::Game { rel } => self.read_path_rel(&rel),
            #[cfg(not(windows))]
            Target::Reject(_) => None,
        }
    }

    pub fn read_path_rel(&self, rel: &str) -> Option<String> {
        let clean = clean_rel(rel)?;
        if self.visible_in_overlay(&clean) {
            #[cfg(windows)]
            return Some(self.overlay_path(&clean));
            #[cfg(not(windows))]
            return self.checked_overlay_path(&clean).ok();
        }
        if self.is_whiteout(&clean) {
            return None;
        }
        #[cfg(windows)]
        {
            Some(self.original_path(&clean))
        }
        #[cfg(not(windows))]
        {
            self.checked_original_path(&clean).ok()
        }
    }

    /// Stage the overlay path for a new/overwritten file (e.g. copy/move destination)
    /// without removing whiteouts or recording the write yet. Call `commit_write(rel)`
    /// only after the underlying file operation succeeds so a failed operation cannot
    /// resurrect a whited-out original.
    pub fn stage_write(&self, rel: &str) -> Result<String, u32> {        let _guard = self.write_lock.lock().unwrap();
        self.stage_write_unlocked(rel)
    }

    fn stage_write_unlocked(&self, rel: &str) -> Result<String, u32> {
        // Upstream #1560: traversal rels fail before anything is created.
        let Some(rel_clean) = clean_rel(rel) else {
            return Err(ERROR_PATH_NOT_FOUND);
        };
        if let Some((parent_rel, _)) = rel_clean.rsplit_once('\\') {
            if !self.is_dir(parent_rel) {
                return Err(ERROR_PATH_NOT_FOUND);
            }
        }
        #[cfg(windows)]
        let ov = self.overlay_path(&rel_clean);
        #[cfg(not(windows))]
        let ov = self.checked_overlay_path(&rel_clean)?;
        self.mark_overlay_used();
        if let Some(parent) = Path::new(&ov).parent() {
            std::fs::create_dir_all(parent).map_err(|_| ERROR_PATH_NOT_FOUND)?;
        }
        #[cfg(not(windows))]
        self.checked_overlay_path(&rel_clean)?;
        Ok(ov)
    }

    /// Finalise a staged write once the underlying file operation succeeded.
    pub fn commit_write(&self, rel: &str) {
        let _guard = self.write_lock.lock().unwrap();
        self.commit_write_unlocked(rel)
    }

    /// Commit a staged write once the underlying file operation succeeded. The caller
    /// must hold `write_lock` (e.g. the guard returned by `open_decision_linux`); this
    /// is the commit half of `stage_write`.
    pub(crate) fn commit_write_unlocked(&self, rel: &str) {
        #[cfg(not(windows))]
        if Self::checked_rel(rel).is_err() {
            return;
        }
        // Upstream #1560: record the canonical spelling.
        let clean = clean_rel(rel).unwrap_or_else(|| rel.to_string());
        self.remove_whiteout(&clean);
        self.record_write(&clean);
    }

    /// Commit a staged game-file open once the underlying host open succeeded. The
    /// caller must hold `write_lock`; this is the commit half of
    /// `open_decision_staged_*` / `open_decision_linux`.
    pub fn commit_open(&self, rel: &str, is_new: bool) {
        let _guard = self.write_lock.lock().unwrap();
        self.commit_open_unlocked(rel, is_new)
    }

    /// Commit a staged game-file open once the underlying host open succeeded. The
    /// caller must hold `write_lock` (e.g. the guard returned by `open_decision_linux`).
    pub(crate) fn commit_open_unlocked(&self, rel: &str, is_new: bool) {
        #[cfg(not(windows))]
        if Self::checked_rel(rel).is_err() {
            return;
        }
        // Upstream #1560: record the canonical spelling.
        let clean = clean_rel(rel).unwrap_or_else(|| rel.to_string());
        // A new overlay entry replaces a whiteout. Reopening an existing entry
        // must keep the whiteout that hides its original after delete-on-close.
        if is_new {
            self.remove_whiteout(&clean);
        }
        self.record_write(&clean);
    }

    /// Record removal of an overlay file and hide the game file it covered.
    pub fn commit_overlay_removal(&self, rel: &str) {
        let _guard = self.write_lock.lock().unwrap();
        self.commit_overlay_removal_unlocked(rel)
    }

    pub(crate) fn commit_overlay_removal_unlocked(&self, rel: &str) {
        #[cfg(not(windows))]
        if Self::checked_rel(rel).is_err() {
            return;
        }
        // Upstream #1560: record the canonical spelling.
        let clean = clean_rel(rel).unwrap_or_else(|| rel.to_string());
        #[cfg(windows)]
        let original_exists = Path::new(&self.original_path(&clean)).exists();
        #[cfg(not(windows))]
        let original_exists = self
            .checked_original_path(&clean)
            .is_ok_and(|path| Path::new(&path).exists());
        if original_exists {
            self.add_whiteout(&clean);
        }
        self.record_write(&clean);
    }

    /// Ensure `rel` exists in the overlay (copying the original when `keep_content`)
    /// without removing whiteouts or recording the write yet. Call
    /// `commit_open(rel, is_new)` only after the underlying file operation succeeds
    /// so a failed open cannot resurrect a whited-out original.
    pub fn prepare_write_staged(&self, rel: &str, keep_content: bool) -> Result<String, u32> {
        let _guard = self.write_lock.lock().unwrap();
        let (ov, _) = self.prepare_write_staged_unlocked(rel, keep_content)?;
        Ok(ov)
    }

    fn prepare_write_staged_unlocked(&self, rel: &str, keep_content: bool) -> Result<(String, bool), u32> {
        // Upstream #1560: canonicalise first; a traversal rel fails with
        // PATH_NOT_FOUND before the parent check or the containment layer.
        // (Linux keeps `checked_rel` on the cleaned rel for absolute/symlink
        //  roots, and #29's pin moves to PATH_NOT_FOUND with this audit.)
        let Some(rel_clean) = clean_rel(rel) else {
            return Err(ERROR_PATH_NOT_FOUND);
        };
        // Linux: reject absolute rels with the containment error before the
        // parent check below can shadow it as PATH_NOT_FOUND.
        #[cfg(not(windows))]
        Self::checked_rel(&rel_clean)?;
        // Win32 fails a create-write with ERROR_PATH_NOT_FOUND when the parent folder
        // does not exist; do not conjure missing parents (same check as `stage_write`).
        if let Some((parent_rel, _)) = rel_clean.rsplit_once('\\') {
            if !self.is_dir(parent_rel) {
                return Err(ERROR_PATH_NOT_FOUND);
            }
        }
        #[cfg(windows)]
        let ov = self.overlay_path(&rel_clean);
        #[cfg(not(windows))]
        let ov = self.checked_overlay_path(&rel_clean)?;
        self.mark_overlay_used();
        if let Some(parent) = Path::new(&ov).parent() {
            std::fs::create_dir_all(parent).map_err(|_| ERROR_PATH_NOT_FOUND)?;
        }
        #[cfg(not(windows))]
        self.checked_overlay_path(&rel_clean)?;
        // A new overlay entry replaces a whiteout; the commit half
        // (`commit_open`) applies that once the host open succeeds.
        let is_new = !Path::new(&ov).exists();
        if is_new && keep_content && self.visible_in_original(&rel_clean) {
            #[cfg(windows)]
            let orig = self.original_path(&rel_clean);
            #[cfg(not(windows))]
            let orig = self.checked_original_path(&rel_clean)?;
            if Path::new(&orig).is_dir() {
                std::fs::create_dir_all(&ov).map_err(|_| ERROR_ACCESS_DENIED)?;
            } else {
                std::fs::copy(&orig, &ov).map_err(|_| ERROR_ACCESS_DENIED)?;
            }
        }
        Ok((ov, is_new))
    }

    /// Ensure `rel` exists in the overlay (copying the original when `keep_content`)
    /// and return the overlay path.
    pub fn prepare_write(&self, rel: &str, keep_content: bool) -> Result<String, u32> {
        let _guard = self.write_lock.lock().unwrap();
        self.prepare_write_unlocked(rel, keep_content)
    }

    fn prepare_write_unlocked(&self, rel: &str, keep_content: bool) -> Result<String, u32> {
        let (ov, is_new) = self.prepare_write_staged_unlocked(rel, keep_content)?;
        self.commit_open_unlocked(rel, is_new);
        Ok(ov)
    }

    pub(crate) fn has_write_access(access: u32) -> bool {
        access & WRITE_ACCESS != 0
    }

    /// Rights that permit reading/writing file contents, as opposed to metadata or handle
    /// rights that still require copy-on-write bookkeeping.
    #[cfg(not(windows))]
    pub(crate) fn has_data_write_access(access: u32) -> bool {
        access & (GENERIC_WRITE | 0x1000_0000 | FILE_WRITE_DATA | FILE_APPEND_DATA) != 0
    }

    /// Append rights do not authorize the truncation performed by CREATE_ALWAYS.
    #[cfg(not(windows))]
    pub(crate) fn has_overwrite_access(access: u32) -> bool {
        access & (GENERIC_WRITE | 0x1000_0000 | FILE_WRITE_DATA) != 0
    }

    pub(crate) fn is_write(access: u32, disposition: u32, flags: u32) -> bool {
        Self::has_write_access(access)
            || matches!(disposition, CREATE_NEW | CREATE_ALWAYS | TRUNCATE_EXISTING)
            || flags & FILE_FLAG_DELETE_ON_CLOSE != 0
    }

    #[cfg(not(windows))]
    pub(crate) fn is_read(access: u32) -> bool {
        access & (GENERIC_READ | 0x1000_0000 | FILE_READ_DATA) != 0
    }

    /// Decide how to open `target`, staging any overlay write without removing whiteouts
    /// or recording the write yet. Callers must call `commit_open` on the returned
    /// commit only after the underlying open succeeds.
    pub fn open_decision_staged_for(
        &self,
        target: Target,
        access: u32,
        disposition: u32,
        flags: u32,
    ) -> (Decision, Option<OpenCommit>) {
        let _guard = self.write_lock.lock().unwrap();
        self.open_decision_staged_for_unlocked(target, access, disposition, flags)
    }

    fn open_decision_staged_for_unlocked(
        &self,
        target: Target,
        access: u32,
        disposition: u32,
        flags: u32,
    ) -> (Decision, Option<OpenCommit>) {
        match target {
            Target::Pass => (Decision::Pass, None),
            #[cfg(not(windows))]
            Target::Reject(error) => (Decision::Fail(error), None),
            Target::Translated(p) => {
                if Self::is_write(access, disposition, flags) || disposition == OPEN_ALWAYS {
                    if let Some(parent) = Path::new(&p).parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                }
                (Decision::Use(p), None)
            }
            Target::Game { rel } => {
                // Upstream #1560: a traversal rel (e.g. the `..` sentinel from
                // `classify`) fails safely instead of probing the host.
                let Some(clean) = clean_rel(&rel) else {
                    return (Decision::Fail(ERROR_PATH_NOT_FOUND), None);
                };
                // a plain read: only `read_path_rel` asks the file system (titles open their files
                // for reading thousands of times; each probe below costs about as much as the open)
                if !Self::is_write(access, disposition, flags) && disposition != OPEN_ALWAYS {
                    let decision = match self.read_path_rel(&clean) {
                        Some(p) => Decision::Use(p),
                        None => Decision::Fail(ERROR_FILE_NOT_FOUND),
                    };
                    return (decision, None);
                }
                let in_ov = self.visible_in_overlay(&clean);
                let in_orig = self.visible_in_original(&clean);
                let visible = in_ov || in_orig;
                let write = Self::is_write(access, disposition, flags) || (disposition == OPEN_ALWAYS && !visible);
                if !write {
                    let decision = match self.read_path_rel(&clean) {
                        Some(p) => Decision::Use(p),
                        None => Decision::Fail(ERROR_FILE_NOT_FOUND),
                    };
                    return (decision, None);
                }
                // Win32 CreateFile requires GENERIC_WRITE for TRUNCATE_EXISTING before checking existence.
                if disposition == TRUNCATE_EXISTING && access & GENERIC_WRITE == 0 {
                    return (Decision::Fail(ERROR_INVALID_PARAMETER), None);
                }
                match disposition {
                    CREATE_NEW if visible => return (Decision::Fail(ERROR_FILE_EXISTS), None),
                    OPEN_EXISTING | TRUNCATE_EXISTING if !visible => return (Decision::Fail(ERROR_FILE_NOT_FOUND), None),
                    _ => {}
                }
                let ov_entry = in_ov || {
                    #[cfg(windows)]
                    let overlay_entry = Path::new(&self.overlay_path(&clean)).symlink_metadata().is_ok();
                    #[cfg(not(windows))]
                    let overlay_entry = match self.checked_overlay_path(&clean) {
                        Ok(path) => Path::new(&path).symlink_metadata().is_ok(),
                        Err(error) => return (Decision::Fail(error), None),
                    };
                    overlay_entry
                };
                if disposition == TRUNCATE_EXISTING {
                    let check_path = if in_ov { self.overlay_path(&clean) } else { self.original_path(&clean) };
                    let is_dir = if flags & FILE_FLAG_OPEN_REPARSE_POINT != 0 {
                        Path::new(&check_path).symlink_metadata().is_ok_and(|m| m.is_dir())
                    } else {
                        Path::new(&check_path).is_dir()
                    };
                    if is_dir {
                        let err = if flags & FILE_FLAG_BACKUP_SEMANTICS != 0 {
                            ERROR_INVALID_PARAMETER
                        } else {
                            ERROR_ACCESS_DENIED
                        };
                        return (Decision::Fail(err), None);
                    }
                }
                let keep = matches!(disposition, OPEN_EXISTING | OPEN_ALWAYS);
                let replaces_original = disposition == CREATE_ALWAYS
                    && !ov_entry
                    && in_orig
                    && Path::new(&self.original_path(&clean)).is_file();
                match self.prepare_write_staged_unlocked(&clean, keep) {
                    Ok((p, is_new)) if disposition == TRUNCATE_EXISTING && !ov_entry => {
                        (Decision::UseWithDisposition(p, CREATE_ALWAYS), Some(OpenCommit { rel: clean, is_new }))
                    }
                    Ok((p, is_new)) if replaces_original => {
                        (Decision::CreateOverOriginal(p), Some(OpenCommit { rel: clean, is_new }))
                    }
                    Ok((p, is_new)) => (Decision::Use(p), Some(OpenCommit { rel: clean, is_new })),
                    Err(e) => (Decision::Fail(e), None),
                }
            }
        }
    }

    pub fn open_decision_for(
        &self,
        target: Target,
        access: u32,
        disposition: u32,
        flags: u32,
    ) -> Decision {
        let _guard = self.write_lock.lock().unwrap();
        self.open_decision_for_unlocked(target, access, disposition, flags)
    }

    fn open_decision_for_unlocked(
        &self,
        target: Target,
        access: u32,
        disposition: u32,
        flags: u32,
    ) -> Decision {
        let (decision, commit) = self.open_decision_staged_for_unlocked(target, access, disposition, flags);
        if let Some(commit) = commit {
            self.commit_open_unlocked(&commit.rel, commit.is_new);
        }
        decision
    }

    pub fn open_decision_staged(&self, path: &str, access: u32, disposition: u32, flags: u32) -> (Decision, Option<OpenCommit>) {
        self.open_decision_staged_for(self.classify(path), access, disposition, flags)
    }

    pub fn open_decision(&self, path: &str, access: u32, disposition: u32, flags: u32) -> Decision {
        self.open_decision_for(self.classify(path), access, disposition, flags)
    }

    /// Stage a Linux host open: decide how to open `path` without removing whiteouts
    /// or recording the write yet. Returns the staged decision, the game-relative commit
    /// to `commit_open_unlocked` only after `open(2)` succeeds, and the `write_lock`
    /// guard held across the host open so concurrent VFS writes cannot interleave.
    /// A failed host open needs no rollback: nothing was committed.
    #[cfg(not(windows))]
    pub(crate) fn open_decision_linux(
        &self,
        path: &str,
        access: u32,
        disposition: u32,
        flags: u32,
    ) -> (Decision, Option<OpenCommit>, std::sync::MutexGuard<'_, ()>) {
        let guard = self.write_lock.lock().unwrap();
        let target = self.classify(path);
        let visible = match &target {
            Target::Game { rel } => self.exists(rel),
            Target::Translated(path) => Path::new(path).exists(),
            Target::Pass => Path::new(path).exists(),
            Target::Reject(_) => false,
        };
        // Append rights may create a missing file, but cannot authorize truncating an
        // existing one through CREATE_ALWAYS.
        if disposition == CREATE_ALWAYS
            && !Self::has_overwrite_access(access)
            && visible
            && !matches!(&target, Target::Reject(_))
        {
            return (Decision::Fail(ERROR_ACCESS_DENIED), None, guard);
        }
        // OPEN_ALWAYS may open an existing file read-only, but creating a missing one needs
        // data-write access. Reject before the VFS creates parents or changes whiteouts.
        if disposition == OPEN_ALWAYS
            && !Self::has_data_write_access(access)
            && !matches!(&target, Target::Reject(_))
            && !visible
        {
            return (Decision::Fail(ERROR_ACCESS_DENIED), None, guard);
        }
        let (decision, commit) =
            self.open_decision_staged_for_unlocked(target, access, disposition, flags);
        (decision, commit, guard)
    }

    pub fn delete_file(&self, rel: &str) -> Result<(), u32> {
        let _guard = self.write_lock.lock().unwrap();
        // Upstream #1560: a traversal rel is FILE_NOT_FOUND before probing the host.
        let Some(rel_clean) = clean_rel(rel) else {
            return Err(ERROR_FILE_NOT_FOUND);
        };
        #[cfg(windows)]
        let ov = self.overlay_path(&rel_clean);
        #[cfg(not(windows))]
        let ov = self.checked_overlay_path(&rel_clean)?;
        let ov_path = Path::new(&ov);
        if let Ok(meta) = std::fs::metadata(ov_path) {
            if meta.is_dir() {
                return Err(ERROR_ACCESS_DENIED);
            }
            std::fs::remove_file(ov_path).map_err(|_| ERROR_ACCESS_DENIED)?;
            self.commit_overlay_removal_unlocked(&rel_clean);
            return Ok(());
        }
        if !self.visible_in_original(&rel_clean) {
            return Err(ERROR_FILE_NOT_FOUND);
        }
        #[cfg(windows)]
        let original_is_dir = Path::new(&self.original_path(&rel_clean)).is_dir();
        #[cfg(not(windows))]
        let original_is_dir = self
            .checked_original_path(&rel_clean)
            .is_ok_and(|path| Path::new(&path).is_dir());
        if original_is_dir {
            return Err(ERROR_ACCESS_DENIED);
        }
        self.add_whiteout(&rel_clean);
        self.record_write(&rel_clean);
        Ok(())
    }

    pub fn create_dir(&self, rel: &str) -> Result<(), u32> {
        let _guard = self.write_lock.lock().unwrap();
        // Upstream #1560: a traversal rel is PATH_NOT_FOUND before probing the host.
        let Some(rel_clean) = clean_rel(rel) else {
            return Err(ERROR_PATH_NOT_FOUND);
        };
        if self.visible_in_overlay(&rel_clean) || self.visible_in_original(&rel_clean) {
            return Err(ERROR_ALREADY_EXISTS);
        }
        #[cfg(windows)]
        let ov = self.overlay_path(&rel_clean);
        #[cfg(not(windows))]
        let ov = self.checked_overlay_path(&rel_clean)?;
        self.mark_overlay_used();
        std::fs::create_dir_all(&ov).map_err(|_| ERROR_PATH_NOT_FOUND)?;
        #[cfg(not(windows))]
        self.checked_overlay_path(&rel_clean)?;
        self.remove_whiteout(&rel_clean);
        self.record_write(&rel_clean);
        Ok(())
    }

    fn check_original_dir_empty(&self, rel: &str) -> Result<(), u32> {
        // Callers pass canonical rels; a traversal rel here is defensive-only.
        let Some(rel_clean) = clean_rel(rel) else {
            return Err(ERROR_PATH_NOT_FOUND);
        };
        if self.is_whiteout(&rel_clean) {
            return Ok(());
        }
        let entries = match std::fs::read_dir(self.original_path(&rel_clean)) {
            Ok(entries) => entries,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return Ok(()),
            Err(e) => return Err(e.raw_os_error().map(|c| c as u32).unwrap_or(ERROR_ACCESS_DENIED)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| e.raw_os_error().map(|c| c as u32).unwrap_or(ERROR_ACCESS_DENIED))?;
            let name = entry.file_name();
            let child = if rel_clean.is_empty() {
                name.to_string_lossy().to_string()
            } else {
                format!("{rel_clean}\\{}", name.to_string_lossy())
            };
            if !self.is_whiteout(&child) {
                return Err(ERROR_DIR_NOT_EMPTY);
            }
        }
        Ok(())
    }

    pub fn remove_dir(&self, rel: &str) -> Result<(), u32> {
        let _guard = self.write_lock.lock().unwrap();
        // Upstream #1560: a traversal rel is PATH_NOT_FOUND before probing the host.
        let Some(rel_clean) = clean_rel(rel) else {
            return Err(ERROR_PATH_NOT_FOUND);
        };
        if rel_clean.is_empty() {
            return Err(ERROR_ACCESS_DENIED);
        }
        #[cfg(windows)]
        let ov = self.overlay_path(&rel_clean);
        #[cfg(not(windows))]
        let ov = self.checked_overlay_path(&rel_clean)?;
        let ov_path = Path::new(&ov);
        if let Ok(meta) = std::fs::metadata(ov_path) {
            if !meta.is_dir() {
                return Err(ERROR_DIRECTORY);
            }
            self.check_original_dir_empty(&rel_clean)?;
            std::fs::remove_dir(ov_path).map_err(|_| ERROR_DIR_NOT_EMPTY)?;
            #[cfg(windows)]
            let original_exists = Path::new(&self.original_path(&rel_clean)).exists();
            #[cfg(not(windows))]
            let original_exists = self
                .checked_original_path(&rel_clean)
                .is_ok_and(|path| Path::new(&path).exists());
            if original_exists {
                self.add_whiteout(&rel_clean);
            }
            self.record_write(&rel_clean);
            return Ok(());
        }
        if !self.visible_in_original(&rel_clean) {
            return Err(ERROR_FILE_NOT_FOUND);
        }
        #[cfg(windows)]
        let original_is_dir = Path::new(&self.original_path(&rel_clean)).is_dir();
        #[cfg(not(windows))]
        let original_is_dir = self
            .checked_original_path(&rel_clean)
            .is_ok_and(|path| Path::new(&path).is_dir());
        if !original_is_dir {
            return Err(ERROR_DIRECTORY);
        }
        self.check_original_dir_empty(&rel_clean)?;
        self.add_whiteout(&rel_clean);
        self.record_write(&rel_clean);
        Ok(())
    }

    /// Whether directory listings under `rel_dir` must merge overlay + original.
    pub fn needs_merge(&self, rel_dir: &str) -> bool {
        let Some(clean) = clean_rel(rel_dir) else {
            return false;
        };
        #[cfg(windows)]
        let overlay_is_dir = self.overlay_used.load(Ordering::Acquire) && Path::new(&self.overlay_path(&clean)).is_dir();
        #[cfg(not(windows))]
        let overlay_is_dir = self.overlay_used.load(Ordering::Acquire)
            && self
                .checked_overlay_path(&clean)
                .is_ok_and(|path| Path::new(&path).is_dir());
        overlay_is_dir || self.has_whiteouts_under(&clean)
    }

    pub fn unmapped_paths(&self) -> Vec<String> {
        self.unmapped.lock().map(|g| g.iter().cloned().collect()).unwrap_or_default()
    }

    pub fn redirected_writes(&self) -> Vec<String> {
        self.writes.lock().map(|g| g.iter().cloned().collect()).unwrap_or_default()
    }

    pub fn whiteouts(&self) -> Vec<String> {
        let mut v: Vec<String> = self.whiteouts.lock().map(|g| g.iter().cloned().collect()).unwrap_or_default();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        vfs: Vfs,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let dir = std::env::temp_dir().join(format!("xcl_vfs_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let game = dir.join("game");
            std::fs::create_dir_all(game.join("data")).unwrap();
            std::fs::write(game.join("data").join("hello.txt"), b"original").unwrap();
            std::fs::write(game.join("data").join("gone.txt"), b"bye").unwrap();
            let data = dir.join("xcldata");
            let mounts = vec![
                ("G:".to_string(), game.to_string_lossy().to_string()),
                ("GAME:".to_string(), game.to_string_lossy().to_string()),
                ("D:".to_string(), game.to_string_lossy().to_string()),
                ("T:".to_string(), data.join("temp").to_string_lossy().to_string()),
                ("S:".to_string(), data.join("pls").to_string_lossy().to_string()),
            ];
            let vfs = Vfs::new(&game, &data, mounts);
            Fixture { dir, vfs }
        }
        fn game(&self) -> PathBuf {
            self.dir.join("game")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn mounts_and_classification() {
        let f = Fixture::new("cls");
        let v = &f.vfs;
        assert_eq!(v.classify("G:\\data\\hello.txt"), Target::Game { rel: "data\\hello.txt".into() });
        assert_eq!(v.classify("g:/data/hello.txt"), Target::Game { rel: "data\\hello.txt".into() });
        assert_eq!(v.classify("GAME:\\data\\hello.txt"), Target::Game { rel: "data\\hello.txt".into() });
        assert_eq!(v.classify("D:\\data\\hello.txt"), Target::Game { rel: "data\\hello.txt".into() });
        assert_eq!(v.classify("G:\\"), Target::Game { rel: String::new() });
        // device interfaces (XInput opens the controller by one) are never the title's files
        assert_eq!(v.classify("\\\\?\\hid#vid_045e&pid_0b12&ig_00#8&1a2b3c&0&0000#{ec87f1e3-c13b-4100-b5f7-8b84d54260cb}"), Target::Pass);
        assert_eq!(v.classify("\\??\\USB#VID_045E&PID_0B12#3033#{a5dcbf10-6530-11d2-901f-00c04fb951ed}"), Target::Pass);
        // "GAME:\x" absolutised by a runtime as if it were relative
        let mangled = format!("\\\\?\\{}\\GAME:\\data\\hello.txt", f.game().display());
        assert_eq!(v.classify(&mangled), Target::Game { rel: "data\\hello.txt".into() });
        assert_eq!(v.classify("C:\\somewhere\\G:\\data\\hello.txt"), Target::Game { rel: "data\\hello.txt".into() });
        let real = f.game().join("data").join("hello.txt");
        assert_eq!(v.classify(&real.to_string_lossy()), Target::Game { rel: "data\\hello.txt".into() });
        #[cfg(windows)]
        let translated_tail = "xcldata\\temp\\scratch.bin";
        #[cfg(not(windows))]
        let translated_tail = "xcldata/temp/scratch.bin";
        assert!(matches!(v.classify("T:\\scratch.bin"), Target::Translated(p) if p.ends_with(translated_tail)));
        #[cfg(not(windows))]
        assert!(matches!(v.classify("S:\\save.dat"), Target::Translated(p) if p.ends_with("xcldata/pls/save.dat")));
        assert_eq!(v.classify("CONOUT$"), Target::Pass);
        assert_eq!(v.classify("\\\\.\\pipe\\x"), Target::Pass);
        #[cfg(windows)]
        assert_eq!(v.classify("C:\\Windows\\notepad.exe"), Target::Pass);
        #[cfg(not(windows))]
        assert_eq!(
            v.classify("C:\\Windows\\notepad.exe"),
            Target::Reject(ERROR_PATH_NOT_FOUND)
        );
    }

    #[test]
    fn dotdot_stops_at_mount_root() {
        // Upstream #1170: a `..` that would leave the mount stops at the mount's
        // root instead of escaping to the PC folder next to the game.
        let f = Fixture::new("dotdot");
        let v = &f.vfs;
        assert_eq!(
            v.classify("G:\\..\\data\\hello.txt"),
            Target::Game { rel: "data\\hello.txt".into() }
        );
        assert_eq!(v.classify("G:\\.."), Target::Game { rel: String::new() });
        assert_eq!(v.classify("G:\\..\\"), Target::Game { rel: String::new() });
        assert_eq!(
            v.classify("g:/../data/hello.txt"),
            Target::Game { rel: "data\\hello.txt".into() }
        );
        assert_eq!(
            v.classify("G:\\.\\data\\hello.txt"),
            Target::Game { rel: "data\\hello.txt".into() }
        );
        assert_eq!(
            v.classify("G:\\data\\..\\..\\data\\hello.txt"),
            Target::Game { rel: "data\\hello.txt".into() }
        );
        // `S:\..\` stays at the S: root, so the PLS quota (not the host total)
        // answers free-space queries there.
        match v.classify("S:\\..\\") {
            Target::Translated(p) => assert!(v.pls_root_for(&p).is_some(), "{p}"),
            other => panic!("expected Translated, got {other:?}"),
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_mount_traversal_cannot_escape_to_a_host_file() {
        // Upstream #1170: an above-mount `..` clamps at the mount root instead
        // of escaping, so these name the (missing) game file `outside.txt` —
        // never the host sibling next to the game folder.
        let f = Fixture::new("traversal");
        let outside = f.dir.join("outside.txt");
        std::fs::write(&outside, b"host secret").unwrap();
        let v = &f.vfs;

        for path in [
            "G:\\..\\outside.txt".to_string(),
            "g:/../outside.txt".to_string(),
            "GAME:\\..\\outside.txt".to_string(),
            "C:\\somewhere\\G:\\..\\outside.txt".to_string(),
            format!("\\\\?\\{}\\GAME:\\..\\outside.txt", f.game().display()),
            format!("{}\\GAME:\\..\\outside.txt", f.game().display()),
        ] {
            assert_eq!(
                v.classify(&path),
                Target::Game { rel: "outside.txt".into() },
                "{path}"
            );
            assert_eq!(
                v.open_decision(&path, GENERIC_WRITE, OPEN_EXISTING, 0),
                Decision::Fail(ERROR_FILE_NOT_FOUND),
                "{path}"
            );
            // Inside the mount (like any other missing game file) — never the
            // host sibling.
            let got = v.real_path_for_read(&path);
            assert!(
                matches!(&got, Some(p) if p.ends_with("game/outside.txt")),
                "{path} -> {got:?}"
            );
        }

        // Parent components that stay inside the mount are still normalised.
        assert_eq!(
            v.classify("G:\\data\\..\\data\\hello.txt"),
            Target::Game { rel: "data\\hello.txt".into() }
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"host secret");
        assert!(v.redirected_writes().is_empty());
        // Upstream #1560 reconcile: a traversal rel fails with PATH_NOT_FOUND
        // (the Win32 code for the operation) instead of the containment layer's
        // ACCESS_DENIED — the symlink-escape Reject legs below are unchanged.
        assert_eq!(v.prepare_write("..\\outside.txt", false), Err(ERROR_PATH_NOT_FOUND));
        assert_eq!(v.read_path_rel("..\\outside.txt"), None);
    }

    #[cfg(unix)]
    #[test]
    fn linux_mount_symlink_cannot_escape_to_a_host_file() {
        use std::os::unix::fs::symlink;

        let f = Fixture::new("symlink_escape");
        let outside = f.dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"host secret").unwrap();
        symlink(&outside, f.game().join("escape")).unwrap();

        assert_eq!(
            f.vfs.classify("G:\\escape\\secret.txt"),
            Target::Reject(ERROR_ACCESS_DENIED)
        );
        assert_eq!(
            f.vfs.open_decision("G:\\escape\\secret.txt", GENERIC_READ, OPEN_EXISTING, 0),
            Decision::Fail(ERROR_ACCESS_DENIED)
        );
        assert_eq!(f.vfs.real_path_for_read("G:\\escape\\secret.txt"), None);
    }

    #[cfg(unix)]
    #[test]
    fn linux_data_mount_symlink_cannot_leave_the_title_data_directory() {
        use std::os::unix::fs::symlink;

        let f = Fixture::new("data_symlink_escape");
        let outside = f.dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir(f.dir.join("xcldata")).unwrap();
        symlink(&outside, f.dir.join("xcldata").join("temp")).unwrap();

        assert_eq!(
            f.vfs.classify("T:\\secret.txt"),
            Target::Reject(ERROR_ACCESS_DENIED)
        );
        assert_eq!(
            f.vfs.open_decision("T:\\secret.txt", GENERIC_WRITE, CREATE_ALWAYS, 0),
            Decision::Fail(ERROR_ACCESS_DENIED)
        );
        assert!(!outside.join("secret.txt").exists());
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_staged_open_serializes_concurrent_write_bookkeeping() {
        use std::sync::mpsc;
        use std::time::Duration;

        let f = Fixture::new("write_serialization");
        let v = &f.vfs;
        let (decision, commit_rel, guard) =
            v.open_decision_linux("G:\\data\\hello.txt", GENERIC_READ, OPEN_EXISTING, 0);
        assert!(matches!(decision, Decision::Use(_)));
        assert!(commit_rel.is_none());

        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let _ = v.delete_file("data\\gone.txt");
                tx.send(()).unwrap();
            });
            assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
            drop(guard);
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
        });
        assert!(v.is_whiteout("data\\gone.txt"));
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_unmapped_drive_is_rejected_without_a_host_open() {
        let f = Fixture::new("unmapped");
        assert_eq!(
            f.vfs.classify("Z:\\Windows\\notepad.exe"),
            Target::Reject(ERROR_PATH_NOT_FOUND)
        );
        assert_eq!(
            f.vfs.open_decision("Z:\\Windows\\notepad.exe", GENERIC_READ, OPEN_EXISTING, 0),
            Decision::Fail(ERROR_PATH_NOT_FOUND)
        );
        assert_eq!(
            f.vfs.unmapped_paths(),
            vec!["Z:\\Windows\\notepad.exe".to_string()]
        );
    }

    #[test]
    fn mount_prefixes_accept_either_separator() {
        // Upstream #1234: separators inside configured prefixes are normalised
        // before matching, so `GAME:/data` does not fall through to `GAME:`.
        // The `logical` fold in `Vfs::new` already normalises `/` inside
        // prefixes (step-39); this pins the behaviour. Linux joins translated
        // host paths with `/` (Windows `\`); the console spelling keeps `\`.
        let f = Fixture::new("prefix_slashes");
        let game = f.game();
        let target = f.dir.join("redirect").to_string_lossy().into_owned();
        let hello = format!("{target}{}hello.txt", std::path::MAIN_SEPARATOR);
        for prefix in ["GAME:\\data", "GAME:/data", "GaMe:\\data/", "game:/data/"] {
            let v = Vfs::new(
                &game,
                &f.dir.join("xcldata"),
                vec![("GAME:".into(), game.to_string_lossy().into_owned()), (prefix.into(), target.clone())],
            );
            for path in ["GAME:\\data\\hello.txt", "game:/data/hello.txt"] {
                assert_eq!(v.classify(path), Target::Translated(hello.clone()), "{prefix}");
            }
            assert_eq!(v.classify("GAME:\\data"), Target::Translated(target.clone()), "{prefix}");
            assert_eq!(
                v.classify("GAME:\\database\\hello.txt"),
                Target::Game { rel: "database\\hello.txt".into() },
                "{prefix}"
            );
        }
    }

    /// The console's "D:" mount on a PC whose game and XCL data are on its D: drive: paths
    /// already inside those folders are the PC's, everything else on the drive is the console's.
    #[cfg(windows)]
    #[test]
    fn a_console_drive_mount_leaves_the_pcs_own_folders_on_that_drive() {
        let dir = std::env::temp_dir().join(format!("xcl_vfs_drive_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let game = dir.join("game");
        let data = dir.join("xcldata");
        std::fs::create_dir_all(game.join("data")).unwrap();
        let game_s = full_path(&game.to_string_lossy()).unwrap();
        let drive = game_s[..2].to_string(); // the drive the PC keeps the game on
        let v = Vfs::new(Path::new(&game_s), &data, vec![(drive.clone(), game_s.clone())]);

        let exe = format!("{game_s}\\xcl_testtitle.exe");
        assert_eq!(v.classify(&exe), Target::Game { rel: "xcl_testtitle.exe".into() });
        assert_eq!(v.classify(&game_s), Target::Game { rel: String::new() });
        let log = full_path(&data.join("logs").join("xcl.log").to_string_lossy()).unwrap();
        assert_eq!(v.classify(&log), Target::Pass);
        // a console path on that drive still goes to the game folder
        assert_eq!(v.classify(&format!("{drive}\\data\\hello.txt")), Target::Game { rel: "data\\hello.txt".into() });
        // "GAME:\x" made relative to a current directory on that drive
        let v = Vfs::new(Path::new(&game_s), &data, vec![(drive.clone(), game_s.clone()), ("GAME:".into(), game_s.clone())]);
        let mangled = format!("{drive}\\cwd\\GAME:\\data\\hello.txt");
        assert_eq!(v.classify(&mangled), Target::Game { rel: "data\\hello.txt".into() });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shader cache on the drive the console's "D:" mount names: without `own_folder` the
    /// title's process writes it into the overlay (and reads the game folder) – a cache filled
    /// outside the title, where no mount applies, is not the one it then uses.
    #[cfg(windows)]
    #[test]
    fn a_console_drive_mount_leaves_xcls_shader_cache_on_that_drive() {
        let dir = std::env::temp_dir().join(format!("xcl_vfs_cache_{}", std::process::id()));
        let game_s = full_path(&dir.join("game").to_string_lossy()).unwrap();
        let drive = game_s[..2].to_string();
        let cache = full_path(&dir.join("shader cache").join("dxil").to_string_lossy()).unwrap();
        let file = format!("{cache}\\0123456789abcdef.dxil");
        let mounts = || vec![(drive.clone(), game_s.clone())];

        let before = Vfs::new(Path::new(&game_s), &dir.join("xcldata"), mounts());
        // (the mount's target is the game folder: the cache file is taken for a game file)
        assert_eq!(before.classify(&file), Target::Game { rel: file[3..].to_string() });

        let v = Vfs::new(Path::new(&game_s), &dir.join("xcldata"), mounts()).own_folder(Path::new(&cache));
        assert_eq!(v.classify(&file), Target::Pass);
        assert_eq!(v.classify(&cache), Target::Pass);
        assert_eq!(v.classify(&format!("{cache}\\translated\\0_Compute_0_4_1.bin")), Target::Pass);
        // the folder's name is not a prefix of other names, and the rest of the drive is the console's
        assert_eq!(v.classify(&format!("{cache}2\\x.dxil")), Target::Game { rel: format!("{}2\\x.dxil", &cache[3..]) });
        assert_eq!(v.classify(&format!("{drive}\\data\\hello.txt")), Target::Game { rel: "data\\hello.txt".into() });
        // a verbatim path (`std::fs::canonicalize`, which the DXIL cache uses for its folder)
        assert_eq!(v.classify(&format!("\\\\?\\{file}")), Target::Pass);
    }

    /// Linux twin of upstream #2101: a console drive mount whose target holds one of XCL's
    /// own folders must not route the title into it. Windows pins this through
    /// `in_own_folders` (drive-letter resolution of the spelled path); Linux has no drive
    /// letters, so the exemption runs on the translated host path instead.
    #[cfg(not(windows))]
    #[test]
    fn a_console_drive_mount_leaves_xcls_own_folders_outside_the_game() {
        let dir = std::env::temp_dir().join(format!("xcl_vfs_own_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let game = dir.join("game");
        let tools = dir.join("tools");
        std::fs::create_dir_all(game.join("data")).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join("opus.dll"), b"tool").unwrap();
        std::fs::write(game.join("data").join("hello.txt"), b"original").unwrap();
        // the operator mounts the drive at the parent holding both the game and the tools
        let mounts = vec![("D:".to_string(), dir.to_string_lossy().to_string())];
        let v = Vfs::new(&game, &dir.join("xcldata"), mounts).own_folder(&tools);
        // the tool folder stays where it is instead of becoming title files ...
        assert_eq!(v.classify("D:\\tools\\opus.dll"), Target::Pass);
        assert_eq!(v.classify("D:\\tools"), Target::Pass);
        // ... while the game beside it still classifies through the mount ...
        assert_eq!(
            v.classify("D:\\game\\data\\hello.txt"),
            Target::Game { rel: "data\\hello.txt".into() }
        );
        assert_eq!(v.classify("D:\\game"), Target::Game { rel: String::new() });
        // ... and a lookalike prefix is not an own folder.
        assert!(matches!(v.classify("D:\\tools2\\opus.dll"), Target::Translated(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Upstream #2101 ignores bare drive roots in `own_folder` so an empty or root path
    /// cannot disable a whole console drive mount. Linux absolutises `own_folder` inputs
    /// against the cwd, so a bare `D:\` can never resolve to a matchable root – pin that
    /// registering one still leaves the mount classifying `Game` (characterization: passes
    /// with and without the guard; the guard itself is shared-file verbatim).
    #[cfg(not(windows))]
    #[test]
    fn a_bare_drive_root_own_folder_disables_no_console_mount() {
        let f = Fixture::new("own_root_guard");
        let v = Vfs::new(
            &f.game(),
            &f.dir.join("xcldata"),
            vec![("D:".to_string(), f.game().to_string_lossy().to_string())],
        )
        .own_folder(Path::new("D:\\"))
        .own_folder(Path::new("D:\\sub\\.."));
        assert_eq!(
            v.classify("D:\\data\\hello.txt"),
            Target::Game { rel: "data\\hello.txt".into() }
        );
    }

    #[test]
    fn reads_never_touch_and_writes_copy_on_write() {
        let f = Fixture::new("cow");
        let v = &f.vfs;
        let orig = f.game().join("data").join("hello.txt");
        // read → original
        assert_eq!(
            v.open_decision("G:\\data\\hello.txt", 0x8000_0000, OPEN_EXISTING, 0),
            Decision::Use(orig.to_string_lossy().to_string())
        );
        // write → overlay copy with original content
        let d = v.open_decision("G:\\data\\hello.txt", 0x4000_0000, OPEN_EXISTING, 0);
        let Decision::Use(ov) = d else { panic!("{d:?}") };
        assert!(ov.contains("overlay"));
        assert_eq!(std::fs::read(&ov).unwrap(), b"original");
        std::fs::write(&ov, b"modified").unwrap();
        // later reads see the overlay
        assert_eq!(v.open_decision("G:\\data\\hello.txt", 0x8000_0000, OPEN_EXISTING, 0), Decision::Use(ov.clone()));
        // original untouched
        assert_eq!(std::fs::read(&orig).unwrap(), b"original");
        // CREATE_ALWAYS on a new file → overlay, nothing created in the game folder
        let Decision::Use(newp) = v.open_decision("G:\\data\\new.bin", 0x4000_0000, CREATE_ALWAYS, 0) else { panic!() };
        assert!(newp.contains("overlay"));
        assert!(!f.game().join("data").join("new.bin").exists());
        // CREATE_NEW on an existing file fails
        assert_eq!(v.open_decision("G:\\data\\hello.txt", 0x4000_0000, CREATE_NEW, 0), Decision::Fail(ERROR_FILE_EXISTS));
        assert_eq!(v.redirected_writes(), vec!["data\\hello.txt".to_string(), "data\\new.bin".to_string()]);
    }

    #[test]
    fn reads_take_the_original_and_read_only_open_always_still_creates() {
        let f = Fixture::new("rdonly");
        let v = &f.vfs;
        let data = f.game().join("data");
        // a plain read of a missing file names the original: the open itself reports the error
        assert_eq!(
            v.open_decision("G:\\data\\missing.bin", 0x8000_0000, OPEN_EXISTING, 0),
            Decision::Use(data.join("missing.bin").to_string_lossy().to_string())
        );
        // OPEN_ALWAYS reads an existing file where it is …
        assert_eq!(
            v.open_decision("G:\\data\\hello.txt", 0x8000_0000, OPEN_ALWAYS, 0),
            Decision::Use(data.join("hello.txt").to_string_lossy().to_string())
        );
        // … and creates a missing one in the overlay even without write access
        let d = v.open_decision("G:\\data\\created.bin", 0x8000_0000, OPEN_ALWAYS, 0);
        let Decision::Use(p) = d else { panic!("{d:?}") };
        assert!(p.contains("overlay"), "{p}");
        assert!(!data.join("created.bin").exists());
        assert_eq!(v.redirected_writes(), vec!["data\\created.bin".to_string()]);
    }

    #[test]
    fn an_overlay_from_an_earlier_run_is_seen_but_a_whiteout_list_alone_is_none() {
        let f = Fixture::new("ovrun");
        let data_dir = f.dir.join("xcldata");
        let mounts = || vec![("G:".to_string(), f.game().to_string_lossy().to_string())];
        // an earlier run deleted a game file: the overlay folder holds only the whiteout list
        assert_eq!(f.vfs.delete_file("data\\gone.txt"), Ok(()));
        let v = Vfs::new(&f.game(), &data_dir, mounts());
        assert!(!v.overlay_used.load(Ordering::Acquire));
        assert_eq!(v.open_decision("G:\\data\\gone.txt", 0x8000_0000, OPEN_EXISTING, 0), Decision::Fail(ERROR_FILE_NOT_FOUND));
        // an earlier run wrote a game file: this run reads its copy
        let ov = f.vfs.prepare_write("data\\hello.txt", true).unwrap();
        std::fs::write(&ov, b"modified").unwrap();
        let v = Vfs::new(&f.game(), &data_dir, mounts());
        assert_eq!(v.open_decision("G:\\data\\hello.txt", 0x8000_0000, OPEN_EXISTING, 0), Decision::Use(ov));
    }

    #[test]
    fn an_empty_overlay_is_not_asked_until_this_run_writes_to_it() {
        let f = Fixture::new("ovfresh");
        let v = &f.vfs;
        let orig = f.game().join("data").join("hello.txt").to_string_lossy().to_string();
        // a file put into the overlay folder behind the VFS's back is not looked for …
        let ov = v.overlay_path("data\\hello.txt");
        std::fs::create_dir_all(Path::new(&ov).parent().unwrap()).unwrap();
        std::fs::write(&ov, b"elsewhere").unwrap();
        assert_eq!(v.open_decision("G:\\data\\hello.txt", 0x8000_0000, OPEN_EXISTING, 0), Decision::Use(orig));
        // … nor is a folder there, for directory checks and listings …
        std::fs::create_dir_all(v.overlay_path("data\\extra")).unwrap();
        assert!(!v.is_dir("data\\extra"));
        assert!(!v.needs_merge("data"));
        // … until the VFS writes to the overlay itself
        let d = v.open_decision("G:\\data\\new.bin", GENERIC_WRITE, CREATE_ALWAYS, 0);
        assert!(matches!(d, Decision::Use(_)), "{d:?}");
        assert_eq!(v.open_decision("G:\\data\\hello.txt", 0x8000_0000, OPEN_EXISTING, 0), Decision::Use(ov));
        assert!(v.is_dir("data\\extra"));
        assert!(v.needs_merge("data"));
    }

    #[test]
    fn delete_is_a_persistent_whiteout() {
        let f = Fixture::new("wo");
        let v = &f.vfs;
        assert_eq!(v.delete_file("data\\gone.txt"), Ok(()));
        assert!(f.game().join("data").join("gone.txt").exists(), "original must stay");
        assert_eq!(v.open_decision("G:\\data\\gone.txt", 0x8000_0000, OPEN_EXISTING, 0), Decision::Fail(ERROR_FILE_NOT_FOUND));
        assert_eq!(v.delete_file("data\\gone.txt"), Err(ERROR_FILE_NOT_FOUND));
        assert!(v.needs_merge("data"));
        // persisted
        let v2 = Vfs::new(&f.game(), &f.dir.join("xcldata"), vec![]);
        assert!(v2.is_whiteout("data\\gone.txt"));
        // re-creating the file un-hides it (in the overlay)
        let Decision::Use(p) = v.open_decision("G:\\data\\gone.txt", 0x4000_0000, CREATE_ALWAYS, 0) else { panic!() };
        assert!(p.contains("overlay"));
        assert!(!v.is_whiteout("data\\gone.txt"));
    }

    #[test]
    fn directories() {
        let f = Fixture::new("dir");
        let v = &f.vfs;
        assert_eq!(v.create_dir("data"), Err(ERROR_ALREADY_EXISTS));
        assert_eq!(v.create_dir("saves\\slot1"), Ok(()));
        assert!(!f.game().join("saves").exists());
        assert!(v.visible_in_overlay("saves\\slot1"));
        assert_eq!(v.remove_dir("data"), Err(ERROR_DIR_NOT_EMPTY));
        assert!(!v.is_whiteout("data"));
        assert!(v.visible_in_original("data\\hello.txt"));
        assert_eq!(v.delete_file("data\\hello.txt"), Ok(()));
        assert_eq!(v.delete_file("data\\gone.txt"), Ok(()));
        assert_eq!(v.remove_dir("data"), Ok(()));
        assert!(f.game().join("data").join("hello.txt").exists());
        let reloaded = Vfs::new(&f.game(), &f.dir.join("xcldata"), vec![]);
        assert!(!reloaded.exists("data"));
        assert_eq!(v.remove_dir("saves\\slot1"), Ok(()));
        assert!(!v.exists("saves\\slot1"));
    }

    #[test]
    fn remove_dir_checks_both_layers_before_removing_overlay() {
        let f = Fixture::new("remove_merged_dir");
        let v = &f.vfs;
        std::fs::create_dir_all(v.overlay_path("data")).unwrap();
        // put there behind the VFS's back: in use, as for a run that finds it at start
        v.mark_overlay_used();
        assert_eq!(v.remove_dir("data"), Err(ERROR_DIR_NOT_EMPTY));
        assert!(v.visible_in_overlay("data"));
        assert!(!v.is_whiteout("data"));
        assert_eq!(v.delete_file("data\\hello.txt"), Ok(()));
        assert_eq!(v.delete_file("data\\gone.txt"), Ok(()));

        // Overlay entries remain visible even when their original names are whited out.
        std::fs::write(v.overlay_path("data\\hello.txt"), b"overlay").unwrap();
        assert_eq!(v.remove_dir("data"), Err(ERROR_DIR_NOT_EMPTY));
        assert!(v.visible_in_overlay("data\\hello.txt"));
        assert!(!v.is_whiteout("data"));
        std::fs::remove_file(v.overlay_path("data\\hello.txt")).unwrap();
        assert_eq!(v.remove_dir("data"), Ok(()));
        assert!(!v.exists("data"));
        assert_eq!(std::fs::read(f.game().join("data").join("hello.txt")).unwrap(), b"original");
    }

    #[test]
    fn remove_dir_rejects_visible_subdirectories() {
        let f = Fixture::new("remove_dir_child");
        let v = &f.vfs;
        std::fs::create_dir(f.game().join("data").join("sub")).unwrap();
        assert_eq!(v.delete_file("data\\hello.txt"), Ok(()));
        assert_eq!(v.delete_file("data\\gone.txt"), Ok(()));
        assert_eq!(v.remove_dir("data"), Err(ERROR_DIR_NOT_EMPTY));
        assert_eq!(v.remove_dir("data\\sub"), Ok(()));
        assert_eq!(v.remove_dir("data"), Ok(()));
        assert!(f.game().join("data").join("sub").is_dir());
    }

    #[test]
    fn whiteouts_and_paths_normalize_slashes() {
        let f = Fixture::new("norm_slashes");
        let v = &f.vfs;

        // Path helpers normalize forward slashes and strip leading/trailing separators.
        // Upstream #1299: all spellings name the same overlay/original file.
        let base = v.overlay_path("data\\test.txt");
        assert_eq!(v.overlay_path("data/test.txt"), base);
        assert_eq!(v.overlay_path("/data/test.txt/"), base);
        assert_eq!(v.overlay_path("\\data\\test.txt\\"), base);
        assert!(base.ends_with(&format!("overlay{}data{}test.txt", std::path::MAIN_SEPARATOR, std::path::MAIN_SEPARATOR)));
        let orig_base = v.original_path("data\\test.txt");
        assert_eq!(v.original_path("data/test.txt"), orig_base);
        assert_eq!(v.original_path("/data/test.txt/"), orig_base);
        assert_eq!(v.original_path("\\data\\test.txt\\"), orig_base);

        // Whiteouts query and match regardless of forward, leading or trailing slashes.
        assert_eq!(v.delete_file("data/gone.txt"), Ok(()));
        assert!(v.is_whiteout("data\\gone.txt"));
        assert!(v.is_whiteout("data/gone.txt"));
        assert!(v.is_whiteout("/data/gone.txt"));
        assert!(v.is_whiteout("\\data\\gone.txt"));

        // Whiteouts under a directory work with trailing slashes.
        assert!(v.has_whiteouts_under("data"));
        assert!(v.has_whiteouts_under("data\\"));
        assert!(v.has_whiteouts_under("data/"));
        assert!(v.has_whiteouts_under("/data/"));
        assert!(v.needs_merge("data"));
        assert!(v.needs_merge("data\\"));
        assert!(v.needs_merge("data/"));

        // Directory whiteout hides children when queried with forward slashes.
        assert_eq!(v.delete_file("data\\hello.txt"), Ok(()));
        assert_eq!(v.remove_dir("data/"), Ok(()));
        assert!(v.is_whiteout("data"));
        assert!(v.is_whiteout("data/hello.txt"));
        assert!(v.is_whiteout("data\\hello.txt"));
        assert!(v.is_whiteout("data/sub/file.txt"));
    }

    #[cfg(windows)]
    #[test]
    fn truncate_existing_rewrites_unoverlaid_disposition_and_opens_with_win32() {
        let f = Fixture::new("trunc");
        let v = &f.vfs;
        let orig = f.game().join("data").join("hello.txt");

        // Un-overlaid game file: TRUNCATE_EXISTING rewrites disposition to CREATE_ALWAYS in the
        // overlay WITHOUT publishing a placeholder file before the native CreateFile call runs.
        let Decision::UseWithDisposition(ov, disp) =
            v.open_decision("G:\\data\\hello.txt", GENERIC_WRITE, TRUNCATE_EXISTING, 0)
        else {
            panic!("expected Decision::UseWithDisposition for TRUNCATE_EXISTING on un-overlaid game file");
        };
        assert!(ov.contains("overlay"));
        assert_eq!(disp, CREATE_ALWAYS);
        assert!(!Path::new(&ov).exists(), "no placeholder file before native open");
        assert_eq!(std::fs::read(&orig).unwrap(), b"original");

        // Opening the returned overlay path with the rewritten disposition creates a 0-byte overlay file.
        let w = crate::util::wide(&ov);
        let h = unsafe {
            windows_sys::Win32::Storage::FileSystem::CreateFileW(
                w.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                disp,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(h, windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
        assert_eq!(std::fs::read(&ov).unwrap(), b"");
        assert_eq!(std::fs::read(&orig).unwrap(), b"original");

        // Already-overlaid file keeps TRUNCATE_EXISTING and is truncated by Win32 on open.
        std::fs::write(&ov, b"overlay-content").unwrap();
        assert_eq!(
            v.open_decision("G:\\data\\hello.txt", GENERIC_WRITE, TRUNCATE_EXISTING, 0),
            Decision::Use(ov.clone())
        );
        assert_eq!(std::fs::read(&ov).unwrap(), b"overlay-content");
        let h2 = unsafe {
            windows_sys::Win32::Storage::FileSystem::CreateFileW(
                w.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                TRUNCATE_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(h2, windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h2) };
        assert_eq!(std::fs::read(&ov).unwrap(), b"");
        assert_eq!(std::fs::read(&orig).unwrap(), b"original");
    }

    #[cfg(not(windows))]
    #[test]
    fn truncate_existing_rewrites_unoverlaid_disposition_on_host_open() {
        use std::fs::OpenOptions;
        let f = Fixture::new("trunc_host");
        let v = &f.vfs;
        let orig = f.game().join("data").join("hello.txt");

        // Un-overlaid game file: the disposition is rewritten to CREATE_ALWAYS in the overlay,
        // and the path is not published before the host open runs.
        let Decision::UseWithDisposition(ov, disp) =
            v.open_decision("G:\\data\\hello.txt", GENERIC_WRITE, TRUNCATE_EXISTING, 0)
        else {
            panic!("expected Decision::UseWithDisposition for TRUNCATE_EXISTING on un-overlaid game file");
        };
        assert!(ov.contains("overlay"));
        assert_eq!(disp, CREATE_ALWAYS);
        assert!(
            !Path::new(&ov).exists(),
            "no placeholder file before the host open"
        );
        assert_eq!(std::fs::read(&orig).unwrap(), b"original");
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&ov)
            .unwrap();
        assert_eq!(std::fs::read(&ov).unwrap(), b"");
        assert_eq!(std::fs::read(&orig).unwrap(), b"original");

        // Already overlaid: TRUNCATE_EXISTING is kept and the overlay copy is truncated.
        std::fs::write(&ov, b"overlay-content").unwrap();
        assert_eq!(
            v.open_decision("G:\\data\\hello.txt", GENERIC_WRITE, TRUNCATE_EXISTING, 0),
            Decision::Use(ov.clone())
        );
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&ov)
            .unwrap();
        assert_eq!(std::fs::read(&ov).unwrap(), b"");
        assert_eq!(std::fs::read(&orig).unwrap(), b"original");
    }

    #[test]
    fn truncate_existing_validation_errors() {
        let f = Fixture::new("trunc_err");
        let v = &f.vfs;

        // Missing GENERIC_WRITE fails with ERROR_INVALID_PARAMETER (even if missing) without touching overlay.
        assert_eq!(
            v.open_decision("G:\\data\\hello.txt", 0x8000_0000, TRUNCATE_EXISTING, 0),
            Decision::Fail(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            v.open_decision("G:\\data\\hello.txt", 0x0000_0002, TRUNCATE_EXISTING, 0),
            Decision::Fail(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            v.open_decision("G:\\data\\hello.txt", 0x1000_0000, TRUNCATE_EXISTING, 0),
            Decision::Fail(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            v.open_decision("G:\\data\\missing.txt", 0x8000_0000, TRUNCATE_EXISTING, 0),
            Decision::Fail(ERROR_INVALID_PARAMETER)
        );
        assert!(!v.visible_in_overlay("data\\hello.txt"));

        // Missing or whited-out file with GENERIC_WRITE fails with ERROR_FILE_NOT_FOUND.
        assert_eq!(
            v.open_decision("G:\\data\\missing.txt", GENERIC_WRITE, TRUNCATE_EXISTING, 0),
            Decision::Fail(ERROR_FILE_NOT_FOUND)
        );
        assert_eq!(v.delete_file("data\\gone.txt"), Ok(()));
        assert_eq!(
            v.open_decision("G:\\data\\gone.txt", GENERIC_WRITE, TRUNCATE_EXISTING, 0),
            Decision::Fail(ERROR_FILE_NOT_FOUND)
        );

        // Directory fails without creating an overlay entry.
        #[cfg(windows)]
        {
            // Windows' own CreateFileW codes are the reference; Wine answers a directory opened
            // with FILE_FLAG_BACKUP_SEMANTICS and TRUNCATE_EXISTING with ERROR_FILE_EXISTS.
            let dir_w = crate::util::wide(&f.game().join("data").to_string_lossy());
            for (flags, expected) in [
                (0, ERROR_ACCESS_DENIED),
                (FILE_FLAG_BACKUP_SEMANTICS, ERROR_INVALID_PARAMETER),
            ] {
                let h = unsafe {
                    windows_sys::Win32::Storage::FileSystem::CreateFileW(
                        dir_w.as_ptr(),
                        GENERIC_WRITE,
                        7,
                        std::ptr::null(),
                        TRUNCATE_EXISTING,
                        flags,
                        std::ptr::null_mut(),
                    )
                };
                // (read at once: the first under_wine() looks up an export ntdll does not have on
                // Windows, which sets the thread's last error to ERROR_PROC_NOT_FOUND)
                let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
                assert_eq!(h, windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
                if !crate::util::under_wine() {
                    assert_eq!(err, expected);
                }
            }
        }
        for (flags, expected) in [
            (0, ERROR_ACCESS_DENIED),
            (FILE_FLAG_BACKUP_SEMANTICS, ERROR_INVALID_PARAMETER),
        ] {
            assert_eq!(
                v.open_decision("G:\\data", GENERIC_WRITE, TRUNCATE_EXISTING, flags),
                Decision::Fail(expected)
            );
        }
        assert!(!v.visible_in_overlay("data"));
    }

    #[test]
    fn delete_file_rejects_directories_and_remove_dir_rejects_files() {
        let f = Fixture::new("type_checks");
        let v = &f.vfs;

        // Match native Win32 error codes for mismatched DeleteFileW / RemoveDirectoryW.
        #[cfg(windows)]
        {
            let dir_w = crate::util::wide(&f.game().join("data").to_string_lossy());
            let file_w =
                crate::util::wide(&f.game().join("data").join("hello.txt").to_string_lossy());
            // SAFETY: dir_w and file_w are NUL-terminated UTF-16 paths; native Win32 rejects them without modifying disk.
            unsafe {
                assert_eq!(
                    windows_sys::Win32::Storage::FileSystem::DeleteFileW(dir_w.as_ptr()),
                    0
                );
                assert_eq!(
                    windows_sys::Win32::Foundation::GetLastError(),
                    ERROR_ACCESS_DENIED
                );
                assert_eq!(
                    windows_sys::Win32::Storage::FileSystem::RemoveDirectoryW(file_w.as_ptr()),
                    0
                );
                assert_eq!(
                    windows_sys::Win32::Foundation::GetLastError(),
                    ERROR_DIRECTORY
                );
            }
        }

        // delete_file must reject both original and overlay directories without adding whiteouts.
        assert_eq!(v.delete_file("data"), Err(ERROR_ACCESS_DENIED));
        assert!(!v.is_whiteout("data"));
        assert!(v.exists("data"));

        assert_eq!(v.create_dir("saves"), Ok(()));
        assert_eq!(v.delete_file("saves"), Err(ERROR_ACCESS_DENIED));
        assert!(v.visible_in_overlay("saves"));

        // remove_dir must reject both original and overlay files without adding whiteouts.
        assert_eq!(v.remove_dir("data\\hello.txt"), Err(ERROR_DIRECTORY));
        assert!(!v.is_whiteout("data\\hello.txt"));
        assert!(v.exists("data\\hello.txt"));

        let ov_file = v.prepare_write("data\\ov_only.txt", false).unwrap();
        std::fs::write(&ov_file, b"hi").unwrap();
        assert_eq!(v.remove_dir("data\\ov_only.txt"), Err(ERROR_DIRECTORY));
        assert!(v.visible_in_overlay("data\\ov_only.txt"));
    }

    #[test]
    fn recreating_removed_directory_does_not_resurrect_original_children() {
        let f = Fixture::new("recreate_dir");
        let v = &f.vfs;
        std::fs::create_dir_all(f.game().join("data").join("sub")).unwrap();
        std::fs::write(f.game().join("data").join("sub").join("deep.txt"), b"deep").unwrap();

        assert_eq!(v.delete_file("data\\hello.txt"), Ok(()));
        assert_eq!(v.delete_file("data\\gone.txt"), Ok(()));
        assert_eq!(v.delete_file("data\\sub\\deep.txt"), Ok(()));
        assert_eq!(v.remove_dir("data\\sub"), Ok(()));
        assert_eq!(v.remove_dir("data"), Ok(()));
        assert!(v.is_whiteout("data"));
        assert!(v.is_whiteout("data\\hello.txt"));
        assert!(v.is_whiteout("data\\sub\\deep.txt"));

        // Recreating `data` must push whiteouts down to original children so none resurrect.
        assert_eq!(v.create_dir("data"), Ok(()));
        assert!(!v.is_whiteout("data"));
        assert!(v.is_dir("data"));
        assert!(v.is_whiteout("data\\hello.txt"));
        assert!(v.is_whiteout("data\\gone.txt"));
        assert!(v.is_whiteout("data\\sub"));
        assert!(v.is_whiteout("data\\sub\\deep.txt"));
        assert_eq!(v.read_path_rel("data\\hello.txt"), None);
        assert_eq!(v.read_path_rel("data\\sub\\deep.txt"), None);

        // Recreating a nested subdirectory also must not resurrect its original children.
        assert_eq!(v.create_dir("data\\sub"), Ok(()));
        assert!(!v.is_whiteout("data\\sub"));
        assert!(v.is_whiteout("data\\sub\\deep.txt"));
        assert_eq!(v.read_path_rel("data\\sub\\deep.txt"), None);

        // Whiteouts persist across reload.
        let v2 = Vfs::new(&f.game(), &f.dir.join("xcldata"), vec![]);
        assert!(!v2.is_whiteout("data"));
        assert!(v2.is_whiteout("data\\hello.txt"));
        assert!(v2.is_whiteout("data\\gone.txt"));
        assert!(v2.is_whiteout("data\\sub\\deep.txt"));

        // Recreating one file un-hides only that file.
        let Decision::Use(p) = v.open_decision("G:\\data\\hello.txt", GENERIC_WRITE, CREATE_ALWAYS, 0) else {
            panic!("expected overlay path");
        };
        std::fs::write(&p, b"fresh").unwrap();
        assert!(!v.is_whiteout("data\\hello.txt"));
        assert!(v.is_whiteout("data\\gone.txt"));
        assert!(v.is_whiteout("data\\sub\\deep.txt"));
    }

    #[test]
    fn write_below_a_missing_parent_fails_without_creating_it() {
        let f = Fixture::new("noparent");
        let v = &f.vfs;
        // Win32: CreateFileW CREATE_ALWAYS under a folder that exists in neither layer
        // fails with ERROR_PATH_NOT_FOUND (3); it does not conjure the folder.
        assert_eq!(v.prepare_write("nodir\\f.bin", true), Err(ERROR_PATH_NOT_FOUND));
        assert!(!Path::new(&v.overlay_path("nodir")).exists());
    }

    #[test]
    fn write_below_a_removed_dir_does_not_resurrect_it() {
        let f = Fixture::new("norez");
        let v = &f.vfs;
        std::fs::create_dir(f.game().join("empty")).unwrap();
        assert_eq!(v.remove_dir("empty"), Ok(()));
        assert_eq!(v.prepare_write("empty\\f.bin", true), Err(ERROR_PATH_NOT_FOUND));
        assert!(!Path::new(&v.overlay_path("empty")).exists());
        assert!(v.is_whiteout("empty"));
    }

    #[test]
    fn write_below_a_game_only_parent_still_works() {
        let f = Fixture::new("gameparent");
        let v = &f.vfs;
        std::fs::create_dir(f.game().join("gonly")).unwrap();
        let p = v.prepare_write("gonly\\n.txt", false).expect("game-only parent is visible");
        std::fs::write(&p, b"new").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
    }

    #[test]
    fn reopening_an_existing_overlay_entry_keeps_delete_on_close_whiteout() {
        // Upstream #1038: an open with FILE_FLAG_DELETE_ON_CLOSE removes only the
        // overlay file on close, so a whiteout is recorded once the open succeeds.
        // A second handle opening the existing overlay entry must keep that
        // whiteout instead of clearing it.
        let f = Fixture::new("doc_whiteout");
        let v = &f.vfs;
        const DELETE: u32 = 0x0001_0000;
        // First handle: DELETE_ON_CLOSE open of a game-original file (mirrors the
        // title's GENERIC_READ|DELETE + OPEN_EXISTING open).
        let Decision::Use(first) = v.open_decision(
            "G:\\data\\hello.txt",
            GENERIC_READ | DELETE,
            OPEN_EXISTING,
            FILE_FLAG_DELETE_ON_CLOSE,
        ) else {
            panic!("delete-on-close open must stage the overlay copy");
        };
        assert_eq!(std::fs::read(&first).unwrap(), b"original");
        // The native open succeeded: record the whiteout the close leaves behind.
        v.commit_overlay_removal("data\\hello.txt");
        assert!(v.is_whiteout("data\\hello.txt"));
        // Second handle opens the existing overlay entry; committing it must not
        // clear the whiteout hiding the original.
        let Decision::Use(second) =
            v.open_decision("G:\\data\\hello.txt", GENERIC_WRITE, OPEN_EXISTING, 0)
        else {
            panic!("second open must reuse the existing overlay entry");
        };
        assert_eq!(first, second);
        assert!(
            v.is_whiteout("data\\hello.txt"),
            "reopening an existing overlay entry must keep the delete-on-close whiteout"
        );
        // A genuinely new overlay entry still replaces a whiteout (re-creation).
        std::fs::remove_file(&second).unwrap();
        let Decision::Use(_) =
            v.open_decision("G:\\data\\hello.txt", GENERIC_WRITE, CREATE_ALWAYS, 0)
        else {
            panic!("re-creation must succeed");
        };
        assert!(!v.is_whiteout("data\\hello.txt"), "a new overlay entry replaces the whiteout");
    }

    #[test]
    fn unc_game_root_protects_game_folder_from_writes() {
        let game = Path::new(r"\\server\share\Game");
        let data = Path::new(r"\\server\share\Data");
        let v = Vfs::new(game, data, vec![]);
        let full = |p: &str| Some(p.to_string());

        assert_eq!(
            v.classify_with(r"\\server\share\Game\save.dat", full),
            Target::Game { rel: "save.dat".to_string() }
        );
        assert_eq!(
            v.classify_with(r"\\server\share\Game", full),
            Target::Game { rel: String::new() }
        );
        assert_eq!(
            v.classify_with(r"\\server\share\Other\file.txt", full),
            Target::Pass
        );
        assert_eq!(
            v.classify_with(r"\\other_server\share\Game\save.dat", full),
            Target::Pass
        );
    }

    #[test]
    fn classify_handles_unicode_case_folding_and_verbatim_roots() {
        // Upstream #316: match the game root per separator-separated component
        // against its lowercased form, so folding-expanding chars (ẞ: 3 bytes
        // -> ß: 2 bytes, İ: 2 -> 3, K: 3 -> 1) classify to the same
        // `Game{rel}` in the input's original casing instead of slicing a
        // lowercased string at a stale byte offset; verbatim-prefixed roots
        // (`\\?\`, `\??\`, `UNC` case-insensitive) normalise like plain roots.

        // The matcher itself: upstream table, with the root lowercased the way
        // `Vfs::new` stores it.
        for (root, abs, want) in [
            ("C:\\Games\\GROẞẞ", "C:\\Games\\großß\\Data\\a.bin", Some("Data\\a.bin")),
            ("C:\\Games\\großß", "C:\\Games\\GROẞẞ\\Data\\a.bin", Some("Data\\a.bin")),
            ("C:\\Games\\İİ", "C:\\Games\\i\u{307}i\u{307}\\Sub\\b.bin", Some("Sub\\b.bin")),
            ("C:\\Games\\i\u{307}i\u{307}", "C:\\Games\\İİ\\Sub\\b.bin", Some("Sub\\b.bin")),
            ("C:\\Games\\K", "C:\\Games\\k\\c.bin", Some("c.bin")),
            ("C:\\Games\\k", "C:\\Games\\K\\c.bin", Some("c.bin")),
            ("C:\\Games\\GROẞ", "C:\\Games\\groß", Some("")),
            ("C:\\Games\\Title", "C:\\Games\\Title\\\\Data\\x.bin", Some("Data\\x.bin")),
            ("C:\\Games\\GROẞ", "C:\\Games\\groß2\\a.bin", None),
            // Either separator matches either side (Linux host paths use `/`).
            ("C:\\Games\\GROẞẞ", "C:/Games/großß/Data/a.bin", Some("Data/a.bin")),
        ] {
            assert_eq!(
                strip_path_prefix_ci(abs, &root.to_lowercase()).map(|s| s.to_string()),
                want.map(|s| s.to_string()),
                "root={root:?} abs={abs:?}"
            );
        }

        // Verbatim prefixes strip case-insensitively (upstream #316).
        for (input, want) in [
            ("\\\\?\\C:\\Games\\Title", "C:\\Games\\Title"),
            ("\\\\?\\UNC\\server\\share\\f.bin", "\\\\server\\share\\f.bin"),
            ("\\\\?\\unc\\server\\share\\f.bin", "\\\\server\\share\\f.bin"),
            ("\\??\\UNC\\server\\share\\f.bin", "\\\\server\\share\\f.bin"),
            ("\\??\\C:\\Games\\Title", "C:\\Games\\Title"),
            ("C:\\Games\\Title", "C:\\Games\\Title"),
        ] {
            assert_eq!(strip_verbatim(input), want, "input={input:?}");
        }

        // Verbatim-spelled roots normalise like plain roots (upstream #316).
        for (verbatim, plain) in [
            ("\\\\?\\C:\\Games\\Title", "C:\\Games\\Title"),
            ("\\\\?\\unc\\server\\share\\Game", "\\\\server\\share\\Game"),
            ("\\??\\UNC\\server\\share\\Game", "\\\\server\\share\\Game"),
        ] {
            let a = Vfs::new(Path::new(verbatim), Path::new("C:\\Data\\Title"), vec![]);
            let b = Vfs::new(Path::new(plain), Path::new("C:\\Data\\Title"), vec![]);
            assert_eq!(
                a.game_root(),
                b.game_root(),
                "verbatim root {verbatim:?} must normalise like {plain:?}"
            );
        }

        // End to end through `classify_with` / `classify`.
        #[cfg(windows)]
        {
            // Upstream test as-is (drive letters resolve on Windows).
            let id = |p: &str| Some(p.to_string());
            for (root, input, want_rel) in [
                ("C:\\Games\\GROẞẞ", "C:\\Games\\großß\\Data\\a.bin", "Data\\a.bin"),
                ("C:\\Games\\großß", "C:\\Games\\GROẞẞ\\Data\\a.bin", "Data\\a.bin"),
                ("C:\\Games\\İİ", "C:\\Games\\i\u{307}i\u{307}\\Sub\\b.bin", "Sub\\b.bin"),
                ("C:\\Games\\i\u{307}i\u{307}", "C:\\Games\\İİ\\Sub\\b.bin", "Sub\\b.bin"),
                ("C:\\Games\\K", "C:\\Games\\k\\c.bin", "c.bin"),
                ("C:\\Games\\k", "C:\\Games\\K\\c.bin", "c.bin"),
                ("C:\\Games\\GROẞ", "C:\\Games\\groß", ""),
                ("C:\\Games\\GROẞẞ", "C:/Games/großß/Data/a.bin", "Data\\a.bin"),
                ("C:\\Games\\Title", "C:\\Games\\Title\\\\Data\\a.bin", "Data\\a.bin"),
                ("\\\\?\\C:\\Games\\Title", "C:\\Games\\Title\\Data\\x.bin", "Data\\x.bin"),
                ("\\\\?\\C:\\Games\\Title", "\\\\?\\C:\\Games\\Title\\Data\\x.bin", "Data\\x.bin"),
            ] {
                let v = Vfs::new(Path::new(root), Path::new("\\\\?\\C:\\Data\\Title"), vec![]);
                assert_eq!(
                    v.classify_with(input, id),
                    Target::Game { rel: want_rel.to_string() },
                    "root={root:?} input={input:?}"
                );
            }

            // Sibling directories sharing a prefix must not match.
            let v = Vfs::new(Path::new("C:\\Games\\GROẞ"), Path::new("C:\\Data\\Title"), vec![]);
            assert_eq!(v.classify_with("C:\\Games\\groß2\\a.bin", id), Target::Pass);
            // Case-insensitive verbatim UNC prefixes stay UNC and never resolve relative to game_root.
            let rel_to_game = |p: &str| {
                if p.starts_with("\\\\") || p.as_bytes().get(1) == Some(&b':') {
                    Some(p.to_string())
                } else {
                    Some(format!("C:\\Games\\GROẞ\\{p}"))
                }
            };
            assert_eq!(v.classify_with("\\\\?\\unc\\server\\share\\file.bin", rel_to_game), Target::Pass);
            assert_eq!(v.classify_with("\\??\\UNC\\server\\share\\file.bin", rel_to_game), Target::Pass);
            // Verbatim roots still recognise own folders in in_own_folders (prevents C: mount hijack).
            let vc = Vfs::new(
                Path::new("\\\\?\\C:\\Games\\Title"),
                Path::new("\\\\?\\C:\\Data\\Title"),
                vec![("C:".into(), "C:\\Games\\Title".into())],
            );
            assert_eq!(vc.classify_with("C:\\Data\\Title\\saves\\s.dat", id), Target::Pass);
            assert_eq!(vc.classify_with("C:\\Games\\Title\\Data\\x.bin", id), Target::Game { rel: "Data\\x.bin".into() });
        }
        #[cfg(not(windows))]
        {
            // Linux has no drive letters: an untranslated `C:\...` console
            // path is Reject, so the same spellings run through UNC roots
            // (which keep `\` separators on both sides) and real temp-dir
            // roots instead.
            let id = |p: &str| Some(p.to_string());
            // A folding-expanding UNC root classifies a differently-cased
            // input to the same rel (the old byte-offset slice panicked here).
            let v = Vfs::new(Path::new("\\\\server\\share\\groß"), Path::new("C:\\Data\\Title"), vec![]);
            assert_eq!(
                v.classify_with("\\\\server\\share\\GROẞ\\a.bin", id),
                Target::Game { rel: "a.bin".into() }
            );
            // Sibling directories sharing a prefix must not match.
            let v = Vfs::new(Path::new("\\\\server\\share\\GROẞ"), Path::new("C:\\Data\\Title"), vec![]);
            assert_eq!(v.classify_with("\\\\server\\share\\groß2\\a.bin", id), Target::Pass);
            // Case-insensitive verbatim UNC prefixes stay UNC and never
            // resolve relative to the game root.
            assert_eq!(v.classify_with("\\\\?\\unc\\server\\share\\file.bin", id), Target::Pass);
            assert_eq!(v.classify_with("\\??\\UNC\\server\\share\\file.bin", id), Target::Pass);
            // Unicode roots through real host paths keep their rel casing.
            let dir = std::env::temp_dir().join(format!("xcl_vfs_fold_{}", std::process::id()));
            let game = dir.join("GROẞẞ");
            std::fs::create_dir_all(game.join("Data")).unwrap();
            std::fs::write(game.join("Data").join("a.bin"), b"x").unwrap();
            let v = Vfs::new(&game, &dir.join("xcldata"), vec![]);
            let abs = game.join("Data").join("a.bin").to_string_lossy().into_owned();
            assert_eq!(v.classify(&abs), Target::Game { rel: "Data\\a.bin".into() });
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn clean_rel_resolves_dots_and_detects_boundary_traversal() {
        // Upstream #1560 audit: `/` folds to `\`, leading/trailing/double
        // separators collapse, `.` drops out, `..` pops inside the root and
        // returns None past it (as do drive/UNC/device spellings).
        assert_eq!(clean_rel("data/hello.txt"), Some("data\\hello.txt".into()));
        assert_eq!(clean_rel("data\\sub\\..\\hello.txt"), Some("data\\hello.txt".into()));
        assert_eq!(clean_rel("data\\.\\hello.txt"), Some("data\\hello.txt".into()));
        assert_eq!(clean_rel("/data/hello.txt/"), Some("data\\hello.txt".into()));
        assert_eq!(clean_rel("data\\\\hello.txt"), Some("data\\hello.txt".into()));
        assert_eq!(clean_rel(""), Some(String::new()));
        assert_eq!(clean_rel("/"), Some(String::new()));
        assert_eq!(clean_rel("\\"), Some(String::new()));
        assert_eq!(clean_rel("."), Some(String::new()));
        assert_eq!(clean_rel("./"), Some(String::new()));
        assert_eq!(clean_rel(".\\"), Some(String::new()));

        // Out-of-bounds directory traversal attempts return None.
        assert_eq!(clean_rel(".."), None);
        assert_eq!(clean_rel("..\\something"), None);
        assert_eq!(clean_rel("data\\..\\..\\secret.txt"), None);
        assert_eq!(clean_rel("a/b/../../../c"), None);

        // Drive letters, UNC, and device paths return None.
        assert_eq!(clean_rel("C:\\secret.txt"), None);
        assert_eq!(clean_rel("\\\\server\\share"), None);
        assert_eq!(clean_rel("data:stream"), None);
    }

    #[test]
    fn case_insensitive_path_canonicalization_and_verbatim_unc() {
        // Upstream #1560 audit: verbatim UNC prefixes strip case-insensitively,
        // mount prefixes normalise `/`, and whiteouts stay slash-insensitive.
        let f = Fixture::new("canon");
        let v = &f.vfs;

        // Verbatim UNC paths are case-insensitively stripped and classified as Pass.
        assert_eq!(v.classify("\\\\?\\unc\\server\\share\\file.txt"), Target::Pass);
        assert_eq!(v.classify("\\\\?\\Unc\\server\\share\\file.txt"), Target::Pass);
        assert_eq!(v.classify("\\\\?\\UNC\\server\\share\\file.txt"), Target::Pass);
        assert_eq!(v.classify("\\??\\unc\\server\\share\\file.txt"), Target::Pass);
        assert_eq!(v.classify("\\??\\UNC\\server\\share\\file.txt"), Target::Pass);

        // Verbatim DOS paths with \??\ prefix: `classify_host` strips the
        // verbatim prefix first, so the spelled host file resolves on both
        // platforms (Linux host-absolute, Windows drive-absolute).
        let real_str = f.game().join("data").join("hello.txt").to_string_lossy().to_string();
        let qmark = format!("\\??\\{real_str}");
        assert_eq!(v.classify(&qmark), Target::Game { rel: "data\\hello.txt".into() });

        // Forward slash mount definition normalization.
        let dir = std::env::temp_dir().join(format!("xcl_vfs_mount_slash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mnt_target = dir.join("target");
        std::fs::create_dir_all(&mnt_target).unwrap();
        let v_mnt = Vfs::new(
            &f.game(),
            &f.dir.join("xcldata"),
            vec![("MNT/DIR:".to_string(), mnt_target.to_string_lossy().to_string())],
        );
        let mnt_res = v_mnt.classify("mnt/dir:/sub/file.txt");
        #[cfg(windows)]
        assert!(matches!(mnt_res, Target::Translated(ref p) if p.ends_with("sub\\file.txt")), "{mnt_res:?}");
        #[cfg(not(windows))]
        assert!(matches!(mnt_res, Target::Translated(ref p) if p.ends_with("sub/file.txt")), "{mnt_res:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whiteouts_case_insensitivity_and_traversal_equivalence() {
        // Upstream #1560 audit: every spelling of a path (slashes, case, dots)
        // names the same whiteout entry.
        let f = Fixture::new("wo_canon");
        let v = &f.vfs;

        // Delete with forward slash or mixed case creates canonical whiteout.
        assert_eq!(v.delete_file("data/gone.txt"), Ok(()));
        assert!(v.is_whiteout("data\\gone.txt"));
        assert!(v.is_whiteout("data/gone.txt"));
        assert!(v.is_whiteout("DATA/GONE.TXT"));
        assert!(v.is_whiteout("data\\.\\gone.txt"));
        assert!(v.is_whiteout("data\\sub\\..\\gone.txt"));
        assert!(v.is_whiteout("\\data\\gone.txt\\"));
        assert!(v.is_whiteout("data\\\\gone.txt"));

        // Reads are properly hidden across all equivalent path forms.
        assert_eq!(v.read_path_rel("data/gone.txt"), None);
        assert_eq!(v.read_path_rel("DATA\\GONE.TXT"), None);
        assert_eq!(v.read_path_rel("data\\.\\gone.txt"), None);

        // Directory whiteout queries handle trailing and forward slashes.
        assert!(v.has_whiteouts_under("data"));
        assert!(v.has_whiteouts_under("data\\"));
        assert!(v.has_whiteouts_under("data/"));
        assert!(v.has_whiteouts_under("DATA"));
        assert!(v.has_whiteouts_under("data/./"));
        assert!(v.needs_merge("data"));
        assert!(v.needs_merge("data\\"));
        assert!(v.needs_merge("data/"));

        // Recreating file via forward slash removes whiteout.
        let Decision::Use(p) = v.open_decision("G:/data/gone.txt", GENERIC_WRITE, CREATE_ALWAYS, 0) else {
            panic!("expected overlay path");
        };
        std::fs::write(&p, b"recreated").unwrap();
        assert!(!v.is_whiteout("data\\gone.txt"));
        assert!(!v.is_whiteout("data/gone.txt"));
    }

    #[test]
    fn boundary_checks_reject_directory_traversal_escapes() {
        // Upstream #1560 audit: rels that escape the root fail with safe
        // codes (or None/false) without touching the host filesystem.
        let f = Fixture::new("bounds");
        let v = &f.vfs;

        // Path operations attempting to escape root return safe error codes / None without touching filesystem.
        assert_eq!(v.delete_file("..\\secret.txt"), Err(ERROR_FILE_NOT_FOUND));
        assert_eq!(v.delete_file("data\\..\\..\\secret.txt"), Err(ERROR_FILE_NOT_FOUND));
        assert_eq!(v.stage_write("..\\secret.txt"), Err(ERROR_PATH_NOT_FOUND));
        assert_eq!(v.stage_write("data\\..\\..\\secret.txt"), Err(ERROR_PATH_NOT_FOUND));
        assert_eq!(v.prepare_write("..\\secret.txt", true), Err(ERROR_PATH_NOT_FOUND));
        assert_eq!(v.create_dir("..\\escaped"), Err(ERROR_PATH_NOT_FOUND));
        assert_eq!(v.remove_dir("..\\escaped"), Err(ERROR_PATH_NOT_FOUND));
        assert_eq!(v.read_path_rel("..\\secret.txt"), None);
        assert!(!v.is_whiteout("..\\secret.txt"));
        assert!(!v.has_whiteouts_under("..\\escape"));
        assert!(!v.exists("..\\secret.txt"));
        assert!(!v.is_dir("..\\secret.txt"));
        assert!(!v.visible_in_overlay("..\\secret.txt"));
        assert!(!v.visible_in_original("..\\secret.txt"));

        // Open decision for Target::Game with traversal rel fails safely.
        assert_eq!(
            v.open_decision_for(Target::Game { rel: "..\\secret.txt".into() }, 0x8000_0000, OPEN_EXISTING, 0),
            Decision::Fail(ERROR_PATH_NOT_FOUND)
        );
    }

    #[test]
    fn translated_mount_validates_path_against_mount_root_and_rejects_traversal() {
        // Upstream #1560 audit: a mount-relative `..` must not escape the mount
        // root to a host sibling. Windows rejects with the `..` sentinel
        // (Game `..` -> PATH_NOT_FOUND); Linux keeps the #1170 clamp
        // (below_mount + ensure_mount_contained), so the same inputs stay
        // inside the mount. Both never touch the host sibling.
        let f = Fixture::new("mnt_bounds");
        let v = &f.vfs;

        let dir = std::env::temp_dir().join(format!("xcl_vfs_mnt_bounds_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mnt_root = dir.join("mount");
        std::fs::create_dir_all(&mnt_root).unwrap();
        let mnt_str = full_path(&mnt_root.to_string_lossy()).unwrap();

        let v_mnt = Vfs::new(
            &f.game(),
            &f.dir.join("xcldata"),
            vec![("MNT:".to_string(), mnt_str.clone()), ("G:".to_string(), mnt_str.clone())],
        );

        // Valid relative paths within translated mount are accepted.
        let ok_res = v_mnt.classify("MNT:\\file.txt");
        #[cfg(windows)]
        assert!(matches!(ok_res, Target::Translated(ref p) if p.ends_with("mount\\file.txt")), "{ok_res:?}");
        #[cfg(not(windows))]
        assert!(matches!(ok_res, Target::Translated(ref p) if p.ends_with("mount/file.txt")), "{ok_res:?}");
        let ok_nested = v_mnt.classify("MNT:\\sub\\..\\file.txt");
        #[cfg(windows)]
        assert!(matches!(ok_nested, Target::Translated(ref p) if p.ends_with("mount\\file.txt")), "{ok_nested:?}");
        #[cfg(not(windows))]
        assert!(matches!(ok_nested, Target::Translated(ref p) if p.ends_with("mount/file.txt")), "{ok_nested:?}");
        #[cfg(windows)]
        let want_use = format!("{mnt_str}\\file.txt");
        #[cfg(not(windows))]
        let want_use = format!("{mnt_str}/file.txt");
        assert_eq!(
            v_mnt.open_decision("MNT:\\file.txt", 0x8000_0000, OPEN_EXISTING, 0),
            Decision::Use(want_use)
        );

        // Traversal escaping the mount root must not reach a host sibling.
        #[cfg(windows)]
        {
            // Traversal escaping translated mount root is rejected.
            assert_eq!(v_mnt.classify("MNT:\\..\\outside.txt"), Target::Game { rel: "..".into() });
            assert_eq!(
                v_mnt.open_decision("MNT:\\..\\outside.txt", 0x8000_0000, OPEN_EXISTING, 0),
                Decision::Fail(ERROR_PATH_NOT_FOUND)
            );
            assert_eq!(v_mnt.real_path_for_read("MNT:\\..\\outside.txt"), None);

            // Traversal escaping translated G: mount rooted at non-game folder is rejected.
            assert_eq!(v_mnt.classify("G:\\..\\outside.txt"), Target::Game { rel: "..".into() });
            assert_eq!(
                v_mnt.open_decision("G:\\..\\outside.txt", 0x8000_0000, OPEN_EXISTING, 0),
                Decision::Fail(ERROR_PATH_NOT_FOUND)
            );
            assert_eq!(v_mnt.real_path_for_read("G:\\..\\outside.txt"), None);

            // Traversal escaping via multi-level subdirectories is rejected.
            assert_eq!(v_mnt.classify("MNT:\\sub\\..\\..\\outside.txt"), Target::Game { rel: "..".into() });
            assert_eq!(
                v_mnt.open_decision("MNT:\\sub\\..\\..\\outside.txt", 0x8000_0000, OPEN_EXISTING, 0),
                Decision::Fail(ERROR_PATH_NOT_FOUND)
            );
            assert_eq!(v_mnt.real_path_for_read("MNT:\\sub\\..\\..\\outside.txt"), None);

            // Traversal escaping game root via default G: mount is rejected.
            assert_eq!(v.classify("G:\\..\\outside.txt"), Target::Game { rel: "..".into() });
            assert_eq!(
                v.open_decision("G:\\..\\outside.txt", 0x8000_0000, OPEN_EXISTING, 0),
                Decision::Fail(ERROR_PATH_NOT_FOUND)
            );
            assert_eq!(v.real_path_for_read("G:\\..\\outside.txt"), None);
        }
        #[cfg(not(windows))]
        {
            // Linux clamp: `..` past the root stays inside the mount, never the
            // host sibling next to it.
            for path in ["MNT:\\..\\outside.txt", "G:\\..\\outside.txt", "MNT:\\sub\\..\\..\\outside.txt"] {
                let got = v_mnt.classify(path);
                assert!(
                    matches!(got, Target::Translated(ref p) if p.ends_with("mount/outside.txt")),
                    "{path} -> {got:?}"
                );
                let want = format!("{mnt_str}/outside.txt");
                assert_eq!(
                    v_mnt.open_decision(path, 0x8000_0000, OPEN_EXISTING, 0),
                    Decision::Use(want.clone()),
                    "{path}"
                );
                assert_eq!(v_mnt.real_path_for_read(path), Some(want), "{path}");
            }
            // The default game mount clamps the same way (the dedicated
            // `linux_mount_traversal_*` test pins the host sibling is untouched).
            assert_eq!(
                v.classify("G:\\..\\outside.txt"),
                Target::Game { rel: "outside.txt".into() }
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
