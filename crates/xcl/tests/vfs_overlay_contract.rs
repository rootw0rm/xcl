//! Integration tests pinning the VFS overlay / whiteout / open-decision contract.
//!
//! `crates/xcl/src/vfs.rs` is a public module exercised only through its inline tests;
//! this file pins the same public contract through an outside crate's view of the API
//! (`xcl::vfs::Vfs`), covering the overlay lifecycle, whiteout persistence across
//! `Vfs::new`, create_dir/remove_dir error cases, needs_merge, and open_decision.

use std::path::{Path, PathBuf};

use xcl::vfs::{Decision, Target, Vfs, ERROR_ALREADY_EXISTS, ERROR_DIR_NOT_EMPTY, ERROR_FILE_NOT_FOUND};

const GENERIC_WRITE: u32 = 0x4000_0000;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const TRUNCATE_EXISTING: u32 = 5;

struct Fixture {
    dir: PathBuf,
    game: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("xcl_vfs_overlay_{tag}_{}_{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let game = dir.join("game");
        let data = dir.join("xcldata");
        std::fs::create_dir_all(game.join("data")).unwrap();
        std::fs::create_dir_all(game.join("saves")).unwrap();
        std::fs::write(game.join("data").join("hello.txt"), b"original").unwrap();
        std::fs::write(game.join("data").join("gone.txt"), b"bye").unwrap();
        std::fs::write(game.join("saves").join("slot1.sav"), b"save").unwrap();
        Fixture { dir, game, data }
    }

