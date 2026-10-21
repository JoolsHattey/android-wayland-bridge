//! GPU compositing with Vulkan (the device's Mesa ANV, through Android's
//! loader). Client dmabufs are imported as images once, when their wl_buffer
//! is created; wl_shm textures are uploaded when they change. Each window
//! draws into a swapchain on its ANativeWindow, one textured quad per
//! surface. The caption band is copied back afterwards, so the header
//! analysis (control colour, gesture exclusion) works as on the CPU path.
//!
//! All work is ordered on one queue and tracked on one timeline semaphore:
//! every submission signals the next serial. Client buffers are released,
//! and images destroyed, once the last serial that used them completes.

use std::cell::Cell;
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use ash::vk;
use wayland_server::Resource;
use wayland_server::backend::ObjectId;
use wayland_server::protocol::wl_buffer::WlBuffer;

use crate::android::ANativeWindow;
use crate::direct::{AHardwareBuffer, Layer};
use crate::output::to_px;
use crate::render::Band;
use crate::shm::{Rect, Texture, rect_clamp, rect_empty, rect_union};
use crate::state::{SYNC_TIMEOUT, State};
use crate::Shared;
use crate::syncobj::Release;

const fn fourcc(s: &[u8; 4]) -> u32 {
    s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}

/// DRM formats we import: fourcc, Vulkan format, alpha ignored.
const FORMATS: [(u32, vk::Format, bool); 8] = [
    (fourcc(b"AR24"), vk::Format::B8G8R8A8_UNORM, false),
    (fourcc(b"XR24"), vk::Format::B8G8R8A8_UNORM, true),
    (fourcc(b"AB24"), vk::Format::R8G8B8A8_UNORM, false),
    (fourcc(b"XB24"), vk::Format::R8G8B8A8_UNORM, true),
    (fourcc(b"AR30"), vk::Format::A2R10G10B10_UNORM_PACK32, false),
    (fourcc(b"XR30"), vk::Format::A2R10G10B10_UNORM_PACK32, true),
    (fourcc(b"AB30"), vk::Format::A2B10G10R10_UNORM_PACK32, false),
    (fourcc(b"XB30"), vk::Format::A2B10G10R10_UNORM_PACK32, true),
];

/// 0xff202020, as on the CPU path.
const BACKGROUND: [f32; 4] = [0x20 as f32 / 255.0, 0x20 as f32 / 255.0, 0x20 as f32 / 255.0, 1.0];

/// Descriptor sets per window frame, i.e. surfaces drawn per window.
const MAX_DRAWS: u32 = 128;

/// Frames a window may have queued for its display: enough to keep it busy,
/// few enough that clients are not paced frames ahead of the screen.
const MAX_QUEUED: u32 = 2;

static VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/quad.vert.spv"));
static FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/quad.frag.spv"));

fn err<T>(what: &str, r: Result<T, vk::Result>) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {e}"))
}

#[derive(Clone, Copy, Debug)]
pub struct DmabufFormat {
    pub fourcc: u32,
    pub modifier: u64,
    /// Memory planes the modifier needs (CCS adds aux planes).
    pub planes: u32,
    /// Can also be copied from (for the caption readback in direct mode).
    pub transfer: bool,
}

impl DmabufFormat {
    /// Whether SurfaceFlinger can take such a buffer as is (direct mode).
    pub fn direct(&self) -> bool {
        self.planes == 1 && crate::direct::hal_format(self.fourcc).is_some() && crate::direct::tiling(self.modifier).is_some()
    }
}

enum Retired {
    Image(vk::Image, vk::ImageView, vk::DeviceMemory),
    Buffer(vk::Buffer, vk::DeviceMemory),
}

struct Pass {
    format: vk::Format,
    readback: bool,
    render_pass: vk::RenderPass,
    pipeline: vk::Pipeline,
}

/// The Vulkan device and everything shared by all windows. Used by the
/// compositor thread, and by the UI thread only to tear down a window's
/// swapchain (under its slot lock).
pub struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    surface_fn: ash::khr::surface::Instance,
    android_fn: ash::khr::android_surface::Instance,
    pd: vk::PhysicalDevice,
    device: ash::Device,
    swapchain_fn: ash::khr::swapchain::Device,
    memfd_fn: ash::khr::external_memory_fd::Device,
    /// Tells which presented frames the display has shown (for pacing).
    timing_fn: Option<ash::google::display_timing::Device>,
    queue: vk::Queue,
    qfi: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    timeline: vk::Semaphore,
    /// A binary semaphore every submit also signals, exported right after
    /// as a sync_file (see `fence`), with VK_KHR_external_semaphore_fd.
    export: Option<(ash::khr::external_semaphore_fd::Device, vk::Semaphore)>,
    dsl: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    vert: vk::ShaderModule,
    frag: vk::ShaderModule,
    passes: Mutex<Vec<Pass>>,
    retired: Mutex<Vec<(u64, Retired)>>,
    /// What clients may allocate, for zwp_linux_dmabuf_v1.
    pub formats: Vec<DmabufFormat>,
    /// dev_t of the GPU's render node.
    pub render_node: u64,
}

