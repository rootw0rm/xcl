//! Linux Vulkan compute runner (Phase D steps 15-16).
//!
//! `ash` compute dispatch for caller-supplied SPIR-V compute words: enumerate the
//! instance/physical-device/compute-queue, build a compute pipeline from the words,
//! dispatch one group of 64 work-items (the trivial kernel's `local_size_x = 64`) and
//! read the single storage buffer back.
//!
//! Subgroup width (step 16): the device's `subgroupSize` selects the compute lane
//! path, mirroring the recompiler's 64-vs-split-32 decision (`upper_lanes`,
//! `x64_masks`, `mesh_wave`, `runner::split_wave64` on Windows). `WORK_ITEMS = 64`
//! shaping is kept for both paths: one group of 64 is one wave64 natively and two
//! subgroups of 32 on the split path, both valid Vulkan workgroup sizing.
//!
//! `ash` 0.38 is `MIT OR Apache-2.0` (used under the MIT grant, like `libc`) and loads
//! `libvulkan.so.1` at runtime via `libloading` (`Entry::load`), so the static exe ships
//! no `.so` and links no Vulkan code. No window, no graphics pipeline (the VS Vertex
//! words from steps 13/14 need one), no D3D12.X routing change, no `TRANSLATOR_SOURCES`
//! change (no disk cache). Kernel-mode recompiler SPIR-V emission stays a follow-up.

use std::ffi::{CStr, CString};

/// Work-items per dispatch: the trivial kernel is `local_size_x = 64`, one group.
pub const WORK_ITEMS: usize = 64;

/// A GPU test (or part of one) that cannot run here: `SKIPPED <test>: <reason>` written
/// to stderr itself, mirroring `runner::skip_test` on Windows (`XCL_REQUIRE_GPU`
/// fast-fail preserved). Reasons here are `"no Vulkan"` / `"no device"`.
pub fn skip_test(reason: &str) {
    use std::io::Write;
    let thread = std::thread::current();
    let test = thread.name().unwrap_or("?");
    let required = std::env::var("XCL_REQUIRE_GPU").unwrap_or_default();
    let fails = required.split(',').map(str::trim).any(|r| match r {
        "wave64" => reason != "no mesh shaders",
        "mesh" => reason != "no wave64",
        _ => false,
    });
    if fails {
        panic!("{test}: {reason}, but XCL_REQUIRE_GPU={required}");
    }
    let _ = writeln!(std::io::stderr(), "SKIPPED {test}: {reason}");
}

/// Compute lane path selected from the device `subgroupSize`, mirroring the
/// recompiler's 64-vs-split-32 decision (`runner::split_wave64` on Windows:
/// split means the adapter runs 32-lane waves; `upper_lanes` shadow analysis,
/// `x64_masks` exchange liveness, `mesh_wave` requirements).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanePath {
    /// 64-lane waves run natively (device `subgroupSize == 64`).
    Native64,
    /// 32-lane path: a 64-wide wave is two subgroups of 32 (device
    /// `subgroupSize != 64`, including 32 and unknown/zero).
    Split32,
}

/// Select the compute lane path for a `subgroupSize` (pure, device-independent
/// so tests force both widths even where only one device exists).
pub fn lane_path_for(subgroup_size: u32) -> LanePath {
    if subgroup_size == 64 {
        LanePath::Native64
    } else {
        LanePath::Split32
    }
}

/// Whether a `subgroupSize` needs the split-32 path (`lane_path_for(...) == Split32`).
pub fn requires_split(subgroup_size: u32) -> bool {
    lane_path_for(subgroup_size) == LanePath::Split32
}

