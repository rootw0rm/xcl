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

pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PATH_NOT_FOUND: u32 = 3;
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_FILE_EXISTS: u32 = 80;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const ERROR_DIR_NOT_EMPTY: u32 = 145;
pub const ERROR_DIRECTORY: u32 = 267;

/// State to restore when a Linux host open fails after the VFS staged a game write.
#[cfg(not(windows))]
pub(crate) struct FailedWriteRollback {
    rel: String,
    whiteouts: Vec<String>,
    was_recorded: bool,
}

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
const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;

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
    let s = strip_verbatim(&s.replace('\\', "/"));
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
    let path = normalize_host(Path::new(&join_rel(root, tail)));
    ensure_mount_contained(Path::new(root), Path::new(&path))?;
    Ok(path)
}

fn strip_verbatim(p: &str) -> String {
    if let Some(r) = p.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{r}");
    }
    if let Some(r) = p.strip_prefix("\\\\?\\") {
        return r.to_string();
    }
    if let Some(r) = p.strip_prefix("\\??\\") {
        return r.to_string();
    }
    p.to_string()
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
        let game_root = trim_sep(&host_path_string(&game_root.to_string_lossy()));
        let overlay_dir = data_dir.join("overlay");
        let whiteout_file = overlay_dir.join(".xcl_whiteouts");
        let whiteouts = std::fs::read_to_string(&whiteout_file)
            .map(|s| s.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        let mut mounts: Vec<(String, String)> = mounts
            .into_iter()
            .map(|(p, t)| {
                (
                    p.to_ascii_lowercase()
                        .trim_end_matches(['\\', '/'])
                        .to_string(),
                    trim_sep(&host_path_string(&t)),
                )
            })
            .collect();
        // longest prefix first ("GAME:" before "G:")
        mounts.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        let data_root = data_dir.to_string_lossy();
        let data_root = full_path(&data_root).unwrap_or_else(|| data_root.into_owned());
        let pls_root = Path::new(&data_root).join("pls");
        let pls_root_lc = strip_verbatim(&norm(&pls_root.to_string_lossy())).to_lowercase();
        Vfs {
            game_root_lc: game_root.to_lowercase(),
            data_root_lc: trim_sep(&norm(&data_root)).to_lowercase(),
            pls_root,
            pls_root_lc,
            own_roots_lc: Vec::new(),
            game_root,
            overlay: trim_sep(&norm(&overlay_dir.to_string_lossy())),
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

    /// `dir` is a folder of XCL's own outside the title's data folder – the shader cache, which
    /// titles share. A console drive mount must not translate its paths either: with the cache on
    /// the PC's D: drive ("D:" is the console's game drive) the title's process wrote its DXIL
    /// into the overlay and looked for it in the game folder, so what `xcl shaders --precompile`
    /// had put into the cache was never found and every run compiled again.
    pub fn own_folder(mut self, dir: &Path) -> Vfs {
        let dir = dir.to_string_lossy();
        let dir = full_path(&dir).unwrap_or_else(|| dir.into_owned());
        let dir = trim_sep(&strip_verbatim(&norm(&dir))).to_lowercase();
        if !dir.is_empty() {
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
            .map(|(target, tail)| join_rel(target, tail))
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
            return Target::Pass; // UNC
        }
        #[cfg(windows)]
        let (p, translated) = match self.apply_mounts(&p) {
            Some(t) if !self.in_own_folders(&p, &full) => (t, true),
            _ => (p, false),
        };
        #[cfg(not(windows))]
        let (p, translated) = match self.apply_mounts_checked(&p) {
            Some(Ok(t)) => (t, true),
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
        let lc = abs.to_lowercase();
        if lc == self.game_root_lc {
            #[cfg(not(windows))]
            if ensure_mount_contained(Path::new(&self.game_root), Path::new(&abs)).is_err() {
                return Target::Reject(ERROR_ACCESS_DENIED);
            }
            return Target::Game { rel: String::new() };
        }
        if let Some(rest) = lc.strip_prefix(&self.game_root_lc) {
            if rest.starts_with(std::path::MAIN_SEPARATOR) {
                #[cfg(not(windows))]
                if ensure_mount_contained(Path::new(&self.game_root), Path::new(&abs)).is_err() {
                    return Target::Reject(ERROR_ACCESS_DENIED);
                }
                return Target::Game {
                    rel: rel_from_host(&abs[self.game_root.len() + 1..]),
                };
            }
        }
        if translated {
            Target::Translated(abs)
        } else {
            Target::Pass
        }
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
        join_rel(&self.overlay, rel)
    }

    pub fn original_path(&self, rel: &str) -> String {
        join_rel(&self.game_root, rel)
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

    fn wkey(rel: &str) -> String {
        rel.to_lowercase()
    }

    pub fn is_whiteout(&self, rel: &str) -> bool {
        let k = Self::wkey(rel);
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
        let k = Self::wkey(rel_dir);
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
        let mut g = self.whiteouts.lock().unwrap();
        if g.insert(Self::wkey(rel)) {
            self.persist_whiteouts(&g);
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn restore_failed_write(&self, rollback: &FailedWriteRollback) {
        {
            let mut g = self.whiteouts.lock().unwrap();
            g.clear();
            g.extend(rollback.whiteouts.iter().cloned());
            self.persist_whiteouts(&g);
        }
        if !rollback.was_recorded {
            self.writes.lock().unwrap().remove(&rollback.rel);
        }
    }

    fn remove_whiteout(&self, rel: &str) {
        #[cfg(not(windows))]
        if Self::checked_rel(rel).is_err() {
            return;
        }
        let k = Self::wkey(rel);
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
                        let name = entry.file_name().to_string_lossy().to_lowercase();
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
        let mut w = self.writes.lock().unwrap();
        if w.len() < 10_000 && w.insert(rel.to_string()) {
            crate::xlog_info!("[VFS] write to a game file redirected to the overlay: {rel}");
        }
    }

    pub fn visible_in_overlay(&self, rel: &str) -> bool {
        #[cfg(windows)]
        let path = self.overlay_path(rel);
        #[cfg(not(windows))]
        let Ok(path) = self.checked_overlay_path(rel) else {
            return false;
        };
        Path::new(&path).exists()
    }

    pub fn visible_in_original(&self, rel: &str) -> bool {
        if self.is_whiteout(rel) {
            return false;
        }
        #[cfg(windows)]
        let path = self.original_path(rel);
        #[cfg(not(windows))]
        let Ok(path) = self.checked_original_path(rel) else {
            return false;
        };
        Path::new(&path).exists()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.visible_in_overlay(rel) || self.visible_in_original(rel)
    }

    pub fn is_dir(&self, rel: &str) -> bool {
        #[cfg(windows)]
        let overlay = self.overlay_path(rel);
        #[cfg(not(windows))]
        let Ok(overlay) = self.checked_overlay_path(rel) else {
            return self.visible_in_original(rel)
                && Path::new(&self.original_path(rel)).is_dir();
        };
        if let Ok(m) = std::fs::metadata(overlay) {
            return m.is_dir();
        }
        self.visible_in_original(rel) && Path::new(&self.original_path(rel)).is_dir()
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
        if self.visible_in_overlay(rel) {
            return self.checked_overlay_path(rel).ok();
        }
        if self.is_whiteout(rel) {
            return None;
        }
        #[cfg(windows)]
        {
            Some(self.original_path(rel))
        }
        #[cfg(not(windows))]
        {
            self.checked_original_path(rel).ok()
        }
    }

    /// Stage the overlay path for a new/overwritten file (e.g. copy/move destination)
    /// without removing whiteouts or recording the write yet. Call `commit_write(rel)`
    /// only after the underlying file operation succeeds so a failed operation cannot
    /// resurrect a whited-out original.
    pub fn stage_write(&self, rel: &str) -> Result<String, u32> {
        let _guard = self.write_lock.lock().unwrap();
        self.stage_write_unlocked(rel)
    }

    fn stage_write_unlocked(&self, rel: &str) -> Result<String, u32> {
        if let Some((parent_rel, _)) = rel.rsplit_once('\\') {
            if !self.is_dir(parent_rel) {
                return Err(ERROR_PATH_NOT_FOUND);
            }
        }
        #[cfg(windows)]
        let ov = self.overlay_path(rel);
        #[cfg(not(windows))]
        let ov = self.checked_overlay_path(rel)?;
        if let Some(parent) = Path::new(&ov).parent() {
            std::fs::create_dir_all(parent).map_err(|_| ERROR_PATH_NOT_FOUND)?;
        }
        #[cfg(not(windows))]
        self.checked_overlay_path(rel)?;
        Ok(ov)
    }

    /// Finalise a staged write once the underlying file operation succeeded.
    pub fn commit_write(&self, rel: &str) {
        let _guard = self.write_lock.lock().unwrap();
        self.commit_write_unlocked(rel)
    }

    fn commit_write_unlocked(&self, rel: &str) {
        #[cfg(not(windows))]
        if Self::checked_rel(rel).is_err() {
            return;
        }
        self.remove_whiteout(rel);
        self.record_write(rel);
    }

    /// Record removal of an overlay file and hide the game file it covered.
    pub fn commit_overlay_removal(&self, rel: &str) {
        let _guard = self.write_lock.lock().unwrap();
        self.commit_overlay_removal_unlocked(rel)
    }

    fn commit_overlay_removal_unlocked(&self, rel: &str) {
        #[cfg(not(windows))]
        if Self::checked_rel(rel).is_err() {
            return;
        }
        #[cfg(windows)]
        let original_exists = Path::new(&self.original_path(rel)).exists();
        #[cfg(not(windows))]
        let original_exists = self
            .checked_original_path(rel)
            .is_ok_and(|path| Path::new(&path).exists());
        if original_exists {
            self.add_whiteout(rel);
        }
        self.record_write(rel);
    }

    /// Ensure `rel` exists in the overlay (copying the original when `keep_content`)
    /// and return the overlay path.
    pub fn prepare_write(&self, rel: &str, keep_content: bool) -> Result<String, u32> {
        let _guard = self.write_lock.lock().unwrap();
        self.prepare_write_unlocked(rel, keep_content)
    }

    fn prepare_write_unlocked(&self, rel: &str, keep_content: bool) -> Result<String, u32> {
        #[cfg(windows)]
        let ov = self.overlay_path(rel);
        #[cfg(not(windows))]
        let ov = self.checked_overlay_path(rel)?;
        if let Some(parent) = Path::new(&ov).parent() {
            std::fs::create_dir_all(parent).map_err(|_| ERROR_PATH_NOT_FOUND)?;
        }
        #[cfg(not(windows))]
        self.checked_overlay_path(rel)?;
        if keep_content && !Path::new(&ov).exists() && self.visible_in_original(rel) {
            #[cfg(windows)]
            let orig = self.original_path(rel);
            #[cfg(not(windows))]
            let orig = self.checked_original_path(rel)?;
            if Path::new(&orig).is_dir() {
                std::fs::create_dir_all(&ov).map_err(|_| ERROR_ACCESS_DENIED)?;
            } else {
                std::fs::copy(&orig, &ov).map_err(|_| ERROR_ACCESS_DENIED)?;
            }
        }
        self.remove_whiteout(rel);
        self.record_write(rel);
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
        match target {
            Target::Pass => Decision::Pass,
            #[cfg(not(windows))]
            Target::Reject(error) => Decision::Fail(error),
            Target::Translated(p) => {
                if Self::is_write(access, disposition, flags) || disposition == OPEN_ALWAYS {
                    if let Some(parent) = Path::new(&p).parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                }
                Decision::Use(p)
            }
            Target::Game { rel } => {
                let in_ov = self.visible_in_overlay(&rel);
                let in_orig = self.visible_in_original(&rel);
                let visible = in_ov || in_orig;
                let write = Self::is_write(access, disposition, flags) || (disposition == OPEN_ALWAYS && !visible);
                if !write {
                    return match self.read_path_rel(&rel) {
                        Some(p) => Decision::Use(p),
                        None => Decision::Fail(ERROR_FILE_NOT_FOUND),
                    };
                }
                // Win32 CreateFile requires GENERIC_WRITE for TRUNCATE_EXISTING before checking existence.
                if disposition == TRUNCATE_EXISTING && access & GENERIC_WRITE == 0 {
                    return Decision::Fail(ERROR_INVALID_PARAMETER);
                }
                match disposition {
                    CREATE_NEW if visible => return Decision::Fail(ERROR_FILE_EXISTS),
                    OPEN_EXISTING | TRUNCATE_EXISTING if !visible => return Decision::Fail(ERROR_FILE_NOT_FOUND),
                    _ => {}
                }
                let ov_entry = in_ov || {
                    #[cfg(windows)]
                    let overlay_entry = Path::new(&self.overlay_path(&rel)).symlink_metadata().is_ok();
                    #[cfg(not(windows))]
                    let overlay_entry = match self.checked_overlay_path(&rel) {
                        Ok(path) => Path::new(&path).symlink_metadata().is_ok(),
                        Err(error) => return Decision::Fail(error),
                    };
                    overlay_entry
                };
                if disposition == TRUNCATE_EXISTING {
                    let check_path = if in_ov { self.overlay_path(&rel) } else { self.original_path(&rel) };
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
                        return Decision::Fail(err);
                    }
                }
                let keep = matches!(disposition, OPEN_EXISTING | OPEN_ALWAYS);
                let replaces_original = disposition == CREATE_ALWAYS
                    && !ov_entry
                    && in_orig
                    && Path::new(&self.original_path(&rel)).is_file();
                match self.prepare_write_unlocked(&rel, keep) {
                    Ok(p) if disposition == TRUNCATE_EXISTING && !ov_entry => Decision::UseWithDisposition(p, CREATE_ALWAYS),
                    Ok(p) if replaces_original => Decision::CreateOverOriginal(p),
                    Ok(p) => Decision::Use(p),
                    Err(e) => Decision::Fail(e),
                }
            }
        }
    }

    pub fn open_decision(&self, path: &str, access: u32, disposition: u32, flags: u32) -> Decision {
        self.open_decision_for(self.classify(path), access, disposition, flags)
    }

    /// Linux host opens can fail after `prepare_write` has removed a whiteout. Return the
    /// affected game-relative path so the shim can restore it if `open(2)` does not create
    /// the overlay entry successfully.
    #[cfg(not(windows))]
    pub(crate) fn open_decision_linux(
        &self,
        path: &str,
        access: u32,
        disposition: u32,
        flags: u32,
    ) -> (Decision, Option<FailedWriteRollback>, std::sync::MutexGuard<'_, ()>) {
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
        let rollback = match &target {
            Target::Game { rel } if self.is_whiteout(rel) && !self.visible_in_overlay(rel) => {
                Some(FailedWriteRollback {
                    rel: rel.clone(),
                    whiteouts: self.whiteouts(),
                    was_recorded: self.writes.lock().unwrap().contains(rel),
                })
            }
            _ => None,
        };
        (
            self.open_decision_for_unlocked(target, access, disposition, flags),
            rollback,
            guard,
        )
    }

    pub fn delete_file(&self, rel: &str) -> Result<(), u32> {
        let _guard = self.write_lock.lock().unwrap();
        #[cfg(windows)]
        let ov = self.overlay_path(rel);
        #[cfg(not(windows))]
        let ov = self.checked_overlay_path(rel)?;
        let ov_path = Path::new(&ov);
        if let Ok(meta) = std::fs::metadata(ov_path) {
            if meta.is_dir() {
                return Err(ERROR_ACCESS_DENIED);
            }
            std::fs::remove_file(ov_path).map_err(|_| ERROR_ACCESS_DENIED)?;
            self.commit_overlay_removal_unlocked(rel);
            return Ok(());
        }
        if !self.visible_in_original(rel) {
            return Err(ERROR_FILE_NOT_FOUND);
        }
        if Path::new(&self.original_path(rel)).is_dir() {
            return Err(ERROR_ACCESS_DENIED);
        }
        self.add_whiteout(rel);
        self.record_write(rel);
        Ok(())
    }

    pub fn create_dir(&self, rel: &str) -> Result<(), u32> {
        let _guard = self.write_lock.lock().unwrap();
        if self.visible_in_overlay(rel) || self.visible_in_original(rel) {
            return Err(ERROR_ALREADY_EXISTS);
        }
        #[cfg(windows)]
        let ov = self.overlay_path(rel);
        #[cfg(not(windows))]
        let ov = self.checked_overlay_path(rel)?;
        std::fs::create_dir_all(&ov).map_err(|_| ERROR_PATH_NOT_FOUND)?;
        #[cfg(not(windows))]
        self.checked_overlay_path(rel)?;
        self.remove_whiteout(rel);
        self.record_write(rel);
        Ok(())
    }

    pub fn remove_dir(&self, rel: &str) -> Result<(), u32> {
        let _guard = self.write_lock.lock().unwrap();
        if rel.is_empty() {
            return Err(ERROR_ACCESS_DENIED);
        }
        #[cfg(windows)]
        let ov = self.overlay_path(rel);
        #[cfg(not(windows))]
        let ov = self.checked_overlay_path(rel)?;
        let ov_path = Path::new(&ov);
        if let Ok(meta) = std::fs::metadata(ov_path) {
            if !meta.is_dir() {
                return Err(ERROR_DIRECTORY);
            }
            std::fs::remove_dir(ov_path).map_err(|_| ERROR_DIR_NOT_EMPTY)?;
            #[cfg(windows)]
            let original_exists = Path::new(&self.original_path(rel)).exists();
            #[cfg(not(windows))]
            let original_exists = self
                .checked_original_path(rel)
                .is_ok_and(|path| Path::new(&path).exists());
            if original_exists {
                self.add_whiteout(rel);
            }
            self.record_write(rel);
            return Ok(());
        }
        if !self.visible_in_original(rel) {
            return Err(ERROR_FILE_NOT_FOUND);
        }
        if !Path::new(&self.original_path(rel)).is_dir() {
            return Err(ERROR_DIRECTORY);
        }
        self.add_whiteout(rel);
        self.record_write(rel);
        Ok(())
    }

    /// Whether directory listings under `rel_dir` must merge overlay + original.
    pub fn needs_merge(&self, rel_dir: &str) -> bool {
        #[cfg(windows)]
        let overlay_is_dir = Path::new(&self.overlay_path(rel_dir)).is_dir();
        #[cfg(not(windows))]
        let overlay_is_dir = self
            .checked_overlay_path(rel_dir)
            .is_ok_and(|path| Path::new(&path).is_dir());
        overlay_is_dir || self.has_whiteouts_under(rel_dir)
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

    #[cfg(not(windows))]
    #[test]
    fn linux_mount_traversal_cannot_escape_to_a_host_file() {
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
            assert_eq!(v.classify(&path), Target::Reject(ERROR_ACCESS_DENIED), "{path}");
            assert_eq!(
                v.open_decision(&path, GENERIC_WRITE, OPEN_EXISTING, 0),
                Decision::Fail(ERROR_ACCESS_DENIED),
                "{path}"
            );
            assert_eq!(v.real_path_for_read(&path), None, "{path}");
        }

        // Parent components that stay inside the mount are still normalised.
        assert_eq!(
            v.classify("G:\\data\\..\\data\\hello.txt"),
            Target::Game { rel: "data\\hello.txt".into() }
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"host secret");
        assert!(v.redirected_writes().is_empty());
        assert_eq!(v.prepare_write("..\\outside.txt", false), Err(ERROR_ACCESS_DENIED));
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
        let (decision, rollback, guard) =
            v.open_decision_linux("G:\\data\\hello.txt", GENERIC_READ, OPEN_EXISTING, 0);
        assert!(matches!(decision, Decision::Use(_)));
        assert!(rollback.is_none());

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
        assert_eq!(v.remove_dir("data"), Ok(()));
        assert!(f.game().join("data").exists());
        assert!(v.is_whiteout("data\\hello.txt"), "children of a removed dir are hidden");
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
                assert_eq!(h, windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
                if !crate::util::under_wine() {
                    assert_eq!(
                        unsafe { windows_sys::Win32::Foundation::GetLastError() },
                        expected
                    );
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
}
