//! Linux: linked-in file shims on the host's own call path (no `LD_PRELOAD`).
//!
//! The Windows build hooks the KERNELBASE file exports. Linux has no KERNELBASE: the HLE
//! `kernelx` file APIs call libc themselves, so the VFS sits *inside* that path — this module
//! is where a console path is resolved and opened. Nothing is interposed process-wide.
//!
//! The step-10c `kernelx` file APIs call `open` here; the CRT entry points stay stubs.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::FromRawFd;
use std::path::Path;

use crate::vfs::{
    self, Decision, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_PATH_NOT_FOUND,
};

/// A file opened through the VFS. `already_existed` is the Win32 `ERROR_ALREADY_EXISTS`
/// answer `CREATE_ALWAYS` gives when it replaced a file the caller could see.
/// `delete_on_close_rel` carries the game-relative path when the open asked for
/// `FILE_FLAG_DELETE_ON_CLOSE` on a game-original file: the caller records a whiteout
/// once the open succeeds and deletes the overlay copy at the last close (upstream #1038).
pub struct Opened {
    pub file: File,
    pub already_existed: bool,
    pub delete_on_close_rel: Option<String>,
}

/// Open `path` (a console or host path) through the process VFS, falling back to a plain host
/// open before `Vfs::init`.
pub fn open(path: &str, access: u32, disposition: u32, flags: u32) -> Result<Opened, u32> {
    validate_access(access, disposition)?;
    match vfs::get() {
        Some(v) => open_with(v, path, access, disposition, flags),
        None => {
            if disposition == vfs::CREATE_ALWAYS
                && !vfs::Vfs::has_overwrite_access(access)
                && Path::new(path).exists()
            {
                return Err(ERROR_ACCESS_DENIED);
            }
            if disposition == vfs::OPEN_ALWAYS
                && !vfs::Vfs::has_data_write_access(access)
                && !Path::new(path).exists()
            {
                return Err(ERROR_ACCESS_DENIED);
            }
            open_host(
                path,
                access,
                disposition,
                flags,
                disposition == vfs::CREATE_ALWAYS && Path::new(path).exists(),
            )
        }
    }
}

fn open_with(
    v: &vfs::Vfs,
    path: &str,
    access: u32,
    disposition: u32,
    flags: u32,
) -> Result<Opened, u32> {
    validate_access(access, disposition)?;
    let (decision, commit_rel, _write_guard) =
        v.open_decision_linux(path, access, disposition, flags);
    // A title-supplied host-absolute path keeps the title's separators: the
    // runtime absolves a folder (`XGameSave*` returns `/.../saves/<scid>`)
    // and the title appends `\file`. Backslash is never a literal filename
    // char to a Windows title, so the `Pass` arms below probe and open the
    // slash spelling. Console spellings (UNC/device/drive/relative) never
    // reach a successful host open either way.
    let already_existed = match &decision {
        Decision::CreateOverOriginal(_) => true,
        Decision::Use(path) | Decision::UseWithDisposition(path, _)
            if disposition == vfs::CREATE_ALWAYS || disposition == vfs::OPEN_ALWAYS =>
        {
            Path::new(path).exists()
        }
        Decision::Pass if disposition == vfs::CREATE_ALWAYS => Path::new(&path.replace('\\', "/")).exists(),
        _ => false,
    };
    let result = match decision {
        Decision::Fail(error) => Err(error),
        Decision::Pass => open_host(&path.replace('\\', "/"), access, disposition, flags, already_existed),
        Decision::Use(p) => open_host(&p, access, disposition, flags, already_existed),
        Decision::UseWithDisposition(p, disposition) => {
            open_host(&p, access, disposition, flags, already_existed)
        }
        Decision::CreateOverOriginal(p) => {
            open_host(&p, access, disposition, flags, already_existed)
        }
    };
    // Upstream #1038: an open with FILE_FLAG_DELETE_ON_CLOSE removes only the
    // overlay file on close. Only a new overlay entry may clear a whiteout
    // (reopening an existing entry keeps the delete-on-close whiteout), and once
    // the host open succeeds the original is hidden so it cannot resurface.
    let delete_on_close_rel = if result.is_ok() && flags & vfs::FILE_FLAG_DELETE_ON_CLOSE != 0 {
        match v.classify(path) {
            vfs::Target::Game { rel }
                if std::path::Path::new(&v.original_path(&rel)).is_file() =>
            {
                Some(rel)
            }
            _ => None,
        }
    } else {
        None
    };
    if result.is_ok() {
        if let Some(commit) = commit_rel {
            // Commit only after the host open succeeded: a failed open must neither
            // clear whiteouts nor record a write (upstream #1362). The commit runs
            // under the decision's `write_lock` guard, and the guest-visible last
            // error is set by the kernelx caller afterwards from the open result, so
            // no Win32 `GetLastError` save/restore is needed on this path.
            v.commit_open_unlocked(&commit.rel, commit.is_new);
        }
        if let Some(rel) = &delete_on_close_rel {
            // An existing overlay entry still wins lookups while the handle is open.
            // Once the overlay copy is deleted on close, the original must stay
            // hidden instead of resurfacing.
            v.commit_overlay_removal_unlocked(rel);
        }
    }
    result.map(|mut opened| {
        opened.delete_on_close_rel = delete_on_close_rel;
        opened
    })
}

