//! Linux windowed Vulkan present (Phase D step 17, slice c).
//!
//! The recompiler-driven title triangle (`translate_vs_spirv` Vertex words over
//! a bare `VertexIndex` draw, constant-green fragment) presented through a real
//! `VK_KHR_swapchain` swapchain in a real X11 window — no offscreen image, no
//! `winit`, no new crates.
//!
//! Windowing without new dependencies: the X11 client calls (`XOpenDisplay`,
//! `XCreateSimpleWindow`, `XMapWindow`, ...) are resolved at runtime with
//! `libc::dlopen`/`dlsym` (`libX11.so.6`, falling back to `libX11.so`), the
//! same sanctioned pattern the runner uses for `libvulkan.so.1`: the static
//! exe links no X11 code and carries no extra `.so`. When no X display (or no
//! Xlib WSI / device / present queue) exists the entry point fails closed
//! with a `"no ..."` error so tests [`skip`](super::vulkan_runner::skip_test)
//! cleanly, mirroring `runner::skip_test`; anything else is fail-closed. The
//! shipped path spawns nothing: the fragment words are hand-assembled, the
//! vertex words come from the in-process `translate_vs_spirv`.
//!
//! Pixel contract: the swapchain image is cleared to the title blue
//! ([`TRIANGLE_CLEAR`](super::vulkan_graphics::TRIANGLE_CLEAR)), the
//! recompiler VS draws the title NDC triangle, and the image is copied to a
//! host-visible buffer *before* `queue_present`, so the readback asserts the
//! same center-green/corner-blue contract as the offscreen draws while the
//! present call itself proves the swapchain path on the device.

use std::ffi::CString;

use super::vulkan_graphics::{IMAGE_H, IMAGE_W, TRIANGLE_CLEAR};
use super::vulkan_runner::{find_memory_type, pick_graphics_device};

/// Opaque X11 display connection (ash `vk::Display` is `c_void`: the pointer
/// flows straight into `XlibSurfaceCreateInfoKHR::dpy`).
type XDisplay = ash::vk::Display;

/// X11 client entry points resolved at runtime (see module docs).
struct Xlib {
    handle: *mut libc::c_void,
    open_display: unsafe extern "C" fn(*const libc::c_char) -> *mut XDisplay,
    default_screen: unsafe extern "C" fn(*mut XDisplay) -> libc::c_int,
    root_window: unsafe extern "C" fn(*mut XDisplay, libc::c_int) -> ash::vk::Window,
    create_simple_window: unsafe extern "C" fn(
        *mut XDisplay,
        ash::vk::Window,
        libc::c_int,
        libc::c_int,
        libc::c_uint,
        libc::c_uint,
        libc::c_uint,
        libc::c_ulong,
        libc::c_ulong,
    ) -> ash::vk::Window,
    map_window: unsafe extern "C" fn(*mut XDisplay, ash::vk::Window) -> libc::c_int,
    sync_display: unsafe extern "C" fn(*mut XDisplay, libc::c_int) -> libc::c_int,
    destroy_window: unsafe extern "C" fn(*mut XDisplay, ash::vk::Window) -> libc::c_int,
    close_display: unsafe extern "C" fn(*mut XDisplay) -> libc::c_int,
}

impl Xlib {
    /// `dlopen` X11 and resolve the window calls. Fail-closed `"no display"`
    /// when the library, the display, or any symbol is missing.
    unsafe fn load() -> Result<Self, String> {
        let name6 = CString::new("libX11.so.6").unwrap();
        let name = CString::new("libX11.so").unwrap();
        let handle = libc::dlopen(name6.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        let handle = if handle.is_null() {
            libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL)
        } else {
            handle
        };
        if handle.is_null() {
            return Err("no display: dlopen libX11 failed".into());
        }
        macro_rules! sym {
            ($sym:expr, $ty:ty) => {{
                let n = CString::new($sym).unwrap();
                let p = libc::dlsym(handle, n.as_ptr());
                if p.is_null() {
                    libc::dlclose(handle);
                    return Err(format!("no display: dlsym {n:?} failed"));
                }
                // SAFETY: each symbol is cast to its documented X11 signature;
                // a missing symbol fails closed above, never called as the
                // wrong type.
                std::mem::transmute::<*mut libc::c_void, $ty>(p)
            }};
        }
        // SAFETY: each symbol is cast to its documented X11 signature; a
        // missing symbol fails closed above, never called as the wrong type.
        Ok(Self {
            handle,
            open_display: sym!(
                "XOpenDisplay",
                unsafe extern "C" fn(*const libc::c_char) -> *mut XDisplay
            ),
            default_screen: sym!(
                "XDefaultScreen",
                unsafe extern "C" fn(*mut XDisplay) -> libc::c_int
            ),
            root_window: sym!(
                "XRootWindow",
                unsafe extern "C" fn(*mut XDisplay, libc::c_int) -> ash::vk::Window
            ),
            create_simple_window: sym!(
                "XCreateSimpleWindow",
                unsafe extern "C" fn(
                    *mut XDisplay,
                    ash::vk::Window,
                    libc::c_int,
                    libc::c_int,
                    libc::c_uint,
                    libc::c_uint,
                    libc::c_uint,
                    libc::c_ulong,
                    libc::c_ulong,
                ) -> ash::vk::Window
            ),
            map_window: sym!(
                "XMapWindow",
                unsafe extern "C" fn(*mut XDisplay, ash::vk::Window) -> libc::c_int
            ),
            sync_display: sym!(
                "XSync",
                unsafe extern "C" fn(*mut XDisplay, libc::c_int) -> libc::c_int
            ),
            destroy_window: sym!(
                "XDestroyWindow",
                unsafe extern "C" fn(*mut XDisplay, ash::vk::Window) -> libc::c_int
            ),
            close_display: sym!(
                "XCloseDisplay",
                unsafe extern "C" fn(*mut XDisplay) -> libc::c_int
            ),
        })
    }
}

impl Drop for Xlib {
    fn drop(&mut self) {
        unsafe {
            libc::dlclose(self.handle);
        }
    }
}

/// An open display with one mapped 64×64 window; drop destroys the window,
/// closes the display, then `dlclose`s X11 (field drop order).
struct XWindow {
    dpy: *mut XDisplay,
    win: ash::vk::Window,
    xlib: Xlib,
}

impl XWindow {
    /// Open the display (`$DISPLAY`), create + map a tiny window for the
    /// swapchain surface. Fail-closed `"no display"` (skipped by tests).
    unsafe fn open() -> Result<Self, String> {
        let xlib = Xlib::load()?;
        let dpy = (xlib.open_display)(std::ptr::null());
        if dpy.is_null() {
            return Err("no display: XOpenDisplay failed".into());
        }
        let screen = (xlib.default_screen)(dpy);
        let root = (xlib.root_window)(dpy, screen);
        if root == 0 {
            (xlib.close_display)(dpy);
            return Err("no display: XRootWindow failed".into());
        }
        let win = (xlib.create_simple_window)(dpy, root, 0, 0, IMAGE_W, IMAGE_H, 0, 0, 0);
        if win == 0 {
            (xlib.close_display)(dpy);
            return Err("no display: XCreateSimpleWindow failed".into());
        }
        (xlib.map_window)(dpy, win);
        // Push the map request through before the surface is created so the
        // window exists by first present; no event loop is needed.
        (xlib.sync_display)(dpy, 0);
        Ok(Self { dpy, win, xlib })
    }
}