    fn vfs(&self) -> Vfs {
        Vfs::new(
            &self.game,
            &self.data,
            vec![
                ("G:".to_string(), self.game.to_string_lossy().to_string()),
                ("GAME:".to_string(), self.game.to_string_lossy().to_string()),
                ("T:".to_string(), self.data.join("temp").to_string_lossy().to_string()),
            ],
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn open_decision_never_writes_the_original_folder() {
    let f = Fixture::new("open_write");
    let v = f.vfs();

    // CREATE_ALWAYS on an existing game file: overlay path, original untouched.
    // (The file is replaced in the overlay, so no content copy is made.)
    let orig = f.game.join("data").join("hello.txt");
    let ov = match v.open_decision("G:\\data\\hello.txt", GENERIC_WRITE, CREATE_ALWAYS, 0) {
        Decision::Use(p) | Decision::CreateOverOriginal(p) => p,
        other => panic!("expected overlay path, got {other:?}"),
    };
    assert!(ov.contains("overlay"), "expected overlay path: {ov}");
    assert_eq!(std::fs::read(&orig).unwrap(), b"original");

    // CREATE_ALWAYS on a file that does not exist: overlay only, original folder untouched.
    let newp = match v.open_decision("G:\\data\\new.bin", GENERIC_WRITE, CREATE_ALWAYS, 0) {
        Decision::Use(p) => p,
        other => panic!("expected overlay path, got {other:?}"),
    };
    assert!(newp.contains("overlay"));
    assert!(!f.game.join("data").join("new.bin").exists());

    // A copy-on-write open (OPEN_EXISTING + write access) carries the original content.
    let f = Fixture::new("cow_open");
    let v = f.vfs();
    let orig = f.game.join("data").join("hello.txt");
    let cow = match v.open_decision("G:\\data\\hello.txt", GENERIC_WRITE, OPEN_EXISTING, 0) {
        Decision::Use(p) => p,
        other => panic!("expected Use, got {other:?}"),
    };
    assert!(cow.contains("overlay"));
    assert_eq!(std::fs::read(&cow).unwrap(), b"original");
    assert_eq!(std::fs::read(&orig).unwrap(), b"original");

    // TRUNCATE_EXISTING on an un-overlaid game file: rewritten to CREATE_ALWAYS in the overlay.
    let v2 = f.vfs();
    let (tr_ov, disp) = match v2.open_decision("G:\\data\\gone.txt", GENERIC_WRITE, TRUNCATE_EXISTING, 0) {
        Decision::UseWithDisposition(p, d) => (p, d),
        other => panic!("expected UseWithDisposition, got {other:?}"),
    };
    assert!(tr_ov.contains("overlay"));
    assert_eq!(disp, CREATE_ALWAYS);
    assert!(!Path::new(&tr_ov).exists(), "no placeholder file before native open");
    assert_eq!(std::fs::read(f.game.join("data").join("gone.txt")).unwrap(), b"bye");
}

#[test]
fn delete_file_records_persistent_whiteout() {
    let f = Fixture::new("whiteout");
    let v = f.vfs();
    let orig = f.game.join("data").join("gone.txt");

    assert_eq!(v.delete_file("data\\gone.txt"), Ok(()));
    assert!(orig.exists(), "original must stay on disk");
    assert!(v.is_whiteout("data\\gone.txt"));
    assert_eq!(v.read_path_rel("data\\gone.txt"), None);
    assert_eq!(
        v.open_decision("G:\\data\\gone.txt", 0x8000_0000, OPEN_EXISTING, 0),
        Decision::Fail(ERROR_FILE_NOT_FOUND)
    );

    // Persistence: a fresh Vfs on the same data folder still sees the whiteout.
    let v2 = f.vfs();
    assert!(v2.is_whiteout("data\\gone.txt"));
    assert_eq!(v2.read_path_rel("data\\gone.txt"), None);

    // Deleting a file that does not exist fails ERROR_FILE_NOT_FOUND.
    assert_eq!(v2.delete_file("data\\missing.txt"), Err(ERROR_FILE_NOT_FOUND));
}

#[test]
fn create_dir_on_existing_original_fails_already_exists() {
    let f = Fixture::new("create_dir");
    let v = f.vfs();

    // Existing original directories fail with ERROR_ALREADY_EXISTS.
    assert_eq!(v.create_dir("data"), Err(ERROR_ALREADY_EXISTS));
    assert_eq!(v.create_dir("saves"), Err(ERROR_ALREADY_EXISTS));

    // A fresh directory succeeds in the overlay, not the game folder.
    assert_eq!(v.create_dir("saves\\slot2"), Ok(()));
    assert!(!f.game.join("saves").join("slot2").exists());
    assert!(v.visible_in_overlay("saves\\slot2"));

    // needs_merge / has_whiteouts_under: nothing under "data" yet, "saves" has an overlay child.
    assert!(!v.needs_merge("data"));
    assert!(v.needs_merge("saves"));
    assert!(!v.has_whiteouts_under("data"));
}

#[test]
fn remove_dir_with_unwhited_child_errors_dir_not_empty() {
    let f = Fixture::new("remove_dir");
    let v = f.vfs();

    // Un-whited-out child: ERROR_DIR_NOT_EMPTY, nothing whited out.
    assert_eq!(v.remove_dir("data"), Err(ERROR_DIR_NOT_EMPTY));
    assert!(!v.is_whiteout("data"));
    assert!(v.visible_in_original("data\\hello.txt"));

    // Whited-out the child: remove_dir succeeds and records a whiteout on the directory.
    assert_eq!(v.delete_file("data\\hello.txt"), Ok(()));
    assert_eq!(v.delete_file("data\\gone.txt"), Ok(()));
    assert_eq!(v.remove_dir("data"), Ok(()));
    assert!(v.is_whiteout("data"));
    assert!(!v.exists("data"));
    assert!(f.game.join("data").join("hello.txt").exists(), "original stays");
}

#[test]
fn mangled_absolute_path_with_game_prefix_maps_to_game_folder() {
    let f = Fixture::new("mangled");
    let v = f.vfs();

    // A console mount embedded in an absolute path resolves to the game folder.
    let mangled = format!("\\\\?\\{}\\GAME:\\data\\hello.txt", f.game.display());
    assert_eq!(v.classify(&mangled), Target::Game { rel: "data\\hello.txt".into() });

    // An own_folder (XCL's own shader cache) is never translated.
    let cache = f.data.join("shader cache");
    let own = Vfs::new(&f.game, &f.data, vec![]).own_folder(&cache);
    let cache_file = format!("{}\\0123456789abcdef.dxil", cache.display());
    assert_eq!(own.classify(&cache_file), Target::Pass);
    assert_eq!(own.classify(&cache.display().to_string()), Target::Pass);
}

#[test]
fn needs_merge_and_whiteouts_under_are_consistent() {
    let f = Fixture::new("merge");
    let v = f.vfs();

    assert!(!v.needs_merge("data"));
    assert!(!v.has_whiteouts_under("data"));

    // A whiteout anywhere is picked up by `has_whiteouts_under("")`.
    assert_eq!(v.delete_file("data\\gone.txt"), Ok(()));
    assert!(v.needs_merge("data"));
    assert!(v.has_whiteouts_under("data"));
    assert!(v.has_whiteouts_under(""));

    // `has_whiteouts_under` must not claim directories that have nothing under them.
    assert!(!v.has_whiteouts_under("saves"));
}

#[test]
fn read_path_rel_handles_whiteout_and_overlay_precedence() {
    let f = Fixture::new("read_path");
    let v = f.vfs();

    // Original read when nothing overlaid.
    let orig = v.read_path_rel("data\\hello.txt").unwrap();
    assert!(orig.ends_with("hello.txt"));

    // Copy-on-write: a write lands in the overlay and later reads prefer it.
    let ov = match v.open_decision("G:\\data\\hello.txt", GENERIC_WRITE, OPEN_EXISTING, 0) {
        Decision::Use(p) => p,
        other => panic!("expected Use, got {other:?}"),
    };
    std::fs::write(&ov, b"modified").unwrap();
    assert_eq!(v.read_path_rel("data\\hello.txt").unwrap(), ov);

    // A whiteout hides the original even though the file exists.
    assert_eq!(v.delete_file("data\\gone.txt"), Ok(()));
    assert_eq!(v.read_path_rel("data\\gone.txt"), None);

    // Unmapped console paths pass through unchanged on Windows; Linux has no host
    // drives, so an untranslated console drive is rejected (no host open).
    #[cfg(windows)]
    assert_eq!(v.real_path_for_read("Z:\\nowhere.bin"), Some("Z:\\nowhere.bin".to_string()));
    #[cfg(not(windows))]
    assert_eq!(v.real_path_for_read("Z:\\nowhere.bin"), None);
}

#[test]
fn stage_and_commit_write_keep_overlay_content() {
    let f = Fixture::new("stage");
    let v = f.vfs();

    // stage_write gives a path whose parents exist, without removing whiteouts yet.
    let staged = v.stage_write("data\\staged.bin").unwrap();
    assert!(staged.contains("overlay"));
    assert!(!Path::new(&staged).exists());

    std::fs::write(&staged, b"hi").unwrap();
    v.commit_write("data\\staged.bin");
    assert!(v.redirected_writes().contains(&"data\\staged.bin".to_string()));

    // prepare_write with keep_content copies an original into the overlay.
    let kept = v.prepare_write("data\\hello.txt", true).unwrap();
    assert_eq!(std::fs::read(&kept).unwrap(), b"original");
}