impl Gpu {
    fn new() -> Result<Arc<Gpu>, String> {
        unsafe {
            let entry = ash::Entry::load().map_err(|e| format!("libvulkan: {e}"))?;
            let app = vk::ApplicationInfo::default().application_name(c"awb").api_version(vk::API_VERSION_1_3);
            let exts = [ash::khr::surface::NAME.as_ptr(), ash::khr::android_surface::NAME.as_ptr()];
            let instance = err(
                "vkCreateInstance",
                entry.create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&exts),
                    None,
                ),
            )?;
            let required = [
                ash::khr::swapchain::NAME,
                ash::khr::external_memory_fd::NAME,
                ash::ext::external_memory_dma_buf::NAME,
                ash::ext::image_drm_format_modifier::NAME,
                ash::ext::queue_family_foreign::NAME,
            ];
            let mut chosen = None;
            for pd in err("vkEnumeratePhysicalDevices", instance.enumerate_physical_devices())? {
                let props = instance.get_physical_device_properties(pd);
                if props.api_version < vk::API_VERSION_1_2 {
                    continue;
                }
                let have = instance.enumerate_device_extension_properties(pd).unwrap_or_default();
                let has = |name: &std::ffi::CStr| have.iter().any(|e| e.extension_name_as_c_str() == Ok(name));
                if !required.iter().all(|n| has(n)) {
                    continue;
                }
                let qfi = instance
                    .get_physical_device_queue_family_properties(pd)
                    .iter()
                    .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS));
                if let Some(qfi) = qfi {
                    log::info!("gpu: {:?}", props.device_name_as_c_str().unwrap_or_default());
                    let optional = (
                        has(ash::ext::physical_device_drm::NAME),
                        has(ash::google::display_timing::NAME),
                        has(ash::khr::external_semaphore_fd::NAME) && Self::sync_fd_exportable(&instance, pd),
                    );
                    chosen = Some((pd, qfi as u32, optional));
                    break;
                }
            }
            let Some((pd, qfi, (has_drm, has_timing, has_export))) = chosen else {
                return Err("no Vulkan device can import dmabufs".into());
            };

            let prio = [1.0];
            let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(qfi).queue_priorities(&prio)];
            let mut names: Vec<_> = required.iter().map(|n| n.as_ptr()).collect();
            if has_timing {
                names.push(ash::google::display_timing::NAME.as_ptr());
            }
            if has_export {
                names.push(ash::khr::external_semaphore_fd::NAME.as_ptr());
            }
            let mut v12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
            let info = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queues)
                .enabled_extension_names(&names)
                .push_next(&mut v12);
            let device = err("vkCreateDevice", instance.create_device(pd, &info, None))?;
            let queue = device.get_device_queue(qfi, 0);

            let mut render_node = 0;
            if has_drm {
                let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
                let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
                instance.get_physical_device_properties2(pd, &mut p2);
                if drm.has_render != 0 {
                    render_node = libc::makedev(drm.render_major as u32, drm.render_minor as u32) as u64;
                }
            }
            if render_node == 0 {
                let mut st: libc::stat = std::mem::zeroed();
                if libc::stat(c"/dev/dri/renderD128".as_ptr(), &mut st) == 0 {
                    render_node = st.st_rdev as u64;
                }
            }

            let mut tl = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
            let timeline =
                err("timeline", device.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut tl), None))?;
            let export = if has_export {
                let mut ex = vk::ExportSemaphoreCreateInfo::default().handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
                match device.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut ex), None) {
                    Ok(sem) => Some((ash::khr::external_semaphore_fd::Device::new(&instance, &device), sem)),
                    Err(e) => {
                        log::warn!("gpu: exportable semaphore: {e}");
                        None
                    }
                }
            } else {
                None
            };
            log::info!("gpu: sync_file fences {}", if export.is_some() { "on" } else { "off" });
            let sampler = err(
                "sampler",
                device.create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::LINEAR)
                        .min_filter(vk::Filter::LINEAR)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                    None,
                ),
            )?;
            let samplers = [sampler];
            let bindings = [vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .immutable_samplers(&samplers)];
            let dsl = err(
                "set layout",
                device.create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None),
            )?;
            let sets = [dsl];
            let ranges = [vk::PushConstantRange { stage_flags: vk::ShaderStageFlags::VERTEX, offset: 0, size: 32 }];
            let layout = err(
                "pipeline layout",
                device.create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&sets).push_constant_ranges(&ranges),
                    None,
                ),
            )?;
            let module = |spv: &[u8]| {
                let words = ash::util::read_spv(&mut std::io::Cursor::new(spv)).map_err(|e| e.to_string())?;
                err("shader", device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None))
            };
            let (vert, frag) = (module(VERT)?, module(FRAG)?);

            let mut gpu = Gpu {
                surface_fn: ash::khr::surface::Instance::new(&entry, &instance),
                android_fn: ash::khr::android_surface::Instance::new(&entry, &instance),
                swapchain_fn: ash::khr::swapchain::Device::new(&instance, &device),
                memfd_fn: ash::khr::external_memory_fd::Device::new(&instance, &device),
                timing_fn: has_timing.then(|| ash::google::display_timing::Device::new(&instance, &device)),
                mem_props: instance.get_physical_device_memory_properties(pd),
                _entry: entry,
                instance,
                pd,
                device,
                queue,
                qfi,
                timeline,
                export,
                dsl,
                layout,
                vert,
                frag,
                passes: Mutex::new(Vec::new()),
                retired: Mutex::new(Vec::new()),
                formats: Vec::new(),
                render_node,
            };
            gpu.formats = gpu.query_formats();
            log::info!("gpu: {} dmabuf format/modifier pairs, render node {render_node:#x}", gpu.formats.len());
            Ok(Arc::new(gpu))
        }
    }

    /// Format/modifier pairs that can be imported as a dmabuf and sampled.
    fn query_formats(&self) -> Vec<DmabufFormat> {
        let mut out = Vec::new();
        for (fourcc, format, _) in FORMATS {
            unsafe {
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
                let mut fp = vk::FormatProperties2::default().push_next(&mut list);
                self.instance.get_physical_device_format_properties2(self.pd, format, &mut fp);
                let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
                let mut fp = vk::FormatProperties2::default().push_next(&mut list);
                self.instance.get_physical_device_format_properties2(self.pd, format, &mut fp);
                for m in &mods {
                    let features = m.drm_format_modifier_tiling_features;
                    if !features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
                        || !self.importable(format, m.drm_format_modifier, vk::ImageUsageFlags::SAMPLED)
                    {
                        continue;
                    }
                    let transfer = features.contains(vk::FormatFeatureFlags::TRANSFER_SRC)
                        && self.importable(
                            format,
                            m.drm_format_modifier,
                            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC,
                        );
                    out.push(DmabufFormat {
                        fourcc,
                        modifier: m.drm_format_modifier,
                        planes: m.drm_format_modifier_plane_count,
                        transfer,
                    });
                }
            }
        }
        out
    }

    /// Whether a dmabuf with this format and modifier imports for `usage`.
    fn importable(&self, format: vk::Format, modifier: u64, usage: vk::ImageUsageFlags) -> bool {
        let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut ext =
            vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .push_next(&mut drm)
            .push_next(&mut ext);
        let mut eprops = vk::ExternalImageFormatProperties::default();
        let mut props = vk::ImageFormatProperties2::default().push_next(&mut eprops);
        let ok = unsafe { self.instance.get_physical_device_image_format_properties2(self.pd, &info, &mut props) }.is_ok();
        ok && eprops.external_memory_properties.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    }

    /// Whether binary semaphores can be exported as sync_files.
    unsafe fn sync_fd_exportable(instance: &ash::Instance, pd: vk::PhysicalDevice) -> bool {
        let info = vk::PhysicalDeviceExternalSemaphoreInfo::default().handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let mut props = vk::ExternalSemaphoreProperties::default();
        unsafe { instance.get_physical_device_external_semaphore_properties(pd, &info, &mut props) };
        props.external_semaphore_features.contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE)
    }

    /// The semaphore a submit should also signal, for `fence`.
    fn export_semaphore(&self) -> Option<vk::Semaphore> {
        self.export.as_ref().map(|(_, s)| *s)
    }

    /// A sync_file for the submit that just signalled `export_semaphore`
    /// (exporting resets the semaphore for the next one).
    fn fence(&self) -> Option<OwnedFd> {
        let (f, sem) = self.export.as_ref()?;
        let info = vk::SemaphoreGetFdInfoKHR::default().semaphore(*sem).handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        match unsafe { f.get_semaphore_fd(&info) } {
            // -1: already signalled.
            Ok(fd) if fd >= 0 => Some(unsafe { OwnedFd::from_raw_fd(fd) }),
            Ok(_) => None,
            Err(e) => {
                log::warn!("gpu: exporting a fence: {e}");
                None
            }
        }
    }

    /// The highest serial the GPU has finished.
    fn completed(&self) -> u64 {
        unsafe { self.device.get_semaphore_counter_value(self.timeline) }.unwrap_or(0)
    }

    /// Blocks until `serial` has finished (bounded, so a hung GPU cannot
    /// wedge the compositor forever).
    fn wait(&self, serial: u64) {
        if serial == 0 {
            return;
        }
        let (sems, vals) = ([self.timeline], [serial]);
        let info = vk::SemaphoreWaitInfo::default().semaphores(&sems).values(&vals);
        if let Err(e) = unsafe { self.device.wait_semaphores(&info, 2_000_000_000) } {
            log::warn!("gpu: waiting for frame {serial}: {e}");
        }
    }

    fn retire(&self, serial: u64, r: Retired) {
        if serial <= self.completed() {
            self.destroy(r);
        } else {
            self.retired.lock().unwrap_or_else(|e| e.into_inner()).push((serial, r));
        }
    }

    fn destroy(&self, r: Retired) {
        unsafe {
            match r {
                Retired::Image(image, view, memory) => {
                    self.device.destroy_image_view(view, None);
                    self.device.destroy_image(image, None);
                    self.device.free_memory(memory, None);
                }
                Retired::Buffer(buffer, memory) => {
                    self.device.destroy_buffer(buffer, None);
                    self.device.free_memory(memory, None);
                }
            }
        }
    }

    fn collect(&self) {
        let done = self.completed();
        let ready: Vec<Retired> = {
            let mut list = self.retired.lock().unwrap_or_else(|e| e.into_inner());
            let (ready, keep) = std::mem::take(&mut *list).into_iter().partition(|(s, _)| *s <= done);
            *list = keep;
            ready.into_iter().map(|(_, r)| r).collect()
        };
        for r in ready {
            self.destroy(r);
        }
    }

    fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|&i| {
            bits & (1 << i) != 0 && self.mem_props.memory_types[i as usize].property_flags.contains(flags)
        })
    }

    /// A host-visible, coherent buffer, mapped.
    fn host_buffer(&self, size: u64, usage: vk::BufferUsageFlags) -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8), String> {
        unsafe {
            let d = &self.device;
            let buffer = err("buffer", d.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage), None))?;
            let req = d.get_buffer_memory_requirements(buffer);
            let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let Some(ti) = self.memory_type(req.memory_type_bits, host) else {
                d.destroy_buffer(buffer, None);
                return Err("no host-visible memory".into());
            };
            let alloc = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ti);
            let memory = match d.allocate_memory(&alloc, None) {
                Ok(m) => m,
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(format!("buffer memory: {e}"));
                }
            };
            let ptr = d
                .bind_buffer_memory(buffer, memory, 0)
                .and_then(|_| d.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()));
            match ptr {
                Ok(p) => Ok((buffer, memory, p as *mut u8)),
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    d.free_memory(memory, None);
                    Err(format!("map: {e}"))
                }
            }
        }
    }

    fn view(&self, image: vk::Image, format: vk::Format, opaque: bool) -> Result<vk::ImageView, String> {
        let a = if opaque { vk::ComponentSwizzle::ONE } else { vk::ComponentSwizzle::IDENTITY };
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(vk::ComponentMapping { a, ..Default::default() })
            .subresource_range(COLOR);
        err("image view", unsafe { self.device.create_image_view(&info, None) })
    }

    /// The render pass and pipeline for a swapchain format.
    fn pass(&self, format: vk::Format, readback: bool) -> Result<(vk::RenderPass, vk::Pipeline), String> {
        let mut passes = self.passes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = passes.iter().find(|p| p.format == format && p.readback == readback) {
            return Ok((p.render_pass, p.pipeline));
        }
        unsafe {
            let d = &self.device;
            let final_layout =
                if readback { vk::ImageLayout::TRANSFER_SRC_OPTIMAL } else { vk::ImageLayout::PRESENT_SRC_KHR };
            let atts = [vk::AttachmentDescription::default()
                .format(format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(final_layout)];
            let color = [vk::AttachmentReference { attachment: 0, layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL }];
            let subpasses =
                [vk::SubpassDescription::default().pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS).color_attachments(&color)];
            let mut deps = vec![vk::SubpassDependency {
                src_subpass: vk::SUBPASS_EXTERNAL,
                dst_subpass: 0,
                src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                dst_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                src_access_mask: vk::AccessFlags::empty(),
                dst_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                dependency_flags: vk::DependencyFlags::empty(),
            }];
            if readback {
                deps.push(vk::SubpassDependency {
                    src_subpass: 0,
                    dst_subpass: vk::SUBPASS_EXTERNAL,
                    src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    dst_stage_mask: vk::PipelineStageFlags::TRANSFER,
                    src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    dst_access_mask: vk::AccessFlags::TRANSFER_READ,
                    dependency_flags: vk::DependencyFlags::empty(),
                });
            }
            let rp = vk::RenderPassCreateInfo::default().attachments(&atts).subpasses(&subpasses).dependencies(&deps);
            let render_pass = err("render pass", d.create_render_pass(&rp, None))?;

            let stages = [
                vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(self.vert).name(c"main"),
                vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(self.frag).name(c"main"),
            ];
            let vi = vk::PipelineVertexInputStateCreateInfo::default();
            let ia = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
            let vp = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
            let rs = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::NONE)
                .line_width(1.0);
            let ms = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let blend = [vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD)
                .color_write_mask(vk::ColorComponentFlags::RGBA)];
            let cb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend);
            let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let ds = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
            let info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vi)
                .input_assembly_state(&ia)
                .viewport_state(&vp)
                .rasterization_state(&rs)
                .multisample_state(&ms)
                .color_blend_state(&cb)
                .dynamic_state(&ds)
                .layout(self.layout)
                .render_pass(render_pass)
                .subpass(0);
            let pipeline = match d.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) {
                Ok(p) => p[0],
                Err((_, e)) => {
                    d.destroy_render_pass(render_pass, None);
                    return Err(format!("pipeline: {e}"));
                }
            };
            passes.push(Pass { format, readback, render_pass, pipeline });
            Ok((render_pass, pipeline))
        }
    }

    /// Imports a client's dmabuf planes as a sampled image. All planes must
    /// live in one buffer object (as Mesa allocates CCS aux planes).
    pub fn import(
        self: &Arc<Self>,
        planes: Vec<(OwnedFd, u32, u32)>,
        width: i32,
        height: i32,
        fourcc: u32,
        modifier: u64,
        y_invert: bool,
    ) -> Result<Dmabuf, String> {
        let (_, format, opaque) = *FORMATS.iter().find(|f| f.0 == fourcc).ok_or("unsupported format")?;
        let entry = *self.formats.iter().find(|f| f.fourcc == fourcc && f.modifier == modifier).ok_or("unsupported modifier")?;
        let mut usage = vk::ImageUsageFlags::SAMPLED;
        if entry.transfer {
            usage |= vk::ImageUsageFlags::TRANSFER_SRC;
        }
        let first = &planes.first().ok_or("no planes")?.0;
        let ino = |fd: &OwnedFd| unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            (libc::fstat(fd.as_raw_fd(), &mut st) == 0).then_some((st.st_dev, st.st_ino))
        };
        if planes.iter().any(|p| ino(&p.0).is_none() || ino(&p.0) != ino(first)) {
            return Err("planes in separate buffers".into());
        }
        let size = unsafe { libc::lseek(first.as_raw_fd(), 0, libc::SEEK_END) };
        if size > 0 {
            let (_, off, stride) = planes[0];
            if off as i64 + stride as i64 * height as i64 > size as i64 || planes.iter().any(|p| p.1 as i64 >= size as i64) {
                return Err("planes outside the buffer".into());
            }
        }
        let d = &self.device;
        unsafe {
            let layouts: Vec<vk::SubresourceLayout> = planes
                .iter()
                .map(|&(_, offset, stride)| vk::SubresourceLayout {
                    offset: offset as u64,
                    size: 0,
                    row_pitch: stride as u64,
                    array_pitch: 0,
                    depth_pitch: 0,
                })
                .collect();
            let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                .drm_format_modifier(modifier)
                .plane_layouts(&layouts);
            let mut ext =
                vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D { width: width as u32, height: height as u32, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut ext)
                .push_next(&mut explicit);
            let image = err("import image", d.create_image(&info, None))?;
            let fail = |e: String| {
                d.destroy_image(image, None);
                Err(e)
            };
            let fd = libc::dup(first.as_raw_fd());
            if fd < 0 {
                return fail("dup".into());
            }
            let mut fdp = vk::MemoryFdPropertiesKHR::default();
            if let Err(e) = self.memfd_fn.get_memory_fd_properties(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, fd, &mut fdp) {
                libc::close(fd);
                return fail(format!("dmabuf properties: {e}"));
            }
            let req = d.get_image_memory_requirements(image);
            let Some(ti) = self.memory_type(req.memory_type_bits & fdp.memory_type_bits, vk::MemoryPropertyFlags::empty()) else {
                libc::close(fd);
                return fail("no memory type for the dmabuf".into());
            };
            let mut import =
                vk::ImportMemoryFdInfoKHR::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT).fd(fd);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ti)
                .push_next(&mut import)
                .push_next(&mut dedicated);
            // On success Vulkan owns `fd`.
            let memory = match d.allocate_memory(&alloc, None) {
                Ok(m) => m,
                Err(e) => {
                    libc::close(fd);
                    return fail(format!("import memory: {e}"));
                }
            };
            if let Err(e) = d.bind_image_memory(image, memory, 0) {
                d.free_memory(memory, None);
                return fail(format!("bind: {e}"));
            }
            let view = match self.view(image, format, opaque) {
                Ok(v) => v,
                Err(e) => {
                    d.free_memory(memory, None);
                    return fail(e);
                }
            };
            let poll_fd = OwnedFd::from_raw_fd(libc::dup(first.as_raw_fd()).max(0));
            let (_, offset, stride) = planes[0];
            drop(planes);
            Ok(Dmabuf {
                gpu: self.clone(),
                width,
                height,
                y_invert,
                image,
                view,
                memory,
                poll_fd,
                last_use: Cell::new(0),
                format: entry,
                bgra: format == vk::Format::B8G8R8A8_UNORM,
                opaque,
                offset,
                stride,
                size: size.max(0) as u64,
                ahb: Cell::new(std::ptr::null_mut()),
                sf_holds: Cell::new(0),
            })
        }
    }
}