/// Name of the first compute-capable physical device (None text on error).
/// Returns `(physical device, queue family index, name, subgroupSize)`.
fn pick_device(
    instance: &ash::Instance,
) -> Result<(ash::vk::PhysicalDevice, u32, String, u32), String> {
    let phys = unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| format!("no device: enumerate_physical_devices: {e}"))?;
    if phys.is_empty() {
        return Err("no device: zero physical devices".into());
    }
    for &p in &phys {
        let props = unsafe { instance.get_physical_device_properties(p) };
        let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let queues = unsafe { instance.get_physical_device_queue_family_properties(p) };
        let Some(qfi) = queues
            .iter()
            .position(|q| q.queue_flags.contains(ash::vk::QueueFlags::COMPUTE))
        else {
            continue;
        };
        // Step-16 lane decision: core `subgroupSize` plus the
        // `VK_EXT_subgroup_size_control` min/max window where the driver
        // reports it (zeroes when the extension is absent — selection still
        // comes from `subgroupSize` alone).
        let mut subgroup = ash::vk::PhysicalDeviceSubgroupProperties::default();
        let mut size_control = ash::vk::PhysicalDeviceSubgroupSizeControlProperties::default();
        let mut props2 = ash::vk::PhysicalDeviceProperties2::default()
            .push_next(&mut subgroup)
            .push_next(&mut size_control);
        unsafe {
            instance.get_physical_device_properties2(p, &mut props2);
        }
        let lane = lane_path_for(subgroup.subgroup_size);
        let lane_text = match lane {
            LanePath::Native64 => "native-64",
            LanePath::Split32 => "split-32",
        };
        if size_control.min_subgroup_size != 0 || size_control.max_subgroup_size != 0 {
            eprintln!(
                "vulkan: {name} subgroupSize {} (min {} max {}) -> {lane_text}",
                subgroup.subgroup_size,
                size_control.min_subgroup_size,
                size_control.max_subgroup_size
            );
        } else {
            eprintln!(
                "vulkan: {name} subgroupSize {} -> {lane_text}",
                subgroup.subgroup_size
            );
        }
        return Ok((p, qfi as u32, name, subgroup.subgroup_size));
    }
    Err("no device: no compute queue family".into())
}

/// `subgroupSize` of the first compute-capable physical device.
pub fn device_subgroup_size() -> Result<u32, String> {
    unsafe {
        let entry = ash::Entry::load().map_err(|e| format!("no Vulkan: Entry::load: {e}"))?;
        let app = CString::new("xcl").unwrap();
        let app_info = ash::vk::ApplicationInfo::default()
            .application_name(&app)
            .api_version(ash::vk::make_api_version(0, 1, 3, 0));
        let create = ash::vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance = entry
            .create_instance(&create, None)
            .map_err(|e| format!("no Vulkan: create_instance: {e}"))?;
        let (_, _, _, subgroup) = pick_device(&instance).map_err(|e| {
            instance.destroy_instance(None);
            e
        })?;
        instance.destroy_instance(None);
        Ok(subgroup)
    }
}

/// Lane path the current device selects (`device_subgroup_size` mapped through
/// `lane_path_for`).
pub fn device_lane_path() -> Result<LanePath, String> {
    device_subgroup_size().map(lane_path_for)
}

fn find_memory_type(
    props: &ash::vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: ash::vk::MemoryPropertyFlags,
) -> Option<u32> {
    for i in 0..props.memory_type_count {
        let t = props.memory_types[i as usize];
        if bits & (1 << i) != 0 && t.property_flags.contains(want) {
            return Some(i);
        }
    }
    None
}

