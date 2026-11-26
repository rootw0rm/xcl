//! What a capture of one draw or dispatch is on disk: a folder with `manifest.json` (this
//! module's `Manifest`) and, beside it, the files it names – the console shader containers,
//! `memory.bin` (the ranges of unified memory the call can read, one after the other) and a
//! `tex_<id>.bin` per texture (all its subresources in the layout of `GetCopyableFootprints`
//! from offset 0). Descriptions D3D12 gives as plain structures (the pipeline, views,
//! samplers) are kept as their bytes in hex: a capture is read by the build that wrote it, or
//! one with the same `windows` crate structures.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The manifest's file in a capture folder.
pub const FILE: &str = "manifest.json";
/// `Manifest::version` this build writes and reads.
pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(tag = "kind")]
pub enum Call {
    Draw { vertices: u32, instances: u32, first_vertex: u32, first_instance: u32 },
    DrawIndexed { indices: u32, instances: u32, first_index: u32, base_vertex: i32, first_instance: u32 },
    Dispatch { x: u32, y: u32, z: u32 },
    /// ExecuteIndirect: its arguments are not captured
    Indirect { max: u32 },
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Shader {
    /// "vs", "ps", "gs", "cs"
    pub stage: String,
    pub hash: String,
    pub file: String,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct InputElement {
    pub name: String,
    pub index: u32,
    pub format: i32,
    pub slot: u32,
    pub offset: u32,
    pub class: i32,
    pub step: u32,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct Pso {
    /// D3D12_GRAPHICS_PIPELINE_STATE_DESC as the PC pipeline was made with (formats and topology
    /// type the PC's), bytes in hex; its pointers mean nothing
    pub desc: String,
    pub input: Vec<InputElement>,
    /// the pipeline got XCL's rect list GS
    pub rect_list_gs: bool,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct RootParam {
    /// "constants", "descriptor", "table"
    pub kind: String,
    pub at: u32,
    pub count: u32,
    /// table: its 32-bit console address; descriptor: the 64-bit address
    pub value: u64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Root {
    pub params: Vec<RootParam>,
    /// the packed root arguments, hex
    pub args: String,
    /// bytes of them passed in user SGPRs
    pub inline: u32,
    /// the `io` buffer (u0): the SGPRs the program starts with
    pub io: Vec<u32>,
    /// where the rest of the arguments was put (unified memory)
    pub table_at: u64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Uma {
    pub base: u64,
    /// (base, size) of each shader window
    pub windows: Vec<(u64, u64)>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    pub topology: Option<u32>,
    /// x, y, width, height, min depth, max depth
    pub viewports: Vec<[f32; 6]>,
    /// left, top, right, bottom
    pub scissors: Vec<[i32; 4]>,
    pub blend_factor: Option<[f32; 4]>,
    pub stencil_ref: Option<u32>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Target {
    pub handle: u64,
    /// index in `textures` (None: XCL does not know the view's resource)
    pub texture: Option<usize>,
    /// the PC view description, hex (None: the default view)
    pub desc: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct VertexBuffer {
    pub slot: u32,
    /// unified-memory address of what the PC view names (0: not in unified memory)
    pub address: u64,
    pub size: u32,
    pub stride: u32,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct IndexBuffer {
    pub address: u64,
    pub size: u32,
    pub format: i32,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Range {
    pub address: u64,
    pub len: u64,
    pub window: u32,
    /// offset in the window
    pub offset: u64,
    /// offset in `memory_file`
    pub file_offset: u64,
    pub what: Vec<String>,
    /// bytes the CPU could not read (written as zeros)
    pub unreadable: u64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Texture {
    pub id: usize,
    pub resource: String,
    pub dimension: i32,
    pub width: u64,
    pub height: u32,
    pub depth_or_array: u16,
    pub mips: u16,
    pub format: i32,
    pub samples: u32,
    pub flags: i32,
    pub title_address: Option<u64>,
    /// the D3D12_RESOURCE_STATES the copy took the texture to be in, and why
    pub state: u32,
    pub state_source: String,
    pub uses: Vec<String>,
    /// all subresources in the layout of GetCopyableFootprints (base offset 0); None: left out
    pub file: Option<String>,
    pub bytes: u64,
    pub subresources: u32,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct View {
    /// index in the shader-visible heap (an SRV at a T#'s word 0, its UAV at + 1)
    pub index: u32,
    pub uav: bool,
    pub texture: usize,
    pub desc: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Sampler {
    /// index in the sampler heap (an S#'s word 3 [11:0])
    pub index: u32,
    /// D3D12_SAMPLER_DESC, hex
    pub desc: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Slot {
    pub param: u32,
    /// byte offset in the table
    pub offset: u32,
    pub words: [u32; 8],
    /// "image" (T#), "buffer" (V#), "other"
    pub kind: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Manifest {
    pub version: u32,
    pub hash: String,
    pub call_number: u32,
    pub call: Option<Call>,
    pub graphics: bool,
    pub shaders: Vec<Shader>,
    /// `pso::mem_windows()` and the split flag the shaders were translated with
    pub mem_windows: u32,
    pub split_wave64: bool,
    pub pso: Option<Pso>,
    pub root: Root,
    pub uma: Uma,
    pub state: State,
    pub targets: Vec<Target>,
    pub depth: Option<Target>,
    pub vertex_buffers: Vec<VertexBuffer>,
    pub index_buffer: Option<IndexBuffer>,
    pub memory_file: String,
    pub memory: Vec<Range>,
    pub textures: Vec<Texture>,
    pub views: Vec<View>,
    pub slots: Vec<Slot>,
    /// every sampler the title has made in a console sampler heap, by its index there (the
    /// heaps are not told apart: the last one made at an index)
    #[serde(default)]
    pub samplers: Vec<Sampler>,
    pub left_out: Vec<String>,
}

pub fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter().fold(String::with_capacity(2 * b.len()), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

/// The bytes `hex` wrote; an odd number of digits or anything but hex digits is an error.
pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("{} hex digits (an odd number)", s.len()));
    }
    s.as_bytes()
        .chunks_exact(2)
        .enumerate()
        .map(|(i, d)| std::str::from_utf8(d).ok().and_then(|d| u8::from_str_radix(d, 16).ok()).filter(|_| d.iter().all(u8::is_ascii_hexdigit)).ok_or_else(|| format!("not hex at digit {}", 2 * i)))
        .collect()
}

/// `name` is one plain file name (no folder, no `..`, no drive): what a capture names lies in
/// its own folder.
pub fn plain_name(what: &str, name: &str) -> Result<(), String> {
    use std::path::Component;
    let mut c = Path::new(name).components();
    match (c.next(), c.next()) {
        (Some(Component::Normal(n)), None) if n == std::ffi::OsStr::new(name) => Ok(()),
        _ => Err(format!("{what}: {name:?} is not a plain file name")),
    }
}

/// `s` holds `bytes` bytes in hex (`bytes`: None, any number).
fn check_hex(what: &str, s: &str, bytes: Option<usize>) -> Result<(), String> {
    let b = unhex(s).map_err(|e| format!("{what}: {e}"))?;
    match bytes {
        Some(n) if b.len() != n => Err(format!("{what}: {} bytes, this build's structure has {n}", b.len())),
        _ => Ok(()),
    }
}

impl Manifest {
    /// `manifest.json` in `dir` (the folder is made).
    pub fn write(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(FILE), text).map_err(|e| format!("{}: {e}", dir.join(FILE).display()))
    }

    /// The manifest of the capture in `dir`; one of another version is refused.
    pub fn read(dir: &Path) -> Result<Manifest, String> {
        let path = dir.join(FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let m: Manifest = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if m.version != VERSION {
            return Err(format!("{}: version {} (this build reads {VERSION})", path.display(), m.version));
        }
        m.check().map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(m)
    }

    /// Every field in hex is hex, and those of a D3D12 structure have its size in this build: a
    /// damaged or foreign capture is refused rather than read as another pipeline or view.
    fn check(&self) -> Result<(), String> {
        use std::mem::size_of;
        use windows::Win32::Graphics::Direct3D12::*;
        for s in &self.shaders {
            plain_name(&format!("{} shader file", s.stage), &s.file)?;
        }
        if !self.memory_file.is_empty() {
            plain_name("memory file", &self.memory_file)?;
        }
        for t in &self.textures {
            if let Some(f) = &t.file {
                plain_name(&format!("texture {} file", t.id), f)?;
            }
        }
        if let Some(p) = &self.pso {
            // (the capture keeps the description of a graphics pipeline only)
            check_hex("pipeline description", &p.desc, self.graphics.then(size_of::<D3D12_GRAPHICS_PIPELINE_STATE_DESC>))?;
        }
        check_hex("root arguments", &self.root.args, None)?;
        if self.root.args.len() % 8 != 0 {
            return Err(format!("root arguments: {} hex digits, not whole dwords", self.root.args.len()));
        }
        for (i, t) in self.targets.iter().enumerate() {
            if let Some(d) = &t.desc {
                check_hex(&format!("render target {i} view"), d, Some(size_of::<D3D12_RENDER_TARGET_VIEW_DESC>()))?;
            }
        }
        if let Some(d) = self.depth.as_ref().and_then(|t| t.desc.as_ref()) {
            check_hex("depth view", d, Some(size_of::<D3D12_DEPTH_STENCIL_VIEW_DESC>()))?;
        }
        for v in &self.views {
            if let Some(d) = &v.desc {
                let n = if v.uav { size_of::<D3D12_UNORDERED_ACCESS_VIEW_DESC>() } else { size_of::<D3D12_SHADER_RESOURCE_VIEW_DESC>() };
                check_hex(&format!("view {}", v.index), d, Some(n))?;
            }
        }
        for x in &self.samplers {
            check_hex(&format!("sampler {}", x.index), &x.desc, Some(size_of::<D3D12_SAMPLER_DESC>()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let b: Vec<u8> = (0..=255).collect();
        assert_eq!(unhex(&hex(&b)).unwrap(), b);
        assert_eq!(hex(&[0x0f, 0xa0]), "0fa0");
        assert_eq!(unhex("0FA0").unwrap(), vec![0x0f, 0xa0]);
        // an odd tail and what is not hex are errors, not dropped
        assert!(unhex("0fa").unwrap_err().contains("odd"));
        assert!(unhex("zz10").unwrap_err().contains("digit 0"));
        assert!(unhex("10zz").unwrap_err().contains("digit 2"));
        assert!(unhex("+1").is_err(), "a sign is not a hex digit");
        assert!(unhex("é1").is_err());
    }

    fn sample() -> Manifest {
        use std::mem::size_of;
        use windows::Win32::Graphics::Direct3D12::*;
        Manifest {
            version: VERSION,
            hash: "0123456789abcdef".into(),
            call_number: 3,
            call: Some(Call::DrawIndexed { indices: 72, instances: 6000, first_index: 3, base_vertex: -2, first_instance: 1 }),
            graphics: true,
            shaders: vec![Shader { stage: "ps".into(), hash: "0123456789abcdef".into(), file: "ps.bin".into() }],
            mem_windows: 4,
            pso: Some(Pso { desc: hex(&vec![1; size_of::<D3D12_GRAPHICS_PIPELINE_STATE_DESC>()]), input: vec![InputElement { name: "POSITION".into(), format: 2, ..Default::default() }], rect_list_gs: false }),
            root: Root { params: vec![RootParam { kind: "table".into(), at: 8, count: 1, value: 0x41be_0400 }], args: hex(&[0; 16]), inline: 8, io: vec![7; 128], table_at: 0x40_0000_1000 },
            uma: Uma { base: 0x40_0000_0000, windows: vec![(0x40_0000_0000, 0xFFFF_0000)] },
            state: State { topology: Some(4), viewports: vec![[0.0, 0.0, 240.0, 136.0, 0.0, 1.0]], scissors: vec![[0, 0, 240, 136]], blend_factor: Some([1.0; 4]), stencil_ref: None },
            targets: vec![Target { handle: 0x1000, texture: Some(0), desc: None }],
            memory_file: "memory.bin".into(),
            memory: vec![Range { address: 0x41_0000_1000, len: 64, window: 1, offset: 0x1000, file_offset: 0, what: vec!["p0 descriptor table".into()], unreadable: 0 }],
            textures: vec![Texture { id: 0, dimension: 3, width: 240, height: 136, depth_or_array: 1, mips: 1, format: 10, samples: 1, file: Some("tex_0.bin".into()), subresources: 1, ..Default::default() }],
            views: vec![View { index: 174598, uav: false, texture: 0, desc: Some(hex(&vec![9; size_of::<D3D12_SHADER_RESOURCE_VIEW_DESC>()])) }],
            slots: vec![Slot { param: 0, offset: 0xd20, words: [174598, 2, 3, 0xA000_0000, 5, 6, 7, 8], kind: "image".into() }],
            samplers: vec![Sampler { index: 5, desc: hex(&vec![4; size_of::<D3D12_SAMPLER_DESC>()]) }],
            left_out: vec!["texture 9: multisampled".into()],
            ..Default::default()
        }
    }

    /// A manifest written to a folder reads back as it was.
    #[test]
    fn a_manifest_reads_back_as_written() {
        let dir = std::env::temp_dir().join(format!("xcl-capture-manifest-{}", std::process::id()));
        let m = sample();
        m.write(&dir).unwrap();
        let back = Manifest::read(&dir).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), serde_json::to_string(&m).unwrap());
        assert!(matches!(back.call, Some(Call::DrawIndexed { indices: 72, instances: 6000, first_index: 3, base_vertex: -2, first_instance: 1 })));
        assert_eq!(back.slots[0].words[3], 0xA000_0000);
        assert_eq!(back.memory[0].address, 0x41_0000_1000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manifest of another version, or none, is an error that names the file.
    #[test]
    fn another_version_is_refused() {
        let dir = std::env::temp_dir().join(format!("xcl-capture-version-{}", std::process::id()));
        assert!(Manifest::read(&dir).unwrap_err().contains(FILE));
        Manifest { version: VERSION + 1, ..sample() }.write(&dir).unwrap();
        assert!(Manifest::read(&dir).unwrap_err().contains("version"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A damaged capture is refused, naming what is wrong: a field that is not hex, an odd
    /// number of digits, or a D3D12 structure of another size than this build's.
    #[test]
    fn a_damaged_manifest_is_refused() {
        let dir = std::env::temp_dir().join(format!("xcl-capture-damaged-{}", std::process::id()));
        let refused = |m: Manifest| {
            m.write(&dir).unwrap();
            Manifest::read(&dir).unwrap_err()
        };
        let mut m = sample();
        m.views[0].desc = Some("0fa".into());
        assert!(refused(m).contains("view 174598"));
        let mut m = sample();
        m.views[0].uav = true;
        m.views[0].desc = Some(hex(&[9; 3]));
        assert!(refused(m).contains("view 174598: 3 bytes"));
        let mut m = sample();
        m.pso.as_mut().unwrap().desc = hex(&[1, 2, 3]);
        assert!(refused(m).contains("pipeline description: 3 bytes"));
        let mut m = sample();
        m.samplers[0].desc = "zz".repeat(52);
        assert!(refused(m).contains("sampler 5"));
        let mut m = sample();
        m.root.args = hex(&[0; 3]);
        assert!(refused(m).contains("root arguments"));
        // the files it names lie in its own folder
        let mut m = sample();
        m.shaders[0].file = "../ps.bin".into();
        assert!(refused(m).contains("ps shader file"));
        let mut m = sample();
        m.textures[0].file = Some("C:\\x\\tex_0.bin".into());
        assert!(refused(m).contains("texture 0 file"));
        let mut m = sample();
        m.memory_file = "sub/memory.bin".into();
        assert!(refused(m).contains("memory file"));
        // as written, it reads
        sample().write(&dir).unwrap();
        assert!(Manifest::read(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_names_only() {
        for ok in ["ps.bin", "tex_12.bin", "memory.bin", "a..b"] {
            assert!(plain_name("f", ok).is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "C:x", "C:\\x", "\\x", "/x"] {
            assert!(plain_name("f", bad).is_err(), "{bad}");
        }
    }

    /// A manifest without the fields a later build added still reads (`samplers`).
    #[test]
    fn a_manifest_without_samplers_reads() {
        let mut v: serde_json::Value = serde_json::to_value(sample()).unwrap();
        v.as_object_mut().unwrap().remove("samplers");
        let m: Manifest = serde_json::from_value(v).unwrap();
        assert!(m.samplers.is_empty() && m.views.len() == 1);
    }
}
