//! xcl.exe – command line front-end and (in host mode) the process the game runs in.

#[cfg(windows)]
use std::ffi::c_void;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::atomic::Ordering;

/// Our own TLS callback: ntdll calls it for every thread start/exit in the process.
/// In host mode it initialises the game's static TLS for the new thread and forwards
/// the notification to the game's TLS callbacks / DllMains.
#[cfg(windows)]
unsafe extern "system" fn xcl_tls_callback(_h: *mut c_void, reason: u32, _r: *mut c_void) {
    if xcl::host::HOST_MODE.load(Ordering::Acquire) {
        xcl::loader::on_tls_event(reason);
    }
}

#[cfg(windows)]
#[used]
#[link_section = ".CRT$XLY"]
static XCL_TLS_CALLBACK: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) = xcl_tls_callback;

const HELP_HEADER: &str = "\
XCL – Xbox Series X|S → Windows compatibility layer

Usage:
";

// One block per command. `xcl --help` prints all of them and `xcl <command> --help|-h`
// prints exactly the command's block: the usage text has a single definition.
const RUN_USAGE: &str = "  xcl run <game folder | file.exe> [-- game arguments]
      Runs the game. The game folder is only read; writes, game saves and logs go to
      %LOCALAPPDATA%\\XCL\\<title>\\ on Windows or $XDG_DATA_HOME/XCL/<title>/ on Linux
      (%XCL_DATA_ROOT%\\<title>\\ overrides the root).
";

const INSPECT_USAGE: &str = "  xcl inspect <file.exe|file.dll> [--json]
      Analysis without running: imports and what XCL has for them (HLE / Windows / stub).
";

const PREFLIGHT_USAGE: &str = "  xcl preflight [gpu]
      Checks the PC against what a run needs (DXC, the system XGameRuntime, Shader Model 6.6,
      mesh shaders, wave width) without starting a game: a missing piece shows up now rather
      than after the world load. The same checks `xcl run` prints, without the load. `gpu` is a
      d3d12_adapter value (default: auto). Exit code 1 if any check is an Error, else 0.
";

const DISASM_USAGE: &str =
    "  xcl disasm <file> [--offset N] [--length N] [--all] [--hex] [--fssm FILE]
      Disassembler for console GPU code (RDNA2) – e.g. for shaders saved in <data>\\shaders\\.
      Console containers (XDAT/XBCH/FSSM) are unpacked; --fssm saves the unpacked FSSM.
";

const SHADERS_USAGE: &str =
    "  xcl shaders <file | folder | package.rpf> [--top N] [--json] [--recomp] [--compile N]
      Shader corpus statistics: formats, stages, RDNA2 instruction histogram (priorities
      for the recompiler). RPF9 packages are scanned for shader records.
      --recomp: translates every RDNA2 program → HLSL and shows what is missing;
      --compile N: additionally compiles N complete translations with DXC.
      --precompile [--only STAGE]: caches compiled shaders before a run (e.g. --only cs).
          --pipelines also builds compute pipelines and caches them for the next run.
          --used-only skips compute shaders whose pipelines were not used in earlier runs.
      --export-hlsl DIR [--count N] [--min-kb N] [--record-hash HEX]: writes deterministic
          compute HLSL for investigation; an exact hash bypasses the size threshold.
      --measure-pso HEX [--repeat N]: times cache-disabled driver compilation of one exact
          compute record with XCL's real root signature and static samplers.
      --direct [--sample N] [--seed S] [--validate] [--compare]: direct DXIL translator
      coverage over a seeded sample (default 200 programs, 0 = all); also
      --timeout, --max-bytes, --threads, --json, --hash H, --dump DIR.
      --record-vs [--sample N] [--seed S] [--top N] [--threads N] [--json]: classifies
          records as console VS (S1 interface + S2/S3 SPIR-V) and counts what the
          bring-up backend rejects or cannot translate; no device, no DXC.
";

const RPF_USAGE: &str = "  xcl rpf <package.rpf> [--top N] [--find NAME] [--extract NAME --out DIR]
      Lists or extracts readable inline records. Archives larger than 1 GB use a
      bounded-memory header probe; --probe requests it explicitly and --full overrides it.
      Use --probe only for header summaries, without --find, --extract, --out or --full.
";