/// Dispatch caller-supplied SPIR-V compute words against `data` and return the buffer.
///
/// Contract: `spirv` starts with the SPIR-V magic and holds a `GLCompute` entry `main`
/// with `local_size_x = 64`; `data` holds exactly [`WORK_ITEMS`] dwords bound as the
/// single `set 0 / binding 0` storage buffer (the trivial kernel reads/writes it in
/// place). One group is dispatched; the fence wait is 5 s. Errors are `String`s, never
/// panics; missing Vulkan / device errors start with `"no "` so tests can skip.
pub fn dispatch_compute(spirv: &[u32], data: &[u32]) -> Result<Vec<u32>, String> {
    if spirv.len() < 5 || spirv[0] != 0x0723_0203 {
        return Err("bad SPIR-V: missing magic".into());
    }
    if data.len() != WORK_ITEMS {
        return Err(format!(
            "bad input: need {} dwords, got {}",
            WORK_ITEMS,
            data.len()
        ));
    }
    unsafe {
        let entry = ash::Entry::load().map_err(|e| format!("no Vulkan: Entry::load: {e}"))?;
        let app = CString::new("xcl").unwrap();
        let app_info = ash::vk::ApplicationInfo::default()
            .application_name(&app)
            .api_version(ash::vk::make_api_version(0, 1, 3, 0));
        let create = ash::vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance = entry
            .create_instance(&create, None)
            .map_err(|e| format!("no Vulkan: create_instance: {e}"))?;
        let (phys, qfi, _name, subgroup) = match pick_device(&instance) {
            Ok(v) => v,
            Err(e) => {
                instance.destroy_instance(None);
                return Err(e);
            }
        };
        // Step-16 selection: the 64-work-item shape is kept for both paths —
        // one wave64 natively, two subgroups of 32 on the split path.
        // The lane decision is consumed here (not dead state): both paths
        // dispatch one group of 64, and the step-17a backend selector owns
        // the device-level `device_lane_path()` agreement check.
        let lane = lane_path_for(subgroup);
        eprintln!("vulkan: dispatch {WORK_ITEMS} work-items as {lane:?}");
        // Scoped teardown for the fallible steps below: every created handle
        // is owned by `Scope` and destroyed on drop, so an early `?` return
        // cannot leak the device or a half-built pipeline (step-15 follow-up).
        struct Scope<'a> {
            device: &'a ash::Device,
            buf: Option<ash::vk::Buffer>,
            mem: Option<ash::vk::DeviceMemory>,
            set_layout: Option<ash::vk::DescriptorSetLayout>,
            pipe_layout: Option<ash::vk::PipelineLayout>,
            pool: Option<ash::vk::DescriptorPool>,
            module: Option<ash::vk::ShaderModule>,
            pipe: Option<ash::vk::Pipeline>,
            cmd_pool: Option<ash::vk::CommandPool>,
            fence: Option<ash::vk::Fence>,
        }
        impl Drop for Scope<'_> {
            fn drop(&mut self) {
                let d = self.device;
                unsafe {
                    if let Some(f) = self.fence.take() {
                        d.destroy_fence(f, None);
                    }
                    if let Some(p) = self.cmd_pool.take() {
                        d.destroy_command_pool(p, None);
                    }
                    if let Some(p) = self.pipe.take() {
                        d.destroy_pipeline(p, None);
                    }
                    if let Some(m) = self.module.take() {
                        d.destroy_shader_module(m, None);
                    }
                    if let Some(p) = self.pool.take() {
                        d.destroy_descriptor_pool(p, None);
                    }
                    if let Some(p) = self.pipe_layout.take() {
                        d.destroy_pipeline_layout(p, None);
                    }
                    if let Some(s) = self.set_layout.take() {
                        d.destroy_descriptor_set_layout(s, None);
                    }
                    if let Some(b) = self.buf.take() {
                        d.destroy_buffer(b, None);
                    }
                    if let Some(m) = self.mem.take() {
                        d.free_memory(m, None);
                    }
                }
            }
        }
        let run = || -> Result<Vec<u32>, String> {
            let prio = [1.0f32];
            let queue_info = ash::vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qfi)
                .queue_priorities(&prio);
            let dev_info = ash::vk::DeviceCreateInfo::default()
                .queue_create_infos(std::slice::from_ref(&queue_info));
            let device = instance
                .create_device(phys, &dev_info, None)
                .map_err(|e| format!("create_device: {e}"))?;
            let queue = device.get_device_queue(qfi, 0);
            // Fallible body with scoped teardown: `scope` owns every handle
            // and destroys it on drop, so `?` never leaks a half-built pipe.
            // `device` itself is destroyed after the scope drops.
            let out = (|| -> Result<Vec<u32>, String> {
                let mut scope = Scope {
                    device: &device,
                    buf: None,
                    mem: None,
                    set_layout: None,
                    pipe_layout: None,
                    pool: None,
                    module: None,
                    pipe: None,
                    cmd_pool: None,
                    fence: None,
                };
                let d = scope.device;
                let size = (data.len() * 4) as u64;
                // Storage buffer, host-visible + coherent so the fence wait publishes writes.
                let buf_info = ash::vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(ash::vk::BufferUsageFlags::STORAGE_BUFFER)
                    .sharing_mode(ash::vk::SharingMode::EXCLUSIVE);
                let buf = d
                    .create_buffer(&buf_info, None)
                    .map_err(|e| format!("create_buffer: {e}"))?;
                scope.buf = Some(buf);
                let req = d.get_buffer_memory_requirements(buf);
                let mem_props = instance.get_physical_device_memory_properties(phys);
                let want = ash::vk::MemoryPropertyFlags::HOST_VISIBLE
                    | ash::vk::MemoryPropertyFlags::HOST_COHERENT;
                let mem_idx = find_memory_type(&mem_props, req.memory_type_bits, want)
                    .ok_or_else(|| "no host-visible coherent memory type".to_string())?;
                let alloc = ash::vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(mem_idx);
                let mem = d
                    .allocate_memory(&alloc, None)
                    .map_err(|e| format!("allocate_memory: {e}"))?;
                scope.mem = Some(mem);
                d.bind_buffer_memory(buf, mem, 0)
                    .map_err(|e| format!("bind_buffer_memory: {e}"))?;
                let dst = d
                    .map_memory(mem, 0, size, ash::vk::MemoryMapFlags::empty())
                    .map_err(|e| format!("map_memory: {e}"))? as *mut u32;
                std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
                d.unmap_memory(mem);
                // Descriptor: set 0 binding 0 = the storage buffer.
                let binding = ash::vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(ash::vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(ash::vk::ShaderStageFlags::COMPUTE);
                let set_layout_info = ash::vk::DescriptorSetLayoutCreateInfo::default()
                    .bindings(std::slice::from_ref(&binding));
                let set_layout = d
                    .create_descriptor_set_layout(&set_layout_info, None)
                    .map_err(|e| format!("create_descriptor_set_layout: {e}"))?;
                scope.set_layout = Some(set_layout);
                let layouts = [set_layout];
                let pipe_layout_info =
                    ash::vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts);
                let pipe_layout = d
                    .create_pipeline_layout(&pipe_layout_info, None)
                    .map_err(|e| format!("create_pipeline_layout: {e}"))?;
                scope.pipe_layout = Some(pipe_layout);
                let pool_size = ash::vk::DescriptorPoolSize {
                    ty: ash::vk::DescriptorType::STORAGE_BUFFER,
                    descriptor_count: 1,
                };
                let pool_info = ash::vk::DescriptorPoolCreateInfo::default()
                    .pool_sizes(std::slice::from_ref(&pool_size))
                    .max_sets(1);
                let pool = d
                    .create_descriptor_pool(&pool_info, None)
                    .map_err(|e| format!("create_descriptor_pool: {e}"))?;
                scope.pool = Some(pool);
                let alloc_info = ash::vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts);
                let sets = d
                    .allocate_descriptor_sets(&alloc_info)
                    .map_err(|e| format!("allocate_descriptor_sets: {e}"))?;
                let buf_write_info = ash::vk::DescriptorBufferInfo::default()
                    .buffer(buf)
                    .offset(0)
                    .range(size);
                let write = ash::vk::WriteDescriptorSet::default()
                    .dst_set(sets[0])
                    .dst_binding(0)
                    .descriptor_type(ash::vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(&buf_write_info));
                d.update_descriptor_sets(std::slice::from_ref(&write), &[]);
                // Compute pipeline from the caller-supplied words.
                let shader_info = ash::vk::ShaderModuleCreateInfo::default().code(spirv);
                let module = d
                    .create_shader_module(&shader_info, None)
                    .map_err(|e| format!("create_shader_module: {e}"))?;
                scope.module = Some(module);
                let entry_name = CString::new("main").unwrap();
                let stage = ash::vk::PipelineShaderStageCreateInfo::default()
                    .stage(ash::vk::ShaderStageFlags::COMPUTE)
                    .module(module)
                    .name(&entry_name);
                let pipe_info = ash::vk::ComputePipelineCreateInfo::default()
                    .stage(stage)
                    .layout(pipe_layout);
                let pipe = d
                    .create_compute_pipelines(ash::vk::PipelineCache::null(), &[pipe_info], None)
                    .map_err(|e| format!("create_compute_pipelines: {e:?}"))?[0];
                scope.pipe = Some(pipe);
                // One group of 64 work-items for both lane paths (native-64: one
                // wave; split-32: two subgroups of 32).
                let cmd_pool_info = ash::vk::CommandPoolCreateInfo::default()
                    .queue_family_index(qfi)
                    .flags(ash::vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
                let cmd_pool = d
                    .create_command_pool(&cmd_pool_info, None)
                    .map_err(|e| format!("create_command_pool: {e}"))?;
                scope.cmd_pool = Some(cmd_pool);
                let cmd_alloc = ash::vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(ash::vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1);
                let cmds = d
                    .allocate_command_buffers(&cmd_alloc)
                    .map_err(|e| format!("allocate_command_buffers: {e}"))?;
                let begin = ash::vk::CommandBufferBeginInfo::default()
                    .flags(ash::vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
                d.begin_command_buffer(cmds[0], &begin)
                    .map_err(|e| format!("begin_command_buffer: {e}"))?;
                d.cmd_bind_pipeline(cmds[0], ash::vk::PipelineBindPoint::COMPUTE, pipe);
                d.cmd_bind_descriptor_sets(
                    cmds[0],
                    ash::vk::PipelineBindPoint::COMPUTE,
                    pipe_layout,
                    0,
                    &sets,
                    &[],
                );
                d.cmd_dispatch(cmds[0], 1, 1, 1);
                d.end_command_buffer(cmds[0])
                    .map_err(|e| format!("end_command_buffer: {e}"))?;
                let fence_info = ash::vk::FenceCreateInfo::default();
                let fence = d
                    .create_fence(&fence_info, None)
                    .map_err(|e| format!("create_fence: {e}"))?;
                scope.fence = Some(fence);
                let submit =
                    ash::vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmds[0]));
                d.queue_submit(queue, &[submit], fence)
                    .map_err(|e| format!("queue_submit: {e}"))?;
                let waited = d.wait_for_fences(&[fence], true, 5_000_000_000);
                if waited.is_ok() {
                    let src = d
                        .map_memory(mem, 0, size, ash::vk::MemoryMapFlags::empty())
                        .map_err(|e| format!("map_memory(readback): {e}"))?
                        as *const u32;
                    let mut v = vec![0u32; data.len()];
                    std::ptr::copy_nonoverlapping(src, v.as_mut_ptr(), v.len());
                    d.unmap_memory(mem);
                    Ok(v)
                } else {
                    Err(format!("wait_for_fences: {waited:?}"))
                }
                // `scope` drops here and destroys every handle above; `device`
                // is destroyed by the outer closure below.
            })();
            device.destroy_device(None);
            out
        };
        let out = run();
        instance.destroy_instance(None);
        out
    }
}