const COLOR: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

/// A client dmabuf imported as an image. Dropped with its wl_buffer and the
/// last surface showing it; the image is destroyed once the GPU is done.
pub struct Dmabuf {
    gpu: Arc<Gpu>,
    pub width: i32,
    pub height: i32,
    pub y_invert: bool,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    /// For readiness: a dmabuf polls readable once its writers are done.
    poll_fd: OwnedFd,
    /// Last serial that sampled the image.
    last_use: Cell<u64>,
    format: DmabufFormat,
    bgra: bool,
    opaque: bool,
    offset: u32,
    stride: u32,
    size: u64,
    /// The buffer wrapped for SurfaceFlinger (direct mode), once needed.
    ahb: Cell<*mut AHardwareBuffer>,
    /// Transactions that gave the buffer to SurfaceFlinger and whose
    /// release is still outstanding.
    sf_holds: Cell<u32>,
}

impl Dmabuf {
    /// Whether both the GPU and SurfaceFlinger are done with the buffer.
    fn idle(&self, completed: u64) -> bool {
        self.last_use.get() <= completed && self.sf_holds.get() == 0
    }

    /// The buffer as an AHardwareBuffer, wrapping it on first use.
    fn hardware_buffer(&self) -> Option<*mut AHardwareBuffer> {
        if self.ahb.get().is_null() && self.size > 0 {
            let plane = crate::direct::Plane { fd: &self.poll_fd, offset: self.offset, stride: self.stride, size: self.size };
            if let Some(b) = crate::direct::wrap(&plane, self.width, self.height, self.format.fourcc, self.format.modifier) {
                self.ahb.set(b);
            }
        }
        let b = self.ahb.get();
        (!b.is_null()).then_some(b)
    }

    /// Whether the client's rendering into the buffer has finished
    /// (implicit sync: the dmabuf's write fences have signalled).
    pub fn ready(&self) -> bool {
        let mut p = libc::pollfd { fd: self.poll_fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut p, 1, 0) == 1 && p.revents & libc::POLLIN != 0 }
    }
}

impl Drop for Dmabuf {
    fn drop(&mut self) {
        // SurfaceFlinger keeps its own reference while it shows the buffer.
        crate::direct::release(self.ahb.get());
        self.gpu.retire(self.last_use.get(), Retired::Image(self.image, self.view, self.memory));
    }
}

// ---- per-window swapchain ---------------------------------------------

/// A window's swapchain and per-frame resources. Lives in the window's
/// `NativeSlot`, so it is torn down (on the UI thread) before the
/// ANativeWindow is released.
pub struct Target {
    gpu: Arc<Gpu>,
    surface: vk::SurfaceKHR,
    swapchain: vk::SwapchainKHR,
    format: vk::Format,
    extent: (i32, i32),
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    /// Signalled by each image's rendering, waited on by its present.
    rendered: Vec<vk::Semaphore>,
    acquired: vk::Semaphore,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    descriptors: vk::DescriptorPool,
    render_pass: vk::RenderPass,
    pipeline: vk::Pipeline,
    readable: bool,
    readback: Option<(vk::Buffer, vk::DeviceMemory, *mut u8, u64)>,
    /// Caption rows copied by the last frame that read them back: first
    /// row, rows, width, and whether they are BGRA; and that frame.
    band: (i32, i32, i32, bool),
    band_serial: u64,
    last_serial: u64,
    /// The last submit's sync_file, for the Renderer to take.
    fence: Option<OwnedFd>,
    /// The swapchain must be recreated before the next frame.
    stale: bool,
    /// Present ids: the last one queued, and the last the display showed.
    presented: u32,
    shown: u32,
    /// The display's refresh period and when it last showed one of our
    /// frames (CLOCK_MONOTONIC ns; 0 if unknown), to wait for its next
    /// vsync instead of polling.
    refresh_ns: u64,
    last_shown_ns: u64,
    /// The child layer for direct mode, created when first needed.
    layer: Option<Layer>,
    /// Unique per Target, so direct-mode bookkeeping notices a new one.
    pub generation: u64,
}

static TARGETS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

// Only used under the slot's mutex.
unsafe impl Send for Target {}

