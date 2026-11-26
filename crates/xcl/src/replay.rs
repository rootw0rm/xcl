//! `xcl replay <capture folder>`: one draw or dispatch captured with `XCL_CAPTURE_DRAW`
//! (`hle::d3d12x::capture`) run alone on a device of its own.
//!
//! The console shaders are translated again with this build's translator, the memory windows,
//! textures, views and samplers are made again at the addresses and heap indices the capture
//! has, the call runs once, and the targets and UAV textures it wrote are read back as PNG and
//! raw files in `<capture>\replay_<name>\` with the time the GPU took (`gpu`). The GPU part
//! runs in a child process: a call that hangs the GPU ends the child, not the caller. `--out <name>`
//! is one plain name. Calls with a VS and PS, or a CS, are replayed: the capture also writes a
//! pipeline's GS (the rect-list stand-in), which replay refuses up front.
//!
//! To try a change without a build: `--sub from=>to||…` substitutes text in the translated
//! HLSL (as `XCL_DEBUG_PS_SUB` does in a run), `--profile` compiles a stage with another
//! profile, `--split` forces the wave64 split flag, and `--loops` counts the passes of every
//! `[loop]` of the pixel shader per lane.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::gpu::recomp::pso::{self, Stage, StaticSampler};
use crate::hle::d3d12x::capture::manifest::Manifest;

mod gpu;

struct Opts {
    subs: Vec<(String, String)>,
    profile: Option<String>,
    loops: bool,
    split: Option<bool>,
    timeout: Duration,
    repeat: u32,
    debug: bool,
    no_run: bool,
    child: bool,
    out: String,
}

/// Seconds over 0 that a `Duration` holds (no NaN, infinity or negative time).
fn seconds(v: &str) -> Option<Duration> {
    v.parse::<f64>().ok().filter(|x| x.is_finite() && *x > 0.0).and_then(|x| Duration::try_from_secs_f64(x).ok())
}

fn opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { subs: Vec::new(), profile: None, loops: false, split: None, timeout: Duration::from_secs(15), repeat: 1, debug: false, no_run: false, child: false, out: String::new() };
    let mut i = 0;
    let value = |i: &mut usize| -> Result<&String, String> {
        *i += 1;
        args.get(*i).ok_or_else(|| format!("{} needs a value", args[*i - 1]))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--sub" => {
                for s in value(&mut i)?.split("||") {
                    let (from, to) = s.split_once("=>").ok_or_else(|| format!("--sub: no => in {s:?}"))?;
                    if from.is_empty() {
                        return Err(format!("--sub: nothing to replace in {s:?}"));
                    }
                    o.subs.push((from.to_string(), to.to_string()));
                }
            }
            "--profile" => o.profile = Some(value(&mut i)?.clone()),
            "--loops" => o.loops = true,
            "--split" => {
                o.split = Some(match value(&mut i)?.as_str() {
                    "0" => false,
                    "1" => true,
                    v => return Err(format!("--split: 0 or 1, not {v:?}")),
                })
            }
            "--timeout" => o.timeout = seconds(value(&mut i)?).ok_or("--timeout: a number of seconds over 0")?,
            "--repeat" => o.repeat = value(&mut i)?.parse().ok().filter(|&n: &u32| n > 0).ok_or("--repeat: a count of 1 or more")?,
            "--debug" => o.debug = true,
            "--no-run" => o.no_run = true,
            // (the GPU part of a replay: `parent` starts it)
            "--child" => o.child = true,
            "--out" => o.out = value(&mut i)?.clone(),
            other => return Err(format!("unknown option {other}")),
        }
        i += 1;
    }
    if o.out.is_empty() {
        o.out = format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    }
    // (`replay_<name>` in the capture folder, and nowhere else)
    crate::hle::d3d12x::capture::manifest::plain_name("--out", &o.out)?;
    Ok(o)
}

/// The capture's stages are ones replay runs (VS, PS, CS).
fn replayable(m: &Manifest) -> Result<(), String> {
    match m.shaders.iter().find(|s| !matches!(s.stage.as_str(), "vs" | "ps" | "cs")) {
        Some(s) => Err(format!("the capture has a {} shader: xcl replay runs draws with a VS and PS, or dispatches", s.stage)),
        None => Ok(()),
    }
}

