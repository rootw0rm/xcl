//! Linux headless Vulkan graphics smoke (Phase D step 17b).
//!
//! Offscreen execution of the same blit-class vertex shader steps 13/14 cover:
//! the caller supplies `translate_vs_spirv` Vertex words plus one `vec4`
//! `POSITION` per vertex, and [`draw_offscreen`] renders them into a 64×64
//! `R8G8B8A8_UNORM` image with no window, no swapchain and no vertex-index
//! trickery, then reads the pixels back.
//!
//! A small vertex buffer feeds `POSITION` (location 0) with a fullscreen
//! triangle instead of deriving corners from `VertexIndex`: the translated
//! blit VS is a fetch-shader passthrough (`pos = (in.x, in.y, 0, 1)`), so
//! unbound inputs would make coverage driver-dependent and the readback
//! assertion flaky. The buffer keeps the position-output/coverage check the
//! GLSL gate asserts fully deterministic. The fragment shader is a
//! hand-assembled location-0 passthrough, so covered pixels prove the
//! VS→FS param linkage too.
//!
//! `ash` 0.38 is `MIT OR Apache-2.0` (used under the MIT grant, like `libc`)
//! and loads `libvulkan.so.1` at runtime via `libloading`
//! (`Entry::load`), so the static exe ships no `.so` and links no Vulkan
//! code. Errors starting with `"no "` mean no Vulkan/device (tests
//! [`skip`](super::vulkan_runner::skip_test) cleanly, mirroring
//! `runner::skip_test`); anything else is fail-closed. The shipped path
//! spawns nothing: the fragment words are hand-assembled, the vertex words
//! come from the in-process `translate_vs_spirv`.

use std::ffi::CString;

use super::vulkan_runner::{find_memory_type, pick_graphics_device, skip_test};

/// Offscreen image extent (one 64×64 tile is enough for a coverage check).
pub const IMAGE_W: u32 = 64;
/// Offscreen image extent (one 64×64 tile is enough for a coverage check).
pub const IMAGE_H: u32 = 64;

/// Clear color: opaque magenta, distinct from anything the blit passthrough
/// can emit (its `z = 0` and clamped `x/y >= -1` never produce `(1, 0, 1)`).
pub const CLEAR: [f32; 4] = [1.0, 0.0, 1.0, 1.0];

/// Fullscreen-triangle `POSITION`s (`(x, y, 0, 1)` in NDC covers `[-1, 1]²`).
pub const FULLSCREEN_TRIANGLE: [[f32; 4]; 3] = [
    [-1.0, -1.0, 0.0, 1.0],
    [3.0, -1.0, 0.0, 1.0],
    [-1.0, 3.0, 0.0, 1.0],
];

