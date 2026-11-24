//! Host platform leaves that are needed before the Windows-only modules are ported:
//! the XCL data root and the process's private memory.

use std::path::PathBuf;

/// Data root: `XCL_DATA_ROOT`, else the platform's per-user data folder, else a temp folder.
///
/// Windows keeps the existing contract: `%XCL_DATA_ROOT%` or `%LOCALAPPDATA%\XCL`
/// (`%TEMP%\XCL` when `LOCALAPPDATA` is not set).
#[cfg(windows)]
pub fn data_root() -> PathBuf {
    if let Ok(p) = std::env::var("XCL_DATA_ROOT") {
        return PathBuf::from(p);
    }
    let base = std::env::var("LOCALAPPDATA")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    PathBuf::from(base).join("XCL")
}

/// Data root: `XCL_DATA_ROOT` > `XDG_DATA_HOME/XCL` > `$HOME/.local/share/XCL` > `$TMPDIR/XCL`.
#[cfg(not(windows))]
pub fn data_root() -> PathBuf {
    let xcl = std::env::var_os("XCL_DATA_ROOT").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    data_root_from(
        xcl.as_deref(),
        xdg.as_deref(),
        home.as_deref(),
        &std::env::temp_dir(),
    )
}

/// Pure precedence rule behind [`data_root`], so it can be tested without touching the
/// process environment (tests run in parallel).
#[cfg(not(windows))]
fn data_root_from(
    xcl: Option<&std::path::Path>,
    xdg: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
    temp: &std::path::Path,
) -> PathBuf {
    if let Some(root) = xcl {
        return root.to_path_buf();
    }
    if let Some(xdg) = xdg.filter(|p| !p.as_os_str().is_empty()) {
        return xdg.join("XCL");
    }
    if let Some(home) = home.filter(|p| !p.as_os_str().is_empty()) {
        return home.join(".local").join("share").join("XCL");
    }
    temp.join("XCL")
}

/// Bytes of private memory committed by the process.
///
/// Windows: `PROCESS_MEMORY_COUNTERS_EX::PrivateUsage` (the commit charge) in **bytes**;
/// Wine fills only the earlier fields, so `PagefileUsage` (the same number on Windows) is
/// the fallback.
///
/// Linux: `/proc/self/status` `VmData` (size of private writable mappings) in **KiB**,
/// multiplied by 1024. Linux has no commit-charge counter; `VmData` is the closest
/// analogue but is virtual, not resident, and excludes the stack (`VmStk`) and private
/// file-backed/image mappings. `VmRSS`/`RssAnon` are deliberately not used because they
/// count physical residency, while the title budget tracks committed memory.
pub fn process_private_bytes() -> u64 {
    imp::process_private_bytes()
}

#[cfg(windows)]
mod imp {
    pub fn process_private_bytes() -> u64 {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
        };
        unsafe {
            let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
            c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            if K32GetProcessMemoryInfo(
                windows_sys::Win32::System::Threading::GetCurrentProcess(),
                &mut c as *mut _ as *mut _,
                c.cb,
            ) != 0
            {
                // Wine fills the fields before PrivateUsage only; PagefileUsage is the same
                // number on Windows (the commit charge)
                if c.PrivateUsage != 0 {
                    c.PrivateUsage as u64
                } else {
                    c.PagefileUsage as u64
                }
            } else {
                0
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn process_private_bytes() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .map(|s| super::parse_proc_status_private_bytes(&s))
            .unwrap_or(0)
    }
}

/// `VmData` from `/proc/self/status` (KiB) as bytes; falls back to `VmRSS` when the
/// kernel does not report `VmData`. A missing or malformed field is 0.
#[cfg(not(windows))]
fn parse_proc_status_private_bytes(status: &str) -> u64 {
    ["VmData", "VmRSS"]
        .into_iter()
        .find_map(|key| parse_proc_status_kib(status, key))
        .unwrap_or(0)
        .saturating_mul(1024)
}

#[cfg(not(windows))]
fn parse_proc_status_kib(status: &str, key: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim() != key {
            return None;
        }
        let mut fields = value.split_whitespace();
        let kib = fields.next()?.parse::<u64>().ok()?;
        // proc(5): these fields are in kB; reject a line with an unexpected unit.
        if let Some(unit) = fields.next() {
            if !unit.eq_ignore_ascii_case("kB") {
                return None;
            }
        }
        Some(kib)
    })
}

#[cfg(test)]
mod tests {
    #[cfg(not(windows))]
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn data_root_precedence_is_xcl_then_xdg_then_home_then_temp() {
        use std::path::Path;

        let temp = Path::new("/tmp/xcl");
        assert_eq!(
            data_root_from(
                Some(Path::new("/data/xcl")),
                Some(Path::new("/xdg")),
                Some(Path::new("/home/u")),
                temp
            ),
            Path::new("/data/xcl")
        );
        assert_eq!(
            data_root_from(
                None,
                Some(Path::new("/xdg")),
                Some(Path::new("/home/u")),
                temp
            ),
            Path::new("/xdg/XCL")
        );
        // an empty XDG_DATA_HOME falls through to HOME, as an unset one would
        assert_eq!(
            data_root_from(None, Some(Path::new("")), Some(Path::new("/home/u")), temp),
            Path::new("/home/u/.local/share/XCL")
        );
        assert_eq!(
            data_root_from(None, None, Some(Path::new("/home/u")), temp),
            Path::new("/home/u/.local/share/XCL")
        );
        assert_eq!(
            data_root_from(None, None, Some(Path::new("")), temp),
            Path::new("/tmp/xcl/XCL")
        );
        assert_eq!(
            data_root_from(None, None, None, temp),
            Path::new("/tmp/xcl/XCL")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn proc_status_private_bytes_uses_vmdata_in_kib_and_falls_back_to_vmrss() {
        let status = "Name:\txcl\nVmPeak:\t  131072 kB\nVmData:\t   2048 kB\nVmRSS:\t    1024 kB\n";
        assert_eq!(parse_proc_status_private_bytes(status), 2048 * 1024);

        let fallback = "Name:\txcl\nVmRSS:\t     512 kB\n";
        assert_eq!(parse_proc_status_private_bytes(fallback), 512 * 1024);

        // Present but zero stays zero (no fallback): the process has no private data.
        assert_eq!(
            parse_proc_status_private_bytes("VmData:\t       0 kB\nVmRSS:\t 512 kB\n"),
            0
        );

        // Missing, wrong unit or malformed value: no memory count.
        assert_eq!(parse_proc_status_private_bytes("Name:\txcl\n"), 0);
        assert_eq!(parse_proc_status_private_bytes("VmData:\t 2048 MB\n"), 0);
        assert_eq!(parse_proc_status_private_bytes("VmData:\t lots kB\n"), 0);

        // A bogus huge field saturates instead of wrapping.
        assert_eq!(
            parse_proc_status_private_bytes("VmData:\t 18446744073709551615 kB\n"),
            u64::MAX
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_private_bytes_is_nonzero_on_linux() {
        assert!(process_private_bytes() > 0);
    }
}