/// `xcl replay <dir> [options]`; the process's exit code (0: done, 1: failed, 2: usage, 3: the
/// call timed out, 4: the device was removed).
pub fn run(dir: &Path, args: &[String]) -> i32 {
    let o = match opts(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("xcl replay: {e} (see xcl --help)");
            return 2;
        }
    };
    if !o.child && !o.no_run {
        return parent(dir, args, &o);
    }
    match work(dir, &o) {
        Ok(code) => code,
        Err(e) => {
            println!("REPLAY RESULT: failed: {e}");
            1
        }
    }
}

/// The GPU part in a child process: its lines are passed on, and it is ended when the call it
/// submitted has not finished within the timeout.
fn parent(dir: &Path, args: &[String], o: &Opts) -> i32 {
    use std::io::BufRead;
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("xcl replay: {e}");
            return 1;
        }
    };
    // (the same output folder for the child)
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("replay").arg(dir).args(args).arg("--child").stdout(std::process::Stdio::piped());
    if !args.iter().any(|a| a == "--out") {
        cmd.arg("--out").arg(&o.out);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xcl replay: {e}");
            return 1;
        }
    };
    let out = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut submitted: Option<Instant> = None;
    let mut last_submit = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                if line.starts_with("REPLAY submitted") {
                    submitted = Some(Instant::now());
                    last_submit = Instant::now();
                } else if line.starts_with("REPLAY finished") || line.starts_with("REPLAY RESULT") {
                    submitted = None;
                }
                println!("{line}");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // (a little past the child's own timeout: it reports first when it still can)
                if let Some(t) = submitted.filter(|t| t.elapsed() > o.timeout + Duration::from_secs(2)) {
                    let _ = child.kill();
                    let _ = child.wait();
                    println!("REPLAY RESULT: timed out after {} ms (the child did not answer and was ended)", t.elapsed().as_millis());
                    return 3;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    match child.wait() {
        Ok(s) if s.success() => 0,
        Ok(s) => {
            let code = s.code().unwrap_or(-1);
            if !matches!(code, 1 | 3 | 4) {
                println!("REPLAY RESULT: the child ended with code {code:#x}, {} ms after its last submission", last_submit.elapsed().as_millis());
            }
            code
        }
        Err(e) => {
            eprintln!("xcl replay: {e}");
            1
        }
    }
}

/// A stage of the capture as DXIL.
struct Program {
    stage: Stage,
    name: String,
    dxil: Vec<u8>,
}

