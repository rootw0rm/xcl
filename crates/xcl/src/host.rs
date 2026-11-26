//! The host process: started by the launcher with the game's command line. Sets up
//! logging, the VFS, hooks and HLE, maps the title and jumps to its entry point.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Boot;

pub static HOST_MODE: AtomicBool = AtomicBool::new(false);

pub fn host_main(boot_path: &Path) -> i32 {
    let boot: Boot = match std::fs::read_to_string(boot_path).map_err(|e| e.to_string()).and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("xcl host: cannot read {}: {e}", boot_path.display());
            return 0x7C1_0001;
        }
    };
    crate::log::init(
        Some(&boot.logs_dir().join("xcl.log")),
        crate::log::Level::parse(&boot.config.log_level),
        boot.config.log_to_stderr,
    );
    crate::log::log_panics();
    crate::xlog_info!("XCL {} – host for '{}'", env!("CARGO_PKG_VERSION"), boot.title);
    crate::xlog_info!("game: {} (read-only folder: {})", boot.exe_path.display(), boot.game_root.display());
    crate::xlog_info!("XCL data: {}", boot.data_dir.display());
    crate::xlog_info!(
        "console: {} (title memory {} MiB, mode {})",
        if boot.config.is_series_s() { "Xbox Series S (lockhart)" } else { "Xbox Series X (anaconda)" },
        boot.config.title_memory_mb,
        boot.config.title_memory
    );

    if std::env::var_os("XCL_UMA_MB").is_none() && boot.config.uma_mb != 0 {
        std::env::set_var("XCL_UMA_MB", boot.config.uma_mb.to_string());
    }
    crate::state::set_boot(boot.clone());
    // Load host files before the title's VFS remaps console drive letters.
    crate::hle::gameinput::init_frame_rules();
    let _ = std::thread::Builder::new().name("xcl-split-counts".into()).spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
        crate::hle::d3d12x::real::log_dispatch_chain_counts();
    });
    // `XCL_ACTIVITY=1`: every 2 s what the title did since (a timeline of where it stops)
    if std::env::var_os("XCL_ACTIVITY").is_some() {
        let _ = std::thread::Builder::new().name("xcl-activity".into()).spawn(|| {
            let snap = || {
                let (p, e) = crate::hle::d3d12x::present::activity();
                let (d, x) = crate::hle::d3d12x::bind::activity();
                let f = crate::hle::d3d12x::frame::activity();
                let (r, b, fail) = crate::hle::dstorage::counters();
                [p, e, d, x, f[0], f[1], f[2], f[3], r, b >> 20, fail]
            };
            let mut last = snap();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let now = snap();
                let d: Vec<u64> = now.iter().zip(last).map(|(a, b)| a - b).collect();
                crate::xlog_info!(
                    "activity (2 s): {} presents, {} submits, {} draws, {} dispatches, {} frame-event waits ({} timed out), {} frame statistics ({} not flipped); DirectStorage {} requests, {} MB, {} failed",
                    d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7], d[8], d[9], d[10]
                );
                // where the memory is: the process's commit, video memory, and XCL's parts of it
                let private = crate::platform::process_private_bytes();
                let (local, non_local) = crate::hle::d3d12x::real::video_memory().unwrap_or((0, 0));
                let (arenas, arenas_made) = crate::hle::d3d12x::vram::arena_bytes();
                let (pools, pools_mapped) = crate::hle::d3d12x::sparse::pool_bytes();
                let (pages, page_pools) = crate::hle::d3d12x::vram::page_bytes();
                let (thin, thin_bytes) = crate::hle::d3d12x::vram::skipped_textures();
                let mb = |b: u64| b >> 20;
                crate::xlog_info!(
                    "memory: {} MB committed by the process; video memory {} MB local, {} MB non-local; target pages {} MB (pools {} MB); arenas {} MB in use, {} MB made; sparse pools {} MB ({} MB mapped); top mips left out of {} textures ({} MB)",
                    mb(private),
                    mb(local),
                    mb(non_local),
                    mb(pages as u64),
                    mb(page_pools as u64),
                    mb(arenas as u64),
                    mb(arenas_made as u64),
                    mb(pools as u64),
                    mb(pools_mapped as u64),
                    thin,
                    mb(thin_bytes)
                );
                last = now;
            }
        });
    }
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
    // `XCL_RUN_SECONDS=n`: the title ends itself after n seconds – the GPU drained, the log and
    // report written (a process killed from outside with GPU work in flight can hang in the driver)
    if let Some(secs) = std::env::var("XCL_RUN_SECONDS").ok().and_then(|v| v.parse::<u64>().ok()) {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            let (shown, presented) = crate::hle::d3d12x::present::counters();
            crate::xlog_info!("XCL_RUN_SECONDS: {secs} s – ending the title ({shown} frames shown, {presented} PresentX calls)");
            crate::report::write_final(None);
            crate::log::flush();
            crate::hle::d3d12x::real::end_title(0);
        });
    }
    crate::stubs::init_db(&boot.config.stub_returns);
    crate::hle::init(&boot.config);
    crate::hle::kernelx::fine_timer();
    crate::loader::set_cache_dir(boot.cache_dir());
    let mut dirs = vec![boot.game_root.clone()];
    if let Some(p) = boot.exe_path.parent() {
        if p != boot.game_root {
            dirs.insert(0, p.to_path_buf());
        }
    }
    crate::resolver::set_search_dirs(dirs);

    let mut extra_own: Vec<std::path::PathBuf> = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).into_iter().collect();
    for var in ["XCL_DXC_DIR", "XCL_D3D12_DIR", "XCL_OPUS_DIR"] {
        if let Some(dir) = std::env::var_os(var).and_then(trim_dir) {
            extra_own.push(dir);
        }
    }
    crate::vfs::init(build_vfs(&boot, &crate::gpu::recomp::dxc::cache_dir(), &extra_own));
    let _ = std::fs::create_dir_all(boot.data_dir.join("temp"));

    if let Err(e) = crate::vfs_hooks::install() {
        crate::xlog_error!("VFS: not all hooks installed: {e}");
    }
    if let Err(e) = crate::eh::install_pc_hook() {
        crate::xlog_error!("hook RtlPcToFileHeader: {e} – C++ exceptions in the game may not work");
    }
    crate::eh::install_crash_handler();
    crate::eh::start_probes();
    HOST_MODE.store(true, Ordering::Release);

    let main = match crate::loader::load_main(&boot.exe_path) {
        Ok(m) => m,
        Err(e) => {
            crate::xlog_error!("loading the game failed: {e}");
            crate::report::write_final(Some(format!("loader: {e}")));
            return 0x7C1_0002;
        }
    };
    unsafe { crate::hle::kernelx::adopt_title_hooks(&main) };
    crate::report::write_final(None);
    let code = crate::loader::run_main(&main);
    crate::xlog_info!("game entry point returned code {code}");
    crate::report::write_final(None);
    crate::log::flush();
    code as i32
}

