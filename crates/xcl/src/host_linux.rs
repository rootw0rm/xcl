//! Linux host (Phase C step 10e): in-process title execution for `xcl run`.
//!
//! Sets up logging, boot state, the VFS, crash reporting, HLE + resolver state,
//! then maps the title with `loader::load_main` and enters `loader::run_main`.
//! No Windows DLL is loaded; unresolved imports fail the load cleanly because
//! `load_main` rejects them before any image code runs.

use std::path::Path;

use crate::config::Boot;

/// Run a prepared boot record in this process. Returns the title's exit code,
/// or `0x7C1_xxxx` on host-side failures (mirroring the Windows host codes).
pub fn run_boot(boot: &Boot) -> u32 {
    crate::log::init(
        Some(&boot.logs_dir().join("xcl.log")),
        crate::log::Level::parse(&boot.config.log_level),
        boot.config.log_to_stderr,
    );
    crate::xlog_info!(
        "XCL {} – host for '{}'",
        env!("CARGO_PKG_VERSION"),
        boot.title
    );
    crate::xlog_info!(
        "game: {} (read-only folder: {})",
        boot.exe_path.display(),
        boot.game_root.display()
    );
    crate::xlog_info!("XCL data: {}", boot.data_dir.display());

    crate::state::set_boot(boot.clone());
    crate::stubs::init_db(&boot.config.stub_returns);
    crate::hle::init(&boot.config);

    let mounts: Vec<(String, String)> = boot
        .config
        .mounts
        .iter()
        .map(|(k, v)| (k.clone(), boot.expand(v)))
        .collect();
    crate::vfs::init(crate::vfs::Vfs::new(
        &boot.game_root,
        &boot.data_dir,
        mounts,
    ));
    let _ = std::fs::create_dir_all(boot.data_dir.join("temp"));

    // Linux has no KERNELBASE hooks to install and no RtlPcToFileHeader hook;
    // the crash handler is the sigaction watchdog.
    crate::eh::install_crash_handler();
    // Port of upstream #194: the memory line whenever commit grows by a GiB (30 s cadence).
    crate::memstat::start();

    // `XCL_THREAD_DUMP_AT=s1,s2…`: where every thread is at those seconds (a stalled title)
    if let Ok(v) = std::env::var("XCL_THREAD_DUMP_AT") {
        let at: Vec<f64> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        let _ = std::thread::Builder::new().name("xcl-thread-dump".into()).spawn(move || {
            let t0 = std::time::Instant::now();
            for s in at {
                let left = s - t0.elapsed().as_secs_f64();
                if left > 0.0 {
                    std::thread::sleep(std::time::Duration::from_secs_f64(left));
                }
                crate::eh::dump_threads();
            }
        });
    }

    let mut dirs = vec![boot.game_root.clone()];
    if let Some(p) = boot.exe_path.parent() {
        if p != boot.game_root {
            dirs.insert(0, p.to_path_buf());
        }
    }
    crate::resolver::set_search_dirs(dirs);

    let main = match crate::loader::load_main(&boot.exe_path) {
        Ok(m) => m,
        Err(e) => {
            crate::xlog_error!("loading the game failed: {e}");
            crate::report::write_final(Some(format!("loader: {e}")));
            crate::log::flush();
            return 0x7C1_0002;
        }
    };
    crate::report::write_final(None);
    let code = crate::loader::run_main(&main);
    crate::xlog_info!("game entry point returned code {code}");
    crate::report::write_final(None);
    crate::log::flush();
    code
}