/// `[loop]` passes counted per lane: the HLSL with a counter in every loop and, where the pixel
/// shader returns, the counters added to the buffer at heap index `at` and the most passes per
/// pixel kept in the image at `at + 1`. The buffer's words: all passes (low, high), lanes, the
/// most in a lane; then for each of the first 16 loops, in the order of the text, its passes
/// and its most in a lane (from word 4); then for each of eight debug words a `--sub` may set
/// (`xcl_d0 = s3;` …) its most, its OR over the lanes and the lanes it is not 0 in (from word
/// 36). (hlsl, loops counted)
fn count_loops(hlsl: &str, at: u32) -> Result<(String, usize), String> {
    const LOOP: &str = "[loop] for (;;) {";
    const END: &str = "    return xo;\n}";
    const HEAD: &str = "static uint lane_id;";
    let n = hlsl.matches(LOOP).count();
    let end = hlsl.rfind(END).ok_or("--loops: the pixel shader's return was not found")?;
    if !hlsl.contains(HEAD) || !hlsl.contains("xcl_pos : SV_Position") {
        return Err("--loops: the prelude or SV_Position was not found".into());
    }
    let mut heads = String::from("static uint xcl_iters = 0u;\n");
    let mut tail = format!(
        "    {{ RWByteAddressBuffer xcl_lb = ResourceDescriptorHeap[{at}u]; uint xcl_lo; xcl_lb.InterlockedAdd(0u, xcl_iters, xcl_lo); if (xcl_lo + xcl_iters < xcl_lo) xcl_lb.InterlockedAdd(4u, 1u); xcl_lb.InterlockedAdd(8u, 1u); xcl_lb.InterlockedMax(12u, xcl_iters); RWTexture2D<uint> xcl_lt = ResourceDescriptorHeap[{}u]; InterlockedMax(xcl_lt[uint2(xi.xcl_pos.xy)], xcl_iters);",
        at + 1
    );
    for k in 0..n.min(16) {
        heads.push_str(&format!("static uint xcl_it{k} = 0u;\n"));
        tail.push_str(&format!(" xcl_lb.InterlockedAdd({}u, xcl_it{k}); xcl_lb.InterlockedMax({}u, xcl_it{k});", 16 + 8 * k, 20 + 8 * k));
    }
    for j in 0..8 {
        heads.push_str(&format!("static uint xcl_d{j} = 0u;\n"));
        tail.push_str(&format!(" xcl_lb.InterlockedMax({}u, xcl_d{j}); xcl_lb.InterlockedOr({}u, xcl_d{j}); if (xcl_d{j} != 0u) xcl_lb.InterlockedAdd({}u, 1u);", 144 + 12 * j, 148 + 12 * j, 152 + 12 * j));
    }
    tail.push_str(" }\n");
    let with_tail = format!("{}{tail}{}", &hlsl[..end], &hlsl[end..]).replacen(HEAD, &format!("{heads}{HEAD}"), 1);
    // (every head keeps the same first statement, so that a `--sub` of the loop heads finds them all)
    let mut parts = with_tail.split(LOOP);
    let mut out = parts.next().unwrap_or_default().to_string();
    for (k, part) in parts.enumerate() {
        out.push_str(LOOP);
        out.push_str(" xcl_iters++;");
        if k < 16 {
            out.push_str(&format!(" xcl_it{k}++;"));
        }
        out.push_str(part);
    }
    Ok((out, n))
}

/// Descriptors of the view heap: past every index the capture names, and two more.
fn heap_size(m: &Manifest) -> u32 {
    let views = m.views.iter().map(|v| v.index + 1).max().unwrap_or(0);
    let slots = m.slots.iter().filter(|s| s.kind == "image").map(|s| s.words[0] + 2).max().unwrap_or(0);
    views.max(slots).max(16) + 2
}

/// The capture's shaders translated and compiled; the static samplers of all of them.
fn translate(dir: &Path, out: &Path, m: &Manifest, o: &Opts) -> Result<(Vec<Program>, Vec<StaticSampler>), String> {
    let windows = (m.mem_windows & 0xFF).max(1);
    let split = o.split.unwrap_or(m.split_wave64);
    let mut sub_counts = vec![0usize; o.subs.len()];
    let mut profile_used = false;
    let mut programs = Vec::new();
    let mut samplers: Vec<StaticSampler> = Vec::new();
    for s in &m.shaders {
        let stage = match s.stage.as_str() {
            "vs" => Stage::Vertex,
            "ps" => Stage::Pixel,
            "cs" => Stage::Compute,
            other => return Err(format!("a console {other} stage: not replayed")),
        };
        let blob = std::fs::read(dir.join(&s.file)).map_err(|e| format!("{}: {e}", s.file))?;
        let t0 = Instant::now();
        let (t, profile) = pso::console_translation(&blob, stage, samplers.len() as u32, windows, split).map_err(|e| format!("{}: {e}", s.stage))?;
        let mut hlsl = t.hlsl;
        samplers.extend(t.static_samplers);
        // (the loops first: a substitution may rewrite their heads)
        if o.loops && stage == Stage::Pixel {
            let (h, n) = count_loops(&hlsl, heap_size(m) - 2)?;
            println!("REPLAY ps: --loops: {n} loop(s) counted");
            hlsl = h;
        } else if o.loops {
            println!("REPLAY {}: --loops is for pixel shaders only: not counted", s.stage);
        }
        for (k, (from, to)) in o.subs.iter().enumerate() {
            let n = hlsl.matches(from.as_str()).count();
            if n > 0 {
                println!("REPLAY {}: --sub {k}: {n} occurrence(s) of {from:?} replaced", s.stage);
                hlsl = hlsl.replace(from.as_str(), to);
                sub_counts[k] += n;
            }
        }
        let profile = match &o.profile {
            Some(p) if p.get(..2) == profile.get(..2) => {
                profile_used = true;
                p.as_str()
            }
            _ => profile,
        };
        std::fs::write(out.join(format!("{}.hlsl", s.stage)), &hlsl).map_err(|e| e.to_string())?;
        let t1 = Instant::now();
        let (dxil, cached) = crate::gpu::recomp::dxc::compile_cached(&hlsl, "main", profile).map_err(|e| format!("{} ({profile}): {e}", s.stage))?;
        println!(
            "REPLAY {} {}: translated in {} ms ({} lines, windows {windows}, split {split}), {profile} DXIL {} bytes in {} ms{}",
            s.stage,
            s.hash,
            (t1 - t0).as_millis(),
            hlsl.lines().count(),
            dxil.len(),
            t1.elapsed().as_millis(),
            if cached { " (cached)" } else { "" }
        );
        if hlsl.contains("SamplerDescriptorHeap") && m.samplers.is_empty() {
            println!("REPLAY {}: WARNING: samples through the sampler heap, and the capture has no samplers (default ones)", s.stage);
        }
        programs.push(Program { stage, name: s.stage.clone(), dxil });
    }
    for (k, n) in sub_counts.iter().enumerate() {
        if *n == 0 {
            println!("REPLAY WARNING: --sub {k}: {:?} was not found in any shader", o.subs[k].0);
        }
    }
    if let Some(p) = o.profile.as_ref().filter(|_| !profile_used) {
        println!("REPLAY WARNING: --profile {p}: no stage of the capture has that kind (its stages: {})", m.shaders.iter().map(|s| s.stage.as_str()).collect::<Vec<_>>().join(", "));
    }
    Ok((programs, samplers))
}