impl Drop for XWindow {
    fn drop(&mut self) {
        unsafe {
            (self.xlib.destroy_window)(self.dpy, self.win);
            (self.xlib.close_display)(self.dpy);
        }
    }
}

/// A created `SurfaceKHR` destroyed exactly once on scope exit (covers the
/// early-return paths before the swapchain `Scope` takes over teardown).
struct SurfaceGuard<'a> {
    ext: &'a ash::khr::surface::Instance,
    surface: Option<ash::vk::SurfaceKHR>,
}

impl Drop for SurfaceGuard<'_> {
    fn drop(&mut self) {
        if let Some(s) = self.surface.take() {
            unsafe {
                self.ext.destroy_surface(s, None);
            }
        }
    }
}

/// Readback-capable surface format: prefer `R8G8B8A8` (no host swizzle),
/// else `B8G8R8A8` (host-swizzled on readback, see the copy path), else the
/// first format. `None` when the surface offers none (pure, for tests).
fn pick_surface_format(formats: &[ash::vk::SurfaceFormatKHR]) -> Option<ash::vk::SurfaceFormatKHR> {
    use ash::vk::Format as F;
    if formats.is_empty() {
        return None;
    }
    formats
        .iter()
        .find(|f| f.format == F::R8G8B8A8_UNORM)
        .or_else(|| formats.iter().find(|f| f.format == F::R8G8B8A8_SRGB))
        .or_else(|| formats.iter().find(|f| f.format == F::B8G8R8A8_UNORM))
        .or_else(|| formats.iter().find(|f| f.format == F::B8G8R8A8_SRGB))
        .copied()
        .or_else(|| formats.first().copied())
}

/// First supported composite alpha in presentation order (pure, for tests).
fn pick_composite_alpha(
    supported: ash::vk::CompositeAlphaFlagsKHR,
) -> Option<ash::vk::CompositeAlphaFlagsKHR> {
    use ash::vk::CompositeAlphaFlagsKHR as A;
    [A::OPAQUE, A::PRE_MULTIPLIED, A::POST_MULTIPLIED, A::INHERIT]
        .into_iter()
        .find(|a| supported.contains(*a))
}

/// Swapchain extent for the 64×64 present tile: the surface's fixed extent
/// when it has one, else 64×64 clamped into the allowed range (pure, for
/// tests).
fn clamp_swap_extent(caps: &ash::vk::SurfaceCapabilitiesKHR) -> ash::vk::Extent2D {
    if caps.current_extent.width != u32::MAX {
        return caps.current_extent;
    }
    ash::vk::Extent2D {
        width: IMAGE_W.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
        height: IMAGE_H.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
    }
}

/// Queue family with `GRAPHICS` that can also present to `surface`: the
/// graphics pick first (the common case), else the first family covering
/// both. Error text starts with `"no "` so tests skip cleanly.
fn pick_present_queue(
    instance: &ash::Instance,
    surface_ext: &ash::khr::surface::Instance,
    phys: ash::vk::PhysicalDevice,
    surface: ash::vk::SurfaceKHR,
    graphics_qfi: u32,
) -> Result<u32, String> {
    let supported = |qfi: u32| unsafe {
        surface_ext
            .get_physical_device_surface_support(phys, qfi, surface)
            .unwrap_or(false)
    };
    if supported(graphics_qfi) {
        return Ok(graphics_qfi);
    }
    let families = unsafe { instance.get_physical_device_queue_family_properties(phys) };
    for (i, q) in families.iter().enumerate() {
        if q.queue_flags.contains(ash::vk::QueueFlags::GRAPHICS) && supported(i as u32) {
            return Ok(i as u32);
        }
    }
    Err("no present queue: no GRAPHICS family presents to the surface".into())
}