/// Rendered pixels: tight RGBA8 rows, top row first.
#[derive(Debug, Clone)]
pub struct OffscreenPixels {
    /// Image width in pixels ([`IMAGE_W`]).
    pub width: u32,
    /// Image height in pixels ([`IMAGE_H`]).
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

// SPIR-V opcode / enum literals (per `spirv-headers`, unified1).
const OP_NAME: u16 = 5;
const OP_DECORATE: u16 = 71;
const OP_CAPABILITY: u16 = 17;
const OP_MEMORY_MODEL: u16 = 14;
const OP_ENTRY_POINT: u16 = 15;
const OP_EXECUTION_MODE: u16 = 16;
const OP_TYPE_VOID: u16 = 19;
const OP_TYPE_FLOAT: u16 = 22;
const OP_TYPE_VECTOR: u16 = 23;
const OP_TYPE_POINTER: u16 = 32;
const OP_TYPE_FUNCTION: u16 = 33;
const OP_VARIABLE: u16 = 59;
const OP_FUNCTION: u16 = 54;
const OP_LABEL: u16 = 248;
const OP_LOAD: u16 = 61;
const OP_STORE: u16 = 62;
const OP_RETURN: u16 = 253;
const OP_FUNCTION_END: u16 = 56;

const EXEC_MODEL_FRAGMENT: u32 = 4;
const EXEC_MODE_ORIGIN_UPPER_LEFT: u32 = 7;
const ADDR_LOGICAL: u32 = 0;
const MEM_GLSL450: u32 = 1;
const CAP_SHADER: u32 = 1;
const STORAGE_INPUT: u32 = 1;
const STORAGE_OUTPUT: u32 = 3;
const DECOR_LOCATION: u32 = 30;

fn inst(out: &mut Vec<u32>, opcode: u16, operands: &[u32]) {
    out.push(((operands.len() as u32 + 1) << 16) | opcode as u32);
    out.extend_from_slice(operands);
}

fn string_words(s: &str) -> Vec<u32> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Hand-assembled trivial fragment shader words: `layout(location = 0) in vec4`
/// straight to `layout(location = 0) out vec4` (entry `main`, SPIR-V 1.0).
/// No compiler, no process spawn on any path.
pub fn trivial_fs_spv() -> Vec<u32> {
    let void_ty = 1u32;
    let float_ty = 2u32;
    let v4float_ty = 3u32;
    let ptr_in = 4u32;
    let ptr_out = 5u32;
    let func_ty = 6u32;
    let func = 7u32;
    let in_c = 8u32;
    let out_c = 9u32;
    let label = 10u32;
    let loaded = 11u32;
    let bound = 12u32;
    let mut out = vec![0x0723_0203, 0x0001_0000, 0x584C_0002, bound, 0];
    inst(&mut out, OP_CAPABILITY, &[CAP_SHADER]);
    inst(&mut out, OP_MEMORY_MODEL, &[ADDR_LOGICAL, MEM_GLSL450]);
    let mut ep = vec![EXEC_MODEL_FRAGMENT, func];
    ep.extend(string_words("main"));
    ep.extend([in_c, out_c]);
    inst(&mut out, OP_ENTRY_POINT, &ep);
    inst(
        &mut out,
        OP_EXECUTION_MODE,
        &[func, EXEC_MODE_ORIGIN_UPPER_LEFT],
    );
    let mut name_in = vec![in_c];
    name_in.extend(string_words("in_c"));
    inst(&mut out, OP_NAME, &name_in);
    inst(&mut out, OP_DECORATE, &[in_c, DECOR_LOCATION, 0]);
    inst(&mut out, OP_DECORATE, &[out_c, DECOR_LOCATION, 0]);
    inst(&mut out, OP_TYPE_VOID, &[void_ty]);
    inst(&mut out, OP_TYPE_FLOAT, &[float_ty, 32]);
    inst(&mut out, OP_TYPE_VECTOR, &[v4float_ty, float_ty, 4]);
    inst(
        &mut out,
        OP_TYPE_POINTER,
        &[ptr_in, STORAGE_INPUT, v4float_ty],
    );
    inst(
        &mut out,
        OP_TYPE_POINTER,
        &[ptr_out, STORAGE_OUTPUT, v4float_ty],
    );
    inst(&mut out, OP_TYPE_FUNCTION, &[func_ty, void_ty]);
    inst(&mut out, OP_VARIABLE, &[ptr_in, in_c, STORAGE_INPUT]);
    inst(&mut out, OP_VARIABLE, &[ptr_out, out_c, STORAGE_OUTPUT]);
    inst(&mut out, OP_FUNCTION, &[void_ty, func, 0, func_ty]);
    inst(&mut out, OP_LABEL, &[label]);
    inst(&mut out, OP_LOAD, &[v4float_ty, loaded, in_c]);
    inst(&mut out, OP_STORE, &[out_c, loaded]);
    inst(&mut out, OP_RETURN, &[]);
    inst(&mut out, OP_FUNCTION_END, &[]);
    out
}

/// Draw `vs_words` (SPIR-V 1.0 Vertex words with entry `main`, e.g. the
/// `translate_vs_spirv` blit output) offscreen: one render pass over a
/// 64×64 color image cleared to [`CLEAR`], `positions` bound as location-0
/// `vec4` vertex input, trivial fragment passthrough, then readback.
///
/// Contract: `vs_words` starts with the SPIR-V magic and declares a `Vertex`
/// entry `main` with one `Input` `vec4` at location 0; `positions` holds
/// 1..=1024 vertices. One draw; the fence wait is 5 s. Errors are `String`s,
/// never panics; missing Vulkan / device errors start with `"no "`.
pub fn draw_offscreen(vs_words: &[u32], positions: &[[f32; 4]]) -> Result<OffscreenPixels, String> {
    if vs_words.len() < 5 || vs_words[0] != 0x0723_0203 {
        return Err("bad SPIR-V: missing magic".into());
    }
    if positions.is_empty() || positions.len() > 1024 {
        return Err(format!(
            "bad input: need 1..=1024 vertices, got {}",
            positions.len()
        ));
    }
    let fs_words = trivial_fs_spv();
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
        // One instance for the whole draw: `pick_graphics_device` reuses the
        // runner's shared query, never a second throwaway instance.
        let (phys, qfi, name, _subgroup) = match pick_graphics_device(&instance) {
            Ok(v) => v,
            Err(e) => {
                instance.destroy_instance(None);
                return Err(e);
            }
        };
        eprintln!("vulkan: headless graphics on {name}");
        let fmt = ash::vk::Format::R8G8B8A8_UNORM;
        let fmt_props = instance.get_physical_device_format_properties(phys, fmt);
        let need = ash::vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | ash::vk::FormatFeatureFlags::TRANSFER_SRC;
        if !fmt_props.optimal_tiling_features.contains(need) {
            instance.destroy_instance(None);
            return Err("no device: R8G8B8A8_UNORM lacks color-attachment+transfer-src".into());
        }
        let run = || -> Result<OffscreenPixels, String> {
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
            // Scoped teardown: every created handle is owned by `Scope` and
            // destroyed on drop, so an early `?` return cannot leak (same
            // pattern as the compute runner).
            struct Scope<'a> {
                device: &'a ash::Device,
                vbuf: Option<ash::vk::Buffer>,
                vbuf_mem: Option<ash::vk::DeviceMemory>,
                img: Option<ash::vk::Image>,
                img_mem: Option<ash::vk::DeviceMemory>,
                view: Option<ash::vk::ImageView>,
                rp: Option<ash::vk::RenderPass>,
                fbo: Option<ash::vk::Framebuffer>,
                layout: Option<ash::vk::PipelineLayout>,
                vs_mod: Option<ash::vk::ShaderModule>,
                fs_mod: Option<ash::vk::ShaderModule>,
                pipe: Option<ash::vk::Pipeline>,
                pool: Option<ash::vk::CommandPool>,
                rbuf: Option<ash::vk::Buffer>,
                rbuf_mem: Option<ash::vk::DeviceMemory>,
                fence: Option<ash::vk::Fence>,
            }
            impl Drop for Scope<'_> {
                fn drop(&mut self) {
                    let d = self.device;
                    unsafe {
                        if let Some(f) = self.fence.take() {
                            d.destroy_fence(f, None);
                        }
                        if let Some(p) = self.pool.take() {
                            d.destroy_command_pool(p, None);
                        }
                        if let Some(p) = self.pipe.take() {
                            d.destroy_pipeline(p, None);
                        }
                        if let Some(m) = self.fs_mod.take() {
                            d.destroy_shader_module(m, None);
                        }
                        if let Some(m) = self.vs_mod.take() {
                            d.destroy_shader_module(m, None);
                        }
                        if let Some(l) = self.layout.take() {
                            d.destroy_pipeline_layout(l, None);
                        }
                        if let Some(f) = self.fbo.take() {
                            d.destroy_framebuffer(f, None);
                        }
                        if let Some(r) = self.rp.take() {
                            d.destroy_render_pass(r, None);
                        }
                        if let Some(v) = self.view.take() {
                            d.destroy_image_view(v, None);
                        }
                        if let Some(i) = self.img.take() {
                            d.destroy_image(i, None);
                        }
                        if let Some(m) = self.img_mem.take() {
                            d.free_memory(m, None);
                        }
                        if let Some(b) = self.rbuf.take() {
                            d.destroy_buffer(b, None);
                        }
                        if let Some(m) = self.rbuf_mem.take() {
                            d.free_memory(m, None);
                        }
                        if let Some(b) = self.vbuf.take() {
                            d.destroy_buffer(b, None);
                        }
                        if let Some(m) = self.vbuf_mem.take() {
                            d.free_memory(m, None);
                        }
                    }
                }
            }
            let out = (|| -> Result<OffscreenPixels, String> {
                let mut scope = Scope {
                    device: &device,
                    vbuf: None,
                    vbuf_mem: None,
                    img: None,
                    img_mem: None,
                    view: None,
                    rp: None,
                    fbo: None,
                    layout: None,
                    vs_mod: None,
                    fs_mod: None,
                    pipe: None,
                    pool: None,
                    rbuf: None,
                    rbuf_mem: None,
                    fence: None,
                };
                let d = scope.device;
                let mem_props = instance.get_physical_device_memory_properties(phys);
                let host_coherent = ash::vk::MemoryPropertyFlags::HOST_VISIBLE
                    | ash::vk::MemoryPropertyFlags::HOST_COHERENT;
                // Vertex buffer: one location-0 vec4 per position.
                let vbytes = (positions.len() * 16) as u64;
                let vbuf_info = ash::vk::BufferCreateInfo::default()
                    .size(vbytes)
                    .usage(ash::vk::BufferUsageFlags::VERTEX_BUFFER)
                    .sharing_mode(ash::vk::SharingMode::EXCLUSIVE);
                let vbuf = d
                    .create_buffer(&vbuf_info, None)
                    .map_err(|e| format!("create_buffer(vertex): {e}"))?;
                scope.vbuf = Some(vbuf);
                let vreq = d.get_buffer_memory_requirements(vbuf);
                let vmem_idx = find_memory_type(&mem_props, vreq.memory_type_bits, host_coherent)
                    .ok_or_else(|| "no host-visible coherent memory type".to_string())?;
                let valloc = ash::vk::MemoryAllocateInfo::default()
                    .allocation_size(vreq.size)
                    .memory_type_index(vmem_idx);
                let vmem = d
                    .allocate_memory(&valloc, None)
                    .map_err(|e| format!("allocate_memory(vertex): {e}"))?;
                scope.vbuf_mem = Some(vmem);
                d.bind_buffer_memory(vbuf, vmem, 0)
                    .map_err(|e| format!("bind_buffer_memory(vertex): {e}"))?;
                let vdst = d
                    .map_memory(vmem, 0, vbytes, ash::vk::MemoryMapFlags::empty())
                    .map_err(|e| format!("map_memory(vertex): {e}"))?
                    as *mut f32;
                for (k, p) in positions.iter().enumerate() {
                    std::ptr::copy_nonoverlapping(p.as_ptr(), vdst.add(k * 4), 4);
                }
                d.unmap_memory(vmem);
                // Offscreen color image (device-local where available).
                let img_info = ash::vk::ImageCreateInfo::default()
                    .image_type(ash::vk::ImageType::TYPE_2D)
                    .format(fmt)
                    .extent(ash::vk::Extent3D {
                        width: IMAGE_W,
                        height: IMAGE_H,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(ash::vk::SampleCountFlags::TYPE_1)
                    .tiling(ash::vk::ImageTiling::OPTIMAL)
                    .usage(
                        ash::vk::ImageUsageFlags::COLOR_ATTACHMENT
                            | ash::vk::ImageUsageFlags::TRANSFER_SRC,
                    )
                    .sharing_mode(ash::vk::SharingMode::EXCLUSIVE)
                    .initial_layout(ash::vk::ImageLayout::UNDEFINED);
                let img = d
                    .create_image(&img_info, None)
                    .map_err(|e| format!("create_image: {e}"))?;
                scope.img = Some(img);
                let ireq = d.get_image_memory_requirements(img);
                let idev_local = ash::vk::MemoryPropertyFlags::DEVICE_LOCAL;
                let imem_idx = find_memory_type(&mem_props, ireq.memory_type_bits, idev_local)
                    .or_else(|| {
                        (0..mem_props.memory_type_count)
                            .find(|i| ireq.memory_type_bits & (1 << i) != 0)
                    })
                    .ok_or_else(|| "no image memory type".to_string())?;
                let ialloc = ash::vk::MemoryAllocateInfo::default()
                    .allocation_size(ireq.size)
                    .memory_type_index(imem_idx);
                let imem = d
                    .allocate_memory(&ialloc, None)
                    .map_err(|e| format!("allocate_memory(image): {e}"))?;
                scope.img_mem = Some(imem);
                d.bind_image_memory(img, imem, 0)
                    .map_err(|e| format!("bind_image_memory: {e}"))?;
                let view_info = ash::vk::ImageViewCreateInfo::default()
                    .image(img)
                    .view_type(ash::vk::ImageViewType::TYPE_2D)
                    .format(fmt)
                    .subresource_range(ash::vk::ImageSubresourceRange {
                        aspect_mask: ash::vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    });
                let view = d
                    .create_image_view(&view_info, None)
                    .map_err(|e| format!("create_image_view: {e}"))?;
                scope.view = Some(view);
                // Render pass: clear the image, store it for transfer-out.
                let attach = ash::vk::AttachmentDescription {
                    format: fmt,
                    samples: ash::vk::SampleCountFlags::TYPE_1,
                    load_op: ash::vk::AttachmentLoadOp::CLEAR,
                    store_op: ash::vk::AttachmentStoreOp::STORE,
                    stencil_load_op: ash::vk::AttachmentLoadOp::DONT_CARE,
                    stencil_store_op: ash::vk::AttachmentStoreOp::DONT_CARE,
                    initial_layout: ash::vk::ImageLayout::UNDEFINED,
                    final_layout: ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ..Default::default()
                };
                let color_ref = ash::vk::AttachmentReference {
                    attachment: 0,
                    layout: ash::vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                };
                let subpass = ash::vk::SubpassDescription::default()
                    .pipeline_bind_point(ash::vk::PipelineBindPoint::GRAPHICS)
                    .color_attachments(std::slice::from_ref(&color_ref));
                let rp_info = ash::vk::RenderPassCreateInfo::default()
                    .attachments(std::slice::from_ref(&attach))
                    .subpasses(std::slice::from_ref(&subpass));
                let rp = d
                    .create_render_pass(&rp_info, None)
                    .map_err(|e| format!("create_render_pass: {e}"))?;
                scope.rp = Some(rp);
                let fbo_info = ash::vk::FramebufferCreateInfo::default()
                    .render_pass(rp)
                    .attachments(std::slice::from_ref(&view))
                    .width(IMAGE_W)
                    .height(IMAGE_H)
                    .layers(1);
                let fbo = d
                    .create_framebuffer(&fbo_info, None)
                    .map_err(|e| format!("create_framebuffer: {e}"))?;
                scope.fbo = Some(fbo);
                // Graphics pipeline: caller VS words + trivial FS, one
                // location-0 vec4 vertex input, triangle list, no culling.
                let layout_info = ash::vk::PipelineLayoutCreateInfo::default();
                let layout = d
                    .create_pipeline_layout(&layout_info, None)
                    .map_err(|e| format!("create_pipeline_layout: {e}"))?;
                scope.layout = Some(layout);
                let vs_info = ash::vk::ShaderModuleCreateInfo::default().code(vs_words);
                let vs_mod = d
                    .create_shader_module(&vs_info, None)
                    .map_err(|e| format!("create_shader_module(vs): {e}"))?;
                scope.vs_mod = Some(vs_mod);
                let fs_info = ash::vk::ShaderModuleCreateInfo::default().code(&fs_words);
                let fs_mod = d
                    .create_shader_module(&fs_info, None)
                    .map_err(|e| format!("create_shader_module(fs): {e}"))?;
                scope.fs_mod = Some(fs_mod);
                let entry_name = CString::new("main").unwrap();
                let vs_stage = ash::vk::PipelineShaderStageCreateInfo::default()
                    .stage(ash::vk::ShaderStageFlags::VERTEX)
                    .module(vs_mod)
                    .name(&entry_name);
                let fs_stage = ash::vk::PipelineShaderStageCreateInfo::default()
                    .stage(ash::vk::ShaderStageFlags::FRAGMENT)
                    .module(fs_mod)
                    .name(&entry_name);
                let stages = [vs_stage, fs_stage];
                let binding = ash::vk::VertexInputBindingDescription {
                    binding: 0,
                    stride: 16,
                    input_rate: ash::vk::VertexInputRate::VERTEX,
                };
                let attr = ash::vk::VertexInputAttributeDescription {
                    location: 0,
                    binding: 0,
                    format: ash::vk::Format::R32G32B32A32_SFLOAT,
                    offset: 0,
                };
                let vi_info = ash::vk::PipelineVertexInputStateCreateInfo::default()
                    .vertex_binding_descriptions(std::slice::from_ref(&binding))
                    .vertex_attribute_descriptions(std::slice::from_ref(&attr));
                let ia_info = ash::vk::PipelineInputAssemblyStateCreateInfo::default()
                    .topology(ash::vk::PrimitiveTopology::TRIANGLE_LIST);
                let vp = ash::vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: IMAGE_W as f32,
                    height: IMAGE_H as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                };
                let scissor = ash::vk::Rect2D {
                    offset: ash::vk::Offset2D { x: 0, y: 0 },
                    extent: ash::vk::Extent2D {
                        width: IMAGE_W,
                        height: IMAGE_H,
                    },
                };
                let vp_info = ash::vk::PipelineViewportStateCreateInfo::default()
                    .viewports(std::slice::from_ref(&vp))
                    .scissors(std::slice::from_ref(&scissor));
                let rs_info = ash::vk::PipelineRasterizationStateCreateInfo::default()
                    .polygon_mode(ash::vk::PolygonMode::FILL)
                    .cull_mode(ash::vk::CullModeFlags::NONE)
                    .front_face(ash::vk::FrontFace::COUNTER_CLOCKWISE)
                    .line_width(1.0);
                let ms_info = ash::vk::PipelineMultisampleStateCreateInfo::default()
                    .rasterization_samples(ash::vk::SampleCountFlags::TYPE_1);
                let blend_attach = ash::vk::PipelineColorBlendAttachmentState::default()
                    .color_write_mask(
                        ash::vk::ColorComponentFlags::R
                            | ash::vk::ColorComponentFlags::G
                            | ash::vk::ColorComponentFlags::B
                            | ash::vk::ColorComponentFlags::A,
                    );
                let cb_info = ash::vk::PipelineColorBlendStateCreateInfo::default()
                    .attachments(std::slice::from_ref(&blend_attach));
                let pipe_info = ash::vk::GraphicsPipelineCreateInfo::default()
                    .stages(&stages)
                    .vertex_input_state(&vi_info)
                    .input_assembly_state(&ia_info)
                    .viewport_state(&vp_info)
                    .rasterization_state(&rs_info)
                    .multisample_state(&ms_info)
                    .color_blend_state(&cb_info)
                    .layout(layout)
                    .render_pass(rp)
                    .subpass(0);
                let pipes = d
                    .create_graphics_pipelines(ash::vk::PipelineCache::null(), &[pipe_info], None)
                    .map_err(|e| format!("create_graphics_pipelines: {e:?}"))?;
                scope.pipe = Some(pipes[0]);
                // Readback buffer (host-visible + coherent so the fence wait
                // publishes the copy).
                let rbytes = (IMAGE_W * IMAGE_H * 4) as u64;
                let rbuf_info = ash::vk::BufferCreateInfo::default()
                    .size(rbytes)
                    .usage(ash::vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(ash::vk::SharingMode::EXCLUSIVE);
                let rbuf = d
                    .create_buffer(&rbuf_info, None)
                    .map_err(|e| format!("create_buffer(readback): {e}"))?;
                scope.rbuf = Some(rbuf);
                let rreq = d.get_buffer_memory_requirements(rbuf);
                let rmem_idx = find_memory_type(&mem_props, rreq.memory_type_bits, host_coherent)
                    .ok_or_else(|| "no host-visible coherent memory type".to_string())?;
                let ralloc = ash::vk::MemoryAllocateInfo::default()
                    .allocation_size(rreq.size)
                    .memory_type_index(rmem_idx);
                let rmem = d
                    .allocate_memory(&ralloc, None)
                    .map_err(|e| format!("allocate_memory(readback): {e}"))?;
                scope.rbuf_mem = Some(rmem);
                d.bind_buffer_memory(rbuf, rmem, 0)
                    .map_err(|e| format!("bind_buffer_memory(readback): {e}"))?;
                // Record: clear, draw, barrier, copy out.
                let pool_info = ash::vk::CommandPoolCreateInfo::default()
                    .queue_family_index(qfi)
                    .flags(ash::vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
                let pool = d
                    .create_command_pool(&pool_info, None)
                    .map_err(|e| format!("create_command_pool: {e}"))?;
                scope.pool = Some(pool);
                let cmd_alloc = ash::vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(ash::vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1);
                let cmds = d
                    .allocate_command_buffers(&cmd_alloc)
                    .map_err(|e| format!("allocate_command_buffers: {e}"))?;
                let begin = ash::vk::CommandBufferBeginInfo::default()
                    .flags(ash::vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
                d.begin_command_buffer(cmds[0], &begin)
                    .map_err(|e| format!("begin_command_buffer: {e}"))?;
                let clear = ash::vk::ClearValue {
                    color: ash::vk::ClearColorValue { float32: CLEAR },
                };
                let rp_begin = ash::vk::RenderPassBeginInfo::default()
                    .render_pass(rp)
                    .framebuffer(fbo)
                    .render_area(ash::vk::Rect2D {
                        offset: ash::vk::Offset2D { x: 0, y: 0 },
                        extent: ash::vk::Extent2D {
                            width: IMAGE_W,
                            height: IMAGE_H,
                        },
                    })
                    .clear_values(std::slice::from_ref(&clear));
                d.cmd_begin_render_pass(cmds[0], &rp_begin, ash::vk::SubpassContents::INLINE);
                d.cmd_bind_pipeline(cmds[0], ash::vk::PipelineBindPoint::GRAPHICS, pipes[0]);
                d.cmd_bind_vertex_buffers(cmds[0], 0, &[vbuf], &[0]);
                d.cmd_draw(cmds[0], positions.len() as u32, 1, 0, 0);
                d.cmd_end_render_pass(cmds[0]);
                let barrier = ash::vk::ImageMemoryBarrier::default()
                    .src_access_mask(ash::vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                    .dst_access_mask(ash::vk::AccessFlags::TRANSFER_READ)
                    .old_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .new_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .src_queue_family_index(ash::vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(ash::vk::QUEUE_FAMILY_IGNORED)
                    .image(img)
                    .subresource_range(ash::vk::ImageSubresourceRange {
                        aspect_mask: ash::vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    });
                d.cmd_pipeline_barrier(
                    cmds[0],
                    ash::vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    ash::vk::PipelineStageFlags::TRANSFER,
                    ash::vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier],
                );
                let region = ash::vk::BufferImageCopy {
                    buffer_offset: 0,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: ash::vk::ImageSubresourceLayers {
                        aspect_mask: ash::vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_offset: ash::vk::Offset3D { x: 0, y: 0, z: 0 },
                    image_extent: ash::vk::Extent3D {
                        width: IMAGE_W,
                        height: IMAGE_H,
                        depth: 1,
                    },
                };
                d.cmd_copy_image_to_buffer(
                    cmds[0],
                    img,
                    ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    rbuf,
                    &[region],
                );
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
                        .map_memory(rmem, 0, rbytes, ash::vk::MemoryMapFlags::empty())
                        .map_err(|e| format!("map_memory(readback): {e}"))?
                        as *const u8;
                    let mut v = vec![0u8; rbytes as usize];
                    std::ptr::copy_nonoverlapping(src, v.as_mut_ptr(), v.len());
                    d.unmap_memory(rmem);
                    Ok(OffscreenPixels {
                        width: IMAGE_W,
                        height: IMAGE_H,
                        rgba: v,
                    })
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

#[cfg(test)]
mod tests {
    use super::super::spirv::SpirvVsBackend;
    use super::super::{VsBackend, VsInterface};
    use super::*;
    use crate::gpu::rdna2::encode;

    fn sig(
        name: &str,
        index: u32,
        sv: u32,
        ct: u32,
        reg: u32,
        mask: u8,
    ) -> crate::gpu::dxbc::SigElement {
        crate::gpu::dxbc::SigElement {
            name: name.into(),
            index,
            system_value: sv,
            component_type: ct,
            register: reg,
            mask,
            rw_mask: mask,
            stream: 0,
        }
    }

    /// Same in-tree blit program the GLSL/SPIR-V gates translate (fetch-shader
    /// passthrough: `pos = (in.x, in.y, 0, 1)`, `param0 = pos`).
    fn blit_code() -> Vec<u8> {
        let mut code = Vec::new();
        for (vdst, src) in [(0u64, 260u64), (1, 261u64), (2, 128u64), (3, 242u64)] {
            code.extend(
                encode(
                    "V_MOV_B32",
                    "ENC_VOP1",
                    &[("VDST", vdst), ("SRC0", src)],
                    None,
                )
                .unwrap(),
            );
        }
        code.extend(
            encode(
                "EXP",
                "ENC_EXP",
                &[
                    ("TGT", 12),
                    ("EN", 0xF),
                    ("VSRC0", 0),
                    ("VSRC1", 1),
                    ("VSRC2", 2),
                    ("VSRC3", 3),
                    ("DONE", 0),
                ],
                None,
            )
            .unwrap(),
        );
        code.extend(
            encode(
                "EXP",
                "ENC_EXP",
                &[
                    ("TGT", 32),
                    ("EN", 0xF),
                    ("VSRC0", 0),
                    ("VSRC1", 1),
                    ("VSRC2", 2),
                    ("VSRC3", 3),
                    ("DONE", 1),
                ],
                None,
            )
            .unwrap(),
        );
        code.extend(encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
        code
    }

    fn blit_vs() -> VsInterface {
        VsInterface {
            inputs: vec![sig("POSITION", 0, 0, 3, 0, 0xF)],
            outputs: vec![
                sig("SV_Position", 0, 1, 3, 0, 0xF),
                sig("COLOR", 0, 0, 3, 1, 0xF),
            ],
            attr_base: 4,
            params: None,
        }
    }

    fn word_ops(words: &[u32]) -> Vec<(u16, Vec<u32>)> {
        let mut out = Vec::new();
        let mut i = 5;
        while i < words.len() {
            let w = words[i];
            let count = (w >> 16) as usize;
            let op = (w & 0xFFFF) as u16;
            assert!(count >= 1, "bad SPIR-V word count");
            out.push((op, words[i + 1..i + count].to_vec()));
            i += count;
        }
        out
    }

    #[test]
    fn trivial_fs_is_a_valid_fragment_passthrough() {
        // Structural gate (no device): Fragment entry `main`,
        // OriginUpperLeft, one Location-0 in + one Location-0 out.
        let w = trivial_fs_spv();
        assert!(w.len() > 5, "module too short");
        assert_eq!(w[0], 0x0723_0203, "bad magic");
        assert_eq!(w[1] >> 16, 1, "major version must be 1");
        assert_eq!(w[4], 0, "schema must be 0");
        let ops = word_ops(&w);
        let has =
            |op: u16, pred: &dyn Fn(&[u32]) -> bool| ops.iter().any(|(o, a)| *o == op && pred(a));
        assert!(
            has(OP_CAPABILITY, &|a| a == [CAP_SHADER]),
            "missing Capability Shader"
        );
        assert!(
            has(OP_MEMORY_MODEL, &|a| a == [ADDR_LOGICAL, MEM_GLSL450]),
            "missing Logical GLSL450 memory model"
        );
        assert!(
            has(OP_ENTRY_POINT, &|a| !a.is_empty()
                && a[0] == EXEC_MODEL_FRAGMENT),
            "missing Fragment entry point"
        );
        assert!(
            has(OP_EXECUTION_MODE, &|a| a.len() == 2
                && a[1] == EXEC_MODE_ORIGIN_UPPER_LEFT),
            "missing OriginUpperLeft execution mode"
        );
        let locs: Vec<u32> = ops
            .iter()
            .filter(|(o, a)| *o == OP_DECORATE && a.len() == 3 && a[1] == DECOR_LOCATION)
            .map(|(_, a)| a[2])
            .collect();
        assert_eq!(locs.iter().filter(|&&l| l == 0).count(), 2, "{locs:?}");
        // Real `spirv-val` gate where available, else the structural checks gate.
        let val = std::process::Command::new("spirv-val")
            .arg("--version")
            .output();
        let Ok(which) = val else {
            println!("spirv-val not available – skipping, structural checks gate");
            return;
        };
        if !which.status.success() {
            println!("spirv-val not available – skipping, structural checks gate");
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-fs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spv = dir.join("trivial.frag.spv");
        let bytes: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
        std::fs::write(&spv, &bytes).unwrap();
        let out = std::process::Command::new("spirv-val")
            .arg(&spv)
            .output()
            .expect("spawn spirv-val");
        assert!(
            out.status.success(),
            "spirv-val failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    #[test]
    fn synthetic_blit_draws_fullscreen_triangle() {
        // The in-tree blit is the no-retail gate: translate it with the same
        // backend + interface the SPIR-V gate checks, then execute it.
        let code = blit_code();
        let vs = blit_vs();
        let t = SpirvVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}", t.unsupported);
        // Position-output/coverage equivalent of the GLSL gate: the VS words
        // declare a Vertex entry, a Position builtin and a param Location.
        let ops = word_ops(&t.words);
        let has =
            |op: u16, pred: &dyn Fn(&[u32]) -> bool| ops.iter().any(|(o, a)| *o == op && pred(a));
        assert!(
            has(15, &|a| !a.is_empty() && a[0] == 0),
            "missing Vertex entry point"
        );
        assert!(
            has(71, &|a| a.len() == 3 && a[1] == 11 && a[2] == 0),
            "missing Position builtin"
        );
        assert!(
            has(71, &|a| a.len() == 3 && a[1] == 30),
            "missing Location decoration for params"
        );
        let px = match draw_offscreen(&t.words, &FULLSCREEN_TRIANGLE) {
            Ok(px) => px,
            Err(e) if e.starts_with("no ") => {
                skip_test("no GPU");
                return;
            }
            Err(e) => panic!("draw_offscreen failed: {e}"),
        };
        assert_eq!((px.width, px.height), (IMAGE_W, IMAGE_H));
        assert_eq!(px.rgba.len(), (IMAGE_W * IMAGE_H * 4) as usize);
        // Full coverage: no pixel still shows the magenta clear color. The
        // fullscreen triangle covers every NDC pixel center.
        for px4 in px.rgba.chunks_exact(4) {
            assert!(
                !(px4[0] == 255 && px4[1] == 0 && px4[2] == 255),
                "uncovered pixel: clear color still visible"
            );
            // `w = 1` everywhere the blit reaches, so alpha is exact.
            assert_eq!(px4[3], 255, "bad alpha {px4:?}");
        }
        // Center pixel: NDC ≈ (+0.0156, +0.0156) — Vulkan maps framebuffer row
        // 0 to NDC y = -1, so pixel (32, 32) sits just above/beside the origin
        // and the passthrough color is ≈ (0.0156, 0.0156, 0, 1).
        let at = |x: u32, y: u32| -> [u8; 4] {
            let k = ((y * IMAGE_W + x) * 4) as usize;
            [px.rgba[k], px.rgba[k + 1], px.rgba[k + 2], px.rgba[k + 3]]
        };
        for (x, y) in [(32u32, 32u32), (31, 31), (32, 31), (31, 32)] {
            let c = at(x, y);
            for (got, want) in [(c[0], 4u8), (c[1], 4u8), (c[2], 0u8)] {
                assert!(
                    got.abs_diff(want) <= 8,
                    "center pixel ({x}, {y}) = {c:?}, want ≈ (4, 4, 0, 255)"
                );
            }
        }
        // Interpolation, not a flat fill: the triangle spans NDC x/y in
        // [-1, 3], so clamped UNORM red/green vary across the image.
        let distinct: std::collections::BTreeSet<[u8; 4]> = px
            .rgba
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect();
        assert!(distinct.len() > 32, "flat image: {}", distinct.len());
    }

    #[test]
    fn bad_words_and_empty_draw_fail_closed() {
        assert!(draw_offscreen(&[0x0723_0203, 0x00010000], &FULLSCREEN_TRIANGLE).is_err());
        assert!(draw_offscreen(&[1, 2, 3, 4, 5], &FULLSCREEN_TRIANGLE).is_err());
        let empty: [[f32; 4]; 0] = [];
        match draw_offscreen(&trivial_fs_spv(), &empty) {
            Ok(_) => panic!("empty draw must fail"),
            Err(e) if e.starts_with("no ") => {
                // No device: the length check still fires first for empty
                // input, but a device error here also skips cleanly.
                skip_test("no GPU");
            }
            Err(_) => {}
        }
    }

    #[test]
    fn shipped_path_spawns_no_compiler() {
        // The shipped `draw_offscreen` must never spawn a process: no
        // `Command` / `process` usage outside tests (tool names may appear in
        // docs/comments, so only the spawn mechanism itself is gated).
        let src = include_str!("vulkan_graphics.rs");
        let shipped = src.split("#[cfg(test)]").next().unwrap_or(src);
        for needle in ["Command::new", "std::process"] {
            assert!(
                !shipped.contains(needle),
                "shipped graphics path mentions {needle}"
            );
        }
    }

    #[test]
    fn retail_blit_draw_opt_in() {
        let Ok(path) = std::env::var("XCL_SAMPLE_RPF") else {
            println!("XCL_SAMPLE_RPF not set – skipping");
            return;
        };
        let data = std::fs::read(&path).unwrap();
        let rec = crate::gpu::pack::scan(&data, 0x40)
            .into_iter()
            .find(|r| r.name == "VS_BlitGenAmbCube_0cfdb358_Wrapped")
            .expect("record");
        let blob = crate::gpu::pack::inflate(&data, &rec).unwrap();
        let sh = crate::gpu::xbox::parse(&blob).unwrap();
        let fs = &sh.fssm[0];
        let p = fs.programs[0];
        let code = &fs.data[p.offset..p.offset + p.len];
        let mode = super::super::interface(&blob, &sh, fs).expect("interface");
        let super::super::Mode::Vertex(vs) = mode else {
            panic!("expected a vertex shader, got {mode:?}")
        };
        let t = SpirvVsBackend::translate_vs(code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}", t.unsupported_why);
        // Opt-in execution: the retail blit has extra inputs beyond location
        // 0, so only execution + readback shape are asserted here (coverage
        // stays with the in-tree synthetic gate above).
        match draw_offscreen(&t.words, &FULLSCREEN_TRIANGLE) {
            Ok(px) => assert_eq!(px.rgba.len(), (IMAGE_W * IMAGE_H * 4) as usize),
            Err(e) if e.starts_with("no ") => {
                skip_test("no GPU");
            }
            Err(e) => panic!("draw_offscreen failed: {e}"),
        }
    }
}