impl Target {
    fn new(gpu: Arc<Gpu>, win: *mut ANativeWindow) -> Result<Target, String> {
        unsafe {
            let d = &gpu.device;
            let info = vk::AndroidSurfaceCreateInfoKHR::default().window(win as *mut _);
            let surface = err("android surface", gpu.android_fn.create_android_surface(&info, None))?;
            let supported = gpu.surface_fn.get_physical_device_surface_support(gpu.pd, gpu.qfi, surface).unwrap_or(false);
            if !supported {
                gpu.surface_fn.destroy_surface(surface, None);
                return Err("queue cannot present to the window".into());
            }
            let pool = err(
                "command pool",
                d.create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                        .queue_family_index(gpu.qfi),
                    None,
                ),
            )?;
            let cmd = err(
                "command buffer",
                d.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                ),
            )?[0];
            let sizes = [vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: MAX_DRAWS }];
            let descriptors = err(
                "descriptor pool",
                d.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(MAX_DRAWS).pool_sizes(&sizes), None),
            )?;
            let acquired = err("semaphore", d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None))?;
            Ok(Target {
                gpu: gpu.clone(),
                surface,
                swapchain: vk::SwapchainKHR::null(),
                format: vk::Format::UNDEFINED,
                extent: (0, 0),
                images: Vec::new(),
                views: Vec::new(),
                framebuffers: Vec::new(),
                rendered: Vec::new(),
                acquired,
                pool,
                cmd,
                descriptors,
                render_pass: vk::RenderPass::null(),
                pipeline: vk::Pipeline::null(),
                readable: false,
                readback: None,
                band: (0, 0, 0, false),
                band_serial: 0,
                last_serial: 0,
                fence: None,
                stale: true,
                presented: 0,
                shown: 0,
                refresh_ns: 0,
                last_shown_ns: 0,
                layer: None,
                generation: TARGETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            })
        }
    }

    /// (Re)creates the swapchain at `w` x `h`. Callers have waited for the
    /// last frame.
    fn configure(&mut self, w: i32, h: i32) -> Result<(), String> {
        let gpu = self.gpu.clone();
        unsafe {
            let caps = err("surface caps", gpu.surface_fn.get_physical_device_surface_capabilities(gpu.pd, self.surface))?;
            let formats = err("surface formats", gpu.surface_fn.get_physical_device_surface_formats(gpu.pd, self.surface))?;
            let pick = [vk::Format::R8G8B8A8_UNORM, vk::Format::B8G8R8A8_UNORM]
                .iter()
                .find_map(|f| formats.iter().find(|s| s.format == *f))
                .ok_or("no RGBA swapchain format")?;
            let extent = vk::Extent2D {
                width: (w as u32).clamp(caps.min_image_extent.width, caps.max_image_extent.width.max(1)),
                height: (h as u32).clamp(caps.min_image_extent.height, caps.max_image_extent.height.max(1)),
            };
            let readable = caps.supported_usage_flags.contains(vk::ImageUsageFlags::TRANSFER_SRC);
            let mut usage = vk::ImageUsageFlags::COLOR_ATTACHMENT;
            if readable {
                usage |= vk::ImageUsageFlags::TRANSFER_SRC;
            }
            let mut count = caps.min_image_count.max(3);
            if caps.max_image_count > 0 {
                count = count.min(caps.max_image_count);
            }
            let supported = caps.supported_composite_alpha.as_raw();
            let alpha = [vk::CompositeAlphaFlagsKHR::OPAQUE, vk::CompositeAlphaFlagsKHR::INHERIT]
                .into_iter()
                .find(|a| caps.supported_composite_alpha.contains(*a))
                .unwrap_or(vk::CompositeAlphaFlagsKHR::from_raw(supported & supported.wrapping_neg()));
            let info = vk::SwapchainCreateInfoKHR::default()
                .surface(self.surface)
                .min_image_count(count)
                .image_format(pick.format)
                .image_color_space(pick.color_space)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(usage)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                .pre_transform(caps.current_transform)
                .composite_alpha(alpha)
                .present_mode(vk::PresentModeKHR::FIFO)
                .clipped(true)
                .old_swapchain(self.swapchain);
            let swapchain = err("swapchain", gpu.swapchain_fn.create_swapchain(&info, None))?;
            self.destroy_swapchain();
            self.swapchain = swapchain;
            if let Some(timing) = &gpu.timing_fn {
                self.refresh_ns = timing.get_refresh_cycle_duration(swapchain).map_or(0, |r| r.refresh_duration);
            }
            self.format = pick.format;
            self.extent = (extent.width as i32, extent.height as i32);
            self.readable = readable;
            (self.render_pass, self.pipeline) = gpu.pass(pick.format, readable)?;
            self.images = err("swapchain images", gpu.swapchain_fn.get_swapchain_images(swapchain))?;
            for &image in &self.images {
                let view = gpu.view(image, pick.format, false)?;
                self.views.push(view);
                let atts = [view];
                let fb = vk::FramebufferCreateInfo::default()
                    .render_pass(self.render_pass)
                    .attachments(&atts)
                    .width(extent.width)
                    .height(extent.height)
                    .layers(1);
                self.framebuffers.push(err("framebuffer", gpu.device.create_framebuffer(&fb, None))?);
                self.rendered.push(err("semaphore", gpu.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None))?);
            }
            self.stale = false;
            (self.presented, self.shown) = (0, 0);
            log::info!("gpu: swapchain {}x{} {:?}, {} images", extent.width, extent.height, pick.format, self.images.len());
            Ok(())
        }
    }

    fn destroy_swapchain(&mut self) {
        let d = &self.gpu.device;
        unsafe {
            for fb in self.framebuffers.drain(..) {
                d.destroy_framebuffer(fb, None);
            }
            for v in self.views.drain(..) {
                d.destroy_image_view(v, None);
            }
            for s in self.rendered.drain(..) {
                d.destroy_semaphore(s, None);
            }
            self.images.clear();
            if self.swapchain != vk::SwapchainKHR::null() {
                self.gpu.swapchain_fn.destroy_swapchain(self.swapchain, None);
                self.swapchain = vk::SwapchainKHR::null();
            }
        }
    }

    /// A buffer for `size` bytes of caption readback.
    fn ensure_readback(&mut self, size: u64) -> Result<(), String> {
        if self.readback.is_some_and(|r| r.3 >= size) {
            return Ok(());
        }
        if let Some((b, m, _, _)) = self.readback.take() {
            self.gpu.destroy(Retired::Buffer(b, m));
        }
        let (b, m, p) = self.gpu.host_buffer(size, vk::BufferUsageFlags::TRANSFER_DST)?;
        self.readback = Some((b, m, p, size));
        Ok(())
    }

    /// The caption rows of frame `serial`, as RGBA (0xAABBGGRR) pixels.
    fn band_pixels(&self, serial: u64) -> Option<(Vec<u32>, i32, i32, i32)> {
        let (y0, rows, width, bgra) = self.band;
        let (_, _, ptr, _) = self.readback?;
        if self.band_serial != serial || rows <= 0 || width <= 0 {
            return None;
        }
        let n = (rows * width) as usize;
        let mut px = unsafe { std::slice::from_raw_parts(ptr as *const u32, n) }.to_vec();
        if bgra {
            for v in &mut px {
                *v = (*v & 0xff00ff00) | ((*v >> 16) & 0xff) | ((*v & 0xff) << 16);
            }
        }
        Some((px, width, y0, rows))
    }

    /// Draws one frame: uploads, the quads, then the caption readback
    /// (none if `band` has no rows).
    /// Returns false if nothing was drawn: no image is free yet (the
    /// display still holds them) or the swapchain is out of date.
    fn frame(&mut self, draws: &[Draw], band: (i32, i32), serial: u64) -> Result<bool, String> {
        let gpu = self.gpu.clone();
        let d = &gpu.device;
        let (ew, eh) = self.extent;
        let y0 = band.0.clamp(0, eh);
        let rows = band.1.clamp(0, eh - y0);
        let read = self.readable && rows > 0;
        if read {
            self.ensure_readback((ew * rows * 4) as u64)?;
        }
        unsafe {
            // Never block: a window waiting for its display must not hold up
            // the others. It is retried after the next vsync (`retry_ms`).
            // With display timing we know how many frames are still queued
            // and don't even ask; Android logs every acquire that would block.
            if let Some(timing) = &gpu.timing_fn {
                if let Ok(past) = timing.get_past_presentation_timing(self.swapchain) {
                    if let Some(id) = past.iter().map(|t| t.present_id).max() {
                        self.shown = self.shown.max(id);
                    }
                    if let Some(t) = past.iter().map(|t| t.actual_present_time).max() {
                        self.last_shown_ns = self.last_shown_ns.max(t);
                    }
                }
                if self.presented.wrapping_sub(self.shown) >= MAX_QUEUED {
                    return Ok(false);
                }
            }
            let idx = match gpu.swapchain_fn.acquire_next_image(self.swapchain, 0, self.acquired, vk::Fence::null()) {
                Ok((i, _)) => i as usize,
                Err(vk::Result::NOT_READY | vk::Result::TIMEOUT) => return Ok(false),
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.stale = true;
                    return Ok(false);
                }
                Err(e) => return Err(format!("acquire: {e}")),
            };
            let cmd = self.cmd;
            err("reset pool", d.reset_descriptor_pool(self.descriptors, vk::DescriptorPoolResetFlags::empty()))?;
            err("reset cmd", d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()))?;
            err(
                "begin",
                d.begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                ),
            )?;

            // Uploads of changed shm textures.
            for draw in draws {
                if let Source::Shm { image, staging, width, upload: Some(r), fresh, .. } = draw.source {
                    // Only the changed part; the rest of the image is kept.
                    let old = if fresh { vk::ImageLayout::UNDEFINED } else { vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL };
                    barrier(
                        d,
                        cmd,
                        image,
                        (old, vk::ImageLayout::TRANSFER_DST_OPTIMAL),
                        (vk::PipelineStageFlags::FRAGMENT_SHADER, vk::PipelineStageFlags::TRANSFER),
                        (vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE),
                        None,
                    );
                    let region = vk::BufferImageCopy::default()
                        .buffer_offset((r[1] as u64 * width as u64 + r[0] as u64) * 4)
                        .buffer_row_length(width as u32)
                        .image_subresource(LAYER)
                        .image_offset(vk::Offset3D { x: r[0], y: r[1], z: 0 })
                        .image_extent(vk::Extent3D { width: (r[2] - r[0]) as u32, height: (r[3] - r[1]) as u32, depth: 1 });
                    d.cmd_copy_buffer_to_image(cmd, staging, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
                    barrier(
                        d,
                        cmd,
                        image,
                        (vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                        (vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER),
                        (vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::SHADER_READ),
                        None,
                    );
                }
            }
            // Client dmabufs: acquire from the client's (foreign) queue.
            let mut foreign: Vec<vk::Image> = Vec::new();
            for draw in draws {
                if let Source::Dmabuf { image, .. } = draw.source {
                    if !foreign.contains(&image) {
                        foreign.push(image);
                    }
                }
            }
            for &image in &foreign {
                barrier(
                    d,
                    cmd,
                    image,
                    (vk::ImageLayout::GENERAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                    (vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::FRAGMENT_SHADER),
                    (vk::AccessFlags::empty(), vk::AccessFlags::SHADER_READ),
                    Some((vk::QUEUE_FAMILY_FOREIGN_EXT, gpu.qfi)),
                );
            }

            let clear = [vk::ClearValue { color: vk::ClearColorValue { float32: BACKGROUND } }];
            let area = vk::Rect2D { offset: vk::Offset2D::default(), extent: vk::Extent2D { width: ew as u32, height: eh as u32 } };
            let rp = vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                .framebuffer(self.framebuffers[idx])
                .render_area(area)
                .clear_values(&clear);
            d.cmd_begin_render_pass(cmd, &rp, vk::SubpassContents::INLINE);
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            let viewport = vk::Viewport { x: 0.0, y: 0.0, width: ew as f32, height: eh as f32, min_depth: 0.0, max_depth: 1.0 };
            d.cmd_set_viewport(cmd, 0, &[viewport]);
            d.cmd_set_scissor(cmd, 0, &[area]);
            let layouts = [gpu.dsl];
            for draw in draws.iter().take(MAX_DRAWS as usize) {
                let alloc = vk::DescriptorSetAllocateInfo::default().descriptor_pool(self.descriptors).set_layouts(&layouts);
                let Ok(sets) = d.allocate_descriptor_sets(&alloc) else { break };
                let info = [vk::DescriptorImageInfo::default()
                    .image_view(draw.source.view())
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
                let write = vk::WriteDescriptorSet::default()
                    .dst_set(sets[0])
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&info);
                d.update_descriptor_sets(&[write], &[]);
                d.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::GRAPHICS, gpu.layout, 0, &sets, &[]);
                let [x0, y0, x1, y1] = draw.dst;
                let ndc = |v: i32, size: i32| v as f32 / size as f32 * 2.0 - 1.0;
                let pc: [f32; 8] =
                    [ndc(x0, ew), ndc(y0, eh), ndc(x1, ew), ndc(y1, eh), draw.uv[0], draw.uv[1], draw.uv[2], draw.uv[3]];
                let bytes = std::slice::from_raw_parts(pc.as_ptr() as *const u8, 32);
                d.cmd_push_constants(cmd, gpu.layout, vk::ShaderStageFlags::VERTEX, 0, bytes);
                d.cmd_draw(cmd, 4, 1, 0, 0);
            }
            d.cmd_end_render_pass(cmd);

            if self.readable {
                let image = self.images[idx];
                if read {
                    let (buffer, ..) = self.readback.unwrap();
                    let region = vk::BufferImageCopy::default()
                        .image_subresource(LAYER)
                        .image_offset(vk::Offset3D { x: 0, y: y0, z: 0 })
                        .image_extent(vk::Extent3D { width: ew as u32, height: rows as u32, depth: 1 });
                    d.cmd_copy_image_to_buffer(cmd, image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buffer, &[region]);
                    let host = vk::BufferMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::HOST_READ)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .buffer(buffer)
                        .size(vk::WHOLE_SIZE);
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::HOST,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[host],
                        &[],
                    );
                }
                barrier(
                    d,
                    cmd,
                    image,
                    (vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::ImageLayout::PRESENT_SRC_KHR),
                    (vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::BOTTOM_OF_PIPE),
                    (vk::AccessFlags::TRANSFER_READ, vk::AccessFlags::empty()),
                    None,
                );
            }
            // Hand the client's buffers back.
            for &image in &foreign {
                barrier(
                    d,
                    cmd,
                    image,
                    (vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::ImageLayout::GENERAL),
                    (vk::PipelineStageFlags::FRAGMENT_SHADER, vk::PipelineStageFlags::BOTTOM_OF_PIPE),
                    (vk::AccessFlags::SHADER_READ, vk::AccessFlags::empty()),
                    Some((gpu.qfi, vk::QUEUE_FAMILY_FOREIGN_EXT)),
                );
            }
            err("end", d.end_command_buffer(cmd))?;

            let waits = [self.acquired];
            let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT | vk::PipelineStageFlags::TRANSFER];
            let mut signals = vec![self.rendered[idx], gpu.timeline];
            let mut values = vec![0, serial];
            if let Some(sem) = gpu.export_semaphore() {
                signals.push(sem);
                values.push(0);
            }
            let mut tl = vk::TimelineSemaphoreSubmitInfo::default().wait_semaphore_values(&[0]).signal_semaphore_values(&values);
            let cmds = [cmd];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&cmds)
                .signal_semaphores(&signals)
                .push_next(&mut tl);
            err("submit", d.queue_submit(gpu.queue, &[submit], vk::Fence::null()))?;
            self.fence = gpu.fence();
            self.last_serial = serial;
            if read {
                self.band = (y0, rows, ew, self.format == vk::Format::B8G8R8A8_UNORM);
                self.band_serial = serial;
            }

            let present_waits = [self.rendered[idx]];
            let swapchains = [self.swapchain];
            let indices = [idx as u32];
            self.presented = self.presented.wrapping_add(1);
            let times = [vk::PresentTimeGOOGLE { present_id: self.presented, desired_present_time: 0 }];
            let mut times = vk::PresentTimesInfoGOOGLE::default().times(&times);
            let mut present =
                vk::PresentInfoKHR::default().wait_semaphores(&present_waits).swapchains(&swapchains).image_indices(&indices);
            if gpu.timing_fn.is_some() {
                present = present.push_next(&mut times);
            }
            match gpu.swapchain_fn.queue_present(gpu.queue, &present) {
                Ok(false) => {}
                Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.stale = true,
                Err(e) => log::warn!("gpu: present: {e}"),
            }
        }
        Ok(true)
    }
}