fn work(dir: &Path, o: &Opts) -> Result<i32, String> {
    let m = Manifest::read(dir)?;
    replayable(&m)?;
    let out = dir.join(format!("replay_{}", o.out));
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("REPLAY capture {} call #{}: {:?}; {} memory ranges, {} textures, {} views; output in {}", m.hash, m.call_number, m.call, m.memory.len(), m.textures.len(), m.views.len(), out.display());
    for l in &m.left_out {
        println!("REPLAY note: the capture left out: {l}");
    }
    let (programs, samplers) = translate(dir, &out, &m, o)?;
    if o.no_run {
        println!("REPLAY RESULT: translated and compiled (--no-run)");
        return Ok(0);
    }
    unsafe { gpu::run(dir, &out, &m, o, &programs, &samplers) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::xbox::tests::{container, dxil_part, fssm};
    use crate::hle::d3d12x::capture::manifest as c;

    fn code(parts: &[(&str, &str, &[(&str, u64)], Option<u32>)]) -> Vec<u8> {
        let mut v: Vec<u8> = parts.iter().flat_map(|(n, e, f, l)| crate::gpu::rdna2::encode(n, e, f, *l).unwrap_or_else(|x| panic!("{n}: {x}"))).collect();
        v.extend(crate::gpu::rdna2::encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
        v
    }

    /// An ISG1 / OSG1 part: (semantic, system value, register, mask) of each float element.
    fn signature(elements: &[(&str, u32, u32, u32)]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(elements.len() as u32).to_le_bytes());
        v.extend_from_slice(&8u32.to_le_bytes());
        let mut names = Vec::new();
        for (name, sv, register, mask) in elements {
            let at = (8 + 32 * elements.len() + names.len()) as u32;
            for w in [0u32, at, 0, *sv, 3, *register, mask | mask << 8, 0] {
                v.extend_from_slice(&w.to_le_bytes());
            }
            names.extend_from_slice(name.as_bytes());
            names.push(0);
        }
        v.extend(names);
        v
    }

    /// A console container of our own: `program` as the code of hardware stage `hw` (4 VS, 5 PS).
    fn shader(kind: u32, hw: u8, program: &[u8], inputs: &[(&str, u32, u32, u32)], outputs: &[(&str, u32, u32, u32)]) -> Vec<u8> {
        container(&[(b"SFI0", &[0; 8]), (b"ISG1", &signature(inputs)), (b"OSG1", &signature(outputs)), (b"HASH", &[0; 20]), (b"DXIL", &dxil_part(kind, &[fssm(program, &[(hw, 0)])]))])
    }

    const V: u64 = 256;

    /// A capture folder made here, around a VS and a PS of our own (no title's data): the VS
    /// passes POSITION on, the PS writes (s8 as a float, the word at the address in s[10:11], a
    /// count of 3 loop passes, 1) to target 0. A second target and a 3D UAV texture have
    /// contents the call does not touch; a third target is not in the capture.
    pub(super) fn synthetic_capture(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("xcl-replay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vs = code(&[
            ("V_MOV_B32", "ENC_VOP1", &[("VDST", 0), ("SRC0", V + 4)], None),
            ("V_MOV_B32", "ENC_VOP1", &[("VDST", 1), ("SRC0", V + 5)], None),
            ("V_MOV_B32", "ENC_VOP1", &[("VDST", 2), ("SRC0", 128)], None),
            ("V_MOV_B32", "ENC_VOP1", &[("VDST", 3), ("SRC0", 242)], None),
            ("EXP", "ENC_EXP", &[("TGT", 12), ("EN", 0xF), ("VSRC0", 0), ("VSRC1", 1), ("VSRC2", 2), ("VSRC3", 3), ("DONE", 1)], None),
        ]);
        let ps = code(&[
            ("S_LOAD_DWORD", "ENC_SMEM", &[("SDATA", 12), ("SBASE", 5), ("OFFSET", 0)], None),
            ("S_MOV_B32", "ENC_SOP1", &[("SDST", 9), ("SSRC0", 128)], None),
            // three passes
            ("S_ADD_U32", "ENC_SOP2", &[("SDST", 9), ("SSRC0", 9), ("SSRC1", 129)], None),
            ("S_CMP_LT_U32", "ENC_SOPC", &[("SSRC0", 9), ("SSRC1", 131)], None),
            ("S_CBRANCH_SCC1", "ENC_SOPP", &[("SIMM16", 0xFFFD)], None),
            ("V_MOV_B32", "ENC_VOP1", &[("VDST", 4), ("SRC0", 8)], None),
            ("V_MOV_B32", "ENC_VOP1", &[("VDST", 5), ("SRC0", 12)], None),
            ("V_CVT_F32_U32", "ENC_VOP1", &[("VDST", 6), ("SRC0", 9)], None),
            ("V_MOV_B32", "ENC_VOP1", &[("VDST", 7), ("SRC0", 242)], None),
            ("EXP", "ENC_EXP", &[("TGT", 0), ("EN", 0xF), ("VSRC0", 4), ("VSRC1", 5), ("VSRC2", 6), ("VSRC3", 7), ("DONE", 1), ("VM", 1)], None),
        ]);
        std::fs::write(dir.join("vs.bin"), shader(1, 4, &vs, &[("POSITION", 0, 0, 3)], &[("SV_Position", 1, 0, 0xF)])).unwrap();
        std::fs::write(dir.join("ps.bin"), shader(0, 5, &ps, &[("SV_Position", 1, 0, 0xF)], &[("SV_Target", 64, 0, 0xF)])).unwrap();
        // window 0: a triangle over the whole target at 0x1_0000; window 1 (a reserved buffer in
        // the replay): the word the PS reads, 1 GiB in
        let base = 0x40_0000_0000u64;
        let word_at = base + (1 << 32) + (1 << 30);
        let mut memory: Vec<u8> = [-1.0f32, -1.0, -1.0, 3.0, 3.0, -1.0].iter().flat_map(|f| f.to_le_bytes()).collect();
        memory.resize(0x1000, 0);
        memory.extend_from_slice(&0.25f32.to_le_bytes());
        memory.resize(0x2000, 0);
        std::fs::write(dir.join("memory.bin"), memory).unwrap();
        let mut io = vec![0u32; crate::gpu::recomp::KERNEL_SGPRS];
        io[crate::gpu::recomp::IO_UMA_BASE_HI] = (base >> 32) as u32;
        io[8] = 0.5f32.to_bits();
        (io[10], io[11]) = (word_at as u32, (word_at >> 32) as u32);
        // a target with contents (64 rows of 512 bytes) and a 3D texture (rows of 160 bytes, 256 apart)
        std::fs::write(dir.join("tex_1.bin"), vec![0x3Cu8; 64 * 512]).unwrap();
        std::fs::write(dir.join("tex_2.bin"), vec![1u8; (23 * 192 - 1) * 256 + 160]).unwrap();
        let tex = |id: usize, dimension: i32, width: u64, height: u32, depth: u16, format: i32, flags: i32, file: Option<&str>| c::Texture {
            id,
            dimension,
            width,
            height,
            depth_or_array: depth,
            mips: 1,
            format,
            samples: 1,
            flags,
            file: file.map(String::from),
            subresources: 1,
            ..Default::default()
        };
        // (a pipeline description as the capture keeps it: the structure's bytes; no pointer of it is read)
        use windows::Win32::Graphics::Direct3D12::*;
        let mut d = D3D12_GRAPHICS_PIPELINE_STATE_DESC { SampleMask: u32::MAX, NumRenderTargets: 3, PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE, ..Default::default() };
        d.SampleDesc.Count = 1;
        d.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
        d.RasterizerState.CullMode = D3D12_CULL_MODE_NONE;
        for k in 0..3 {
            d.RTVFormats[k] = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(10);
            d.BlendState.RenderTarget[k].RenderTargetWriteMask = 15;
        }
        let desc = c::hex(unsafe { std::slice::from_raw_parts(&d as *const _ as *const u8, std::mem::size_of_val(&d)) });
        let m = Manifest {
            version: c::VERSION,
            hash: "synthetic".into(),
            call_number: 1,
            call: Some(c::Call::Draw { vertices: 3, instances: 1, first_vertex: 0, first_instance: 0 }),
            graphics: true,
            shaders: vec![c::Shader { stage: "vs".into(), hash: String::new(), file: "vs.bin".into() }, c::Shader { stage: "ps".into(), hash: String::new(), file: "ps.bin".into() }],
            mem_windows: 4,
            split_wave64: true,
            pso: Some(c::Pso { desc, input: vec![c::InputElement { name: "POSITION".into(), index: 0, format: 16, slot: 0, offset: 0, class: 0, step: 0 }], rect_list_gs: false }),
            root: c::Root { io, ..Default::default() },
            uma: c::Uma { base, windows: (0..4).map(|k| (base + (k << 32), (4096u64 << 20) - 0x1_0000)).collect() },
            state: c::State { topology: Some(4), viewports: vec![[0.0, 0.0, 64.0, 64.0, 0.0, 1.0]], scissors: vec![[0, 0, 64, 64]], ..Default::default() },
            targets: vec![c::Target { handle: 1, texture: Some(0), desc: None }, c::Target { handle: 2, texture: Some(1), desc: None }, c::Target { handle: 3, texture: None, desc: None }],
            vertex_buffers: vec![c::VertexBuffer { slot: 0, address: base + 0x1_0000, size: 24, stride: 8 }],
            memory_file: "memory.bin".into(),
            memory: vec![
                c::Range { address: base + 0x1_0000, len: 0x1000, window: 0, offset: 0x1_0000, file_offset: 0, ..Default::default() },
                c::Range { address: word_at, len: 0x1000, window: 1, offset: 1 << 30, file_offset: 0x1000, ..Default::default() },
            ],
            textures: vec![tex(0, 3, 64, 64, 1, 10, 1, None), tex(1, 3, 64, 64, 1, 10, 1, Some("tex_1.bin")), tex(2, 4, 40, 23, 192, 42, 4, Some("tex_2.bin"))],
            views: vec![c::View { index: 20, uav: false, texture: 2, desc: None }, c::View { index: 21, uav: true, texture: 2, desc: None }],
            slots: vec![
                c::Slot { param: 0, offset: 0, words: [20, 0, 0, 0xA000_0000, 0, 0, 0, 0], kind: "image".into() },
                c::Slot { param: 0, offset: 32, words: [30, 0, 0, 0x9000_0000, 0, 0, 0, 0], kind: "image".into() },
            ],
            ..Default::default()
        };
        m.write(&dir).unwrap();
        dir
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// The 16 bytes of texel (x, y) of a 64 x 64 R16G16B16A16_FLOAT read-back as four floats.
    fn rgba16f(raw: &[u8], x: usize, y: usize) -> [f32; 4] {
        let at = 8 * (64 * y + x);
        let half = |k: usize| {
            let h = u16::from_le_bytes([raw[at + 2 * k], raw[at + 2 * k + 1]]) as u32;
            // (the values written here are powers of two between 0.25 and 4)
            f32::from_bits(((h >> 10) + 112) << 23 | (h & 0x3FF) << 13)
        };
        [half(0), half(1), half(2), half(3)]
    }

    /// What a replay needs: a device that takes XCL's root signature (Shader Model 6.6; on
    /// Windows 10 not when the inbox D3D12 was loaded first in the process) and DXC.
    fn gpu_ready() -> bool {
        let device = unsafe { gpu::device() }.is_ok_and(|(dev, _)| pso::abi_root_signature(&dev, true).is_ok());
        if !device || crate::gpu::recomp::dxc::location().is_err() {
            crate::gpu::recomp::runner::skip_test(if device { "no DXC" } else { "no GPU" });
            return false;
        }
        true
    }

    /// A capture made here replays on the GPU: memory in a committed and in a reserved window,
    /// a texture with contents, one without, a target the capture does not have, a 3D UAV
    /// texture at its heap index, a slot without a view. Every pixel of target 0 is what the PS
    /// made of its SGPR, of the word in memory and of its loop.
    #[test]
    fn a_capture_made_here_replays() {
        if !gpu_ready() {
            return;
        }
        let dir = synthetic_capture("run");
        assert_eq!(work(&dir, &opts(&args(&["--child", "--timeout", "10", "--out", "t"])).unwrap()), Ok(0));
        let out = dir.join("replay_t");
        for f in ["vs.hlsl", "ps.hlsl", "rt0.png", "rt1.png", "rt2.png", "uav_tex2.png"] {
            assert!(out.join(f).exists(), "{f}");
        }
        let rt0 = std::fs::read(out.join("rt0.bin")).unwrap();
        assert_eq!(rt0.len(), 64 * 64 * 8);
        for (x, y) in [(0, 0), (63, 0), (31, 31), (0, 63), (63, 63)] {
            assert_eq!(rgba16f(&rt0, x, y), [0.5, 0.25, 3.0, 1.0], "pixel ({x}, {y})");
        }
        // what the call does not write is as the capture had it
        assert_eq!(std::fs::read(out.join("rt1.bin")).unwrap(), vec![0x3Cu8; 64 * 64 * 8]);
        assert_eq!(std::fs::read(out.join("uav_tex2.bin")).unwrap(), vec![1u8; 40 * 23 * 192 * 4]);
        // --no-run: the shaders only
        assert_eq!(work(&dir, &opts(&args(&["--no-run", "--out", "n"])).unwrap()), Ok(0));
        assert!(dir.join("replay_n").join("ps.hlsl").exists() && !dir.join("replay_n").join("rt0.bin").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--loops` puts a counter in every loop and its write before the shader's last return.
    #[test]
    fn loops_are_counted_before_the_return() {
        let hlsl = "static uint lane_id;\nstruct XclIn { float4 xcl_pos : SV_Position; };\nXclOut main(XclIn xi)\n{\n    [loop] for (;;) { break; }\n    [loop] for (;;) { break; }\n    if (!xcl_live) discard;\n    return xo;\n}\n";
        let (out, n) = count_loops(hlsl, 40).unwrap();
        assert_eq!(n, 2);
        assert!(out.starts_with("static uint xcl_iters = 0u;\nstatic uint xcl_it0 = 0u;\nstatic uint xcl_it1 = 0u;\nstatic uint xcl_d0 = 0u;"));
        assert!(out.contains("[loop] for (;;) { xcl_iters++; xcl_it0++; break; }") && out.contains("[loop] for (;;) { xcl_iters++; xcl_it1++; break; }"));
        let write = out.find("ResourceDescriptorHeap[40u]").unwrap();
        assert!(write > out.rfind("xcl_iters++").unwrap() && write < out.rfind("    return xo;").unwrap());
        assert!(out.contains("ResourceDescriptorHeap[41u]") && out.contains("xcl_lb.InterlockedMax(20u, xcl_it0)") && out.contains("xcl_lb.InterlockedOr(148u, xcl_d0)"));
        assert!(count_loops("void main() {}", 0).is_err());
    }

    /// On the GPU: `--loops` finds the three passes of the capture's pixel shader in every
    /// pixel, and a `--sub` changes what the shader writes.
    #[test]
    fn a_replay_counts_loop_passes_and_substitutes() {
        if !gpu_ready() {
            return;
        }
        let dir = synthetic_capture("loops");
        assert_eq!(work(&dir, &opts(&args(&["--child", "--loops", "--sub", "uint s8 = io.Load(32u);=>uint s8 = 0x40000000u;||not there=>x", "--profile", "ps_6_7", "--out", "t"])).unwrap()), Ok(0));
        let out = dir.join("replay_t");
        let hlsl = std::fs::read_to_string(out.join("ps.hlsl")).unwrap();
        assert!(hlsl.contains("xcl_it0++") && hlsl.contains("uint s8 = 0x40000000u;"));
        // the most passes per pixel: 3 everywhere
        let loops = std::fs::read(out.join("loops.bin")).unwrap();
        assert_eq!(loops.len(), 64 * 64 * 4);
        assert!(loops.chunks_exact(4).all(|w| w == [3, 0, 0, 0]));
        let rt0 = std::fs::read(out.join("rt0.bin")).unwrap();
        assert_eq!(rgba16f(&rt0, 31, 31), [2.0, 0.25, 3.0, 1.0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn options_parse() {
        let o = opts(&args(&["--sub", "a=>b||c=>d", "--loops", "--profile", "ps_6_7", "--split", "0", "--timeout", "2.5", "--out", "x"])).unwrap();
        assert_eq!(o.subs, vec![("a".to_string(), "b".to_string()), ("c".to_string(), "d".to_string())]);
        assert!(o.loops && o.profile.as_deref() == Some("ps_6_7") && o.split == Some(false) && o.timeout == Duration::from_millis(2500) && o.out == "x");
        assert!(opts(&args(&["--sub", "nothing"])).is_err());
        assert!(opts(&args(&["--what"])).is_err());
        // --split is 0 or 1; a substitution replaces something
        assert_eq!(opts(&args(&["--split", "1"])).unwrap().split, Some(true));
        for bad in ["2", "yes", "", "01"] {
            assert!(opts(&args(&["--split", bad])).is_err(), "--split {bad:?}");
        }
        assert!(opts(&args(&["--sub", "=>x"])).err().unwrap().contains("nothing to replace"));
        assert!(opts(&args(&["--sub", "a=>b||=>x"])).is_err());
        assert_eq!(opts(&args(&["--sub", "a=>"])).unwrap().subs, vec![("a".to_string(), String::new())], "replacing with nothing is fine");
        // a timeout is a time over 0, a repeat count 1 or more: anything else is a usage error
        for bad in ["-1", "0", "NaN", "inf", "-inf", "1e300", "x"] {
            assert!(opts(&args(&["--timeout", bad])).is_err(), "--timeout {bad}");
        }
        assert_eq!(opts(&args(&["--timeout", "0.25"])).unwrap().timeout, Duration::from_millis(250));
        for bad in ["0", "-1", "x"] {
            assert!(opts(&args(&["--repeat", bad])).is_err(), "--repeat {bad}");
        }
        // the output stays in the capture folder
        for bad in ["x/../../outside", "..", "a\\b", "C:\\x", "/x"] {
            assert!(opts(&args(&["--out", bad])).is_err(), "{bad}");
        }
    }

    /// A capture with a geometry shader is refused before anything is translated or written.
    #[test]
    fn a_capture_with_a_gs_is_refused() {
        let dir = synthetic_capture("gs");
        let mut m = Manifest::read(&dir).unwrap();
        m.shaders.push(crate::hle::d3d12x::capture::manifest::Shader { stage: "gs".into(), hash: "0123456789abcdef".into(), file: "vs.bin".into() });
        m.write(&dir).unwrap();
        let e = work(&dir, &opts(&args(&["--out", "g"])).unwrap()).unwrap_err();
        assert!(e.contains("gs shader"), "{e}");
        assert!(!dir.join("replay_g").exists(), "nothing written");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
