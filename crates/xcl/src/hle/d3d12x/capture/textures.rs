//! The textures of a captured call: copied where the call stands, read back after its submit.
//!
//! A `CopyResource` into a texture of XCL's own is recorded on the title's list at the call:
//! it runs after the uploads of title memory the submit makes and after everything earlier in
//! the list, so the copy holds what the call would read. The copies are read back on a queue
//! of XCL's own once the title's queue is past the list (`read_back`).
//!
//! XCL does not track resource states: the state a texture is copied from is the last
//! transition recorded on the list, else its use as a target, else the last transition
//! recorded on any list, else a shader-resource state (`state_of`; the manifest names which).
//! The copy takes all subresources from that one state, so a texture whose subresources were
//! transitioned apart and are not in one known state is not copied (`copyable`).

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

use super::manifest::Manifest;
use super::resource_desc;

/// The copy of one texture of the manifest.
pub(super) struct Snap {
    texture: usize,
    copy: ID3D12Resource,
    desc: D3D12_RESOURCE_DESC,
    subresources: u32,
}

/// Copies of the manifest's textures (`by_res`: resource → its index there) recorded on the
/// list `this`, within `budget` bytes; what is not copied is noted in `left_out`.
pub(super) unsafe fn copy_all(this: *mut c_void, m: &mut Manifest, by_res: &HashMap<usize, usize>, budget: &mut u64) -> (Vec<Snap>, Option<ID3D12Device>) {
    let mut dev: Option<ID3D12Device> = None;
    if let Some(list) = ID3D12GraphicsCommandList::from_raw_borrowed(&this) {
        let _ = list.GetDevice(&mut dev);
    }
    let Some(d) = &dev else {
        m.left_out.push("no textures: the list has no device".into());
        return (Vec::new(), None);
    };
    // (in the manifest's order: what the budget leaves out is then the same from run to run)
    let mut all: Vec<(usize, usize)> = by_res.iter().map(|(&res, &id)| (id, res)).collect();
    all.sort_unstable();
    let mut snaps = Vec::new();
    for (id, res) in all {
        if let Err(e) = copyable(&m.textures[id]) {
            m.left_out.push(format!("texture {id} ({}, {:?}): {e}", m.textures[id].resource, m.textures[id].uses));
            continue;
        }
        match snapshot(this, d, res, m.textures[id].state, budget) {
            Ok((copy, desc, subresources, bytes)) => {
                m.textures[id].bytes = bytes;
                m.textures[id].subresources = subresources;
                snaps.push(Snap { texture: id, copy, desc, subresources });
            }
            Err(e) => m.left_out.push(format!("texture {id} ({}, {:?}): {e}", m.textures[id].resource, m.textures[id].uses)),
        }
    }
    (snaps, dev)
}

/// Whether the texture can be copied from one state of all its subresources (`snapshot`).
fn copyable(t: &super::manifest::Texture) -> Result<(), String> {
    match t.state_source.strip_prefix(super::PARTIAL_STATE) {
        Some(why) => Err(format!("not copied: its subresources are not in one known state ({})", why.trim())),
        None => Ok(()),
    }
}

/// subresources of a texture (planes included) and the bytes of all of them in a buffer
unsafe fn copy_layout(dev: &ID3D12Device, d: &D3D12_RESOURCE_DESC) -> (u32, u64) {
    let mut info = D3D12_FEATURE_DATA_FORMAT_INFO { Format: d.Format, PlaneCount: 1 };
    let _ = dev.CheckFeatureSupport(D3D12_FEATURE_FORMAT_INFO, &mut info as *mut _ as *mut c_void, std::mem::size_of_val(&info) as u32);
    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { d.DepthOrArraySize as u32 };
    let n = d.MipLevels as u32 * layers * info.PlaneCount.max(1) as u32;
    let mut total = 0u64;
    dev.GetCopyableFootprints(d, 0, n, 0, None, None, None, Some(&mut total));
    (n, total)
}

/// A texture of XCL's own like `res`, and on the list a copy of `res` into it.
unsafe fn snapshot(this: *mut c_void, dev: &ID3D12Device, res: usize, state: u32, budget: &mut u64) -> Result<(ID3D12Resource, D3D12_RESOURCE_DESC, u32, u64), String> {
    let mut d = resource_desc(res).ok_or("no description")?;
    if d.SampleDesc.Count > 1 {
        return Err("multisampled: not read back".into());
    }
    // (a reserved texture's layout and the title's other flags are not the copy's)
    d.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
    d.Alignment = 0;
    d.Flags = D3D12_RESOURCE_FLAGS(d.Flags.0 & (D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 | D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0));
    let (subresources, bytes) = copy_layout(dev, &d);
    if bytes == 0 || bytes == u64::MAX {
        return Err("no copyable layout".into());
    }
    if bytes > *budget {
        return Err(format!("{} MB: over what is left of XCL_CAPTURE_MAX_MB", bytes >> 20));
    }
    let mut copy: Option<ID3D12Resource> = None;
    super::super::title_textures::own_work(|| dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &d, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut copy))
        .map_err(|e| format!("CreateCommittedResource: {e}"))?;
    let copy = copy.ok_or("CreateCommittedResource")?;
    *budget -= bytes;
    let (before, source) = (D3D12_RESOURCE_STATES(state as i32), D3D12_RESOURCE_STATE_COPY_SOURCE);
    if before != source {
        super::super::list_x::transition(this, res, before, source);
    }
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) = std::mem::transmute(super::super::real::orig_fn(this, 17));
    f(this, copy.as_raw(), res as *mut c_void);
    if before != source {
        super::super::list_x::transition(this, res, source, before);
    }
    Ok((copy, d, subresources, bytes))
}