impl Target {
    /// When to try again after `frame` found no image free: just after the
    /// display's next vsync, when it may have shown a frame. Shortly after
    /// a vsync the timing may still lag, so then it polls (as without
    /// timing).
    fn retry_ms(&self) -> i32 {
        const MS: u64 = 1_000_000;
        let (refresh, last) = (self.refresh_ns, self.last_shown_ns);
        let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        let now = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64;
        if refresh < 4 * MS || last == 0 || now < last {
            return 2;
        }
        let since = (now - last) % refresh;
        if since < 3 * MS {
            return 1;
        }
        ((refresh - since + MS).div_ceil(MS) as i32).min(50)
    }

    fn layer(&mut self, win: *mut ANativeWindow) -> Option<&Layer> {
        if self.layer.is_none() {
            self.layer = Layer::new(win);
        }
        self.layer.as_ref()
    }

    /// Direct mode's stand-in for the composited caption: copies the band's
    /// window rows (`band`: first row, rows; `width` px wide) out of the
    /// client's buffer, which lies at `origin` in window px. Callers have
    /// waited for the last frame.
    fn read_direct(&mut self, d: &Dmabuf, origin: (i32, i32), width: i32, band: (i32, i32), serial: u64) -> Result<bool, String> {
        let y0 = band.0.max(origin.1);
        let rows = (band.0 + band.1).min(origin.1 + d.height) - y0;
        if rows <= 0 || width <= 0 || !d.format.transfer {
            return Ok(false);
        }
        let gpu = self.gpu.clone();
        let dev = &gpu.device;
        self.ensure_readback((width * rows * 4) as u64)?;
        let (buffer, ..) = self.readback.unwrap();
        let cmd = self.cmd;
        unsafe {
            err("reset cmd", dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()))?;
            err(
                "begin",
                dev.begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                ),
            )?;
            barrier(
                dev,
                cmd,
                d.image,
                (vk::ImageLayout::GENERAL, vk::ImageLayout::TRANSFER_SRC_OPTIMAL),
                (vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER),
                (vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_READ),
                Some((vk::QUEUE_FAMILY_FOREIGN_EXT, gpu.qfi)),
            );
            let region = vk::BufferImageCopy::default()
                .image_subresource(LAYER)
                .image_offset(vk::Offset3D { x: -origin.0, y: y0 - origin.1, z: 0 })
                .image_extent(vk::Extent3D { width: width as u32, height: rows as u32, depth: 1 });
            dev.cmd_copy_image_to_buffer(cmd, d.image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buffer, &[region]);
            barrier(
                dev,
                cmd,
                d.image,
                (vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::ImageLayout::GENERAL),
                (vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::BOTTOM_OF_PIPE),
                (vk::AccessFlags::TRANSFER_READ, vk::AccessFlags::empty()),
                Some((gpu.qfi, vk::QUEUE_FAMILY_FOREIGN_EXT)),
            );
            let host = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(buffer)
                .size(vk::WHOLE_SIZE);
            dev.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                &[host],
                &[],
            );
            err("end", dev.end_command_buffer(cmd))?;
            let mut signals = vec![gpu.timeline];
            let mut values = vec![serial];
            if let Some(sem) = gpu.export_semaphore() {
                signals.push(sem);
                values.push(0);
            }
            let mut tl = vk::TimelineSemaphoreSubmitInfo::default().signal_semaphore_values(&values);
            let cmds = [cmd];
            let submit = vk::SubmitInfo::default().command_buffers(&cmds).signal_semaphores(&signals).push_next(&mut tl);
            err("submit", dev.queue_submit(gpu.queue, &[submit], vk::Fence::null()))?;
            self.fence = gpu.fence();
        }
        self.last_serial = serial;
        self.band = (y0, rows, width, d.bgra);
        self.band_serial = serial;
        Ok(true)
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        let gpu = self.gpu.clone();
        self.layer = None;
        gpu.wait(self.last_serial);
        self.destroy_swapchain();
        unsafe {
            let d = &gpu.device;
            if let Some((b, m, _, _)) = self.readback.take() {
                gpu.destroy(Retired::Buffer(b, m));
            }
            d.destroy_semaphore(self.acquired, None);
            d.destroy_descriptor_pool(self.descriptors, None);
            d.destroy_command_pool(self.pool, None);
            gpu.surface_fn.destroy_surface(self.surface, None);
        }
    }
}

const LAYER: vk::ImageSubresourceLayers =
    vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 };

#[allow(clippy::too_many_arguments)]
unsafe fn barrier(
    d: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    layouts: (vk::ImageLayout, vk::ImageLayout),
    stages: (vk::PipelineStageFlags, vk::PipelineStageFlags),
    access: (vk::AccessFlags, vk::AccessFlags),
    queues: Option<(u32, u32)>,
) {
    let (src_q, dst_q) = queues.unwrap_or((vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED));
    let b = vk::ImageMemoryBarrier::default()
        .old_layout(layouts.0)
        .new_layout(layouts.1)
        .src_access_mask(access.0)
        .dst_access_mask(access.1)
        .src_queue_family_index(src_q)
        .dst_queue_family_index(dst_q)
        .image(image)
        .subresource_range(COLOR);
    unsafe { d.cmd_pipeline_barrier(cmd, stages.0, stages.1, vk::DependencyFlags::empty(), &[], &[], &[b]) };
}

