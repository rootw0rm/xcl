//! `xcl inspect <exe>` – static analysis without running anything (read-only).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::pe::{Layout, Pe};
use crate::resolver::{self, Source};

#[derive(Serialize)]
struct Entry {
    dll: String,
    function: String,
    delay: bool,
    status: String,
}

#[derive(Serialize)]
struct Out {
    file: String,
    machine: String,
    image_base: String,
    entry_rva: String,
    size_of_image: String,
    subsystem: u16,
    is_dll: bool,
    has_relocations: bool,
    tls_template_bytes: Option<u64>,
    exception_entries: u32,
    stack_reserve: String,
    imports: Vec<Entry>,
}

/// Console-only DLLs a PE imports (kernelx.dll, xgameruntime*, d3d12_xs …).
/// `None` when the file is not a valid x64 PE. A plain Windows program yields `Some([])`.
///
/// On Windows the returned names are the imported DLL keys; on Linux they are the HLE
/// modules they route to (the implementation modules are not compiled before Phase C).
#[cfg(windows)]
pub fn console_imports(path: &Path) -> Option<Vec<String>> {
    let data = std::fs::read(path).ok()?;
    let pe = Pe::parse(&data, Layout::File).ok()?;
    let mut all = pe.imports().unwrap_or_default();
    all.extend(pe.delay_imports().unwrap_or_default());
    let reg = crate::hle::registry();
    let mut out: Vec<String> = Vec::new();
    for d in &all {
        let key = crate::util::dll_key(&d.dll);
        let console = key == crate::hle::KERNELX
            || reg
                .hle_for(&key)
                .map(|m| m.name == crate::hle::XGAMERUNTIME || m.name == crate::hle::D3D12X)
                .unwrap_or(false);
        if console && !out.contains(&key) {
            out.push(key);
        }
    }
    Some(out)
}

#[cfg(not(windows))]
pub fn console_imports(path: &Path) -> Option<Vec<String>> {
    let data = std::fs::read(path).ok()?;
    let pe = Pe::parse(&data, Layout::File).ok()?;
    let mut all = pe.imports().unwrap_or_default();
    all.extend(pe.delay_imports().unwrap_or_default());
    let mut out: Vec<String> = Vec::new();
    for d in &all {
        let key = crate::util::dll_key(&d.dll);
        if !crate::hle_routes::is_console_game_dll(&key) {
            continue;
        }
        if let Some(module) = crate::hle_routes::module_for(&key) {
            if !out.iter().any(|m| m == module) {
                out.push(module.to_string());
            }
        }
    }
    Some(out)
}

/// Function-level classification string.
fn status(s: &Source) -> String {
    match s {
        Source::Hle(m) => format!("HLE ({m})"),
        Source::HleOther(m) => format!("HLE ({m}, other module)"),
        Source::System(d) => format!("Windows ({d})"),
        Source::GameDll(d) => format!("game DLL ({d})"),
        Source::Stub => "STUB".into(),
    }
}

#[cfg(windows)]
fn classify(dll: &str, what: &crate::pe::ImportName, _search_dir: Option<&Path>) -> String {
    status(&resolver::classify(dll, what))
}

/// Linux (Phase C step 10a): the HLE registry builds from preserved export-name tables
/// (address `0` = Stub metadata) and the resolver allocates callable auto-stub thunks.
/// There is no `LoadLibrary`, so unmapped non-HLE imports are `STUB`, never a null.
#[cfg(not(windows))]
fn classify(dll: &str, what: &crate::pe::ImportName, _search_dir: Option<&Path>) -> String {
    status(&resolver::classify(dll, what))
}