struct OwnQueue(usize, ID3D12CommandQueue);
unsafe impl Send for OwnQueue {}
static QUEUE: Mutex<Option<OwnQueue>> = Mutex::new(None);

/// The copies made on the list, read back on a queue of XCL's own once the title's queue is
/// past them.
pub(super) unsafe fn read_back(queue: *mut c_void, dir: &Path, m: &mut Manifest, snaps: &[Snap], dev: Option<&ID3D12Device>) -> Result<(), String> {
    if snaps.is_empty() {
        return Ok(());
    }
    let dev = dev.ok_or("no device")?;
    let e = |x: windows::core::Error| x.to_string();
    let own = {
        let mut g = QUEUE.lock().unwrap();
        match g.as_ref().filter(|q| q.0 == dev.as_raw() as usize) {
            Some(q) => q.1.clone(),
            None => {
                let q: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).map_err(e)?;
                *g = Some(OwnQueue(dev.as_raw() as usize, q.clone()));
                q
            }
        }
    };
    let after: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(e)?;
    ID3D12CommandQueue::from_raw_borrowed(&queue).ok_or("queue")?.Signal(&after, 1).map_err(e)?;
    own.Wait(&after, 1).map_err(e)?;
    let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(e)?;
    let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).map_err(e)?;
    let mut buffers = Vec::new();
    for s in snaps {
        let mut fps = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); s.subresources as usize];
        let mut total = 0u64;
        dev.GetCopyableFootprints(&s.desc, 0, s.subresources, 0, Some(fps.as_mut_ptr()), None, None, Some(&mut total));
        let mut rb: Option<ID3D12Resource> = None;
        dev.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: total,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut rb,
        )
        .map_err(e)?;
        let rb = rb.ok_or("no readback buffer")?;
        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::transmute_copy(&s.copy),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                    StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                }),
            },
        }]);
        for (k, fp) in fps.iter().enumerate() {
            let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&rb), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *fp } };
            let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&s.copy), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: k as u32 } };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }
        buffers.push((s.texture, rb, total));
    }
    list.Close().map_err(e)?;
    own.ExecuteCommandLists(&[Some(list.cast().map_err(e)?)]);
    let done: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(e)?;
    own.Signal(&done, 1).map_err(e)?;
    crate::gpu::fence::wait(&done, 1, dev, None, 30_000).map_err(|x| x.to_string())?;
    for (id, rb, total) in buffers {
        let mut at: *mut c_void = std::ptr::null_mut();
        rb.Map(0, None, Some(&mut at)).map_err(e)?;
        let file = format!("tex_{id}.bin");
        let wrote = std::fs::write(dir.join(&file), std::slice::from_raw_parts(at as *const u8, total as usize));
        rb.Unmap(0, None);
        match wrote {
            Ok(()) => m.textures[id].file = Some(file),
            Err(x) => m.left_out.push(format!("texture {id}: {x}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

    /// A texture whose subresources were transitioned apart is left out, naming why; one in a
    /// known state (or an assumed one) is copied.
    #[test]
    fn a_texture_in_no_one_state_is_not_copied() {
        let t = |source: &str| super::super::manifest::Texture { state: 0x80, state_source: source.into(), ..Default::default() };
        assert!(copyable(&t("last transition on this list")).is_ok());
        assert!(copyable(&t("assumed (no transition seen)")).is_ok());
        let e = copyable(&t(&format!("{} subresources in different states (0x80, 0x4) (last transition on this list)", super::super::PARTIAL_STATE))).unwrap_err();
        assert!(e.contains("not copied") && e.contains("different states"), "{e}");
    }

    /// What a texture's file holds: every mip of every slice, and both planes of a depth-stencil
    /// format; a 3D texture's slices are in its mips.
    #[test]
    fn a_copy_holds_every_subresource_and_plane() {
        let Ok(dev) = crate::gpu::agility::create_device(None, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0) else {
            return crate::gpu::recomp::runner::skip_test("no GPU");
        };
        let desc = |dimension, depth_or_array, mips, format| D3D12_RESOURCE_DESC {
            Dimension: dimension,
            Width: 64,
            Height: 32,
            DepthOrArraySize: depth_or_array,
            MipLevels: mips,
            Format: DXGI_FORMAT(format),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        unsafe {
            // R8G8B8A8_UNORM, 2 slices of 3 mips: 64 x 32, 32 x 16, 16 x 8 texels of 4 bytes at least
            let (n, bytes) = copy_layout(&dev, &desc(D3D12_RESOURCE_DIMENSION_TEXTURE2D, 2, 3, 28));
            assert_eq!(n, 6);
            assert!(bytes >= 2 * 4 * (64 * 32 + 32 * 16 + 16 * 8), "{bytes}");
            // R32_UINT 3D, 8 slices: one subresource
            let (n, bytes) = copy_layout(&dev, &desc(D3D12_RESOURCE_DIMENSION_TEXTURE3D, 8, 1, 42));
            assert_eq!(n, 1);
            assert!(bytes >= 8 * 4 * 64 * 32, "{bytes}");
            // D32_FLOAT_S8X24_UINT: depth and stencil
            assert_eq!(copy_layout(&dev, &desc(D3D12_RESOURCE_DIMENSION_TEXTURE2D, 1, 1, 20)).0, 2);
        }
    }
}