// ---- the renderer ------------------------------------------------------

#[derive(Clone, Copy)]
enum Source {
    Dmabuf { image: vk::Image, view: vk::ImageView },
    /// `upload`: the part of `staging` (a whole texture, `width` px wide)
    /// to copy into the image first; `fresh`: the image has no contents yet.
    Shm { image: vk::Image, view: vk::ImageView, staging: vk::Buffer, width: i32, upload: Option<Rect>, fresh: bool },
}

impl Source {
    fn view(&self) -> vk::ImageView {
        match *self {
            Source::Dmabuf { view, .. } | Source::Shm { view, .. } => view,
        }
    }
}

struct Draw {
    source: Source,
    /// Destination in px: x0, y0, x1, y1.
    dst: [i32; 4],
    uv: [f32; 4],
}

/// What a window's caption band was drawn from. A frame that draws it from
/// the same looks the same there, so it needs no new readback.
struct BandKey {
    size: (i32, i32),
    caption: crate::Caption,
    /// Per surface drawn over the band: surface, destination, uv, commit.
    draws: Vec<(ObjectId, [i32; 4], [f32; 4], u64)>,
}

/// A wl_shm texture on the GPU, per surface.
struct ShmTex {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    staging: vk::Buffer,
    staging_mem: vk::DeviceMemory,
    ptr: *mut u8,
    size: (i32, i32),
    /// Texture serial in the staging buffer, which mirrors the texture.
    staged: u64,
    /// The part of the staging buffer not yet copied to the image.
    dirty: Option<Rect>,
    /// The image has been written (else its contents are undefined).
    written: bool,
    last_use: u64,
}

/// Compositor-thread state of the GPU path.
pub struct Renderer {
    pub gpu: Arc<Gpu>,
    shm: HashMap<ObjectId, ShmTex>,
    serial: u64,
    /// Submitted frames whose caption readback is still to be read.
    frames: Vec<(u64, u32)>,
    /// Per window, what its caption band was last read back from.
    bands: HashMap<u32, BandKey>,
    /// Client buffers and release points to give back once the GPU and
    /// SurfaceFlinger are done with them (a `Release` signals when dropped).
    releases: Vec<(Rc<Dmabuf>, Option<WlBuffer>, Option<Release>)>,
    /// A window was skipped and should be retried within this many ms
    /// (nothing will wake the event loop for it).
    retry: Option<i32>,
    /// sync_files of submitted frames not yet known to be complete.
    fences: Vec<(u64, OwnedFd)>,
    /// Acquire points a window waits for: an eventfd per point.
    acquire_waits: HashMap<(usize, u64), OwnedFd>,
    /// Points waited for in this pass (the others are dropped after it).
    acquire_wanted: Vec<(usize, u64)>,
    /// Dmabufs a window waits for (implicit sync: readable when done).
    implicit_waits: Vec<Rc<Dmabuf>>,
    /// Direct mode (see `direct.rs`): off with `direct=0`.
    direct_enabled: bool,
    shared: Arc<Shared>,
    direct: HashMap<u32, Direct>,
    /// SurfaceFlinger release fences, each for one hold of a buffer.
    sf_fences: Vec<(OwnedFd, Rc<Dmabuf>)>,
    placeholder: *mut AHardwareBuffer,
    next_token: u64,
}

/// A window's direct-mode layer, as last told to SurfaceFlinger.
#[derive(Default)]
struct Direct {
    /// The Target whose layer this describes.
    generation: u64,
    /// The client buffer on the layer.
    shown: Option<Rc<Dmabuf>>,
    /// The transaction not yet complete, if any: windows apply one at a
    /// time, which paces them to their display.
    in_flight: Option<u64>,
    /// When it was handed out: a window process may never apply it.
    in_flight_since: Option<std::time::Instant>,
    /// Per outstanding transaction, the buffer it took off the layer
    /// (released by that transaction's previous-release fence).
    replaced: HashMap<u64, Option<Rc<Dmabuf>>>,
}

impl Direct {
    /// Forgets the layer (it was destroyed with its Target): SurfaceFlinger
    /// drops its buffers with it.
    fn reset(&mut self) {
        for d in self.shown.take().into_iter().chain(self.replaced.drain().filter_map(|(_, d)| d)) {
            d.sf_holds.set(d.sf_holds.get().saturating_sub(1));
        }
        self.in_flight = None;
    }
}

impl Renderer {
    pub fn new(shared: Arc<Shared>, direct_enabled: bool) -> Result<Renderer, String> {
        let gpu = Gpu::new()?;
        let serial = gpu.completed();
        Ok(Renderer {
            gpu,
            shm: HashMap::new(),
            serial,
            frames: Vec::new(),
            bands: HashMap::new(),
            releases: Vec::new(),
            retry: None,
            fences: Vec::new(),
            acquire_waits: HashMap::new(),
            acquire_wanted: Vec::new(),
            implicit_waits: Vec::new(),
            direct_enabled,
            shared,
            direct: HashMap::new(),
            sf_fences: Vec::new(),
            placeholder: if direct_enabled { crate::direct::placeholder() } else { std::ptr::null_mut() },
            next_token: 1,
        })
    }

    /// Whether direct mode may be used, i.e. clients should prefer formats
    /// SurfaceFlinger takes as is.
    pub fn direct_enabled(&self) -> bool {
        self.direct_enabled
    }

    /// What the event loop should also wait for: fds that become readable
    /// when something here can move on (the GPU finishing the last submit,
    /// SurfaceFlinger releasing a buffer, a client's buffer becoming ready),
    /// and a timeout in ms (-1: none) for what has no fd.
    pub fn waits(&self) -> (Vec<RawFd>, i32) {
        let mut fds: Vec<RawFd> = self.sf_fences.iter().map(|(f, _)| f.as_raw_fd()).collect();
        fds.extend(self.acquire_waits.values().map(|f| f.as_raw_fd()));
        fds.extend(self.implicit_waits.iter().map(|d| d.poll_fd.as_raw_fd()));
        let mut timeout = self.retry.unwrap_or(-1);
        if self.serial > self.gpu.completed() {
            // The queue runs in order: the last submit finishing means all did.
            match self.fences.last().filter(|(s, _)| *s == self.serial) {
                Some((_, f)) => fds.push(f.as_raw_fd()),
                None => timeout = if timeout < 0 { 2 } else { timeout.min(2) },
            }
        }
        (fds, timeout)
    }

    /// Wake again within `ms` for a window that was skipped.
    fn retry_in(&mut self, ms: i32) {
        self.retry = Some(self.retry.map_or(ms, |r| r.min(ms)));
    }

    /// The fence of the first submit at or after `serial`, if any.
    fn fence_for(&self, serial: u64) -> Option<&OwnedFd> {
        self.fences.iter().find(|(s, _)| *s >= serial).map(|(_, f)| f)
    }

    /// A window is gone; its layer went with its Target.
    pub fn forget_window(&mut self, win: u32) {
        self.bands.remove(&win);
        if let Some(mut d) = self.direct.remove(&win) {
            d.reset();
        }
    }

    /// SurfaceFlinger completed a direct-mode transaction (`Cmd::DirectDone`).
    pub fn direct_done(&mut self, win: u32, token: u64, fence: Option<OwnedFd>) {
        let Some(state) = self.direct.get_mut(&win) else { return };
        if state.in_flight == Some(token) {
            state.in_flight = None;
        }
        let Some(Some(replaced)) = state.replaced.remove(&token) else { return };
        match fence {
            Some(f) => self.sf_fences.push((f, replaced)),
            // -1: the buffer was already released.
            None => replaced.sf_holds.set(replaced.sf_holds.get().saturating_sub(1)),
        }
    }

    pub fn forget_surface(&mut self, sid: &ObjectId) {
        if let Some(t) = self.shm.remove(sid) {
            self.retire_shm(t);
        }
    }

    fn retire_shm(&self, t: ShmTex) {
        self.gpu.retire(t.last_use, Retired::Image(t.image, t.view, t.memory));
        self.gpu.retire(t.last_use, Retired::Buffer(t.staging, t.staging_mem));
    }

    /// Releases a client buffer (and signals its commit's release point)
    /// once neither the GPU nor SurfaceFlinger reads it.
    pub fn release(&mut self, buffer: Option<WlBuffer>, release: Option<Release>, d: &Rc<Dmabuf>) {
        if d.idle(self.gpu.completed()) {
            if let Some(b) = buffer {
                b.release();
            }
            return;
        }
        // Only the GPU still reads it: the release point signals with the
        // GPU's fence for its last frame, without waiting for us.
        let release = match (release, self.fence_for(d.last_use.get())) {
            (Some(r), Some(f)) if d.sf_holds.get() == 0 => r.signal_after(f.as_fd()),
            (r, _) => r,
        };
        self.releases.push((d.clone(), buffer, release));
    }