fn validate_access(access: u32, disposition: u32) -> Result<(), u32> {
    if disposition == vfs::TRUNCATE_EXISTING && access & vfs::GENERIC_WRITE == 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    if disposition == vfs::CREATE_ALWAYS && !vfs::Vfs::has_data_write_access(access) {
        return Err(ERROR_ACCESS_DENIED);
    }
    if disposition == vfs::CREATE_NEW && !vfs::Vfs::has_data_write_access(access) {
        return Err(ERROR_ACCESS_DENIED);
    }
    Ok(())
}

fn open_host(
    path: &str,
    access: u32,
    disposition: u32,
    flags: u32,
    already_existed: bool,
) -> Result<Opened, u32> {
    validate_access(access, disposition)?;
    let c = CString::new(path).map_err(|_| vfs::ERROR_INVALID_PARAMETER)?;
    let flags = libc_flags(access, disposition, flags);
    // SAFETY: `c` is a NUL-terminated path; open either returns a fresh fd we own or -1.
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC, 0o666) };
    if fd < 0 {
        return Err(errno_to_win(
            io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ));
    }
    // SAFETY: open returned a valid fd and nothing else owns it.
    Ok(Opened {
        file: unsafe { File::from_raw_fd(fd) },
        already_existed,
        delete_on_close_rel: None,
    })
}

fn libc_flags(access: u32, disposition: u32, flags: u32) -> i32 {
    let write = vfs::Vfs::has_data_write_access(access);
    let read = vfs::Vfs::is_read(access);
    let mut open_flags = match (read, write) {
        (true, true) => libc::O_RDWR,
        (false, true) => libc::O_WRONLY,
        (false, false) => libc::O_PATH,
        _ => libc::O_RDONLY,
    };
    open_flags |= match disposition {
        vfs::CREATE_NEW => libc::O_CREAT | libc::O_EXCL,
        vfs::CREATE_ALWAYS => libc::O_CREAT | libc::O_TRUNC,
        vfs::OPEN_ALWAYS if write => libc::O_CREAT,
        vfs::TRUNCATE_EXISTING => libc::O_TRUNC,
        _ => 0,
    };
    // FILE_APPEND_DATA: writes always go to the end, like Win32.
    if access & 0x0000_0004 != 0 {
        open_flags |= libc::O_APPEND;
    }
    // FILE_FLAG_OPEN_REPARSE_POINT: open the link itself, not its target.
    if flags & 0x0020_0000 != 0 {
        open_flags |= libc::O_NOFOLLOW;
    }
    open_flags
}