/// Present the title triangle through a real window + swapchain: `vs_words`
/// is the [`translate_vs_spirv`](super::spirv::translate_vs_spirv) Vertex
/// words (entry `main`, `VertexIndex` input, no vertex buffers) and
/// `fs_words` the constant-green fragment words. One acquire / render /
/// readback / present; the pixels are copied to a host buffer *before*
/// `queue_present`, so the returned image asserts the same contract as the
/// offscreen draws while the present call proves the swapchain path.
///
/// Contract: both word sets start with the SPIR-V magic. One frame; the
/// fence/acquire waits are 5 s. Errors are `String`s, never panics; missing
/// display / WSI / device / present-queue errors start with `"no "`. No
/// compiler, no process spawn on any path.
pub fn present_triangle_windowed(
    vs_words: &[u32],
    fs_words: &[u32],
) -> Result<super::vulkan_graphics::OffscreenPixels, String> {
    use super::vulkan_graphics::OffscreenPixels;
    if vs_words.len() < 5 || vs_words[0] != 0x0723_0203 {
        return Err("bad SPIR-V: missing magic (vs)".into());
    }
    if fs_words.len() < 5 || fs_words[0] != 0x0723_0203 {
        return Err("bad SPIR-V: missing magic (fs)".into());
    }
    unsafe {
        // The window must outlive the surface; it is destroyed after the
        // instance below.
        let window = XWindow::open()?;
        let entry = ash::Entry::load().map_err(|e| format!("no Vulkan: Entry::load: {e}"))?;
        // Instance extensions for the Xlib surface. Presence is NOT probed
        // with `enumerate_instance_extension_properties` first: that global
        // loader query races with concurrent `vkCreateInstance` calls from
        // other threads (observed segfault inside the system loader), so the
        // extensions are enabled unconditionally and absence fails closed at
        // `create_instance` below (every concurrent loader call then keeps
        // the same `Entry::load` + `create_instance` shape the offscreen
        // draws already run in parallel).
        let surface_fn = CString::new("VK_KHR_surface").unwrap();
        let xlib_fn = CString::new("VK_KHR_xlib_surface").unwrap();
        let exts = [surface_fn.as_ptr(), xlib_fn.as_ptr()];
        let app = CString::new("xcl").unwrap();
        let app_info = ash::vk::ApplicationInfo::default()
            .application_name(&app)
            .api_version(ash::vk::make_api_version(0, 1, 3, 0));
        let create = ash::vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&exts);
        let instance = entry.create_instance(&create, None).map_err(|e| {
            // Missing WSI support fails closed to a skip; other instance
            // failures keep the runner's `"no Vulkan"` skip shape.
            if e == ash::vk::Result::ERROR_EXTENSION_NOT_PRESENT {
                format!("no WSI: surface/xlib extension missing ({e:?})")
            } else {
                format!("no Vulkan: create_instance: {e}")
            }
        })?;
        // One instance for the whole present (same shared query as the
        // offscreen draws, never a second throwaway instance).
        let surface_ext = ash::khr::surface::Instance::new(&entry, &instance);
        let xlib_surface_ext = ash::khr::xlib_surface::Instance::new(&entry, &instance);
        let out = (|| -> Result<OffscreenPixels, String> {
            let (phys, graphics_qfi, name, _subgroup) = pick_graphics_device(&instance)?;
            let surface_info = ash::vk::XlibSurfaceCreateInfoKHR::default()
                .dpy(window.dpy)
                .window(window.win);
            let surface = xlib_surface_ext
                .create_xlib_surface(&surface_info, None)
                .map_err(|e| format!("no WSI: create_xlib_surface: {e:?}"))?;
            // Destroyed exactly once here (swapchain teardown below only
            // touches swapchain-owned handles, never the surface itself).
            let _guard = SurfaceGuard {
                ext: &surface_ext,
                surface: Some(surface),
            };
            let qfi = pick_present_queue(&instance, &surface_ext, phys, surface, graphics_qfi)?;
            eprintln!("vulkan: windowed present on {name}");
            // Swapchain device extension: enabled unconditionally (see the
            // instance-extension note above for why presence is not probed
            // with the racy global query). Absence fails closed to a skip;
            // any other device failure stays a hard error.
            let prio = [1.0f32];
            let queue_info = ash::vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qfi)
                .queue_priorities(&prio);
            let swap_fn = CString::new("VK_KHR_swapchain").unwrap();
            let dev_exts = [swap_fn.as_ptr()];
            let dev_info = ash::vk::DeviceCreateInfo::default()
                .queue_create_infos(std::slice::from_ref(&queue_info))
                .enabled_extension_names(&dev_exts);
            let device = instance.create_device(phys, &dev_info, None).map_err(|e| {
                if e == ash::vk::Result::ERROR_EXTENSION_NOT_PRESENT {
                    format!("no WSI: VK_KHR_swapchain missing ({e:?})")
                } else {
                    format!("create_device: {e:?}")
                }
            })?;
            let queue = device.get_device_queue(qfi, 0);
            let swap_ext = ash::khr::swapchain::Device::new(&instance, &device);
            let drawn = (|| -> Result<OffscreenPixels, String> {
                // Scoped teardown: every created handle is owned by `Scope`
                // and destroyed on drop, so an early `?` return cannot leak
                // (same pattern as the compute runner and the offscreen
                // draws); `device` is destroyed by the outer closure below,
                // then `surface` (guard above) / `instance` / window.
                struct Scope<'a> {
                    device: &'a ash::Device,
                    swap_ext: &'a ash::khr::swapchain::Device,
                    swapchain: Option<ash::vk::SwapchainKHR>,
                    views: Vec<ash::vk::ImageView>,
                    rp: Option<ash::vk::RenderPass>,
                    fbos: Vec<ash::vk::Framebuffer>,
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
                            for f in self.fbos.drain(..) {
                                d.destroy_framebuffer(f, None);
                            }
                            if let Some(r) = self.rp.take() {
                                d.destroy_render_pass(r, None);
                            }
                            for v in self.views.drain(..) {
                                d.destroy_image_view(v, None);
                            }
                            if let Some(s) = self.swapchain.take() {
                                self.swap_ext.destroy_swapchain(s, None);
                            }
                            if let Some(b) = self.rbuf.take() {
                                d.destroy_buffer(b, None);
                            }
                            if let Some(m) = self.rbuf_mem.take() {
                                d.free_memory(m, None);
                            }
                        }
                    }
                }
                let mut scope = Scope {
                    device: &device,
                    swap_ext: &swap_ext,
                    swapchain: None,
                    views: Vec::new(),
                    rp: None,
                    fbos: Vec::new(),
                    layout: None,
                    vs_mod: None,
                    fs_mod: None,
                    pipe: None,
                    pool: None,
                    rbuf: None,
                    rbuf_mem: None,
                    fence: None,
                };
                // Surface format / extent / present mode for the tile.
                let formats = surface_ext
                    .get_physical_device_surface_formats(phys, surface)
                    .map_err(|e| format!("no WSI: surface formats: {e:?}"))?;
                let format = pick_surface_format(&formats)
                    .ok_or_else(|| "no WSI: surface offers no formats".to_string())?;
                eprintln!("vulkan: present surface format {format:?}");
                let caps = surface_ext
                    .get_physical_device_surface_capabilities(phys, surface)
                    .map_err(|e| format!("no WSI: surface caps: {e:?}"))?;
                let extent = clamp_swap_extent(&caps);
                let modes = surface_ext
                    .get_physical_device_surface_present_modes(phys, surface)
                    .map_err(|e| format!("no WSI: present modes: {e:?}"))?;
                if !modes.contains(&ash::vk::PresentModeKHR::FIFO) {
                    return Err("no WSI: FIFO present mode missing".into());
                }
                let alpha = pick_composite_alpha(caps.supported_composite_alpha)
                    .ok_or_else(|| "no WSI: no composite alpha".to_string())?;
                if !caps
                    .supported_usage_flags
                    .contains(ash::vk::ImageUsageFlags::COLOR_ATTACHMENT)
                {
                    return Err("no WSI: surface image is no color attachment".into());
                }
                if !caps
                    .supported_usage_flags
                    .contains(ash::vk::ImageUsageFlags::TRANSFER_SRC)
                {
                    return Err("no WSI: surface image is no transfer source".into());
                }
                let mut count = caps.min_image_count + 1;
                if caps.max_image_count > 0 {
                    count = count.min(caps.max_image_count);
                }
                let swap_info = ash::vk::SwapchainCreateInfoKHR::default()
                    .surface(surface)
                    .min_image_count(count)
                    .image_format(format.format)
                    .image_color_space(format.color_space)
                    .image_extent(extent)
                    .image_array_layers(1)
                    .image_usage(
                        ash::vk::ImageUsageFlags::COLOR_ATTACHMENT
                            | ash::vk::ImageUsageFlags::TRANSFER_SRC,
                    )
                    .image_sharing_mode(ash::vk::SharingMode::EXCLUSIVE)
                    .pre_transform(caps.current_transform)
                    .composite_alpha(alpha)
                    .present_mode(ash::vk::PresentModeKHR::FIFO)
                    .clipped(true);
                let swapchain = swap_ext
                    .create_swapchain(&swap_info, None)
                    .map_err(|e| format!("create_swapchain: {e:?}"))?;
                scope.swapchain = Some(swapchain);
                let images = swap_ext
                    .get_swapchain_images(swapchain)
                    .map_err(|e| format!("swapchain images: {e:?}"))?;
                if images.is_empty() {
                    return Err("no WSI: swapchain yielded no images".into());
                }
                for img in &images {
                    let view_info = ash::vk::ImageViewCreateInfo::default()
                        .image(*img)
                        .view_type(ash::vk::ImageViewType::TYPE_2D)
                        .format(format.format)
                        .subresource_range(ash::vk::ImageSubresourceRange {
                            aspect_mask: ash::vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        });
                    let view = device
                        .create_image_view(&view_info, None)
                        .map_err(|e| format!("create_image_view: {e}"))?;
                    scope.views.push(view);
                }
                // Render pass: clear to title blue, store for transfer-out.
                let attach = ash::vk::AttachmentDescription {
                    format: format.format,
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
                let rp = device
                    .create_render_pass(&rp_info, None)
                    .map_err(|e| format!("create_render_pass: {e}"))?;
                scope.rp = Some(rp);
                for view in scope.views.clone() {
                    let fbo_info = ash::vk::FramebufferCreateInfo::default()
                        .render_pass(rp)
                        .attachments(std::slice::from_ref(&view))
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1);
                    let fbo = device
                        .create_framebuffer(&fbo_info, None)
                        .map_err(|e| format!("create_framebuffer: {e}"))?;
                    scope.fbos.push(fbo);
                }
                // Graphics pipeline: caller VS words + caller FS words, bare
                // `VertexIndex` draw with no bindings (same shape as the
                // offscreen triangle draw).
                let layout_info = ash::vk::PipelineLayoutCreateInfo::default();
                let layout = device
                    .create_pipeline_layout(&layout_info, None)
                    .map_err(|e| format!("create_pipeline_layout: {e}"))?;
                scope.layout = Some(layout);
                let vs_info = ash::vk::ShaderModuleCreateInfo::default().code(vs_words);
                let vs_mod = device
                    .create_shader_module(&vs_info, None)
                    .map_err(|e| format!("create_shader_module(vs): {e}"))?;
                scope.vs_mod = Some(vs_mod);
                let fs_info = ash::vk::ShaderModuleCreateInfo::default().code(fs_words);
                let fs_mod = device
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
                let vi_info = ash::vk::PipelineVertexInputStateCreateInfo::default();
                let ia_info = ash::vk::PipelineInputAssemblyStateCreateInfo::default()
                    .topology(ash::vk::PrimitiveTopology::TRIANGLE_LIST);
                let vp = ash::vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                };
                let scissor = ash::vk::Rect2D {
                    offset: ash::vk::Offset2D { x: 0, y: 0 },
                    extent,
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
                let pipes = device
                    .create_graphics_pipelines(ash::vk::PipelineCache::null(), &[pipe_info], None)
                    .map_err(|e| format!("create_graphics_pipelines: {e:?}"))?;
                scope.pipe = Some(pipes[0]);
                // Readback buffer (host-visible + coherent so the fence wait
                // publishes the copy).
                let mem_props = instance.get_physical_device_memory_properties(phys);
                let host_coherent = ash::vk::MemoryPropertyFlags::HOST_VISIBLE
                    | ash::vk::MemoryPropertyFlags::HOST_COHERENT;
                let rbytes = (extent.width * extent.height * 4) as u64;
                let rbuf_info = ash::vk::BufferCreateInfo::default()
                    .size(rbytes)
                    .usage(ash::vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(ash::vk::SharingMode::EXCLUSIVE);
                let rbuf = device
                    .create_buffer(&rbuf_info, None)
                    .map_err(|e| format!("create_buffer(readback): {e}"))?;
                scope.rbuf = Some(rbuf);
                let rreq = device.get_buffer_memory_requirements(rbuf);
                let rmem_idx = find_memory_type(&mem_props, rreq.memory_type_bits, host_coherent)
                    .ok_or_else(|| "no host-visible coherent memory type".to_string())?;
                let ralloc = ash::vk::MemoryAllocateInfo::default()
                    .allocation_size(rreq.size)
                    .memory_type_index(rmem_idx);
                let rmem = device
                    .allocate_memory(&ralloc, None)
                    .map_err(|e| format!("allocate_memory(readback): {e}"))?;
                scope.rbuf_mem = Some(rmem);
                device
                    .bind_buffer_memory(rbuf, rmem, 0)
                    .map_err(|e| format!("bind_buffer_memory(readback): {e}"))?;
                // One command buffer per swapchain image (same commands,
                // different framebuffer); a single frame is presented.
                let pool_info = ash::vk::CommandPoolCreateInfo::default()
                    .queue_family_index(qfi)
                    .flags(ash::vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
                let pool = device
                    .create_command_pool(&pool_info, None)
                    .map_err(|e| format!("create_command_pool: {e}"))?;
                scope.pool = Some(pool);
                let cmd_alloc = ash::vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(ash::vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(images.len() as u32);
                let cmds = device
                    .allocate_command_buffers(&cmd_alloc)
                    .map_err(|e| format!("allocate_command_buffers: {e}"))?;
                let sub_range = ash::vk::ImageSubresourceRange {
                    aspect_mask: ash::vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                for (k, img) in images.iter().enumerate() {
                    let begin = ash::vk::CommandBufferBeginInfo::default()
                        .flags(ash::vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
                    device
                        .begin_command_buffer(cmds[k], &begin)
                        .map_err(|e| format!("begin_command_buffer: {e}"))?;
                    let clear = ash::vk::ClearValue {
                        color: ash::vk::ClearColorValue {
                            float32: TRIANGLE_CLEAR,
                        },
                    };
                    let rp_begin = ash::vk::RenderPassBeginInfo::default()
                        .render_pass(rp)
                        .framebuffer(scope.fbos[k])
                        .render_area(ash::vk::Rect2D {
                            offset: ash::vk::Offset2D { x: 0, y: 0 },
                            extent,
                        })
                        .clear_values(std::slice::from_ref(&clear));
                    device.cmd_begin_render_pass(
                        cmds[k],
                        &rp_begin,
                        ash::vk::SubpassContents::INLINE,
                    );
                    device.cmd_bind_pipeline(
                        cmds[k],
                        ash::vk::PipelineBindPoint::GRAPHICS,
                        pipes[0],
                    );
                    device.cmd_draw(cmds[k], 3, 1, 0, 0);
                    device.cmd_end_render_pass(cmds[k]);
                    // Render target → transfer source for the readback.
                    let to_copy = ash::vk::ImageMemoryBarrier::default()
                        .src_access_mask(ash::vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                        .dst_access_mask(ash::vk::AccessFlags::TRANSFER_READ)
                        .old_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .new_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .src_queue_family_index(ash::vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(ash::vk::QUEUE_FAMILY_IGNORED)
                        .image(*img)
                        .subresource_range(sub_range);
                    device.cmd_pipeline_barrier(
                        cmds[k],
                        ash::vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                        ash::vk::PipelineStageFlags::TRANSFER,
                        ash::vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[to_copy],
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
                            width: extent.width,
                            height: extent.height,
                            depth: 1,
                        },
                    };
                    device.cmd_copy_image_to_buffer(
                        cmds[k],
                        *img,
                        ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        rbuf,
                        &[region],
                    );
                    // Transfer source → present source for the present.
                    let to_present = ash::vk::ImageMemoryBarrier::default()
                        .src_access_mask(ash::vk::AccessFlags::TRANSFER_READ)
                        .dst_access_mask(ash::vk::AccessFlags::empty())
                        .old_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .new_layout(ash::vk::ImageLayout::PRESENT_SRC_KHR)
                        .src_queue_family_index(ash::vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(ash::vk::QUEUE_FAMILY_IGNORED)
                        .image(*img)
                        .subresource_range(sub_range);
                    device.cmd_pipeline_barrier(
                        cmds[k],
                        ash::vk::PipelineStageFlags::TRANSFER,
                        ash::vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                        ash::vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[to_present],
                    );
                    device
                        .end_command_buffer(cmds[k])
                        .map_err(|e| format!("end_command_buffer: {e}"))?;
                }
                let fence_info = ash::vk::FenceCreateInfo::default();
                let fence = device
                    .create_fence(&fence_info, None)
                    .map_err(|e| format!("create_fence: {e}"))?;
                scope.fence = Some(fence);
                // Acquire / draw / read back / present one frame.
                let (idx, _) = swap_ext
                    .acquire_next_image(swapchain, 5_000_000_000, ash::vk::Semaphore::null(), fence)
                    .map_err(|e| format!("acquire_next_image: {e:?}"))?;
                device
                    .wait_for_fences(&[fence], true, 5_000_000_000)
                    .map_err(|e| format!("acquire fence: {e}"))?;
                device
                    .reset_fences(&[fence])
                    .map_err(|e| format!("reset_fences: {e}"))?;
                let submit = ash::vk::SubmitInfo::default()
                    .command_buffers(std::slice::from_ref(&cmds[idx as usize]));
                device
                    .queue_submit(queue, &[submit], fence)
                    .map_err(|e| format!("queue_submit: {e}"))?;
                let waited = device.wait_for_fences(&[fence], true, 5_000_000_000);
                let px = if waited.is_ok() {
                    let src = device
                        .map_memory(rmem, 0, rbytes, ash::vk::MemoryMapFlags::empty())
                        .map_err(|e| format!("map_memory(readback): {e}"))?
                        as *const u8;
                    let mut v = vec![0u8; rbytes as usize];
                    std::ptr::copy_nonoverlapping(src, v.as_mut_ptr(), v.len());
                    device.unmap_memory(rmem);
                    // Readback contract is RGBA byte order (same as the
                    // offscreen draws): swizzle B8G8R8A8 surfaces on the host
                    // (the presented image itself is untouched). sRGB/UNORM
                    // endpoints (0/1) encode exactly, so the exact asserts
                    // below still hold with no tolerance band.
                    match format.format {
                        ash::vk::Format::R8G8B8A8_UNORM | ash::vk::Format::R8G8B8A8_SRGB => {}
                        ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::B8G8R8A8_SRGB => {
                            for px in v.chunks_exact_mut(4) {
                                px.swap(0, 2);
                            }
                        }
                        other => {
                            return Err(format!("no WSI: unreadable surface format {other:?}"));
                        }
                    }
                    Ok(OffscreenPixels {
                        width: extent.width,
                        height: extent.height,
                        rgba: v,
                    })
                } else {
                    Err(format!("wait_for_fences: {waited:?}"))
                };
                // Present even when the readback failed: the present call
                // itself is the swapchain proof (a failed copy must not skip
                // it, and a failed present must surface).
                let present_info = ash::vk::PresentInfoKHR::default()
                    .swapchains(std::slice::from_ref(&swapchain))
                    .image_indices(std::slice::from_ref(&idx));
                swap_ext
                    .queue_present(queue, &present_info)
                    .map_err(|e| format!("queue_present: {e:?}"))?;
                px
                // `scope` drops here and destroys every handle above;
                // `device` is destroyed by the outer closure below.
            })();
            device.destroy_device(None);
            drawn
        })();
        // `_guard` (surface) dropped here, before the instance below; the
        // window (X11 connection) drops last.
        instance.destroy_instance(None);
        out
    }
}

/// Present arbitrary host RGBA bytes through a real window + swapchain (1d(ii):
/// the queue-#30 `PresentX` entry for title frames). Staging-buffer upload —
/// no shaders, no pipeline — then one `queue_present`; BGRA surfaces get a
/// host swizzle. Fail-closed `"no ..."` (headless: skipped, title runs on);
/// other failures are hard errors. Never spawns a process.
pub fn present_rgba_image(width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 || rgba.len() != (width * height * 4) as usize {
        return Err("bad present image (dims/bytes)".into());
    }
    unsafe {
        // The window must outlive the surface; destroyed after the instance below.
        let window = XWindow::open()?;
        let entry = ash::Entry::load().map_err(|e| format!("no Vulkan: Entry::load: {e}"))?;
        // Same no-probe shape as the triangle present: extensions enabled
        // unconditionally, absence fails closed at `create_instance`.
        let surface_fn = CString::new("VK_KHR_surface").unwrap();
        let xlib_fn = CString::new("VK_KHR_xlib_surface").unwrap();
        let exts = [surface_fn.as_ptr(), xlib_fn.as_ptr()];
        let app = CString::new("xcl").unwrap();
        let app_info = ash::vk::ApplicationInfo::default()
            .application_name(&app)
            .api_version(ash::vk::make_api_version(0, 1, 3, 0));
        let create = ash::vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&exts);
        let instance = entry.create_instance(&create, None).map_err(|e| {
            if e == ash::vk::Result::ERROR_EXTENSION_NOT_PRESENT {
                format!("no WSI: surface/xlib extension missing ({e:?})")
            } else {
                format!("no Vulkan: create_instance: {e}")
            }
        })?;
        let surface_ext = ash::khr::surface::Instance::new(&entry, &instance);
        let xlib_ext = ash::khr::xlib_surface::Instance::new(&entry, &instance);
        let out = (|| -> Result<(), String> {
            let (phys, gqfi, _, _) = pick_graphics_device(&instance)?;
            let si = ash::vk::XlibSurfaceCreateInfoKHR::default().dpy(window.dpy).window(window.win);
            let surface = xlib_ext
                .create_xlib_surface(&si, None)
                .map_err(|e| format!("no WSI: create_xlib_surface: {e:?}"))?;
            let _guard = SurfaceGuard { ext: &surface_ext, surface: Some(surface) };
            let qfi = pick_present_queue(&instance, &surface_ext, phys, surface, gqfi)?;
            let prio = [1.0f32];
            let qi = ash::vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qfi)
                .queue_priorities(&prio);
            let swap_fn = CString::new("VK_KHR_swapchain").unwrap();
            let swap_exts = [swap_fn.as_ptr()];
            let di = ash::vk::DeviceCreateInfo::default()
                .queue_create_infos(std::slice::from_ref(&qi))
                .enabled_extension_names(&swap_exts);
            let device = instance.create_device(phys, &di, None).map_err(|e| {
                if e == ash::vk::Result::ERROR_EXTENSION_NOT_PRESENT {
                    format!("no WSI: VK_KHR_swapchain missing ({e:?})")
                } else {
                    format!("create_device: {e:?}")
                }
            })?;
            let queue = device.get_device_queue(qfi, 0);
            let swap_ext = ash::khr::swapchain::Device::new(&instance, &device);
            let drawn = (|| -> Result<(), String> {
                let formats = surface_ext
                    .get_physical_device_surface_formats(phys, surface)
                    .map_err(|e| format!("no WSI: surface formats: {e:?}"))?;
                let format = pick_surface_format(&formats)
                    .ok_or_else(|| "no WSI: surface offers no formats".to_string())?;
                let caps = surface_ext
                    .get_physical_device_surface_capabilities(phys, surface)
                    .map_err(|e| format!("no WSI: surface caps: {e:?}"))?;
                let extent = if caps.current_extent.width != u32::MAX {
                    caps.current_extent
                } else {
                    ash::vk::Extent2D {
                        width: width.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                        height: height.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
                    }
                };
                if extent.width == 0 || extent.height == 0 {
                    return Err("no WSI: empty surface extent".into());
                }
                if !caps.supported_usage_flags.contains(ash::vk::ImageUsageFlags::TRANSFER_DST) {
                    return Err("no WSI: surface image is no transfer dst".into());
                }
                let alpha = pick_composite_alpha(caps.supported_composite_alpha)
                    .ok_or_else(|| "no WSI: no composite alpha".to_string())?;
                // Scale the frame to the surface extent (nearest; exact when equal).
                let (ew, eh) = (extent.width, extent.height);
                let mut frame = vec![0u8; (ew * eh * 4) as usize];
                for y in 0..eh {
                    for x in 0..ew {
                        let (sx, sy) = (x * width / ew, y * height / eh);
                        let (s, d) = ((sy * width + sx) as usize * 4, (y * ew + x) as usize * 4);
                        frame[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
                    }
                }
                match format.format {
                    ash::vk::Format::R8G8B8A8_UNORM | ash::vk::Format::R8G8B8A8_SRGB => {}
                    ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::B8G8R8A8_SRGB => {
                        for px in frame.chunks_exact_mut(4) {
                            px.swap(0, 2);
                        }
                    }
                    other => return Err(format!("no WSI: unreadable surface format {other:?}")),
                }
                let mut count = caps.min_image_count + 1;
                if caps.max_image_count > 0 {
                    count = count.min(caps.max_image_count);
                }
                let swap_info = ash::vk::SwapchainCreateInfoKHR::default()
                    .surface(surface)
                    .min_image_count(count)
                    .image_format(format.format)
                    .image_color_space(format.color_space)
                    .image_extent(extent)
                    .image_array_layers(1)
                    .image_usage(ash::vk::ImageUsageFlags::TRANSFER_DST)
                    .image_sharing_mode(ash::vk::SharingMode::EXCLUSIVE)
                    .pre_transform(caps.current_transform)
                    .composite_alpha(alpha)
                    .present_mode(ash::vk::PresentModeKHR::FIFO)
                    .clipped(true);
                let swapchain = swap_ext
                    .create_swapchain(&swap_info, None)
                    .map_err(|e| format!("create_swapchain: {e:?}"))?;
                // Scoped teardown: every created handle dies on drop, so an
                // early `?` never leaks (same pattern as the triangle present).
                struct Scope<'a> {
                    device: &'a ash::Device,
                    swap_ext: &'a ash::khr::swapchain::Device,
                    swapchain: Option<ash::vk::SwapchainKHR>,
                    pool: Option<ash::vk::CommandPool>,
                    sbuf: Option<ash::vk::Buffer>,
                    sbuf_mem: Option<ash::vk::DeviceMemory>,
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
                            if let Some(s) = self.swapchain.take() {
                                self.swap_ext.destroy_swapchain(s, None);
                            }
                            if let Some(b) = self.sbuf.take() {
                                d.destroy_buffer(b, None);
                            }
                            if let Some(m) = self.sbuf_mem.take() {
                                d.free_memory(m, None);
                            }
                        }
                    }
                }
                let mut scope = Scope {
                    device: &device,
                    swap_ext: &swap_ext,
                    swapchain: Some(swapchain),
                    pool: None,
                    sbuf: None,
                    sbuf_mem: None,
                    fence: None,
                };
                let images = swap_ext
                    .get_swapchain_images(swapchain)
                    .map_err(|e| format!("swapchain images: {e:?}"))?;
                if images.is_empty() {
                    return Err("no WSI: swapchain yielded no images".into());
                }
                // Host-visible staging upload of the whole frame.
                let mem_props = instance.get_physical_device_memory_properties(phys);
                let coherent = ash::vk::MemoryPropertyFlags::HOST_VISIBLE
                    | ash::vk::MemoryPropertyFlags::HOST_COHERENT;
                let sbytes = frame.len() as u64;
                let sbi = ash::vk::BufferCreateInfo::default()
                    .size(sbytes)
                    .usage(ash::vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(ash::vk::SharingMode::EXCLUSIVE);
                let sbuf = device
                    .create_buffer(&sbi, None)
                    .map_err(|e| format!("create_buffer(staging): {e}"))?;
                scope.sbuf = Some(sbuf);
                let sreq = device.get_buffer_memory_requirements(sbuf);
                let sidx = find_memory_type(&mem_props, sreq.memory_type_bits, coherent)
                    .ok_or_else(|| "no host-visible coherent memory type".to_string())?;
                let sai = ash::vk::MemoryAllocateInfo::default()
                    .allocation_size(sreq.size)
                    .memory_type_index(sidx);
                let smem = device
                    .allocate_memory(&sai, None)
                    .map_err(|e| format!("allocate_memory(staging): {e}"))?;
                scope.sbuf_mem = Some(smem);
                device
                    .bind_buffer_memory(sbuf, smem, 0)
                    .map_err(|e| format!("bind_buffer_memory(staging): {e}"))?;
                let dst = device
                    .map_memory(smem, 0, sbytes, ash::vk::MemoryMapFlags::empty())
                    .map_err(|e| format!("map_memory(staging): {e}"))? as *mut u8;
                std::ptr::copy_nonoverlapping(frame.as_ptr(), dst, frame.len());
                device.unmap_memory(smem);
                // Acquire first, then record one buffer for that image.
                let fence = device
                    .create_fence(&ash::vk::FenceCreateInfo::default(), None)
                    .map_err(|e| format!("create_fence: {e}"))?;
                scope.fence = Some(fence);
                let (idx, _) = swap_ext
                    .acquire_next_image(swapchain, 5_000_000_000, ash::vk::Semaphore::null(), fence)
                    .map_err(|e| format!("acquire_next_image: {e:?}"))?;
                device
                    .wait_for_fences(&[fence], true, 5_000_000_000)
                    .map_err(|e| format!("acquire fence: {e}"))?;
                device
                    .reset_fences(&[fence])
                    .map_err(|e| format!("reset_fences: {e}"))?;
                let pool_info = ash::vk::CommandPoolCreateInfo::default()
                    .queue_family_index(qfi)
                    .flags(ash::vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
                let pool = device
                    .create_command_pool(&pool_info, None)
                    .map_err(|e| format!("create_command_pool: {e}"))?;
                scope.pool = Some(pool);
                let cmd_alloc = ash::vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(ash::vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1);
                let cmds = device
                    .allocate_command_buffers(&cmd_alloc)
                    .map_err(|e| format!("allocate_command_buffers: {e}"))?;
                let img = images[idx as usize];
                let sub = ash::vk::ImageSubresourceRange {
                    aspect_mask: ash::vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                let barrier = |old: ash::vk::ImageLayout, new: ash::vk::ImageLayout, src: ash::vk::AccessFlags, dst: ash::vk::AccessFlags| {
                    ash::vk::ImageMemoryBarrier::default()
                        .src_access_mask(src)
                        .dst_access_mask(dst)
                        .old_layout(old)
                        .new_layout(new)
                        .src_queue_family_index(ash::vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(ash::vk::QUEUE_FAMILY_IGNORED)
                        .image(img)
                        .subresource_range(sub)
                };
                device
                    .begin_command_buffer(
                        cmds[0],
                        &ash::vk::CommandBufferBeginInfo::default()
                            .flags(ash::vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                    )
                    .map_err(|e| format!("begin_command_buffer: {e}"))?;
                device.cmd_pipeline_barrier(
                    cmds[0],
                    ash::vk::PipelineStageFlags::TOP_OF_PIPE,
                    ash::vk::PipelineStageFlags::TRANSFER,
                    ash::vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier(
                        ash::vk::ImageLayout::UNDEFINED,
                        ash::vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        ash::vk::AccessFlags::empty(),
                        ash::vk::AccessFlags::TRANSFER_WRITE,
                    )],
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
                    image_extent: ash::vk::Extent3D { width: ew, height: eh, depth: 1 },
                };
                device.cmd_copy_buffer_to_image(
                    cmds[0],
                    sbuf,
                    img,
                    ash::vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
                device.cmd_pipeline_barrier(
                    cmds[0],
                    ash::vk::PipelineStageFlags::TRANSFER,
                    ash::vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    ash::vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier(
                        ash::vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        ash::vk::ImageLayout::PRESENT_SRC_KHR,
                        ash::vk::AccessFlags::TRANSFER_WRITE,
                        ash::vk::AccessFlags::empty(),
                    )],
                );
                device
                    .end_command_buffer(cmds[0])
                    .map_err(|e| format!("end_command_buffer: {e}"))?;
                let submit = ash::vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmds[0]));
                device
                    .queue_submit(queue, &[submit], fence)
                    .map_err(|e| format!("queue_submit: {e}"))?;
                device
                    .wait_for_fences(&[fence], true, 5_000_000_000)
                    .map_err(|e| format!("wait_for_fences: {e:?}"))?;
                let present_info = ash::vk::PresentInfoKHR::default()
                    .swapchains(std::slice::from_ref(&swapchain))
                    .image_indices(std::slice::from_ref(&idx));
                swap_ext
                    .queue_present(queue, &present_info)
                    .map_err(|e| format!("queue_present: {e:?}"))?;
                Ok(())
                // `scope` drops here; `device` destroyed by the outer closure.
            })();
            device.destroy_device(None);
            drawn
        })();
        // `_guard` (surface) dropped here, before the instance below; the
        // window (X11 connection) drops last.
        instance.destroy_instance(None);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::spirv::SpirvVsBackend;
    use super::super::vulkan_graphics::green_fs_spv;
    use super::super::{VsBackend, VsInterface};
    use super::*;
    use crate::gpu::rdna2::encode;
    use crate::gpu::recomp::vulkan_runner::skip_test;

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

    /// Same synthetic VertexIndex-indexed constant-select triangle the
    /// offscreen recompiler draw covers (`y = is1 ? 0.5 : -0.5`,
    /// `x = is0 ? -0.5 : (is1 ? 0 : 0.5)`), but with no params: the
    /// constant-green fragment takes none.
    fn recomp_triangle_code() -> Vec<u8> {
        // v0 holds VertexIndex (bring-up init); z = 0, w = 1.
        let mut code = Vec::new();
        // Materialize float-bit constants (inline-constant operand codes).
        for (vdst, src) in [
            (1u64, 241u64), // -0.5
            (2, 240u64),    // 0.5
            (3, 128u64),    // 0.0
            (4, 242u64),    // 1.0
        ] {
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
        // vcc = (vid == 1).
        code.extend(
            encode(
                "V_CMP_EQ_U32",
                "ENC_VOPC",
                &[("SRC0", 129), ("VSRC1", 0)],
                None,
            )
            .unwrap(),
        );
        // v5 = vcc ? v2(0.5) : v1(-0.5)  → y.
        code.extend(
            encode(
                "V_CNDMASK_B32",
                "ENC_VOP2",
                &[("VDST", 5), ("SRC0", 257), ("VSRC1", 2)],
                None,
            )
            .unwrap(),
        );
        // v6 = vcc ? v3(0.0) : v2(0.5)  → x so far.
        code.extend(
            encode(
                "V_CNDMASK_B32",
                "ENC_VOP2",
                &[("VDST", 6), ("SRC0", 258), ("VSRC1", 3)],
                None,
            )
            .unwrap(),
        );
        // vcc = (vid == 0).
        code.extend(
            encode(
                "V_CMP_EQ_U32",
                "ENC_VOPC",
                &[("SRC0", 128), ("VSRC1", 0)],
                None,
            )
            .unwrap(),
        );
        // v6 = vcc ? v1(-0.5) : v6  → x.
        code.extend(
            encode(
                "V_CNDMASK_B32",
                "ENC_VOP2",
                &[("VDST", 6), ("SRC0", 262), ("VSRC1", 1)],
                None,
            )
            .unwrap(),
        );
        // pos = (x, y, 0, 1), no params (the constant-green FS takes none).
        code.extend(
            encode(
                "EXP",
                "ENC_EXP",
                &[
                    ("TGT", 12),
                    ("EN", 0xF),
                    ("VSRC0", 6),
                    ("VSRC1", 5),
                    ("VSRC2", 3),
                    ("VSRC3", 4),
                    ("DONE", 1),
                ],
                None,
            )
            .unwrap(),
        );
        code.extend(encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
        code
    }

    fn recomp_triangle_vs() -> VsInterface {
        VsInterface {
            inputs: vec![],
            outputs: vec![sig("SV_Position", 0, 1, 3, 0, 0xF)],
            attr_base: 4,
            params: None,
        }
    }

    #[test]
    fn surface_format_prefers_rgba_then_swizzlable_bgra() {
        use ash::vk::{ColorSpaceKHR, Format, SurfaceFormatKHR};
        // RGBA UNORM wins over everything (no host swizzle).
        let offered = [
            SurfaceFormatKHR {
                format: Format::B8G8R8A8_SRGB,
                color_space: ColorSpaceKHR::SRGB_NONLINEAR,
            },
            SurfaceFormatKHR {
                format: Format::R8G8B8A8_UNORM,
                color_space: ColorSpaceKHR::SRGB_NONLINEAR,
            },
        ];
        let picked = pick_surface_format(&offered).unwrap();
        assert_eq!(picked.format, Format::R8G8B8A8_UNORM);
        // No RGBA: BGRA is picked (host-swizzled on readback) over the rest.
        let bgra = [
            SurfaceFormatKHR {
                format: Format::R5G6B5_UNORM_PACK16,
                color_space: ColorSpaceKHR::SRGB_NONLINEAR,
            },
            SurfaceFormatKHR {
                format: Format::B8G8R8A8_SRGB,
                color_space: ColorSpaceKHR::SRGB_NONLINEAR,
            },
        ];
        assert_eq!(
            pick_surface_format(&bgra).unwrap().format,
            Format::B8G8R8A8_SRGB
        );
        // Single unknown offer falls back to itself (the draw then fails
        // closed on the unreadable format, never misreads).
        let lone = [SurfaceFormatKHR {
            format: Format::R5G6B5_UNORM_PACK16,
            color_space: ColorSpaceKHR::SRGB_NONLINEAR,
        }];
        assert_eq!(
            pick_surface_format(&lone).unwrap().format,
            Format::R5G6B5_UNORM_PACK16
        );
        assert!(pick_surface_format(&[]).is_none());
    }

    #[test]
    fn swap_extent_uses_fixed_or_clamped_tile() {
        use ash::vk::{Extent2D, SurfaceCapabilitiesKHR};
        let mut caps = SurfaceCapabilitiesKHR::default();
        // Fixed surface extent wins as-is.
        caps.current_extent = Extent2D {
            width: 800,
            height: 600,
        };
        assert_eq!(
            clamp_swap_extent(&caps),
            Extent2D {
                width: 800,
                height: 600,
            }
        );
        // Sizing surface clamps the 64×64 tile into range.
        caps.current_extent = Extent2D {
            width: u32::MAX,
            height: u32::MAX,
        };
        caps.min_image_extent = Extent2D {
            width: 1,
            height: 1,
        };
        caps.max_image_extent = Extent2D {
            width: 4096,
            height: 4096,
        };
        assert_eq!(
            clamp_swap_extent(&caps),
            Extent2D {
                width: IMAGE_W,
                height: IMAGE_H,
            }
        );
        // A maximum below the tile clamps down (never an error).
        caps.max_image_extent = Extent2D {
            width: 32,
            height: 32,
        };
        assert_eq!(
            clamp_swap_extent(&caps),
            Extent2D {
                width: 32,
                height: 32,
            }
        );
    }

    #[test]
    fn composite_alpha_prefers_opaque() {
        use ash::vk::CompositeAlphaFlagsKHR as A;
        assert_eq!(
            pick_composite_alpha(A::OPAQUE | A::PRE_MULTIPLIED),
            Some(A::OPAQUE)
        );
        assert_eq!(pick_composite_alpha(A::INHERIT), Some(A::INHERIT));
        assert_eq!(pick_composite_alpha(A::empty()), None);
    }

    #[test]
    fn bad_words_fail_closed() {
        let fs = green_fs_spv();
        assert!(present_triangle_windowed(&[], &fs).is_err());
        assert!(present_triangle_windowed(&[0x0723_0203, 1, 2, 3], &fs).is_err());
        let vs = SpirvVsBackend::translate_vs(&recomp_triangle_code(), &recomp_triangle_vs(), true)
            .unwrap();
        assert!(vs.complete());
        assert!(present_triangle_windowed(&vs.words, &[]).is_err());
    }

    #[test]
    fn arbitrary_image_validates_and_presents_or_skips() {
        // 1d(ii): bad dims/bytes fail closed (pure); a 2x2 frame presents for
        // real, or skips cleanly with `"no "` headless (mirrors the triangle test).
        assert!(present_rgba_image(0, 0, &[]).is_err());
        assert!(present_rgba_image(4, 4, &[0u8; 8]).is_err());
        assert!(present_rgba_image(4097, 1, &[0u8; 4097 * 4]).is_err());
        match present_rgba_image(2, 2, &[0x11u8; 2 * 2 * 4]) {
            Ok(()) => {}
            Err(e) if e.starts_with("no ") => {
                skip_test("no display/WSI/device");
            }
            Err(e) => panic!("present_rgba_image failed: {e}"),
        }
    }

    #[test]
    fn windowed_recomp_triangle_presents_center_green_corner_blue() {
        // The recompiler-driven title primitive through a real window +
        // swapchain: synthetic RDNA2 through the in-process SPIR-V backend,
        // rendered into the swapchain image and presented, with the pixels
        // copied out before present asserting the title pixel contract.
        let code = recomp_triangle_code();
        let vs = recomp_triangle_vs();
        let t = SpirvVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}", t.unsupported);
        let fs = green_fs_spv();
        let px = match present_triangle_windowed(&t.words, &fs) {
            Ok(px) => px,
            Err(e) if e.starts_with("no ") => {
                skip_test("no display/WSI/device");
                return;
            }
            Err(e) => panic!("present_triangle_windowed failed: {e}"),
        };
        // The surface picks its extent (normally the 64×64 tile); index the
        // contract off the returned size so a resized drawable still gates.
        let (w, h) = (px.width, px.height);
        assert!(w > 0 && h > 0, "empty presented image");
        assert_eq!(px.rgba.len(), (w * h * 4) as usize);
        let at = |x: u32, y: u32| -> [u8; 4] {
            let k = ((y * w + x) * 4) as usize;
            [px.rgba[k], px.rgba[k + 1], px.rgba[k + 2], px.rgba[k + 3]]
        };
        // Center is inside (-0.5,-0.5)/(0,0.5)/(0.5,-0.5), so the
        // constant-green FS paints it; corners sit outside in the blue clear.
        for (x, y) in [(w / 2, h / 2), (w / 2 - 1, h / 2 - 1)] {
            assert_eq!(at(x, y), [0, 255, 0, 255], "center pixel ({x}, {y})");
        }
        for (x, y) in [(1u32, 1u32), (w - 2, 1), (1, h - 2), (w - 2, h - 2)] {
            assert_eq!(at(x, y), [0, 0, 255, 255], "corner pixel ({x}, {y})");
        }
    }

    #[test]
    fn shipped_path_spawns_no_compiler() {
        // The shipped `present_triangle_windowed` must never spawn a process:
        // no `Command` / `process` usage outside tests (the X11 loader uses
        // `libc::dlopen`, never a child process).
        let src = include_str!("vulkan_present.rs");
        let shipped = src.split("#[cfg(test)]").next().unwrap_or(src);
        for needle in ["Command::new", "std::process"] {
            assert!(
                !shipped.contains(needle),
                "shipped present path mentions {needle}"
            );
        }
    }
}