/// `xcl host` entry used when the launcher hands a boot record file.
/// Kept parallel to the Windows `host_main(boot_path)` for CLI symmetry.
pub fn host_main(boot_path: &Path) -> i32 {
    let boot: Boot = match std::fs::read_to_string(boot_path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(b) => b,
        Err(e) => {
            eprintln!("xcl host: cannot read {}: {e}", boot_path.display());
            return 0x7C1_0001 as i32;
        }
    };
    run_boot(&boot) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_exe_returning(code: u8, image_base: u64) -> Vec<u8> {
        // Minimal PE32+ EXE: one .text section, entry `mov eax, imm; ret`, no imports.
        let entry: Vec<u8> = vec![0xB8, code, 0, 0, 0, 0xC3];
        let mut f = vec![0u8; 0x400];
        f[0..2].copy_from_slice(b"MZ");
        f[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        f[0x40..0x44].copy_from_slice(b"PE\0\0");
        f[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes());
        f[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        f[0x54..0x56].copy_from_slice(&0xF0u16.to_le_bytes());
        f[0x56..0x58].copy_from_slice(&0x0022u16.to_le_bytes());
        let o = 0x58usize;
        f[o..o + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        f[o + 0x10..o + 0x14].copy_from_slice(&0x1000u32.to_le_bytes());
        f[o + 0x14..o + 0x18].copy_from_slice(&0x1000u32.to_le_bytes());
        f[o + 0x18..o + 0x20].copy_from_slice(&image_base.to_le_bytes());
        f[o + 0x20..o + 0x24].copy_from_slice(&0x1000u32.to_le_bytes());
        f[o + 0x24..o + 0x28].copy_from_slice(&0x200u32.to_le_bytes());
        f[o + 0x38..o + 0x3C].copy_from_slice(&0x3000u32.to_le_bytes());
        f[o + 0x3C..o + 0x40].copy_from_slice(&0x200u32.to_le_bytes());
        f[o + 0x44..o + 0x46].copy_from_slice(&3u16.to_le_bytes());
        f[o + 0x48..o + 0x50].copy_from_slice(&0x10_0000u64.to_le_bytes());
        f[o + 0x50..o + 0x58].copy_from_slice(&0x1000u64.to_le_bytes());
        f[o + 0x58..o + 0x60].copy_from_slice(&0x10_0000u64.to_le_bytes());
        f[o + 0x60..o + 0x68].copy_from_slice(&0x1000u64.to_le_bytes());
        f[o + 0x6C..o + 0x70].copy_from_slice(&16u32.to_le_bytes());
        let s = o + 0xF0;
        f[s..s + 5].copy_from_slice(b".text");
        f[s + 8..s + 12].copy_from_slice(&0x200u32.to_le_bytes());
        f[s + 12..s + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        f[s + 16..s + 20].copy_from_slice(&0x200u32.to_le_bytes());
        f[s + 20..s + 24].copy_from_slice(&0x200u32.to_le_bytes());
        f[s + 36..s + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes());
        f[0x200..0x200 + entry.len()].copy_from_slice(&entry);
        f
    }

    struct TempGame {
        dir: std::path::PathBuf,
    }
    impl TempGame {
        fn new(tag: &str, exe_byte: u8, base: u64) -> TempGame {
            let dir =
                std::env::temp_dir().join(format!("xcl_host_linux_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("game.exe"), tiny_exe_returning(exe_byte, base)).unwrap();
            TempGame { dir }
        }
    }
    impl Drop for TempGame {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn non_pe_input_fails_cleanly() {
        const CHILD: &str = "XCL_HOST_LINUX_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let dir = std::path::PathBuf::from(std::env::var("XCL_HOST_LINUX_DIR").unwrap());
            let data = std::path::PathBuf::from(std::env::var("XCL_HOST_LINUX_DATA").unwrap());
            let boot = Boot {
                title: "probe".into(),
                exe_path: dir.join("game.exe"),
                game_root: dir.clone(),
                data_dir: data.clone(),
                config: crate::config::Config::default(),
            };
            let code = run_boot(&boot);
            assert_eq!(code, 0x7C1_0002);
            let report = std::fs::read_to_string(data.join("logs").join("report.json")).unwrap();
            assert!(report.contains("loader"), "{report}");
            return;
        }
        let game = TempGame::new("nonpe", 0x2a, 0x6E60_0000_0000);
        std::fs::write(game.dir.join("game.exe"), b"not a PE").unwrap();
        let data = std::env::temp_dir().join(format!("xcl_host_linux_data_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(data.join("logs")).unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("host::tests::non_pe_input_fails_cleanly")
            .arg("--nocapture")
            .env(CHILD, "1")
            .env("XCL_HOST_LINUX_DIR", &game.dir)
            .env("XCL_HOST_LINUX_DATA", &data)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&data);
        assert!(
            out.status.success(),
            "child failed: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn synthetic_title_returns_its_entry_code() {
        const CHILD: &str = "XCL_HOST_LINUX_RUN_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let dir = std::path::PathBuf::from(std::env::var("XCL_HOST_LINUX_DIR").unwrap());
            let data = std::path::PathBuf::from(std::env::var("XCL_HOST_LINUX_DATA").unwrap());
            let boot = Boot {
                title: "synthetic".into(),
                exe_path: dir.join("game.exe"),
                game_root: dir.clone(),
                data_dir: data.clone(),
                config: crate::config::Config::default(),
            };
            std::fs::create_dir_all(data.join("logs")).unwrap();
            std::fs::create_dir_all(data.join("temp")).unwrap();
            let code = run_boot(&boot);
            assert_eq!(code, 0x2a);
            let report = std::fs::read_to_string(data.join("logs").join("report.json")).unwrap();
            let v: serde_json::Value = serde_json::from_str(&report).unwrap();
            assert_eq!(v["exit_code"], serde_json::Value::Null);
            return;
        }
        let game = TempGame::new("run", 0x2a, 0x6E61_0000_0000);
        let data =
            std::env::temp_dir().join(format!("xcl_host_linux_run_data_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(data.join("logs")).unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("host::tests::synthetic_title_returns_its_entry_code")
            .arg("--nocapture")
            .env(CHILD, "1")
            .env("XCL_HOST_LINUX_DIR", &game.dir)
            .env("XCL_HOST_LINUX_DATA", &data)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&data);
        assert!(
            out.status.success(),
            "child failed: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