const REPLAY_USAGE: &str =
    "  xcl replay <capture folder> [--timeout S] [--repeat N] [--out NAME] [--debug] [--no-run]
             [--sub FROM=>TO||…] [--profile ps_6_7] [--split 0|1] [--loops]
      Runs one draw / dispatch captured with XCL_CAPTURE_DRAW alone on a device of its own:
      its GPU time, and PNG / raw files of what it wrote in the folder replay_<NAME> of the
      capture. --timeout: seconds the call may take (15); --no-run: translate and compile
      only; --debug: with the D3D12 debug layer. --sub: text substitutions in the translated
      HLSL; --loops: the passes of each loop of the pixel shader, per lane.
";

const USAGES: &[(&str, &str)] = &[
    ("run", RUN_USAGE),
    ("inspect", INSPECT_USAGE),
    ("preflight", PREFLIGHT_USAGE),
    ("disasm", DISASM_USAGE),
    ("shaders", SHADERS_USAGE),
    ("rpf", RPF_USAGE),
    ("replay", REPLAY_USAGE),
];

fn main() {
    // keep the TLS callback from being discarded by the linker
    #[cfg(windows)]
    unsafe {
        std::ptr::read_volatile(&XCL_TLS_CALLBACK)
    };

    #[cfg(windows)]
    if let Some(boot) = std::env::var_os("XCL_HOST") {
        std::env::remove_var("XCL_HOST");
        std::process::exit(xcl::host::host_main(&PathBuf::from(boot)));
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(usage) = subcommand_help(&args) {
        print!("Usage:\n{usage}");
        std::process::exit(0);
    }
    let code = match args.first().map(String::as_str) {
        Some("run") if args.len() >= 2 => {
            let rest: Vec<String> = match args.iter().position(|a| a == "--") {
                Some(i) => args[i + 1..].to_vec(),
                None => args[2..].to_vec(),
            };
            run_command(Path::new(&args[1]), &rest)
        }
        Some("inspect") if args.len() >= 2 => xcl::inspect::run(Path::new(&args[1]), &args[2..]),
        Some("disasm") if args.len() >= 2 => xcl::disasm::run(&PathBuf::from(&args[1]), &args[2..]),
        Some("preflight") => preflight_command(&args[1..]),
        Some("shaders") if args.len() >= 2 => xcl::shaders::run(&PathBuf::from(&args[1]), &args[2..]),
        Some("rpf") if args.len() >= 2 => xcl::rpf::run(&PathBuf::from(&args[1]), &args[2..]),
        #[cfg(windows)]
        Some("replay") if args.len() >= 2 => xcl::replay::run(&PathBuf::from(&args[1]), &args[2..]),
        Some("--version") | Some("-V") => {
            println!("xcl {}", env!("CARGO_PKG_VERSION"));
            0
        }
        None => match no_args() {
            Some(code) => code,
            None => help(&args),
        },
        _ => help(&args),
    };
    std::process::exit(code);
}

/// `xcl <command> --help|-h`: that command's block from the table the top-level help prints.
/// Only the first argument after the command counts, so `xcl run <game> -- --help` still
/// passes `--help` to the game.
fn subcommand_help(args: &[String]) -> Option<&'static str> {
    let command = args.first()?.as_str();
    if !matches!(args.get(1).map(String::as_str), Some("--help") | Some("-h")) {
        return None;
    }
    USAGES
        .iter()
        .find(|(name, _)| *name == command)
        .map(|(_, usage)| *usage)
}

fn help(args: &[String]) -> i32 {
    print!("{HELP_HEADER}");
    for (_, usage) in USAGES {
        print!("{usage}");
    }
    println!("  xcl --version");
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        0
    } else {
        2
    }
}

/// `xcl preflight [gpu]`: one optional `d3d12_adapter` value and nothing else. A flag or a
/// second positional is a usage error, like `inspect`/`disasm`, instead of being dropped or
/// probed as the setting.
fn preflight_command(args: &[String]) -> i32 {
    let setting = match args.first() {
        None => "auto",
        Some(value) if value.starts_with('-') => {
            eprintln!("xcl preflight: unknown option: {value}; see `xcl --help`");
            return 2;
        }
        Some(value) => value.as_str(),
    };
    if let Some(extra) = args.get(1) {
        if extra.starts_with('-') {
            eprintln!("xcl preflight: unknown option: {extra}; see `xcl --help`");
        } else {
            eprintln!("xcl preflight: unexpected argument: {extra}; see `xcl --help`");
        }
        return 2;
    }
    let checks = xcl::preflight::run(setting);
    xcl::preflight::print(&checks);
    xcl::preflight::exit_code(&checks)
}