    /// The GPU image for a surface's shm texture, staged for upload if it
    /// changed since the last frame.
    fn shm_source(&mut self, sid: &ObjectId, t: &Texture) -> Result<Source, String> {
        let (w, h) = (t.width, t.height);
        if t.data.len() < (w * h) as usize {
            return Err("short texture".into());
        }
        if self.shm.get(sid).is_some_and(|s| s.size != (w, h)) {
            let old = self.shm.remove(sid).unwrap();
            self.retire_shm(old);
        }
        if !self.shm.contains_key(sid) {
            let tex = self.create_shm(w, h)?;
            self.shm.insert(sid.clone(), tex);
        }
        let s = self.shm.get_mut(sid).unwrap();
        if s.staged != t.serial {
            // What changed since the staged commit, or all of it.
            let r = Some(s.staged).filter(|&v| v != 0).and_then(|v| t.damage_since(v)).unwrap_or([0, 0, w, h]);
            let r = rect_clamp(r, w, h);
            if !rect_empty(r) {
                // The staging buffer may still feed the previous upload.
                self.gpu.wait(s.last_use);
                let (x0, x1) = (r[0] as usize, r[2] as usize);
                for y in r[1] as usize..r[3] as usize {
                    let row = y * w as usize;
                    let src = &t.data[row + x0..row + x1];
                    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), (s.ptr as *mut u32).add(row + x0), x1 - x0) };
                }
                s.dirty = Some(s.dirty.map_or(r, |d| rect_union(d, r)));
            }
            s.staged = t.serial;
        }
        // Counts as uploaded once a frame recording the copy is submitted.
        Ok(Source::Shm { image: s.image, view: s.view, staging: s.staging, width: w, upload: s.dirty, fresh: !s.written })
    }

    fn create_shm(&self, w: i32, h: i32) -> Result<ShmTex, String> {
        let gpu = &self.gpu;
        let d = &gpu.device;
        let format = vk::Format::R8G8B8A8_UNORM;
        unsafe {
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D { width: w as u32, height: h as u32, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            let image = err("texture", d.create_image(&info, None))?;
            let req = d.get_image_memory_requirements(image);
            let Some(ti) = gpu.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL) else {
                d.destroy_image(image, None);
                return Err("no device memory".into());
            };
            let memory = match d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ti), None) {
                Ok(m) => m,
                Err(e) => {
                    d.destroy_image(image, None);
                    return Err(format!("texture memory: {e}"));
                }
            };
            let view = d.bind_image_memory(image, memory, 0).map_err(|e| e.to_string()).and_then(|_| gpu.view(image, format, false));
            let view = match view {
                Ok(v) => v,
                Err(e) => {
                    d.destroy_image(image, None);
                    d.free_memory(memory, None);
                    return Err(e);
                }
            };
            let (staging, staging_mem, ptr) = match gpu.host_buffer((w * h * 4) as u64, vk::BufferUsageFlags::TRANSFER_SRC) {
                Ok(b) => b,
                Err(e) => {
                    gpu.destroy(Retired::Image(image, view, memory));
                    return Err(e);
                }
            };
            Ok(ShmTex { image, memory, view, staging, staging_mem, ptr, size: (w, h), staged: 0, dirty: None, written: false, last_use: 0 })
        }
    }

    /// Finished frames: caption analysis from their readback, buffer
    /// releases and deferred destruction.
    fn collect(&mut self, st: &mut State) {
        self.gpu.collect();
        let done = self.gpu.completed();
        self.fences.retain(|(s, _)| *s > done);
        self.sf_fences.retain(|(fence, d)| {
            let mut p = libc::pollfd { fd: fence.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let signalled = unsafe { libc::poll(&mut p, 1, 0) } == 1;
            if signalled {
                d.sf_holds.set(d.sf_holds.get().saturating_sub(1));
            }
            !signalled
        });
        self.releases.retain(|(d, b, _)| {
            if !d.idle(done) {
                return true;
            }
            if let Some(b) = b.as_ref().filter(|b| b.is_alive()) {
                b.release();
            }
            false
        });
        let (finished, pending): (Vec<_>, Vec<_>) = self.frames.drain(..).partition(|(s, _)| *s <= done);
        self.frames = pending;
        for &(serial, win) in &finished {
            let slot = st.shared.slots.lock().ok().and_then(|s| s.get(&win).cloned());
            let Some(slot) = slot else { continue };
            let band = slot.with_target(|_, t| t.as_ref().and_then(|t| t.band_pixels(serial))).flatten();
            if let Some((rows, width, y0, n)) = band {
                let band = Band { rows: &rows, width, stride: width as usize, y0, y1: y0 + n };
                crate::render::caption_feedback(st, win, &band);
            } else if !self.frames.iter().chain(&finished).any(|&(s, w)| w == win && s > serial) {
                // The window's latest readback is lost: read the next frame.
                self.bands.remove(&win);
            }
        }
    }

    fn render_window(&mut self, st: &mut State, id: u32) {
        let Some(w) = st.windows.get(&id) else { return };
        let slot = st.shared.slot(id);
        if !slot.is_attached() {
            return;
        }
        let display = w.display;
        let list = st.paint_list(w);
        for (sid, _, _) in &list {
            crate::output::ensure_scale(st, sid, display);
        }
        // A synced resize (`Cmd::RedrawSync`) shows only the client's frame
        // for the new size; older frames are dropped (their callbacks still
        // fire, so the client keeps drawing). Past the timeout it draws as
        // usual.
        let sync = st.windows.get(&id).and_then(|w| w.sync.as_ref().map(|s| (w.acked >= s.serial, s.start.elapsed() >= SYNC_TIMEOUT)));
        match sync {
            Some((_, true)) => {
                answer_sync(st, id, None, "timed out");
            }
            Some((false, false)) => {
                let now = st.now_ms();
                for (sid, _, _) in &list {
                    if let Some(s) = st.surfaces.get_mut(sid) {
                        for cb in s.frame_cbs.drain(..) {
                            cb.done(now);
                        }
                    }
                }
                self.retry_in(5);
                return;
            }
            _ => {}
        }
        // Latch only finished client frames, so a slow client cannot stall
        // the queue that every window shares: the acquire point with explicit
        // sync, else the dmabuf's implicit fences.
        // The event loop wakes when they are: an eventfd per acquire point,
        // the dmabuf itself for implicit sync.
        let mut busy = false;
        for (sid, _, _) in &list {
            let Some(t) = st.surfaces.get(sid).and_then(|s| s.texture.as_ref()) else { continue };
            match (&t.sync, &t.dmabuf) {
                (Some(sync), _) if !sync.acquire.signalled() => {
                    busy = true;
                    let key = sync.acquire.key();
                    self.acquire_wanted.push(key);
                    if !self.acquire_waits.contains_key(&key) {
                        match sync.acquire.eventfd() {
                            Some(fd) => {
                                self.acquire_waits.insert(key, fd);
                            }
                            None => self.retry_in(2),
                        }
                    }
                }
                (None, Some((_, d))) if !d.ready() => {
                    busy = true;
                    self.implicit_waits.push(d.clone());
                }
                _ => {}
            }
        }
        if busy {
            return;
        }
        let scale = st.window_scale(id);
        let Some(w) = st.windows.get(&id) else { return };
        let (pw, ph) = w.px;
        let top = w.caption.top;
        let band = (w.caption.top, w.caption.height);
        let mut key = BandKey { size: (pw, ph), caption: w.caption.clone(), draws: Vec::new() };
        let mut draws = Vec::new();
        for (sid, ox, oy) in &list {
            let Some(s) = st.surfaces.get(sid) else { continue };
            let (Some(t), Some((lw, lh)), Some(src)) = (&s.texture, s.logical_size(), s.source_rect()) else { continue };
            // Edges are rounded separately so adjacent surfaces meet exactly.
            let (x, y) = (to_px(*ox as f64, scale), to_px(*oy as f64, scale) + top);
            let (x1, y1) = (to_px((ox + lw) as f64, scale), to_px((oy + lh) as f64, scale) + top);
            if x1 <= x || y1 <= y || t.width <= 0 || t.height <= 0 || src[2] <= 0.0 || src[3] <= 0.0 {
                continue;
            }
            let (tw, th) = (t.width as f64, t.height as f64);
            let mut uv = [src[0] / tw, src[1] / th, (src[0] + src[2]) / tw, (src[1] + src[3]) / th].map(|v| v as f32);
            let source = match &t.dmabuf {
                Some((_, d)) => {
                    if d.y_invert {
                        (uv[1], uv[3]) = (1.0 - uv[1], 1.0 - uv[3]);
                    }
                    Source::Dmabuf { image: d.image, view: d.view }
                }
                None => match self.shm_source(sid, t) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("gpu: texture: {e}");
                        continue;
                    }
                },
            };
            if y < band.0 + band.1 && y1 > band.0 {
                key.draws.push((sid.clone(), [x, y, x1, y1], uv, t.serial));
            }
            draws.push(Draw { source, dst: [x, y, x1, y1], uv });
        }
        // Only a band that may look different is read back and analysed.
        let read = self.band_changed(st, id, &key);

        let generation = slot.with_target(|_, t| t.as_ref().map(|t| t.generation)).flatten();
        self.sync_layer(id, generation);
        if let Some((d, at)) = self.direct_candidate(st, &list, &draws, (pw, ph), top) {
            return self.render_direct(st, id, &slot, &list, d, at, (pw, ph), top, band, (read, key));
        }
        // Composited frames go through the swapchain, outside Android's sync.
        answer_sync(st, id, None, "not direct");

        let serial = self.serial + 1;
        let gpu = self.gpu.clone();
        let result = slot.with_target(|win, target| -> Result<Result<(u64, Option<OwnedFd>), i32>, String> {
            if target.is_none() {
                *target = Some(Target::new(gpu.clone(), win)?);
            }
            let t = target.as_mut().unwrap();
            gpu.wait(t.last_serial);
            if t.stale || t.extent != (pw, ph) {
                t.configure(pw, ph)?;
            }
            if !t.frame(&draws, if read { band } else { (0, 0) }, serial)? {
                return Ok(Err(t.retry_ms()));
            }
            Ok(Ok((t.generation, t.fence.take())))
        });
        let generation = match result {
            Some(Ok(Ok((g, fence)))) => {
                if let Some(f) = fence {
                    self.fences.push((serial, f));
                }
                g
            }
            Some(Ok(Err(retry))) => {
                // No swapchain image free yet: try again around the next vsync.
                self.retry_in(retry);
                return;
            }
            Some(Err(e)) => {
                log::warn!("gpu: toplevel {id}: {e}");
                return;
            }
            None => return,
        };
        self.serial = serial;
        if read {
            self.frames.push((serial, id));
        }
        self.bands.insert(id, key);
        // Composited again: take the client's buffer off the direct layer.
        self.sync_layer(id, Some(generation));
        if let Some(state) = self.direct.get_mut(&id).filter(|s| s.shown.is_some()) {
            let token = self.next_token;
            self.next_token += 1;
            let (shared, placeholder) = (self.shared.clone(), self.placeholder);
            if st.windows.get(&id).is_some_and(|w| w.window_layer) {
                st.host.present(id, None, [0; 4], token);
            } else {
                slot.with_target(|_, t| {
                    if let Some(layer) = t.as_ref().and_then(|t| t.layer.as_ref()) {
                        layer.present(None, placeholder, [0; 4], &shared, id, token);
                    }
                });
            }
            state.replaced.insert(token, state.shown.take());
            log::info!("toplevel {id}: composited");
        }
        let now = st.now_ms();
        for (sid, _, _) in &list {
            if let Some(t) = self.shm.get_mut(sid) {
                if t.dirty.take().is_some() {
                    t.written = true;
                }
                t.last_use = serial;
            }
            if let Some(s) = st.surfaces.get_mut(sid) {
                if let Some((_, d)) = s.texture.as_ref().and_then(|t| t.dmabuf.as_ref()) {
                    d.last_use.set(serial);
                }
                for cb in s.frame_cbs.drain(..) {
                    cb.done(now);
                }
            }
        }
        if let Some(w) = st.windows.get_mut(&id) {
            w.dirty = false;
        }
    }
}