pub fn run(path: &Path, json: bool) -> i32 {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("xcl inspect: cannot read {}: {e}", path.display());
            return 2;
        }
    };
    let pe = match Pe::parse(&data, Layout::File) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "xcl inspect: {}: {e}\n(If this is a file from an XVC/MSIXVC package – XCL only works on already unpacked files.)",
                path.display()
            );
            return 3;
        }
    };
    let search_dir = path.parent();
    if let Some(dir) = search_dir {
        resolver::set_search_dirs(vec![dir.to_path_buf()]);
    }
    let mut imports = Vec::new();
    let mut all = pe.imports().unwrap_or_default();
    all.extend(pe.delay_imports().unwrap_or_default());
    for d in &all {
        for e in &d.entries {
            let s = classify(&d.dll, &e.what, search_dir);
            imports.push(Entry { dll: d.dll.clone(), function: e.what.to_string(), delay: d.delay, status: s });
        }
    }
    let tls = pe.tls().ok().flatten().map(|t| t.total_len());
    let ex = pe.dir(crate::pe::DIR_EXCEPTION);
    let out = Out {
        file: path.to_string_lossy().into_owned(),
        machine: format!("{:#06x}", pe.machine),
        image_base: format!("{:#x}", pe.image_base),
        entry_rva: format!("{:#x}", pe.entry_rva),
        size_of_image: format!("{:#x}", pe.size_of_image),
        subsystem: pe.subsystem,
        is_dll: pe.is_dll(),
        has_relocations: pe.has_relocs(),
        tls_template_bytes: tls,
        exception_entries: ex.size / 12,
        stack_reserve: format!("{:#x}", pe.stack_reserve),
        imports,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return 0;
    }
    println!("File:          {}", out.file);
    println!("Type:          {} x64, subsystem {}", if out.is_dll { "DLL" } else { "EXE" }, out.subsystem);
    println!("ImageBase:     {}  (size {}, entry +{})", out.image_base, out.size_of_image, out.entry_rva);
    println!("Relocations:   {}", if out.has_relocations { "yes" } else { "NO (the image must be mapped at ImageBase)" });
    #[cfg(windows)]
    println!("Static TLS:    {}", tls.map(|n| format!("{n} B (handled by the TLS carrier)")).unwrap_or_else(|| "none".into()));
    // LINUX-TODO: Phase C — the per-thread TLS carrier is not implemented on Linux yet.
    #[cfg(not(windows))]
    println!("Static TLS:    {}", tls.map(|n| format!("{n} B (TLS carrier is Phase C on Linux)")).unwrap_or_else(|| "none".into()));
    println!("Exceptions:    {} .pdata entries", out.exception_entries);
    println!("Stack:         {}", out.stack_reserve);
    match console_imports(path) {
        Some(c) if !c.is_empty() => println!("Console game:  yes (imports {})", c.join(", ")),
        _ => println!("Console game:  NO – no kernelx / xgameruntime / d3d12_xs imports (a regular Windows program?)"),
    }
    let mut per_dll: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    for e in &out.imports {
        per_dll.entry(format!("{}{}", e.dll, if e.delay { " (delay-load)" } else { "" })).or_default().push(e);
    }
    let total_stub = out.imports.iter().filter(|e| e.status == "STUB").count();
    println!("\nImports: {} functions from {} DLLs, {} of them unimplemented (stubs):", out.imports.len(), per_dll.len(), total_stub);
    for (dll, entries) in &per_dll {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for e in entries {
            let k = if e.status.starts_with("HLE") {
                "HLE"
            } else if e.status.starts_with("Windows") {
                "Windows"
            } else if e.status.starts_with("game DLL") {
                "game DLL"
            } else {
                "stubs"
            };
            *counts.entry(k).or_default() += 1;
        }
        let c: Vec<String> = counts.iter().map(|(k, v)| format!("{v} {k}")).collect();
        println!("  {dll}: {}", c.join(", "));
        for e in entries.iter().filter(|e| e.status == "STUB") {
            println!("      - {}", e.function);
        }
    }
    0
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    fn name(s: &str) -> crate::pe::ImportName {
        crate::pe::ImportName::Name { hint: 0, name: s.into() }
    }

    /// Linux has no `LoadLibrary`: unimplemented HLE-routed names are Stub metadata and
    /// every other unmapped import resolves to a callable auto-stub, never a claimed
    /// Windows DLL. Step-10c file APIs are implemented HLE, as is the step-11 CRT slice
    /// (resolved from any api-ms-win-crt-* contract through the cross-HLE lookup).
    #[test]
    fn linux_classification_never_claims_hle() {
        assert_eq!(classify("kernel32.dll", &name("CreateFileW"), None), "HLE (kernelx.dll)");
        assert_eq!(classify("d3d12_xs.dll", &name("D3D12XboxCreateDevice"), None), "HLE (d3d12_xs.dll)");
        assert_eq!(classify("ucrtbase.dll", &name("malloc"), None), "HLE (ucrtbase.dll, other module)");
        assert_eq!(
            classify("api-ms-win-crt-stdio-l1-1-0.dll", &name("__p__fmode"), None),
            "HLE (ucrtbase.dll, other module)"
        );
        assert_eq!(classify("ucrtbase.dll", &name("getenv"), None), "HLE (ucrtbase.dll, other module)");
    }

    #[test]
    fn game_dlls_in_the_inspected_folder_are_recognised_case_insensitively() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("xcl-inspect-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MyLib.DLL"), b"").unwrap();
        resolver::set_search_dirs(vec![dir.clone()]);
        let got = classify("mylib.dll", &name("Thing"), Some(&dir));
        resolver::set_search_dirs(vec![]);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, "game DLL (mylib.dll)");
    }
}