#[cfg(windows)]
fn run_command(target: &Path, rest: &[String]) -> i32 {
    xcl::launcher::run(target, rest)
}

/// Linux (Phase C step 10e): the launcher runs the title in-process.
#[cfg(not(windows))]
fn run_command(target: &Path, rest: &[String]) -> i32 {
    xcl::launcher::run(target, rest)
}

/// xcl.exe opened from Explorer with no arguments: run the picked game.
#[cfg(windows)]
fn no_args() -> Option<i32> {
    started_by_double_click().then(run_picked_game)
}

/// No Explorer menu on Linux: a bare invocation prints the help.
#[cfg(not(windows))]
fn no_args() -> Option<i32> {
    None
}

/// xcl.exe opened from Explorer: the console is its own (from a terminal, the shell shares it).
#[cfg(windows)]
fn started_by_double_click() -> bool {
    let mut ids = [0u32; 4];
    unsafe { windows_sys::Win32::System::Console::GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) == 1 }
}

/// No command line: a small menu (choose a game / run the last one again), then the game runs
/// in this console. On an error the console stays open until Enter, so the message can be read.
#[cfg(windows)]
fn run_picked_game() -> i32 {
    let last_file = xcl::platform::data_root().join("last-game.txt");
    let last = std::fs::read_to_string(&last_file).ok().map(|s| PathBuf::from(s.trim())).filter(|p| p.exists());
    let target = {
        let _ui = Ui::enter();
        let picked = match menu(last.as_deref()) {
            Choice::Again => last.clone(),
            Choice::Cancel => None,
            Choice::Choose | Choice::NoMenu => pick_game_file(),
        };
        // MicrosoftGame.config stands for its folder: XCL picks the exe the config names
        picked.map(|file| match file.file_name() {
            Some(n) if n.eq_ignore_ascii_case("MicrosoftGame.config") => file.parent().map(PathBuf::from).unwrap_or(file),
            _ => file,
        })
    };
    let Some(target) = target else { return 0 };
    let _ = std::fs::create_dir_all(xcl::platform::data_root());
    let _ = std::fs::write(&last_file, target.to_string_lossy().as_bytes());
    println!("xcl run \"{}\"", target.display());
    let code = xcl::launcher::run_from_menu(&target);
    if code != 0 {
        println!("\nXCL ended with code {code}. Press Enter to close.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    code
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `name` of system DLL `dll`, looked up when the menu needs it: xcl.exe does not import the
/// dialog libraries (the game runs in this executable, and must not get them).
#[cfg(windows)]
unsafe fn system_proc(dll: &str, name: &str) -> Option<usize> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let lib = LoadLibraryW(wide(dll).as_ptr());
    if lib.is_null() {
        return None;
    }
    GetProcAddress(lib, format!("{name}\0").as_ptr()).map(|f| f as usize)
}

/// While the menu is open: COM for the file dialog, and the common controls 6 (the task
/// dialog, the current look) through an activation context of its own – not a manifest of
/// xcl.exe, which would apply to the game too.
#[cfg(windows)]
struct Ui {
    actctx: Option<(windows_sys::Win32::Foundation::HANDLE, usize)>,
}

#[cfg(windows)]
impl Ui {
    fn enter() -> Ui {
        use windows_sys::Win32::System::ApplicationInstallationAndServicing::{ActivateActCtx, CreateActCtxW, ACTCTXW};
        const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency><dependentAssembly>
    <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
  </dependentAssembly></dependency>
</assembly>
"#;
        unsafe {
            windows_sys::Win32::System::Com::CoInitializeEx(std::ptr::null(), windows_sys::Win32::System::Com::COINIT_APARTMENTTHREADED as u32);
        }
        let path = std::env::temp_dir().join("xcl-menu.manifest");
        let actctx = std::fs::write(&path, MANIFEST).ok().and_then(|()| unsafe {
            let source = wide(&path.to_string_lossy());
            let mut c: ACTCTXW = std::mem::zeroed();
            c.cbSize = std::mem::size_of::<ACTCTXW>() as u32;
            c.lpSource = source.as_ptr();
            let h = CreateActCtxW(&c);
            if h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                return None;
            }
            let mut cookie = 0usize;
            (ActivateActCtx(h, &mut cookie) != 0).then_some((h, cookie))
        });
        Ui { actctx }
    }
}

#[cfg(windows)]
impl Drop for Ui {
    fn drop(&mut self) {
        use windows_sys::Win32::System::ApplicationInstallationAndServicing::{DeactivateActCtx, ReleaseActCtx};
        unsafe {
            if let Some((h, cookie)) = self.actctx {
                DeactivateActCtx(0, cookie);
                ReleaseActCtx(h);
            }
            windows_sys::Win32::System::Com::CoUninitialize();
        }
    }
}

#[cfg(windows)]
enum Choice {
    Choose,
    Again,
    Cancel,
    /// no task dialog on this system: straight to the file dialog
    NoMenu,
}

/// The menu: a task dialog with "Choose a game…" and, after a first run, "Run again".
#[cfg(windows)]
fn menu(last: Option<&std::path::Path>) -> Choice {
    use windows_sys::Win32::UI::Controls::*;
    const CHOOSE: i32 = 100;
    const AGAIN: i32 = 101;
    type TaskDialogIndirect = unsafe extern "system" fn(*const TASKDIALOGCONFIG, *mut i32, *mut i32, *mut i32) -> i32;
    let Some(f) = (unsafe { system_proc("comctl32.dll", "TaskDialogIndirect") }) else { return Choice::NoMenu };
    let title = wide("XCL");
    let heading = wide("Run an Xbox Series game");
    let content = wide("XCL runs the game from its unpacked files. Choose the game's exe or its MicrosoftGame.config.");
    let again = last.map(|p| wide(&format!("Run again\n{}", p.display())));
    let choose = wide("Choose a game…\nThe game's exe or MicrosoftGame.config");
    let mut buttons = Vec::new();
    if let Some(a) = &again {
        buttons.push(TASKDIALOG_BUTTON { nButtonID: AGAIN, pszButtonText: a.as_ptr() });
    }
    buttons.push(TASKDIALOG_BUTTON { nButtonID: CHOOSE, pszButtonText: choose.as_ptr() });
    let mut c: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
    c.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
    c.hwndParent = unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() };
    c.dwFlags = TDF_USE_COMMAND_LINKS | TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW;
    c.dwCommonButtons = TDCBF_CANCEL_BUTTON;
    c.pszWindowTitle = title.as_ptr();
    c.pszMainInstruction = heading.as_ptr();
    c.pszContent = content.as_ptr();
    c.cButtons = buttons.len() as u32;
    c.pButtons = buttons.as_ptr();
    c.nDefaultButton = if again.is_some() { AGAIN } else { CHOOSE };
    let mut pressed = 0i32;
    let hr = unsafe { std::mem::transmute::<usize, TaskDialogIndirect>(f)(&c, &mut pressed, std::ptr::null_mut(), std::ptr::null_mut()) };
    match (hr, pressed) {
        (h, _) if h < 0 => Choice::NoMenu,
        (_, CHOOSE) => Choice::Choose,
        (_, AGAIN) => Choice::Again,
        _ => Choice::Cancel,
    }
}

/// The standard Windows open dialog (None: cancelled).
#[cfg(windows)]
fn pick_game_file() -> Option<PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    type GetOpenFileName = unsafe extern "system" fn(*mut OPENFILENAMEW) -> i32;
    let f = unsafe { system_proc("comdlg32.dll", "GetOpenFileNameW") }?;
    // pairs of (description, pattern), each ending in NUL, the list in one more
    let filter: Vec<u16> = "Xbox game (*.exe, MicrosoftGame.config)\0*.exe;MicrosoftGame.config\0All files\0*.*\0\0".encode_utf16().collect();
    let title = wide("XCL – choose a game");
    let mut file = vec![0u16; 32768];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() };
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_EXPLORER;
    if unsafe { std::mem::transmute::<usize, GetOpenFileName>(f)(&mut ofn) } == 0 {
        return None;
    }
    let n = file.iter().position(|&c| c == 0).unwrap_or(file.len());
    Some(PathBuf::from(String::from_utf16_lossy(&file[..n])))
}