impl Renderer {
    /// Whether a frame drawn from `key` may show a different caption band
    /// than the last one read back: anything moved, or a surface's damage
    /// since then reaches the band.
    fn band_changed(&self, st: &State, win: u32, key: &BandKey) -> bool {
        let Some(old) = self.bands.get(&win) else { return true };
        if old.size != key.size || old.caption != key.caption || old.draws.len() != key.draws.len() {
            return true;
        }
        let (b0, b1) = (key.caption.top as f64, (key.caption.top + key.caption.height) as f64);
        old.draws.iter().zip(&key.draws).any(|(o, n)| {
            if o.0 != n.0 || o.1 != n.1 || o.2 != n.2 {
                return true;
            }
            if o.3 == n.3 {
                return false;
            }
            let Some(t) = st.surfaces.get(&n.0).and_then(|s| s.texture.as_ref()) else { return true };
            let Some(r) = t.damage_since(o.3) else { return true };
            if rect_empty(r) {
                return false;
            }
            // The damaged rows in window px (uv may run upwards).
            let ([_, y0, _, y1], v0, v1) = (n.1, n.2[1] as f64, n.2[3] as f64);
            if v0 == v1 || t.height <= 0 {
                return true;
            }
            let at = |row: i32| y0 as f64 + (row as f64 / t.height as f64 - v0) / (v1 - v0) * (y1 - y0) as f64;
            let (a, b) = (at(r[1]), at(r[3]));
            // A pixel's neighbours bleed into it when scaled.
            a.min(b) - 1.0 < b1 && a.max(b) + 1.0 > b0
        })
    }

    /// Resets a window's direct-mode state if its Target (and so its
    /// layer) is not the one the state describes.
    fn sync_layer(&mut self, id: u32, generation: Option<u64>) {
        let state = self.direct.entry(id).or_default();
        if generation != Some(state.generation) {
            state.reset();
            state.generation = generation.unwrap_or(0);
        }
    }

    /// The window's buffer and where it lies (window px), if SurfaceFlinger
    /// can show it as is: one dmabuf surface, drawn 1:1, covering the whole
    /// window below the caption's top. Anything outside (GTK's resize
    /// margin) is cropped.
    fn direct_candidate(
        &self,
        st: &State,
        list: &[(ObjectId, i32, i32)],
        draws: &[Draw],
        (pw, ph): (i32, i32),
        top: i32,
    ) -> Option<(Rc<Dmabuf>, [i32; 4])> {
        if !self.direct_enabled || self.placeholder.is_null() || list.len() != 1 || draws.len() != 1 {
            return None;
        }
        let draw = &draws[0];
        let [x0, y0, x1, y1] = draw.dst;
        let covers = x0 <= 0 && y0 <= top && x1 >= pw && y1 >= ph;
        if !matches!(draw.source, Source::Dmabuf { .. }) || !covers || draw.uv != [0.0, 0.0, 1.0, 1.0] {
            return None;
        }
        let (_, d) = st.surfaces.get(&list[0].0)?.texture.as_ref()?.dmabuf.as_ref()?;
        let fits = d.format.direct() && !d.y_invert && (d.width, d.height) == (x1 - x0, y1 - y0);
        (fits && d.hardware_buffer().is_some()).then(|| (d.clone(), draw.dst))
    }

    /// Shows the buffer on the window's layer, one transaction at a time.
    #[allow(clippy::too_many_arguments)]
    fn render_direct(
        &mut self,
        st: &mut State,
        id: u32,
        slot: &Arc<crate::android::NativeSlot>,
        list: &[(ObjectId, i32, i32)],
        d: Rc<Dmabuf>,
        at: [i32; 4],
        (pw, ph): (i32, i32),
        top: i32,
        band: (i32, i32),
        (want_read, key): (bool, BandKey),
    ) {
        if let Some(state) = self.direct.get_mut(&id).filter(|s| s.in_flight.is_some()) {
            if state.in_flight_since.is_some_and(|t| t.elapsed() > DIRECT_WATCHDOG) {
                log::warn!("toplevel {id}: direct transaction {:?} never completed", state.in_flight);
                state.in_flight = None;
            } else {
                // `Cmd::DirectDone` wakes us; this only runs the watchdog.
                self.retry_in(100);
                return;
            }
        }
        let Some(ahb) = d.hardware_buffer() else { return };
        let serial = self.serial + 1;
        let token = self.next_token;
        self.next_token += 1;
        let (gpu, shared, placeholder) = (self.gpu.clone(), self.shared.clone(), self.placeholder);
        // A window that owns its layer sets the buffer itself (`Cmd::OwnsLayer`).
        let window_layer = st.windows.get(&id).is_some_and(|w| w.window_layer);
        // The window's part of the buffer (buffer px), and where it goes.
        let (src, dst) = ([-at[0], top - at[1], pw - at[0], ph - at[1]], [0, top, pw, ph]);
        let result = slot.with_target(|win, target| -> Result<(u64, bool, Option<OwnedFd>), String> {
            if target.is_none() {
                *target = Some(Target::new(gpu.clone(), win)?);
            }
            let t = target.as_mut().unwrap();
            gpu.wait(t.last_serial);
            if !window_layer {
                let layer = t.layer(win).ok_or("cannot create the direct layer")?;
                layer.present(Some((ahb, src, d.opaque)), placeholder, dst, &shared, id, token);
            }
            let read = want_read
                && t.read_direct(&d, (at[0], at[1]), pw, band, serial).unwrap_or_else(|e| {
                    log::warn!("gpu: caption readback: {e}");
                    false
                });
            Ok((t.generation, read, t.fence.take().filter(|_| read)))
        });
        let (generation, read) = match result {
            Some(Ok((g, read, fence))) => {
                if let Some(f) = fence {
                    self.fences.push((serial, f));
                }
                (g, read)
            }
            Some(Err(e)) => {
                log::warn!("gpu: toplevel {id}: {e}");
                return;
            }
            None => return,
        };
        self.sync_layer(id, Some(generation));
        let state = self.direct.get_mut(&id).unwrap();
        d.sf_holds.set(d.sf_holds.get() + 1);
        let replaced = state.shown.replace(d.clone());
        if replaced.is_none() {
            log::info!("toplevel {id}: direct ({}x{} {:#x}/{:#x})", d.width, d.height, d.format.fourcc, d.format.modifier);
        }
        state.replaced.insert(token, replaced);
        state.in_flight = Some(token);
        state.in_flight_since = Some(std::time::Instant::now());
        if window_layer {
            // Android waits for this frame: it goes back with the redraw, and
            // the window shows it with its own synced frame.
            let synced = st.windows.get(&id).is_some_and(|w| w.sync.is_some());
            let handed = synced
                && st.host.hardware_buffer(ahb).is_some_and(|buffer| {
                    let f = crate::WindowFrame { buffer, src, dst, opaque: d.opaque, token };
                    answer_sync(st, id, Some(f), &format!("handed over {}x{}", d.width, d.height))
                });
            if !handed {
                answer_sync(st, id, None, "not handed over");
                st.host.present(id, Some((ahb, src, d.opaque)), dst, token);
            }
        }
        if read {
            self.serial = serial;
            self.frames.push((serial, id));
            d.last_use.set(serial);
        }
        if read || !want_read {
            self.bands.insert(id, key);
        }
        let now = st.now_ms();
        for (sid, _, _) in list {
            if let Some(s) = st.surfaces.get_mut(sid) {
                for cb in s.frame_cbs.drain(..) {
                    cb.done(now);
                }
            }
        }
        if let Some(w) = st.windows.get_mut(&id) {
            w.dirty = false;
        }
    }
}

/// How long a direct transaction may stay incomplete before the window draws
/// again anyway (a window process may never apply one it was handed).
const DIRECT_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(1);

/// Answers a window's pending synced resize, if any; false if there was
/// none or nobody waits for it any more.
fn answer_sync(st: &mut State, id: u32, t: Option<crate::WindowFrame>, what: &str) -> bool {
    let Some(s) = st.windows.get_mut(&id).and_then(|w| w.sync.take()) else { return false };
    log::debug!("toplevel {id}: sync {what} after {} ms", s.start.elapsed().as_millis());
    s.reply.send(t).is_ok()
}

pub fn render_dirty(st: &mut State) {
    let Some(mut r) = st.gpu.take() else { return };
    r.collect(st);
    r.retry = None;
    r.implicit_waits.clear();
    let dirty: Vec<u32> = st.windows.values().filter(|w| w.dirty && w.mapped && w.px.0 > 0).map(|w| w.id).collect();
    for id in dirty {
        r.render_window(st, id);
    }
    let wanted = std::mem::take(&mut r.acquire_wanted);
    r.acquire_waits.retain(|k, _| wanted.contains(k));
    st.gpu = Some(r);
}