/// Linux twin of upstream `delete_file_w_vfs` (#2809): a title delete honors
/// the read-only attribute on the visible file. An overlay entry is removed
/// by Rust itself — libc has no readonly gate (unlink needs only directory
/// permission) — so the attribute is checked explicitly first, and with no
/// overlay entry the shared `whiteout_original_file(rel, true)` refuses
/// read-only originals. (Upstream's `delete_overlay_path` MAX_PATH juggling
/// is Windows-native and stays out.) Retiring a source after a VFS move keeps
/// the existing move policy and goes through `Vfs::delete_file` directly, as
/// upstream.
pub fn delete_file(v: &vfs::Vfs, rel: &str) -> Result<(), u32> {
    let Some(rel_clean) = vfs::clean_rel(rel) else {
        return Err(vfs::ERROR_FILE_NOT_FOUND);
    };
    let overlay = v.checked_overlay_path(&rel_clean)?;
    // A dangling symlink entry still takes precedence over the original game file.
    match std::fs::symlink_metadata(&overlay) {
        Ok(meta) if meta.is_dir() => return Err(ERROR_ACCESS_DENIED),
        Ok(meta) => {
            if meta.permissions().readonly() {
                return Err(ERROR_ACCESS_DENIED);
            }
            std::fs::remove_file(&overlay).map_err(|_| ERROR_ACCESS_DENIED)?;
            v.commit_overlay_removal(&rel_clean);
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(ERROR_ACCESS_DENIED),
    }
    v.whiteout_original_file(&rel_clean, true)
}

fn errno_to_win(errno: i32) -> u32 {
    match errno {
        libc::ENOENT => vfs::ERROR_FILE_NOT_FOUND,
        libc::ENOTDIR => ERROR_PATH_NOT_FOUND,
        libc::EEXIST => vfs::ERROR_FILE_EXISTS,
        libc::ENOTEMPTY => vfs::ERROR_DIR_NOT_EMPTY,
        libc::EINVAL => vfs::ERROR_INVALID_PARAMETER,
        _ => vfs::ERROR_ACCESS_DENIED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{
        Vfs, CREATE_ALWAYS, CREATE_NEW, ERROR_ACCESS_DENIED, ERROR_DIR_NOT_EMPTY,
        ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_PATH_NOT_FOUND,
        GENERIC_READ, GENERIC_WRITE, OPEN_ALWAYS, OPEN_EXISTING, TRUNCATE_EXISTING,
    };
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    struct Fixture {
        dir: PathBuf,
        game: PathBuf,
        data: PathBuf,
        vfs: Vfs,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let dir = std::env::temp_dir()
                .join(format!("xcl_vfs_hooks_linux_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let game = dir.join("game");
            let data = dir.join("xcldata");
            std::fs::create_dir_all(game.join("data")).unwrap();
            std::fs::write(game.join("data").join("hello.txt"), b"original").unwrap();
            std::fs::write(game.join("data").join("gone.txt"), b"bye").unwrap();
            let mounts = vec![
                ("G:".to_string(), game.to_string_lossy().to_string()),
                ("GAME:".to_string(), game.to_string_lossy().to_string()),
                (
                    "T:".to_string(),
                    data.join("temp").to_string_lossy().to_string(),
                ),
            ];
            let vfs = Vfs::new(&game, &data, mounts);
            Fixture {
                dir,
                game,
                data,
                vfs,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Recursive content hash of the game folder (paths + bytes), order-independent.
    fn tree_hash(root: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = entry.path();
                let file_type = entry.file_type().unwrap();
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if file_type.is_dir() {
                    stack.push(p);
                } else if file_type.is_symlink() {
                    // Hash the link itself, never a possible outside target.
                    out.insert(
                        rel,
                        std::fs::read_link(&p)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned()
                            .into_bytes(),
                    );
                } else {
                    out.insert(rel, std::fs::read(&p).unwrap());
                }
            }
        }
        out
    }

    #[test]
    fn overlay_write_through_libc_keeps_the_game_folder_identical() {
        let f = Fixture::new("cow");
        let before = tree_hash(&f.game);

        let mut w = open_with(
            &f.vfs,
            "G:\\data\\hello.txt",
            GENERIC_WRITE,
            OPEN_EXISTING,
            0,
        )
        .unwrap();
        assert!(!w.already_existed);
        w.file.write_all(b"modified").unwrap();
        drop(w);

        // The write landed in the overlay and later reads see it.
        let overlay = f.data.join("overlay").join("data").join("hello.txt");
        assert_eq!(std::fs::read(&overlay).unwrap(), b"modified");
        assert!(f.vfs.visible_in_overlay("data\\hello.txt"));
        assert_eq!(
            f.vfs.redirected_writes(),
            vec!["data\\hello.txt".to_string()]
        );
        let mut r = open_with(
            &f.vfs,
            "G:\\data\\hello.txt",
            GENERIC_READ,
            OPEN_EXISTING,
            0,
        )
        .unwrap();
        let mut got = Vec::new();
        r.file.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"modified");

        // The game folder is byte-for-byte unchanged.
        assert_eq!(
            std::fs::read(f.game.join("data").join("hello.txt")).unwrap(),
            b"original"
        );
        assert_eq!(tree_hash(&f.game), before);
    }

    #[test]
    fn host_absolute_title_path_with_separators_opens_at_the_slash_spelling() {
        let f = Fixture::new("savespell");
        std::fs::create_dir_all(f.data.join("saves").join("test-scid")).unwrap();
        // The runtime absolves a folder (`XGameSave*` returns `/.../saves/<scid>`)
        // and the title appends `\file`: the host open must use the slash
        // spelling, and later opens must see the same file.
        let spelled = format!(
            "{}\\slot0.sav",
            f.data.join("saves").join("test-scid").to_string_lossy()
        );
        let mut w = open_with(&f.vfs, &spelled, GENERIC_WRITE, CREATE_ALWAYS, 0).unwrap();
        w.file.write_all(b"progress").unwrap();
        drop(w);
        assert_eq!(
            std::fs::read(f.data.join("saves").join("test-scid").join("slot0.sav")).unwrap(),
            b"progress"
        );
        // The slash spelling is what later opens probe: read-only reopens see
        // the file instead of failing access-denied on the raw spelling.
        let mut r = open_with(&f.vfs, &spelled, GENERIC_READ, OPEN_EXISTING, 0).unwrap();
        let mut got = Vec::new();
        r.file.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"progress");
        open_with(&f.vfs, &spelled, GENERIC_READ, OPEN_ALWAYS, 0).unwrap();
        assert_eq!(
            open_with(&f.vfs, &spelled, GENERIC_READ, CREATE_ALWAYS, 0).err(),
            Some(ERROR_ACCESS_DENIED)
        );
    }

    #[test]
    fn traversal_symlink_and_unmapped_paths_never_open_host_files() {
        let f = Fixture::new("unsafe_paths");
        let outside = f.dir.join("outside.txt");
        std::fs::write(&outside, b"host secret").unwrap();

        // Upstream #1170: an above-mount `..` clamps at the mount root, so
        // these name the (missing) game file `outside.txt` — opens that miss
        // fail `FILE_NOT_FOUND`, and creates land in the overlay. The host
        // sibling is never touched either way.
        for path in [
            "G:\\..\\outside.txt".to_string(),
            "G:/../outside.txt".to_string(),
            format!("{}\\GAME:\\..\\outside.txt", f.game.display()),
        ] {
            for (access, disposition) in [
                (GENERIC_READ, OPEN_EXISTING),
                (GENERIC_WRITE, OPEN_EXISTING),
            ] {
                assert_eq!(
                    open_with(&f.vfs, &path, access, disposition, 0).err(),
                    Some(ERROR_FILE_NOT_FOUND),
                    "{path}"
                );
            }
        }
        for path in [
            "G:\\..\\outside.txt".to_string(),
            "G:/../outside.txt".to_string(),
            format!("{}\\GAME:\\..\\outside.txt", f.game.display()),
        ] {
            open_with(&f.vfs, &path, GENERIC_WRITE, CREATE_ALWAYS, 0).unwrap();
        }
        assert_eq!(
            std::fs::read(f.data.join("overlay").join("outside.txt")).unwrap(),
            b""
        );

        #[cfg(unix)]
        {
            symlink(&outside, f.game.join("link.txt")).unwrap();
            assert_eq!(
                open_with(&f.vfs, "G:\\link.txt", GENERIC_READ, OPEN_EXISTING, 0).err(),
                Some(ERROR_ACCESS_DENIED)
            );
            assert_eq!(
                open_with(&f.vfs, "G:\\link.txt", GENERIC_WRITE, CREATE_ALWAYS, 0).err(),
                Some(ERROR_ACCESS_DENIED)
            );

            // O_CREAT follows a dangling final symlink and would create the outside target.
            let missing = f.dir.join("must-not-create.txt");
            symlink(&missing, f.game.join("dangling.txt")).unwrap();
            assert_eq!(
                open_with(&f.vfs, "G:\\dangling.txt", GENERIC_WRITE, CREATE_ALWAYS, 0).err(),
                Some(ERROR_ACCESS_DENIED)
            );
            assert!(!missing.exists());
        }

        // The writable layer needs the same containment: a pre-existing overlay symlink
        // must never redirect a G: read or write to a host file.
        #[cfg(unix)]
        {
            let overlay_data = f.data.join("overlay").join("data");
            std::fs::create_dir_all(&overlay_data).unwrap();
            symlink(&outside, overlay_data.join("hello.txt")).unwrap();
            let mut r = open_with(
                &f.vfs,
                "G:\\data\\hello.txt",
                GENERIC_READ,
                OPEN_EXISTING,
                0,
            )
            .unwrap();
            let mut got = Vec::new();
            r.file.read_to_end(&mut got).unwrap();
            assert_eq!(got, b"original");
            assert_eq!(
                open_with(
                    &f.vfs,
                    "G:\\data\\hello.txt",
                    GENERIC_WRITE,
                    OPEN_EXISTING,
                    0
                )
                .err(),
                Some(ERROR_ACCESS_DENIED)
            );

            let overlay_missing = f.dir.join("overlay-must-not-create.txt");
            symlink(&overlay_missing, overlay_data.join("new.txt")).unwrap();
            assert_eq!(
                open_with(&f.vfs, "G:\\data\\new.txt", GENERIC_WRITE, CREATE_ALWAYS, 0).err(),
                Some(ERROR_ACCESS_DENIED)
            );
            assert!(!overlay_missing.exists());
        }

        // A forward-slash console path must not become a relative host path such as
        // "./Z:/Windows/notepad.exe" when its drive has no mount.
        for path in ["Z:\\Windows\\notepad.exe", "Z:/Windows/notepad.exe"] {
            assert_eq!(
                open_with(&f.vfs, path, GENERIC_WRITE, CREATE_ALWAYS, 0).err(),
                Some(ERROR_PATH_NOT_FOUND),
                "{path}"
            );
        }
        assert_eq!(std::fs::read(&outside).unwrap(), b"host secret");
        assert_eq!(tree_hash(&f.game)["data/hello.txt"], b"original");
    }

    #[cfg(unix)]
    #[test]
    fn failed_host_open_restores_a_removed_whiteout() {
        use std::os::unix::fs::PermissionsExt;

        let f = Fixture::new("failed_open");
        let v = &f.vfs;
        v.delete_file("data\\gone.txt").unwrap();
        let overlay_data = f.data.join("overlay").join("data");
        std::fs::create_dir_all(&overlay_data).unwrap();
        std::fs::set_permissions(&overlay_data, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = open_with(v, "G:\\data\\gone.txt", GENERIC_WRITE, CREATE_ALWAYS, 0);
        // Restore write permission before assertions so fixture cleanup always works.
        std::fs::set_permissions(&overlay_data, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(result.err(), Some(ERROR_ACCESS_DENIED));
        assert!(v.is_whiteout("data\\gone.txt"));
        assert_eq!(
            open_with(v, "G:\\data\\gone.txt", GENERIC_READ, OPEN_EXISTING, 0).err(),
            Some(ERROR_FILE_NOT_FOUND)
        );
        assert!(!overlay_data.join("gone.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_host_open_restores_an_ancestor_whiteout() {
        use std::os::unix::fs::PermissionsExt;

        let f = Fixture::new("failed_open_parent");
        let v = &f.vfs;
        // Non-empty directories cannot be removed (upstream #997): white out the
        // children first so `remove_dir("data")` publishes the ancestor whiteout.
        assert_eq!(v.remove_dir("data"), Err(ERROR_DIR_NOT_EMPTY));
        v.delete_file("data\\hello.txt").unwrap();
        v.delete_file("data\\gone.txt").unwrap();
        v.remove_dir("data").unwrap();
        let overlay_data = f.data.join("overlay").join("data");
        std::fs::create_dir_all(&overlay_data).unwrap();
        // put there behind the VFS's back: in use, as for a run that finds it at start
        // (upstream #1106 models this with `mark_overlay_used` in its own fixture).
        v.mark_overlay_used();
        std::fs::set_permissions(&overlay_data, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = open_with(v, "G:\\data\\gone.txt", GENERIC_WRITE, CREATE_ALWAYS, 0);
        std::fs::set_permissions(&overlay_data, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(result.err(), Some(ERROR_ACCESS_DENIED));
        assert!(v.is_whiteout("data"));
        assert!(v.is_whiteout("data\\gone.txt"));
        assert_eq!(
            v.redirected_writes(),
            vec![
                "data".to_string(),
                "data\\gone.txt".to_string(),
                "data\\hello.txt".to_string()
            ]
        );
        assert!(!overlay_data.join("gone.txt").exists());
    }

    /// Linux twin of upstream `failed_create_file_preserves_whiteout_and_last_error`
    /// (#1362): staging a CreateFile* decision must not clear the whiteout or record a
    /// write, a failed host open must leave both untouched, and only a successful open
    /// commits.
    #[cfg(unix)]
    #[test]
    fn failed_create_file_preserves_whiteout_and_writes() {
        use std::os::unix::fs::PermissionsExt;

        let f = Fixture::new("failed_create_whiteout");
        let v = &f.vfs;
        v.delete_file("data\\gone.txt").unwrap();
        assert!(v.is_whiteout("data\\gone.txt"));
        let writes_before = v.redirected_writes();

        // Staging alone changes neither the whiteout nor the write ledger.
        for disp in [CREATE_NEW, CREATE_ALWAYS, vfs::OPEN_ALWAYS] {
            let (decision, commit) =
                v.open_decision_staged("G:\\data\\gone.txt", GENERIC_WRITE, disp, 0);
            assert!(matches!(decision, Decision::Use(_)), "disp={disp}");
            assert_eq!(
                commit,
                Some(vfs::OpenCommit { rel: "data\\gone.txt".to_string(), is_new: true }),
                "disp={disp}"
            );
            assert!(v.is_whiteout("data\\gone.txt"), "staging must not clear the whiteout (disp={disp})");
            assert_eq!(v.redirected_writes(), writes_before, "staging must not record a write (disp={disp})");
            assert!(
                v.real_path_for_read("G:\\data\\gone.txt").is_none(),
                "deleted original must stay hidden while staged (disp={disp})"
            );
        }

        // A failed host open preserves the whiteout and records nothing further.
        let overlay_data = f.data.join("overlay").join("data");
        std::fs::create_dir_all(&overlay_data).unwrap();
        std::fs::set_permissions(&overlay_data, std::fs::Permissions::from_mode(0o500)).unwrap();
        // A failed CREATE_ALWAYS over a visible game file must not record a write either.
        let visible_before = v.redirected_writes();
        assert_eq!(
            open_with(v, "G:\\data\\hello.txt", GENERIC_WRITE, CREATE_ALWAYS, 0).err(),
            Some(ERROR_ACCESS_DENIED)
        );
        assert_eq!(v.redirected_writes(), visible_before);
        for disp in [CREATE_NEW, CREATE_ALWAYS, vfs::OPEN_ALWAYS] {
            assert_eq!(
                open_with(v, "G:\\data\\gone.txt", GENERIC_WRITE, disp, 0).err(),
                Some(ERROR_ACCESS_DENIED),
                "disp={disp}"
            );
            assert!(v.is_whiteout("data\\gone.txt"), "whiteout must remain after failed open (disp={disp})");
            assert!(
                v.real_path_for_read("G:\\data\\gone.txt").is_none(),
                "deleted original must not resurrect (disp={disp})"
            );
        }
        assert_eq!(v.redirected_writes(), writes_before);
        assert!(!overlay_data.join("gone.txt").exists());
        // Restore write permission before assertions so fixture cleanup always works.
        std::fs::set_permissions(&overlay_data, std::fs::Permissions::from_mode(0o700)).unwrap();

        // Once the host open actually succeeds, the whiteout clears.
        let mut w = open_with(v, "G:\\data\\gone.txt", GENERIC_WRITE, CREATE_ALWAYS, 0).unwrap();
        w.file.write_all(b"").unwrap();
        drop(w);
        assert!(!v.is_whiteout("data\\gone.txt"));
        assert_eq!(
            std::fs::read(overlay_data.join("gone.txt")).unwrap(),
            b""
        );

        // Re-opening the overlay file with OPEN_ALWAYS still reports that it existed.
        let again = open_with(v, "G:\\data\\gone.txt", GENERIC_WRITE, vfs::OPEN_ALWAYS, 0).unwrap();
        assert!(again.already_existed);
    }

    #[test]
    fn dispositions_and_translated_drives() {
        let f = Fixture::new("disp");
        let before = tree_hash(&f.game);

        // A host path is a host path: the original file is read directly.
        let mut r = open_with(
            &f.vfs,
            &f.game.join("data").join("hello.txt").to_string_lossy(),
            GENERIC_READ,
            OPEN_EXISTING,
            0,
        )
        .unwrap();
        let mut got = Vec::new();
        r.file.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"original");

        // CREATE_NEW over a visible game file fails without touching either layer.
        assert_eq!(
            open_with(&f.vfs, "G:\\data\\hello.txt", GENERIC_WRITE, CREATE_NEW, 0).err(),
            Some(ERROR_FILE_EXISTS)
        );
        // CREATE_ALWAYS over the original opens the overlay copy and reports that it existed.
        let o = open_with(
            &f.vfs,
            "G:\\data\\hello.txt",
            GENERIC_WRITE,
            CREATE_ALWAYS,
            0,
        )
        .unwrap();
        assert!(o.already_existed);
        drop(o);

        // CREATE_ALWAYS over an existing overlay file also reports that it existed.
        let again = open_with(
            &f.vfs,
            "G:\\data\\hello.txt",
            GENERIC_WRITE,
            CREATE_ALWAYS,
            0,
        )
        .unwrap();
        assert!(again.already_existed);
        drop(again);

        // T: lands under the title data dir, not the game folder.
        let mut w = open_with(&f.vfs, "T:\\scratch.bin", GENERIC_WRITE, CREATE_ALWAYS, 0).unwrap();
        w.file.write_all(b"temp").unwrap();
        drop(w);
        assert_eq!(
            std::fs::read(f.data.join("temp").join("scratch.bin")).unwrap(),
            b"temp"
        );
        assert_eq!(tree_hash(&f.game), before);

        // Creation dispositions never grant write access implicitly. A read-only caller
        // cannot truncate an existing translated file or create a missing one.
        assert_eq!(
            open_with(
                &f.vfs,
                "T:\\scratch.bin",
                GENERIC_READ,
                TRUNCATE_EXISTING,
                0
            )
            .err(),
            Some(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(
            std::fs::read(f.data.join("temp").join("scratch.bin")).unwrap(),
            b"temp"
        );
        assert_eq!(
            open_with(
                &f.vfs,
                "T:\\read-only-create.bin",
                GENERIC_READ,
                CREATE_ALWAYS,
                0
            )
            .err(),
            Some(ERROR_ACCESS_DENIED)
        );
        assert!(!f.data.join("temp").join("read-only-create.bin").exists());

        // Control/metadata rights do not grant content-write access.
        assert_eq!(
            open_with(
                &f.vfs,
                "T:\\scratch.bin",
                0x0001_0000, // DELETE
                CREATE_ALWAYS,
                0
            )
            .err(),
            Some(ERROR_ACCESS_DENIED)
        );
        assert_eq!(
            std::fs::read(f.data.join("temp").join("scratch.bin")).unwrap(),
            b"temp"
        );

        // Append rights can create/append, but cannot authorize CREATE_ALWAYS truncation.
        assert_eq!(
            open_with(
                &f.vfs,
                "T:\\scratch.bin",
                0x0000_0004, // FILE_APPEND_DATA
                CREATE_ALWAYS,
                0
            )
            .err(),
            Some(ERROR_ACCESS_DENIED)
        );
        assert_eq!(
            std::fs::read(f.data.join("temp").join("scratch.bin")).unwrap(),
            b"temp"
        );
        let mut append =
            open_with(&f.vfs, "T:\\append-only.bin", 0x0000_0004, CREATE_NEW, 0).unwrap();
        append.file.write_all(b"appended").unwrap();
        drop(append);
        assert_eq!(
            std::fs::read(f.data.join("temp").join("append-only.bin")).unwrap(),
            b"appended"
        );
        let mut append_create = open_with(
            &f.vfs,
            "T:\\append-create-always.bin",
            0x0000_0004,
            CREATE_ALWAYS,
            0,
        )
        .unwrap();
        append_create.file.write_all(b"new").unwrap();
        drop(append_create);
        assert_eq!(
            std::fs::read(f.data.join("temp").join("append-create-always.bin")).unwrap(),
            b"new"
        );

        // Zero access yields a metadata-only O_PATH descriptor, not a readable fd.
        let mut metadata_only = open_with(&f.vfs, "T:\\scratch.bin", 0, OPEN_EXISTING, 0).unwrap();
        let mut got = Vec::new();
        assert!(metadata_only.file.read_to_end(&mut got).is_err());

        // OPEN_ALWAYS may read an existing file without write access, but may not create
        // a missing one or mutate overlay bookkeeping while failing.
        let mut existing =
            open_with(&f.vfs, "T:\\scratch.bin", GENERIC_READ, vfs::OPEN_ALWAYS, 0).unwrap();
        existing.file.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"temp");
        assert_eq!(
            open_with(
                &f.vfs,
                "T:\\read-only-open.bin",
                GENERIC_READ,
                vfs::OPEN_ALWAYS,
                0
            )
            .err(),
            Some(ERROR_ACCESS_DENIED)
        );
        assert_eq!(
            open_with(
                &f.vfs,
                "G:\\data\\read-only-open.bin",
                GENERIC_READ,
                vfs::OPEN_ALWAYS,
                0
            )
            .err(),
            Some(ERROR_ACCESS_DENIED)
        );
        assert!(!f.data.join("temp").join("read-only-open.bin").exists());
        assert!(!f.vfs.visible_in_overlay("data\\read-only-open.bin"));
    }

    #[test]
    fn delete_on_close_open_records_a_whiteout_for_the_original() {
        // Upstream #1038 (Linux twin): opening a game-original file with
        // FILE_FLAG_DELETE_ON_CLOSE stages the overlay copy, and once the host
        // open succeeds the original is whiteouted so it stays hidden after the
        // overlay copy is deleted on close. A second handle opening the existing
        // overlay entry must keep that whiteout.
        const DELETE: u32 = 0x0001_0000;
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
        let f = Fixture::new("doc");
        std::fs::write(f.game.join("data").join("delete_on_close.txt"), b"original").unwrap();
        let v = &f.vfs;
        let path = "G:\\data\\delete_on_close.txt";
        let mut first = open_with(v, path, GENERIC_READ | DELETE, OPEN_EXISTING, FILE_FLAG_DELETE_ON_CLOSE)
            .expect("delete-on-close open must succeed");
        let mut got = Vec::new();
        first.file.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"original", "open handle must read the original bytes");
        assert!(
            v.is_whiteout("data\\delete_on_close.txt"),
            "successful delete-on-close open must hide the original"
        );
        let second = open_with(v, path, GENERIC_WRITE, OPEN_EXISTING, 0)
            .expect("second handle must open the existing overlay entry");
        drop(first);
        drop(second);
        assert!(
            v.is_whiteout("data\\delete_on_close.txt"),
            "reopening an existing overlay entry must keep the delete-on-close whiteout"
        );
    }

    /// Linux twin of upstream `vfs_readonly_delete` (#2809, `#![cfg(windows)]`
    /// as filed): a title delete honors the read-only attribute on the visible
    /// file, while retiring a source after a VFS move keeps the existing move
    /// policy (no readonly check).
    #[cfg(unix)]
    #[test]
    fn delete_honors_readonly_attributes() {
        fn set_readonly(path: &Path, readonly: bool) {
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_readonly(readonly);
            std::fs::set_permissions(path, perms).unwrap();
        }

        let f = Fixture::new("readonly_delete");
        let v = &f.vfs;
        // A read-only game-original file.
        std::fs::write(
            f.game.join("data").join("readonly-original.txt"),
            b"original",
        )
        .unwrap();
        set_readonly(&f.game.join("data").join("readonly-original.txt"), true);
        // A read-only overlay file shadowing a writable original.
        std::fs::write(
            f.game.join("data").join("shadowed.txt"),
            b"shadowed original",
        )
        .unwrap();
        let overlay_shadowed = f.data.join("overlay").join("data").join("shadowed.txt");
        std::fs::create_dir_all(overlay_shadowed.parent().unwrap()).unwrap();
        std::fs::write(&overlay_shadowed, b"overlay").unwrap();
        set_readonly(&overlay_shadowed, true);
        // A read-only original whose retirement must still succeed.
        std::fs::write(
            f.game.join("data").join("readonly-move.txt"),
            b"readonly move source",
        )
        .unwrap();
        set_readonly(&f.game.join("data").join("readonly-move.txt"), true);
        // Plain writable files.
        std::fs::write(
            f.game.join("data").join("writable.txt"),
            b"writable original",
        )
        .unwrap();
        let overlay_only = f
            .data
            .join("overlay")
            .join("data")
            .join("writable-overlay.txt");
        std::fs::write(&overlay_only, b"writable overlay").unwrap();

        // Deleting a read-only original fails and leaves it visible.
        assert_eq!(
            delete_file(v, "data\\readonly-original.txt"),
            Err(ERROR_ACCESS_DENIED)
        );
        assert_eq!(
            std::fs::read(
                v.real_path_for_read("G:\\data\\readonly-original.txt")
                    .unwrap()
            )
            .unwrap(),
            b"original"
        );
        assert!(!v.is_whiteout("data\\readonly-original.txt"));

        // Deleting a read-only overlay file fails and keeps both copies.
        assert_eq!(
            delete_file(v, "data\\shadowed.txt"),
            Err(ERROR_ACCESS_DENIED)
        );
        assert_eq!(std::fs::read(&overlay_shadowed).unwrap(), b"overlay");
        assert_eq!(
            std::fs::read(f.game.join("data").join("shadowed.txt")).unwrap(),
            b"shadowed original"
        );

        // A writable overlay-only file deletes with no whiteout ...
        assert_eq!(delete_file(v, "data\\writable-overlay.txt"), Ok(()));
        assert!(!v.exists("data\\writable-overlay.txt"));
        assert!(!v.is_whiteout("data\\writable-overlay.txt"));
        assert_eq!(
            delete_file(v, "data\\writable-overlay.txt"),
            Err(ERROR_FILE_NOT_FOUND)
        );

        // ... and a writable original whiteouts, staying hidden on a fresh Vfs.
        assert_eq!(delete_file(v, "data\\writable.txt"), Ok(()));
        assert!(v.real_path_for_read("G:\\data\\writable.txt").is_none());
        assert!(v.is_whiteout("data\\writable.txt"));
        assert_eq!(
            delete_file(v, "data\\writable.txt"),
            Err(ERROR_FILE_NOT_FOUND)
        );

        // Retiring a source after a VFS move keeps the existing move policy:
        // a read-only original still retires through `Vfs::remove_moved_file`.
        assert_eq!(v.remove_moved_file("data\\readonly-move.txt"), Ok(()));
        assert!(v.is_whiteout("data\\readonly-move.txt"));

        // Restore attributes so fixture cleanup never trips on readonly files.
        set_readonly(&f.game.join("data").join("readonly-original.txt"), false);
        set_readonly(&overlay_shadowed, false);
        set_readonly(&f.game.join("data").join("readonly-move.txt"), false);
    }
}
