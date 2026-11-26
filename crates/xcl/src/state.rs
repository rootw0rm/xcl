//! Global host state (set once at host start; defaults in unit tests).

use std::sync::OnceLock;

use crate::config::{Boot, Config};

static BOOT: OnceLock<Boot> = OnceLock::new();
static DEFAULT_CONFIG: OnceLock<Config> = OnceLock::new();
static DEFAULT_DATA_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

pub fn set_boot(b: Boot) {
    let _ = BOOT.set(b);
}

pub fn boot() -> Option<&'static Boot> {
    BOOT.get()
}

pub fn config() -> &'static Config {
    match BOOT.get() {
        Some(b) => &b.config,
        None => DEFAULT_CONFIG.get_or_init(Config::default),
    }
}

/// Folder for title data (saves, PLS, temp…) – a temp folder when no boot record.
pub fn data_dir() -> std::path::PathBuf {
    match BOOT.get() {
        Some(b) => b.data_dir.clone(),
        None => DEFAULT_DATA_DIR
            .get_or_init(|| {
                let temp = std::env::temp_dir();
                cleanup_stale_default_data_dirs(&temp);
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                default_data_dir(&temp, std::process::id(), nonce)
            })
            .clone(),
    }
}

fn default_data_dir(temp: &std::path::Path, pid: u32, nonce: u128) -> std::path::PathBuf {
    temp.join(format!("xcl_unit_data_{pid}_{nonce}"))
}

fn default_data_dir_process(name: &str) -> Option<u32> {
    let Some(rest) = name.strip_prefix("xcl_unit_data_") else {
        return None;
    };
    let Some((pid, nonce)) = rest.split_once('_') else {
        return None;
    };
    let pid = pid.parse::<u32>().ok()?;
    nonce.parse::<u128>().ok()?;
    Some(pid)
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            // Access denied is not proof that the process ended. Only the invalid-PID result
            // lets cleanup safely remove a directory owned by another process.
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        CloseHandle(process);
        true
    }
}

#[cfg(not(windows))]
fn process_is_running(pid: u32) -> bool {
    // Linux: a live PID owns /proc/<pid>. Absent means the process ended, so its
    // stale directory may be reclaimed. Conservative like the Windows twin: only
    // a missing entry counts as "not running".
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn stale_default_data_dir(
    name: &str,
    age: std::time::Duration,
    process_running: impl FnOnce(u32) -> bool,
) -> bool {
    const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    age >= MAX_AGE && default_data_dir_process(name).is_some_and(|pid| !process_running(pid))
}

/// Test processes cannot rely on static destructors running at exit. Bound the leftovers
/// instead: the next test process removes only directories with our exact generated name
/// whose process has ended and that have not been touched for a day. All failures are
/// best-effort and leave data alone.
fn cleanup_stale_default_data_dirs(temp: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let age = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok());
        if age.is_some_and(|age| stale_default_data_dir(name, age, process_is_running)) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_data_folders_are_stable_per_process_and_separate_between_processes() {
        let temp = std::path::Path::new("temp");
        assert_eq!(
            default_data_dir(temp, 17, 42),
            temp.join("xcl_unit_data_17_42")
        );
        assert_eq!(
            default_data_dir(temp, 17, 42),
            default_data_dir(temp, 17, 42)
        );
        assert_ne!(
            default_data_dir(temp, 17, 42),
            default_data_dir(temp, 18, 42)
        );
        assert_ne!(
            default_data_dir(temp, 17, 42),
            default_data_dir(temp, 17, 43)
        );
        assert_eq!(default_data_dir_process("xcl_unit_data_17_42"), Some(17));
        assert_eq!(default_data_dir_process("xcl_unit_data_17"), None);
        assert_eq!(default_data_dir_process("xcl_unit_data_17_42_extra"), None);
        assert_eq!(default_data_dir_process("xcl_unit_data_bad_42"), None);
    }

    #[test]
    fn stale_default_data_folders_are_removed_only_after_their_process_ends() {
        let old = std::time::Duration::from_secs(25 * 60 * 60);
        let young = std::time::Duration::from_secs(23 * 60 * 60);
        assert!(stale_default_data_dir("xcl_unit_data_17_42", old, |pid| {
            assert_eq!(pid, 17);
            false
        }));
        assert!(!stale_default_data_dir("xcl_unit_data_17_42", old, |_| true));
        assert!(!stale_default_data_dir("xcl_unit_data_17_42", young, |_| false));
        assert!(!stale_default_data_dir("other_17_42", old, |_| false));
    }

    #[test]
    fn current_process_default_data_folder_is_not_stale() {
        assert!(process_is_running(std::process::id()));
    }
}