fn trim_dir(raw: std::ffi::OsString) -> Option<std::path::PathBuf> {
    match raw.to_str() {
        Some(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| std::path::PathBuf::from(t))
        }
        None => Some(std::path::PathBuf::from(raw)),
    }
}

/// Build the host VFS, exempting XCL's own folders outside `boot.data_dir` (the shared shader
/// cache, `xcl.exe`'s directory, and configured host tool DLL folders) from console drive mounts.
fn build_vfs(boot: &Boot, cache_dir: &Path, extra_own: &[std::path::PathBuf]) -> crate::vfs::Vfs {
    let mounts: Vec<(String, String)> = boot.config.mounts.iter().map(|(k, v)| (k.clone(), boot.expand(v))).collect();
    let mut vfs = crate::vfs::Vfs::new(&boot.game_root, &boot.data_dir, mounts).own_folder(cache_dir);
    for dir in extra_own {
        vfs = vfs.own_folder(dir);
    }
    vfs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::vfs::Target;
    use std::path::PathBuf;

    fn test_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xcl_host_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let canon = std::fs::canonicalize(&dir).unwrap();
        let s = canon.to_string_lossy();
        PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
    }

    #[test]
    fn console_drive_mount_exempts_host_tool_and_exe_folders() {
        let root = test_root("own_tools");
        let game = root.join("game");
        let data = root.join("xcldata");
        let cache = root.join("dxil_cache");
        let dxc = root.join("sdk").join("x64");
        let d3d12 = root.join("agility");
        let opus = root.join("opus");
        let exe_dir = root.join("bin");
        for d in [&game, &data, &cache, &dxc, &d3d12, &opus, &exe_dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        let drive = root.to_string_lossy()[..2].to_string();
        let mut config = Config::default();
        config.mounts.insert(drive.clone(), "{game}".into());
        let boot = Boot { title: "test".into(), exe_path: game.join("test.exe"), game_root: game, data_dir: data, config };

        let vfs = build_vfs(&boot, &cache, &[exe_dir.clone(), dxc.clone(), d3d12.clone(), opus.clone()]);
        for own in [
            dxc.join("dxcompiler.dll"),
            dxc.join("dxil.dll"),
            d3d12.join("D3D12Core.dll"),
            opus.join("opus.dll"),
            exe_dir.join("dxcompiler.dll"),
            cache.join("a.dxil"),
        ] {
            let s = own.to_string_lossy();
            assert_eq!(vfs.classify(&s), Target::Pass, "{s}");
            assert_eq!(vfs.classify(&format!(r"\\?\{s}")), Target::Pass, "verbatim {s}");
        }
        assert_eq!(vfs.classify(&format!(r"{drive}\data\hello.txt")), Target::Game { rel: r"data\hello.txt".into() });
        let sibling = PathBuf::from(format!("{}2", dxc.display())).join("dxcompiler.dll");
        let sibling_rel = sibling.to_string_lossy()[3..].to_string();
        assert_eq!(vfs.classify(&sibling.to_string_lossy()), Target::Game { rel: sibling_rel });
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn drive_root_or_blank_own_dir_does_not_disable_console_mount() {
        assert_eq!(trim_dir("".into()), None);
        assert_eq!(trim_dir("   \t  ".into()), None);
        assert_eq!(trim_dir(r"  D:\sdk\x64  ".into()), Some(PathBuf::from(r"D:\sdk\x64")));

        let root = test_root("root_guard");
        let game = root.join("game");
        let data = root.join("xcldata");
        let cache = root.join("dxil_cache");
        for d in [&game, &data, &cache] {
            std::fs::create_dir_all(d).unwrap();
        }
        let drive = root.to_string_lossy()[..2].to_string();
        let mut config = Config::default();
        config.mounts.insert(drive.clone(), "{game}".into());
        let boot = Boot { title: "test".into(), exe_path: game.join("test.exe"), game_root: game, data_dir: data, config };

        let vfs = build_vfs(&boot, &cache, &[PathBuf::from(format!(r"{drive}\")), PathBuf::from(format!(r"{drive}\sub\.."))]);
        assert_eq!(vfs.classify(&format!(r"{drive}\data\hello.txt")), Target::Game { rel: r"data\hello.txt".into() });
        let _ = std::fs::remove_dir_all(&root);
    }
}