/// Adapter name of the first compute-capable device (error text starts with `"no "`).
pub fn adapter_name() -> Result<String, String> {
    unsafe {
        let entry = ash::Entry::load().map_err(|e| format!("no Vulkan: Entry::load: {e}"))?;
        let app = CString::new("xcl").unwrap();
        let app_info = ash::vk::ApplicationInfo::default()
            .application_name(&app)
            .api_version(ash::vk::make_api_version(0, 1, 3, 0));
        let create = ash::vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance = entry
            .create_instance(&create, None)
            .map_err(|e| format!("no Vulkan: create_instance: {e}"))?;
        let (_, _, name, _) = pick_device(&instance).map_err(|e| {
            instance.destroy_instance(None);
            e
        })?;
        instance.destroy_instance(None);
        Ok(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Embedded trivial kernel (`trivial.comp`, `local_size_x = 64`):
    /// `buf.data[i] = buf.data[i] * 2 + 1`, SPIR-V 1.6 via `glslc --target-env=vulkan1.3`.
    const TRIVIAL_SPV: &[u32] = &[
        0x07230203, 0x00010600, 0x000d000b, 0x0000001f, 0x00000000, 0x00020011, 0x00000001,
        0x0006000b, 0x00000001, 0x4c534c47, 0x6474732e, 0x3035342e, 0x00000000, 0x0003000e,
        0x00000000, 0x00000001, 0x0007000f, 0x00000005, 0x00000004, 0x6e69616d, 0x00000000,
        0x0000000b, 0x00000013, 0x00060010, 0x00000004, 0x00000011, 0x00000040, 0x00000001,
        0x00000001, 0x00040047, 0x0000000b, 0x0000000b, 0x0000001c, 0x00040047, 0x00000010,
        0x00000006, 0x00000004, 0x00030047, 0x00000011, 0x00000002, 0x00050048, 0x00000011,
        0x00000000, 0x00000023, 0x00000000, 0x00040047, 0x00000013, 0x00000021, 0x00000000,
        0x00040047, 0x00000013, 0x00000022, 0x00000000, 0x00020013, 0x00000002, 0x00030021,
        0x00000003, 0x00000002, 0x00040015, 0x00000006, 0x00000020, 0x00000000, 0x00040017,
        0x00000009, 0x00000006, 0x00000003, 0x00040020, 0x0000000a, 0x00000001, 0x00000009,
        0x0004003b, 0x0000000a, 0x0000000b, 0x00000001, 0x0004002b, 0x00000006, 0x0000000c,
        0x00000000, 0x00040020, 0x0000000d, 0x00000001, 0x00000006, 0x0003001d, 0x00000010,
        0x00000006, 0x0003001e, 0x00000011, 0x00000010, 0x00040020, 0x00000012, 0x0000000c,
        0x00000011, 0x0004003b, 0x00000012, 0x00000013, 0x0000000c, 0x00040015, 0x00000014,
        0x00000020, 0x00000001, 0x0004002b, 0x00000014, 0x00000015, 0x00000000, 0x00040020,
        0x00000018, 0x0000000c, 0x00000006, 0x0004002b, 0x00000006, 0x0000001b, 0x00000002,
        0x0004002b, 0x00000006, 0x0000001d, 0x00000001, 0x00050036, 0x00000002, 0x00000004,
        0x00000000, 0x00000003, 0x000200f8, 0x00000005, 0x00050041, 0x0000000d, 0x0000000e,
        0x0000000b, 0x0000000c, 0x0004003d, 0x00000006, 0x0000000f, 0x0000000e, 0x00060041,
        0x00000018, 0x00000019, 0x00000013, 0x00000015, 0x0000000f, 0x0004003d, 0x00000006,
        0x0000001a, 0x00000019, 0x00050084, 0x00000006, 0x0000001c, 0x0000001a, 0x0000001b,
        0x00050080, 0x00000006, 0x0000001e, 0x0000001c, 0x0000001d, 0x0003003e, 0x00000019,
        0x0000001e, 0x000100fd, 0x00010038,
    ];

    #[test]
    fn embedded_trivial_words_are_spirv() {
        assert!(TRIVIAL_SPV.len() >= 5);
        assert_eq!(TRIVIAL_SPV[0], 0x0723_0203, "bad magic");
        assert_eq!(TRIVIAL_SPV[1] >> 16, 1, "major version must be 1");
        // Entry `main` with `LocalSize 64 1 1` (the 64 work-item pin).
        let text = TRIVIAL_SPV
            .iter()
            .skip(5)
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<u8>>();
        assert!(text.windows(4).any(|w| w == b"main"), "entry main missing");
    }

    #[test]
    fn lane_path_selects_both_widths_without_device() {
        // Deterministic step-16 gate: both lane paths are forced here even
        // though this host reports only one `subgroupSize` (32). Mirrors
        // `runner::split_wave64` (split == 32-lane path).
        assert_eq!(lane_path_for(64), LanePath::Native64);
        assert_eq!(lane_path_for(32), LanePath::Split32);
        // Unknown/zero never assumes 64 lanes — fail closed to split-32.
        assert_eq!(lane_path_for(0), LanePath::Split32);
        assert_eq!(lane_path_for(16), LanePath::Split32);
        assert!(!requires_split(64));
        assert!(requires_split(32));
        assert!(requires_split(0));
    }

    #[test]
    fn device_lane_path_matches_subgroup_size() {
        // On-device agreement: the queried `subgroupSize` maps through the
        // same pure selection above (skips cleanly with no Vulkan/device).
        match device_subgroup_size() {
            Ok(size) => {
                assert!(size != 0, "device reported subgroupSize 0");
                let via_device = device_lane_path().expect("lane path must follow size");
                assert_eq!(via_device, lane_path_for(size));
            }
            Err(e) if e.starts_with("no ") => {
                skip_test("no GPU");
            }
            Err(e) => panic!("device_subgroup_size failed: {e}"),
        }
    }

    #[test]
    fn trivial_copy_add_runs_on_device() {
        // Caller-supplied-words gate: the embedded kernel is only the test's caller.
        let input: Vec<u32> = (0..WORK_ITEMS as u32).collect();
        let expected: Vec<u32> = input.iter().map(|v| v * 2 + 1).collect();
        match dispatch_compute(TRIVIAL_SPV, &input) {
            Ok(out) => assert_eq!(out, expected, "trivial kernel mismatch"),
            Err(e) if e.starts_with("no ") => {
                skip_test("no GPU");
                return;
            }
            Err(e) => panic!("dispatch_compute failed: {e}"),
        }
    }

    #[test]
    fn bad_words_and_len_fail_closed() {
        let input: Vec<u32> = vec![0u32; WORK_ITEMS];
        assert!(dispatch_compute(&[0x0723_0203, 0x00010000], &input).is_err());
        assert!(dispatch_compute(&[1, 2, 3, 4, 5], &input).is_err());
        let short: Vec<u32> = vec![0u32; 4];
        match dispatch_compute(TRIVIAL_SPV, &short) {
            Ok(_) => panic!("short input must fail"),
            Err(e) if e.starts_with("no ") => {
                // No device: the length check still fires first for short inputs,
                // but a device error here also skips cleanly.
                skip_test("no GPU");
            }
            Err(_) => {}
        }
    }
}
