use core::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::platform::error::{Error, Result};
use crate::platform::ffi;
use crate::render::gpu::{AtlasInfo, AtlasUpload, FrameData, TextureKey};
use crate::render::image::ImageStore;
use crate::render::vulkan::abi::*;
use crate::render::vulkan::renderer::{
    allocate_set, color_range, destroy_texture, write_image_set, HostBuffer, ImageTexture, Texture,
};
use crate::render::vulkan::{pick_memory_type, Gpu};

/// How long a frame waits on the CPU for the compositor to finish reading its
/// buffer, when that wait cannot ride the GPU. Past it the frame renders anyway:
/// a frame that may tear beats a render error, which ends the program.
const ACQUIRE_WAIT_MS: u32 = 1000;

/// The staging buffer's starting capacity, enough for a frame of new glyphs.
const STAGING_START: usize = 256 * 1024;
/// Capacity a slot's staging buffer keeps for good, and the granularity it is
/// sized in above that. Atlas regions and a screenful of thumbnails fit in it.
const STAGING_KEEP: usize = 4 * 1024 * 1024;
/// How many frames in a row must need under a quarter of an oversized staging
/// buffer before it is given back. Paging through photographs uploads one every
/// frame or two and so keeps the buffer; settling on one picture releases it.
const STAGING_QUIET_FRAMES: u32 = 8;

/// The bounds on image textures: how many may exist (one descriptor set each,
/// out of a fixed pool), and how many bytes of device memory they may hold
/// before the ones no longer on screen are evicted. Half a gibibyte keeps a
/// handful of 24-megapixel photographs (96 MB each) around for paging back and
/// forth, and is far above any screenful of thumbnails.
const IMAGE_LIMITS: ImageLimits = ImageLimits {
    max_count: IMAGE_SET_CAP as usize,
    max_bytes: 512 * 1024 * 1024,
};

// ---------------------------------------------------------------------------
// Render targets.
// ---------------------------------------------------------------------------

/// Which of the two font atlases a sync/upload targets. The glyph atlas is a
/// single-channel coverage mask; the emoji atlas is full-colour, so they differ
/// only in their texture format (and which `Renderer` slot holds them).
#[derive(Clone, Copy)]
enum AtlasSlot {
    Glyph,
    Emoji,
}

impl AtlasSlot {
    fn format(self) -> u32 {
        match self {
            AtlasSlot::Glyph => VK_FORMAT_R8_UNORM,
            AtlasSlot::Emoji => VK_FORMAT_B8G8R8A8_UNORM,
        }
    }
}

/// One swap slot: the submission state a frame is recorded and synchronized
/// with (command buffer, completion fence, and the two semaphores of the sync
/// hand-off), the host buffers a frame writes, and the [`RenderTarget`] it
/// currently draws into. Only the target depends on the window size, so a resize
/// swaps it with [`Gpu::retarget`] and keeps the rest. Created and destroyed
/// through [`Gpu`]; it holds raw handles, so dropping it without
/// [`Gpu::destroy_image`] leaks GPU memory until the device goes down.
pub struct GpuImage {
    target: RenderTarget,
    cmd: VkCommandBuffer,
    /// Signaled when the last submission from this slot completed.
    fence: VkFence,
    /// Carries the compositor's read fences into the next submission (a
    /// temporary sync-fd import per frame).
    acquire: VkSemaphore,
    /// Signaled by each submission and exported as a sync file, which is how the
    /// compositor learns the render is done. Zero when it could not be created
    /// again after a failed export; the slot then waits each frame out on the CPU.
    release: VkSemaphore,
    /// This frame slot's vertex buffer and upload staging, safe to rewrite
    /// once the slot's fence has signaled.
    vertices: HostBuffer,
    staging: HostBuffer,
    /// Frames in a row that needed far less staging than the buffer holds (see
    /// [`staging_plan`]).
    staging_quiet_frames: u32,
}

/// A device-local image whose memory is exported as the dmabuf behind a
/// `wl_buffer`, with the view and framebuffer a frame renders into it through.
/// Dropping it without [`Gpu::destroy_target`] leaks GPU memory until the device
/// goes down.
pub struct RenderTarget {
    image: VkImage,
    memory: VkDeviceMemory,
    /// The exported dmabuf, kept for the sync-file ioctls; the compositor
    /// holds its own reference once the wl_buffer imports it.
    fd: OwnedFd,
    /// The render-pass view of the image and its framebuffer.
    view: VkImageView,
    framebuffer: VkFramebuffer,
    width: u32,
    height: u32,
    stride: u32,
    offset: u32,
    modifier: u64,
    /// False until the first submission: the first frame enters from
    /// UNDEFINED, later frames re-acquire from the foreign queue family.
    used: bool,
}

impl GpuImage {
    /// The dmabuf to hand to `zwp_linux_buffer_params_v1.add`.
    pub fn fd(&self) -> RawFd {
        self.target.fd.as_raw_fd()
    }

    pub fn stride(&self) -> u32 {
        self.target.stride
    }

    pub fn offset(&self) -> u32 {
        self.target.offset
    }

    /// The modifier the driver actually chose from the allowed list.
    pub fn modifier(&self) -> u64 {
        self.target.modifier
    }
}

/// Partially constructed render target, so an error mid-`create_target` destroys
/// exactly what exists so far (handle 0 = never created).
struct PendingTarget<'a> {
    gpu: &'a Gpu,
    image: VkImage,
    memory: VkDeviceMemory,
    view: VkImageView,
    framebuffer: VkFramebuffer,
}

impl Drop for PendingTarget<'_> {
    fn drop(&mut self) {
        let d = self.gpu.device.raw;
        let f = &self.gpu.fns;
        // SAFETY: every non-zero handle below was created on this device and is
        // destroyed exactly once, children (framebuffer, view) before the image
        // and its memory; no submission still renders into it. The fd (if
        // exported) closes with its OwnedFd owner.
        unsafe {
            if self.framebuffer != 0 {
                (f.destroy_framebuffer)(d, self.framebuffer, core::ptr::null());
            }
            if self.view != 0 {
                (f.destroy_image_view)(d, self.view, core::ptr::null());
            }
            if self.memory != 0 {
                (f.free_memory)(d, self.memory, core::ptr::null());
            }
            if self.image != 0 {
                (f.destroy_image)(d, self.image, core::ptr::null());
            }
        }
    }
}

/// Partially constructed slot state, so an error mid-`create_slot`, or in the
/// target created after it, destroys exactly what exists so far (handle 0 =
/// never created).
struct PendingSlot<'a> {
    gpu: &'a Gpu,
    fence: VkFence,
    acquire: VkSemaphore,
    release: VkSemaphore,
    cmd: VkCommandBuffer,
    vertices: HostBuffer,
    staging: HostBuffer,
}

impl Drop for PendingSlot<'_> {
    fn drop(&mut self) {
        let d = self.gpu.device.raw;
        let f = &self.gpu.fns;
        self.gpu
            .destroy_host_buffer(std::mem::take(&mut self.vertices));
        self.gpu
            .destroy_host_buffer(std::mem::take(&mut self.staging));
        // SAFETY: every non-zero handle below was created on this device and is
        // destroyed exactly once; no submission is in flight with it.
        unsafe {
            if !self.cmd.is_null() {
                (f.free_command_buffers)(d, self.gpu.pool, 1, &self.cmd);
            }
            if self.release != 0 {
                (f.destroy_semaphore)(d, self.release, core::ptr::null());
            }
            if self.acquire != 0 {
                (f.destroy_semaphore)(d, self.acquire, core::ptr::null());
            }
            if self.fence != 0 {
                (f.destroy_fence)(d, self.fence, core::ptr::null());
            }
        }
    }
}

/// Partially constructed sampled texture, so an error mid-`create_texture`
/// destroys exactly what exists so far (handle 0 = never created). Disarmed with
/// [`core::mem::forget`] once the finished [`Texture`] takes the handles.
struct PendingTexture<'a> {
    gpu: &'a Gpu,
    image: VkImage,
    memory: VkDeviceMemory,
    view: VkImageView,
}

impl Drop for PendingTexture<'_> {
    fn drop(&mut self) {
        let d = self.gpu.device.raw;
        let f = &self.gpu.fns;
        // SAFETY: every non-zero handle was created on this device and is
        // destroyed exactly once; nothing was ever submitted with them.
        unsafe {
            if self.view != 0 {
                (f.destroy_image_view)(d, self.view, core::ptr::null());
            }
            if self.memory != 0 {
                (f.free_memory)(d, self.memory, core::ptr::null());
            }
            if self.image != 0 {
                (f.destroy_image)(d, self.image, core::ptr::null());
            }
        }
    }
}

/// Partially constructed host buffer, mirroring [`PendingTexture`] for
/// `create_host_buffer` (handle 0 = never created).
struct PendingBuffer<'a> {
    gpu: &'a Gpu,
    buf: VkBuffer,
    memory: VkDeviceMemory,
}

impl Drop for PendingBuffer<'_> {
    fn drop(&mut self) {
        let d = self.gpu.device.raw;
        let f = &self.gpu.fns;
        // SAFETY: as for PendingTexture; freeing the memory unmaps it.
        unsafe {
            if self.memory != 0 {
                (f.free_memory)(d, self.memory, core::ptr::null());
            }
            if self.buf != 0 {
                (f.destroy_buffer)(d, self.buf, core::ptr::null());
            }
        }
    }
}

impl Gpu {
    /// Create a swap slot drawing into a `width` x `height` target: the slot's
    /// submission state and buffers (see [`Self::create_slot`]) and the target
    /// itself (see [`Self::create_target`]).
    pub fn create_image(&self, width: u32, height: u32, modifiers: &[u64]) -> Result<GpuImage> {
        let mut slot = self.create_slot()?;
        // On an error the slot's guard destroys what it holds.
        let target = self.create_target(width, height, modifiers)?;
        let img = GpuImage {
            target,
            cmd: slot.cmd,
            fence: slot.fence,
            acquire: slot.acquire,
            release: slot.release,
            vertices: std::mem::take(&mut slot.vertices),
            staging: std::mem::take(&mut slot.staging),
            staging_quiet_frames: 0,
        };
        core::mem::forget(slot);
        Ok(img)
    }

    /// Point `img` at a fresh `width` x `height` target and hand back the one it
    /// drew into, for the caller to free with [`Self::destroy_target`] or keep
    /// until the compositor releases it. The slot's command buffer, fence,
    /// semaphores and buffers carry over. The caller ensures no submission of
    /// this slot is in flight ([`Gpu::wait_idle`]): its recorded command buffer
    /// names the old target.
    pub fn retarget(
        &self,
        img: &mut GpuImage,
        width: u32,
        height: u32,
        modifiers: &[u64],
    ) -> Result<RenderTarget> {
        let fresh = self.create_target(width, height, modifiers)?;
        Ok(std::mem::replace(&mut img.target, fresh))
    }

    /// Create a `width` x `height` render target, letting the driver pick a
    /// layout from `modifiers` (the compositor∩device intersection), and
    /// export its memory as a dmabuf. Returns the target with the geometry the
    /// wl_buffer needs (chosen modifier, plane stride and offset).
    fn create_target(&self, width: u32, height: u32, modifiers: &[u64]) -> Result<RenderTarget> {
        if width == 0 || height == 0 {
            return Err(Error::msg("gpu image with a zero dimension"));
        }
        if width > self.max_image_dimension || height > self.max_image_dimension {
            return Err(Error::msg(format!(
                "a {width}x{height} window exceeds this GPU's largest image ({} pixels a side)",
                self.max_image_dimension
            )));
        }
        if modifiers.is_empty() {
            return Err(Error::msg("gpu image with no allowed modifiers"));
        }
        let d = self.device.raw;
        let f = &self.fns;
        let mut pending = PendingTarget {
            gpu: self,
            image: 0,
            memory: 0,
            view: 0,
            framebuffer: 0,
        };

        // The image: renderable, exportable, laid out by one of the allowed
        // modifiers (driver's choice, queried back below). It stores UNORM bytes
        // for the compositor but is mutable so the render pass can drive it
        // through an sRGB view; the format list names both.
        let modifier_list = VkImageDrmFormatModifierListCreateInfoEXT {
            s_type: VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
            p_next: core::ptr::null(),
            drm_format_modifier_count: modifiers.len() as u32,
            p_drm_format_modifiers: modifiers.as_ptr(),
        };
        let external = VkExternalMemoryImageCreateInfo {
            s_type: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
            p_next: (&modifier_list as *const VkImageDrmFormatModifierListCreateInfoEXT).cast(),
            handle_types: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT,
        };
        let view_formats = [VK_FORMAT_B8G8R8A8_UNORM, VK_FORMAT_B8G8R8A8_SRGB];
        let format_list = VkImageFormatListCreateInfo {
            s_type: VK_STRUCTURE_TYPE_IMAGE_FORMAT_LIST_CREATE_INFO,
            p_next: (&external as *const VkExternalMemoryImageCreateInfo).cast(),
            view_format_count: view_formats.len() as u32,
            p_view_formats: view_formats.as_ptr(),
        };
        let image_info = VkImageCreateInfo {
            s_type: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            p_next: (&format_list as *const VkImageFormatListCreateInfo).cast(),
            flags: VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT,
            image_type: VK_IMAGE_TYPE_2D,
            format: VK_FORMAT_B8G8R8A8_UNORM,
            extent: VkExtent3D {
                width,
                height,
                depth: 1,
            },
            mip_levels: 1,
            array_layers: 1,
            samples: VK_SAMPLE_COUNT_1_BIT,
            tiling: VK_IMAGE_TILING_DRM_FORMAT_MODIFIER,
            // Rendered into by the pipeline and nothing else: no copy reads it
            // back, so no transfer usage (which a modifier need not support).
            usage: VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
            sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
            queue_family_index_count: 0,
            p_queue_family_indices: core::ptr::null(),
            initial_layout: VK_IMAGE_LAYOUT_UNDEFINED,
        };
        // SAFETY: d is live; image_info and its chain outlive the call.
        check(
            unsafe { (f.create_image)(d, &image_info, core::ptr::null(), &mut pending.image) },
            "vkCreateImage",
        )?;

        // Dedicated, exportable, device-local memory.
        let mut reqs = VkMemoryRequirements {
            size: 0,
            alignment: 0,
            memory_type_bits: 0,
        };
        // SAFETY: image is live; reqs is a valid out-parameter.
        unsafe { (f.get_image_memory_requirements)(d, pending.image, &mut reqs) };
        let memory_type = pick_memory_type(&self.memory_type_flags, reqs.memory_type_bits, 0)
            .ok_or_else(|| Error::msg("no memory type fits the exported image"))?;
        let dedicated = VkMemoryDedicatedAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
            p_next: core::ptr::null(),
            image: pending.image,
            buffer: 0,
        };
        let export = VkExportMemoryAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
            p_next: (&dedicated as *const VkMemoryDedicatedAllocateInfo).cast(),
            handle_types: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT,
        };
        let alloc = VkMemoryAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            p_next: (&export as *const VkExportMemoryAllocateInfo).cast(),
            allocation_size: reqs.size,
            memory_type_index: memory_type,
        };
        // SAFETY: alloc and its chain outlive the call.
        check(
            unsafe { (f.allocate_memory)(d, &alloc, core::ptr::null(), &mut pending.memory) },
            "vkAllocateMemory",
        )?;
        // SAFETY: image and memory are live and compatible (requirements above).
        check(
            unsafe { (f.bind_image_memory)(d, pending.image, pending.memory, 0) },
            "vkBindImageMemory",
        )?;

        // What layout did the driver pick? The wl_buffer must describe it.
        let mut chosen = VkImageDrmFormatModifierPropertiesEXT {
            s_type: VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_PROPERTIES_EXT,
            p_next: core::ptr::null_mut(),
            drm_format_modifier: 0,
        };
        // SAFETY: image is live; chosen is a valid out-parameter.
        check(
            unsafe { (f.get_image_drm_format_modifier_properties)(d, pending.image, &mut chosen) },
            "vkGetImageDrmFormatModifierPropertiesEXT",
        )?;
        let subresource = VkImageSubresource {
            aspect_mask: VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT,
            mip_level: 0,
            array_layer: 0,
        };
        let mut layout = VkSubresourceLayout {
            offset: 0,
            size: 0,
            row_pitch: 0,
            array_pitch: 0,
            depth_pitch: 0,
        };
        // SAFETY: image is live; the plane-0 aspect is valid for modifier tiling.
        unsafe { (f.get_image_subresource_layout)(d, pending.image, &subresource, &mut layout) };
        let stride = u32::try_from(layout.row_pitch)
            .map_err(|_| Error::msg("image row pitch exceeds the wl_buffer stride range"))?;
        let offset = u32::try_from(layout.offset)
            .map_err(|_| Error::msg("image plane offset exceeds the wl_buffer range"))?;

        // Export the dmabuf.
        let get_fd = VkMemoryGetFdInfoKHR {
            s_type: VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
            p_next: core::ptr::null(),
            memory: pending.memory,
            handle_type: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT,
        };
        let mut raw_fd: i32 = -1;
        // SAFETY: memory is live; raw_fd receives a fresh fd we own.
        check(
            unsafe { (f.get_memory_fd)(d, &get_fd, &mut raw_fd) },
            "vkGetMemoryFdKHR",
        )?;
        if raw_fd < 0 {
            return Err(Error::msg("vkGetMemoryFdKHR returned an invalid fd"));
        }
        // SAFETY: the driver just handed us ownership of this fd.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        // The render-pass plumbing: an sRGB view of the image and its
        // framebuffer, so fixed-function blending runs in linear space.
        pending.view = self.create_view(pending.image, VK_FORMAT_B8G8R8A8_SRGB)?;
        let fb_info = VkFramebufferCreateInfo {
            s_type: VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
            p_next: core::ptr::null(),
            flags: 0,
            render_pass: self.renderer.render_pass,
            attachment_count: 1,
            p_attachments: &pending.view,
            width,
            height,
            layers: 1,
        };
        // SAFETY: the render pass and view are live; fb_info outlives the call.
        check(
            unsafe {
                (f.create_framebuffer)(d, &fb_info, core::ptr::null(), &mut pending.framebuffer)
            },
            "vkCreateFramebuffer",
        )?;

        // Success: move the handles out of the guard.
        let target = RenderTarget {
            image: pending.image,
            memory: pending.memory,
            fd,
            view: pending.view,
            framebuffer: pending.framebuffer,
            width,
            height,
            stride,
            offset,
            modifier: chosen.drm_format_modifier,
            used: false,
        };
        core::mem::forget(pending);
        Ok(target)
    }

    /// Create a slot's submission state: its command buffer, completion fence,
    /// the two semaphores of the sync hand-off, and its vertex and staging
    /// buffers. Returned inside its guard, still armed; the caller moves the
    /// handles out and forgets it.
    fn create_slot(&self) -> Result<PendingSlot<'_>> {
        let d = self.device.raw;
        let f = &self.fns;
        let mut pending = PendingSlot {
            gpu: self,
            fence: 0,
            acquire: 0,
            release: 0,
            cmd: core::ptr::null_mut(),
            vertices: HostBuffer::default(),
            staging: HostBuffer::default(),
        };

        // The fence starts signaled so the first frame's wait passes.
        let fence_info = VkFenceCreateInfo {
            s_type: VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
            p_next: core::ptr::null(),
            flags: 0x1, // VK_FENCE_CREATE_SIGNALED_BIT
        };
        // SAFETY: fence_info outlives the call.
        check(
            unsafe { (f.create_fence)(d, &fence_info, core::ptr::null(), &mut pending.fence) },
            "vkCreateFence",
        )?;
        let plain_sem = VkSemaphoreCreateInfo {
            s_type: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
            p_next: core::ptr::null(),
            flags: 0,
        };
        // SAFETY: plain_sem outlives the call.
        check(
            unsafe { (f.create_semaphore)(d, &plain_sem, core::ptr::null(), &mut pending.acquire) },
            "vkCreateSemaphore (acquire)",
        )?;
        pending.release = self.create_release_semaphore()?;

        let cmd_info = VkCommandBufferAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            p_next: core::ptr::null(),
            command_pool: self.pool,
            level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
            command_buffer_count: 1,
        };
        // SAFETY: the pool is live; one handle is written.
        check(
            unsafe { (f.allocate_command_buffers)(d, &cmd_info, &mut pending.cmd) },
            "vkAllocateCommandBuffers",
        )?;

        // Host-visible per-slot buffers, resized as frames need (the slot's
        // fence has been waited by then, so recreation is safe).
        pending.vertices = self.create_host_buffer(64 * 1024, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT)?;
        pending.staging = self.create_host_buffer(STAGING_START, STAGING_USAGE)?;

        Ok(pending)
    }

    /// A binary semaphore whose signal can be exported as a sync file: a slot's
    /// release semaphore.
    fn create_release_semaphore(&self) -> Result<VkSemaphore> {
        let export_sem = VkExportSemaphoreCreateInfo {
            s_type: VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
            p_next: core::ptr::null(),
            handle_types: VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
        };
        let info = VkSemaphoreCreateInfo {
            s_type: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
            p_next: (&export_sem as *const VkExportSemaphoreCreateInfo).cast(),
            flags: 0,
        };
        let mut semaphore: VkSemaphore = 0;
        // SAFETY: info and its chain outlive the call.
        check(
            unsafe {
                (self.fns.create_semaphore)(
                    self.device.raw,
                    &info,
                    core::ptr::null(),
                    &mut semaphore,
                )
            },
            "vkCreateSemaphore (release)",
        )?;
        Ok(semaphore)
    }

    /// Tear a slot down, target included. The caller ensures no submission is in
    /// flight (waits for release + fence, or [`Gpu::wait_idle`]), and that the
    /// compositor is done with the target's buffer.
    ///
    /// The finished slot holds exactly the handles a [`PendingSlot`] guards, so
    /// rebuild one from them and let its `Drop` run the single teardown sequence.
    pub fn destroy_image(&self, img: GpuImage) {
        let GpuImage {
            target,
            cmd,
            fence,
            acquire,
            release,
            vertices,
            staging,
            staging_quiet_frames: _,
        } = img;
        drop(PendingSlot {
            gpu: self,
            fence,
            acquire,
            release,
            cmd,
            vertices,
            staging,
        });
        self.destroy_target(target);
    }

    /// Tear a render target down. The caller ensures no submission of ours still
    /// renders into it, and that the compositor has released its buffer. The
    /// dmabuf `fd` closes when it drops at the end of this scope.
    pub fn destroy_target(&self, target: RenderTarget) {
        let RenderTarget {
            image,
            memory,
            fd: _fd,
            view,
            framebuffer,
            ..
        } = target;
        drop(PendingTarget {
            gpu: self,
            image,
            memory,
            view,
            framebuffer,
        });
    }

    /// Upload the frame's textures and vertices and record its command buffer,
    /// after waiting out the slot's previous submission. The sync strategy
    /// (implicit bridge or explicit points) is layered on by the caller, which
    /// then submits; on return the command buffer is recorded but not yet in
    /// flight.
    fn prepare_frame(
        &mut self,
        img: &mut GpuImage,
        frame: &FrameData,
        store: &ImageStore,
        background: [f32; 4],
        glyph_gamma: f32,
    ) -> Result<()> {
        let d = self.device.raw;
        let f = &self.fns;

        // The slot's previous submission must be complete before its command
        // buffer and buffers are rewritten (the buffer's release gates reuse of
        // the *pixels*; this gates the CPU-visible state).
        // SAFETY: fence is live; the timeout bounds a broken driver.
        check(
            unsafe { (f.wait_for_fences)(d, 1, &img.fence, 1, FENCE_TIMEOUT_NS) },
            "vkWaitForFences",
        )?;
        // SAFETY: the fence is signaled and nothing else references it.
        check(
            unsafe { (f.reset_fences)(d, 1, &img.fence) },
            "vkResetFences",
        )?;

        // Size the staging buffer before any texture exists, so that running out
        // of host memory for a photograph costs the photograph and not the
        // program: without room to stage them, the frame's new images are left
        // for a later frame and only the atlas regions go up.
        let mut wanted = self.wanted_images(frame, store);
        let atlas_lens = [&frame.glyph_upload, &frame.emoji_upload]
            .into_iter()
            .flatten()
            .map(|up| up.bytes.len());
        let atlas_bytes = staging_end(atlas_lens.clone());
        let all_bytes = staging_end(atlas_lens.chain(wanted.iter().map(|w| w.argb.len() * 4)));
        if let Err(e) = self.fit_staging(img, all_bytes) {
            if wanted.is_empty() {
                return Err(e);
            }
            self.dropped_image_uploads += wanted.len() as u64;
            wanted.clear();
            self.fit_staging(img, atlas_bytes)?;
        }

        // Textures: (re)create atlases and image textures, and list the copies
        // with the bytes each reads.
        let (staging, uploads) = self.sync_frame_textures(frame, wanted)?;
        // Each source goes straight from its owner into mapped memory, at the
        // offset `Staging::push` gave it: the same offset its copy command
        // records, so the two agree by construction.
        for (offset, src) in &staging.sources {
            let bytes = src.as_bytes();
            if offset + bytes.len() > img.staging.capacity {
                return Err(Error::msg("a frame's uploads overran its staging buffer"));
            }
            // SAFETY: `offset + bytes.len()` is within the mapped buffer (checked
            // just above); the GPU is done with the buffer (fence waited above);
            // and the source and the mapped buffer are distinct allocations, so
            // they cannot overlap.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    img.staging.ptr.add(*offset),
                    bytes.len(),
                );
            }
        }

        // Vertices.
        let vbytes = std::mem::size_of_val(frame.vertices.as_slice());
        if vbytes > 0 {
            self.grow_host_buffer(&mut img.vertices, vbytes, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT)?;
            // SAFETY: the vertex buffer is mapped with capacity >= vbytes and
            // idle (fence); Vertex is repr(C) plain data.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    frame.vertices.as_ptr().cast::<u8>(),
                    img.vertices.ptr,
                    vbytes,
                );
            }
        }

        self.record_frame(img, frame, &uploads, background, glyph_gamma)?;
        Ok(())
    }

    /// Render `img` and present it under the implicit-sync bridge: acquire from
    /// the dmabuf's read fences, release the render fence back into the dmabuf
    /// (CPU wait if the bridge is unavailable). The path for compositors that do
    /// not offer explicit sync. Returns with the frame in flight; the caller may
    /// attach/commit immediately.
    pub fn render_list(
        &mut self,
        img: &mut GpuImage,
        frame: &FrameData,
        store: &ImageStore,
        background: [f32; 4],
        glyph_gamma: f32,
    ) -> Result<()> {
        self.prepare_frame(img, frame, store, background, glyph_gamma)?;
        // Acquire: the compositor's unfinished reads become a wait semaphore.
        let wait_acquire = self.import_read_fences(img)?;
        self.submit_frame(img, wait_acquire)
    }

    /// Render `img` and present it under explicit sync: wait on `release_wait`
    /// (the compositor's release point for this buffer's previous frame, `None`
    /// on first use) instead of the dmabuf's implicit fences, and return the
    /// render-done sync file for the caller to publish as this frame's acquire
    /// point. `Ok(None)` means no fence was exported, so the frame was CPU-waited
    /// to completion and the caller publishes an already-signalled acquire point
    /// (the buffer is already safe to read).
    pub fn render_list_explicit(
        &mut self,
        img: &mut GpuImage,
        frame: &FrameData,
        store: &ImageStore,
        background: [f32; 4],
        glyph_gamma: f32,
        release_wait: Option<OwnedFd>,
    ) -> Result<Option<OwnedFd>> {
        self.prepare_frame(img, frame, store, background, glyph_gamma)?;
        // No release point means the buffer's first frame: nothing has read it yet.
        let wait_acquire = release_wait.is_some_and(|sync| self.import_acquire(img, sync));
        self.submit_frame_explicit(img, wait_acquire)
    }

    /// Image-texture uploads given up on since startup, for want of memory or
    /// descriptor sets. Each is a picture that stayed blank for a frame.
    pub fn dropped_image_uploads(&self) -> u64 {
        self.dropped_image_uploads
    }

    /// The acquire side of the per-frame ownership dance: where the image is
    /// coming from (undefined on first use, the compositor after).
    fn acquire_params(&self, img: &GpuImage) -> (u32, u32, u32) {
        if img.target.used {
            (
                VK_IMAGE_LAYOUT_GENERAL,
                VK_QUEUE_FAMILY_FOREIGN,
                self.queue_family,
            )
        } else {
            (
                VK_IMAGE_LAYOUT_UNDEFINED,
                VK_QUEUE_FAMILY_IGNORED,
                VK_QUEUE_FAMILY_IGNORED,
            )
        }
    }

    /// Submit `img`'s recorded command buffer: wait on its acquire semaphore
    /// when `wait_acquire`, signal its release semaphore (when it has one) and
    /// its fence. Sync out (bridge or explicit) is layered on by the callers
    /// below, each of which must leave the release semaphore unsignalled again:
    /// signalling a binary semaphore that still holds a signal is invalid.
    fn submit_only(&self, img: &mut GpuImage, wait_acquire: bool) -> Result<()> {
        let f = &self.fns;
        let wait_stage: u32 = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
        let submit = VkSubmitInfo {
            s_type: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            p_next: core::ptr::null(),
            wait_semaphore_count: u32::from(wait_acquire),
            p_wait_semaphores: &img.acquire,
            p_wait_dst_stage_mask: &wait_stage,
            command_buffer_count: 1,
            p_command_buffers: &img.cmd,
            signal_semaphore_count: u32::from(img.release != 0),
            p_signal_semaphores: &img.release,
        };
        // SAFETY: queue, cmd, semaphores, and fence are live; submit and its
        // pointees outlive the call.
        check(
            unsafe { (f.queue_submit)(self.queue, 1, &submit, img.fence) },
            "vkQueueSubmit",
        )?;
        img.target.used = true;
        Ok(())
    }

    /// CPU-wait for `img`'s render fence to signal, the fallback both submit
    /// paths take when the fence cannot be handed to the compositor. `what`
    /// names the calling path for the error. A slow correct frame beats a torn one.
    fn wait_fence(&self, img: &GpuImage, what: &str) -> Result<()> {
        // SAFETY: fence is live; the timeout bounds a broken driver.
        check(
            unsafe {
                (self.fns.wait_for_fences)(self.device.raw, 1, &img.fence, 1, FENCE_TIMEOUT_NS)
            },
            what,
        )
    }

    /// Submit under the implicit-sync bridge: after the submit, the render-done
    /// fence rides the dmabuf so the compositor waits for it. If the bridge is
    /// unavailable, wait on the CPU instead.
    fn submit_frame(&self, img: &mut GpuImage, wait_acquire: bool) -> Result<()> {
        self.submit_only(img, wait_acquire)?;
        let bridged = match self.render_done_fence(img)? {
            Some(sync) => ffi::dmabuf_import_sync_file(img.fd(), sync.as_raw_fd())?,
            None => false,
        };
        if !bridged {
            self.wait_fence(img, "vkWaitForFences (bridge fallback)")?;
        }
        Ok(())
    }

    /// Submit under explicit sync: after the submit, hand back the render-done
    /// sync file so the caller can publish it as this frame's acquire point. If
    /// none is exported, CPU-wait to completion and return `None`, for which the
    /// caller publishes an already-signalled acquire point.
    fn submit_frame_explicit(
        &self,
        img: &mut GpuImage,
        wait_acquire: bool,
    ) -> Result<Option<OwnedFd>> {
        self.submit_only(img, wait_acquire)?;
        match self.render_done_fence(img)? {
            Some(fd) => Ok(Some(fd)),
            None => {
                self.wait_fence(img, "vkWaitForFences (explicit fallback)")?;
                Ok(None)
            }
        }
    }

    /// Import `sync` (a sync file) as a temporary payload of the acquire
    /// semaphore so the submission waits on it. Returns whether a wait was
    /// armed; when the driver refuses the import, the CPU waits on `sync` here
    /// instead. Consumes `sync` (Vulkan owns it on success).
    fn import_acquire(&self, img: &GpuImage, sync: OwnedFd) -> bool {
        let raw = sync.as_raw_fd();
        let info = VkImportSemaphoreFdInfoKHR {
            s_type: VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_FD_INFO_KHR,
            p_next: core::ptr::null(),
            semaphore: img.acquire,
            flags: VK_SEMAPHORE_IMPORT_TEMPORARY_BIT,
            handle_type: VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
            fd: raw,
        };
        // SAFETY: info outlives the call; on success Vulkan owns the fd.
        let r = unsafe { (self.fns.import_semaphore_fd)(self.device.raw, &info) };
        if r == VK_SUCCESS {
            // Ownership moved to Vulkan; do not let OwnedFd close it.
            core::mem::forget(sync);
            true
        } else {
            // Failed import: we keep ownership (sync drops at the end of this
            // scope) and wait for the compositor here instead.
            ffi::wait_sync_file(raw, ACQUIRE_WAIT_MS);
            false
        }
    }

    /// The implicit-sync bridge's acquire: the dmabuf's current read/write
    /// fences become the acquire wait. A kernel that cannot export them is
    /// waited on the CPU instead.
    fn import_read_fences(&self, img: &GpuImage) -> Result<bool> {
        match ffi::dmabuf_export_sync_file(img.fd())? {
            Some(sync) => Ok(self.import_acquire(img, sync)),
            None => {
                ffi::wait_dmabuf_writable(img.fd(), ACQUIRE_WAIT_MS);
                Ok(false)
            }
        }
    }

    /// Export the just-submitted release semaphore's signal as a sync file,
    /// leaving the semaphore unsignalled for the next submit either way.
    ///
    /// `None` means there is no fence to hand on and the caller must CPU-wait:
    /// the slot has no release semaphore, the driver reported the semaphore
    /// already signalled by returning fd -1 (which the spec allows, and which
    /// still consumes the signal), or the export failed.
    ///
    /// A failed export consumes nothing, so the semaphore would go into the next
    /// submit still signalled. It is replaced here instead, once the submission
    /// that signals it has completed.
    fn render_done_fence(&self, img: &mut GpuImage) -> Result<Option<OwnedFd>> {
        if img.release == 0 {
            return Ok(None);
        }
        let info = VkSemaphoreGetFdInfoKHR {
            s_type: VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR,
            p_next: core::ptr::null(),
            semaphore: img.release,
            handle_type: VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
        };
        let mut raw: i32 = -1;
        // SAFETY: the semaphore has a pending signal from the submit above,
        // which is what a sync-fd export requires.
        let r = unsafe { (self.fns.get_semaphore_fd)(self.device.raw, &info, &mut raw) };
        if r != VK_SUCCESS {
            self.replace_release_semaphore(img)?;
            return Ok(None);
        }
        if raw < 0 {
            return Ok(None);
        }
        // SAFETY: the driver handed us ownership of this sync-file fd.
        Ok(Some(unsafe { OwnedFd::from_raw_fd(raw) }))
    }

    /// Swap `img`'s release semaphore, which still holds the signal of the
    /// submission in flight, for a fresh one. If a fresh one cannot be made the
    /// slot goes on without (see [`GpuImage::release`]).
    fn replace_release_semaphore(&self, img: &mut GpuImage) -> Result<()> {
        // The semaphore may be destroyed only once the submission that signals
        // it has completed.
        self.wait_fence(img, "vkWaitForFences (release export failed)")?;
        let spent = std::mem::replace(&mut img.release, 0);
        // SAFETY: `spent` was created on this device, the only submission that
        // names it has completed (fence waited above), and the slot no longer
        // holds the handle, so it is destroyed exactly once.
        unsafe { (self.fns.destroy_semaphore)(self.device.raw, spent, core::ptr::null()) };
        img.release = self.create_release_semaphore().unwrap_or(0);
        Ok(())
    }

    /// Record `img`'s command buffer for one frame: texture uploads, the
    /// acquire barrier, the render pass (clear plus batched quads), and the
    /// release back to the compositor in GENERAL layout.
    fn record_frame(
        &self,
        img: &GpuImage,
        frame: &FrameData,
        uploads: &[PendingUpload],
        background: [f32; 4],
        glyph_gamma: f32,
    ) -> Result<()> {
        let f = &self.fns;
        let begin = VkCommandBufferBeginInfo {
            s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            p_next: core::ptr::null(),
            flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
            p_inheritance_info: core::ptr::null(),
        };
        // SAFETY: the pool allows reset-on-begin; the previous submission
        // completed (fence wait in prepare_frame).
        check(
            unsafe { (f.begin_command_buffer)(img.cmd, &begin) },
            "vkBeginCommandBuffer",
        )?;

        for up in uploads {
            self.record_upload(img, up);
        }

        let range = color_range();
        let (old_layout, src_qf, dst_qf) = self.acquire_params(img);
        // SAFETY (all cmd_* to the end): cmd is in the recording state; every
        // struct outlives its call; handles are live.
        unsafe {
            let acquire = VkImageMemoryBarrier {
                s_type: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                p_next: core::ptr::null(),
                src_access_mask: 0,
                dst_access_mask: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                old_layout,
                new_layout: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                src_queue_family_index: src_qf,
                dst_queue_family_index: dst_qf,
                image: img.target.image,
                subresource_range: range,
            };
            // The acquire semaphore is waited at COLOR_ATTACHMENT_OUTPUT. Starting
            // the barrier at that stage chains it to the wait, so the transition
            // runs after the compositor's reads are done.
            (f.cmd_pipeline_barrier)(
                img.cmd,
                VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                0,
                0,
                core::ptr::null(),
                0,
                core::ptr::null(),
                1,
                &acquire,
            );

            let clear = VkClearColorValue {
                float32: background,
            };
            let pass = VkRenderPassBeginInfo {
                s_type: VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
                p_next: core::ptr::null(),
                render_pass: self.renderer.render_pass,
                framebuffer: img.target.framebuffer,
                render_area: VkRect2D {
                    offset: VkOffset2D { x: 0, y: 0 },
                    extent: VkExtent2D {
                        width: img.target.width,
                        height: img.target.height,
                    },
                },
                clear_value_count: 1,
                p_clear_values: &clear,
            };
            (f.cmd_begin_render_pass)(img.cmd, &pass, VK_SUBPASS_CONTENTS_INLINE);

            if !frame.vertices.is_empty() {
                (f.cmd_bind_pipeline)(
                    img.cmd,
                    VK_PIPELINE_BIND_POINT_GRAPHICS,
                    self.renderer.pipeline,
                );
                let viewport = VkViewport {
                    x: 0.0,
                    y: 0.0,
                    width: img.target.width as f32,
                    height: img.target.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                };
                (f.cmd_set_viewport)(img.cmd, 0, 1, &viewport);
                let scissor = VkRect2D {
                    offset: VkOffset2D { x: 0, y: 0 },
                    extent: VkExtent2D {
                        width: img.target.width,
                        height: img.target.height,
                    },
                };
                (f.cmd_set_scissor)(img.cmd, 0, 1, &scissor);
                let offset = 0u64;
                (f.cmd_bind_vertex_buffers)(img.cmd, 0, 1, &img.vertices.buf, &offset);
                // Viewport for the vertex stage, glyph mask gamma for the
                // fragment stage, in one push-constant range spanning both.
                let push: [f32; 3] = [
                    img.target.width as f32,
                    img.target.height as f32,
                    glyph_gamma,
                ];
                (f.cmd_push_constants)(
                    img.cmd,
                    self.renderer.pipeline_layout,
                    VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT,
                    0,
                    12,
                    push.as_ptr().cast(),
                );
                (f.cmd_bind_descriptor_sets)(
                    img.cmd,
                    VK_PIPELINE_BIND_POINT_GRAPHICS,
                    self.renderer.pipeline_layout,
                    0,
                    1,
                    &self.renderer.set_atlas,
                    0,
                    core::ptr::null(),
                );
                // Track the bound image set so consecutive non-image batches
                // do not rebind the dummy.
                let mut bound: VkDescriptorSet = 0;
                for batch in &frame.batches {
                    let set = match &batch.image {
                        Some(key) => match self.renderer.images.get(key) {
                            Some(image) => image.set,
                            // No texture: the upload was dropped (out of
                            // memory or sets, or too large for the device), so
                            // the picture is left undrawn this frame.
                            None => continue,
                        },
                        None => self.renderer.set_dummy,
                    };
                    if set != bound {
                        (f.cmd_bind_descriptor_sets)(
                            img.cmd,
                            VK_PIPELINE_BIND_POINT_GRAPHICS,
                            self.renderer.pipeline_layout,
                            1,
                            1,
                            &set,
                            0,
                            core::ptr::null(),
                        );
                        bound = set;
                    }
                    (f.cmd_draw)(img.cmd, batch.count, 1, batch.start, 0);
                }
            }

            (f.cmd_end_render_pass)(img.cmd);

            let release = VkImageMemoryBarrier {
                s_type: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                p_next: core::ptr::null(),
                src_access_mask: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                dst_access_mask: 0,
                old_layout: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                new_layout: VK_IMAGE_LAYOUT_GENERAL,
                src_queue_family_index: self.queue_family,
                dst_queue_family_index: VK_QUEUE_FAMILY_FOREIGN,
                image: img.target.image,
                subresource_range: range,
            };
            (f.cmd_pipeline_barrier)(
                img.cmd,
                VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                0,
                0,
                core::ptr::null(),
                0,
                core::ptr::null(),
                1,
                &release,
            );
        }
        // SAFETY: cmd is in the recording state.
        check(
            unsafe { (f.end_command_buffer)(img.cmd) },
            "vkEndCommandBuffer",
        )
    }

    /// Record one texture upload (or a bare initialization barrier when the
    /// upload carries no bytes, the dummy texture's case).
    fn record_upload(&self, img: &GpuImage, up: &PendingUpload) {
        let f = &self.fns;
        let range = color_range();
        let (old_layout, src_stage, src_access) = if up.was_initialized {
            (
                VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                VK_ACCESS_SHADER_READ_BIT,
            )
        } else {
            (
                VK_IMAGE_LAYOUT_UNDEFINED,
                VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                0,
            )
        };
        // SAFETY (whole fn): cmd is recording; structs outlive their calls.
        unsafe {
            let to_dst = VkImageMemoryBarrier {
                s_type: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                p_next: core::ptr::null(),
                src_access_mask: src_access,
                dst_access_mask: VK_ACCESS_TRANSFER_WRITE_BIT,
                old_layout,
                new_layout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                src_queue_family_index: VK_QUEUE_FAMILY_IGNORED,
                dst_queue_family_index: VK_QUEUE_FAMILY_IGNORED,
                image: up.image,
                subresource_range: range,
            };
            (f.cmd_pipeline_barrier)(
                img.cmd,
                src_stage,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                0,
                0,
                core::ptr::null(),
                0,
                core::ptr::null(),
                1,
                &to_dst,
            );
            if up.w > 0 && up.h > 0 {
                let region = VkBufferImageCopy {
                    buffer_offset: up.staging_offset as u64,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: VkImageSubresourceLayers {
                        aspect_mask: VK_IMAGE_ASPECT_COLOR_BIT,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_offset: VkOffset3D {
                        x: up.x as i32,
                        y: up.y as i32,
                        z: 0,
                    },
                    image_extent: VkExtent3D {
                        width: up.w,
                        height: up.h,
                        depth: 1,
                    },
                };
                (f.cmd_copy_buffer_to_image)(
                    img.cmd,
                    img.staging.buf,
                    up.image,
                    VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                    1,
                    &region,
                );
            }
            let to_read = VkImageMemoryBarrier {
                s_type: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                p_next: core::ptr::null(),
                src_access_mask: VK_ACCESS_TRANSFER_WRITE_BIT,
                dst_access_mask: VK_ACCESS_SHADER_READ_BIT,
                old_layout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                new_layout: VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                src_queue_family_index: VK_QUEUE_FAMILY_IGNORED,
                dst_queue_family_index: VK_QUEUE_FAMILY_IGNORED,
                image: up.image,
                subresource_range: range,
            };
            (f.cmd_pipeline_barrier)(
                img.cmd,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                0,
                0,
                core::ptr::null(),
                0,
                core::ptr::null(),
                1,
                &to_read,
            );
        }
    }

    /// The frame's new image textures that can be made at all: not already
    /// resident, still in the store at the size the frame was built for, and
    /// within the device's largest image.
    fn wanted_images<'a>(
        &self,
        frame: &'a FrameData,
        store: &'a ImageStore,
    ) -> Vec<WantedImage<'a>> {
        let max = self.max_image_dimension;
        frame
            .image_uploads
            .iter()
            .filter(|key| !self.renderer.images.contains_key(*key))
            .filter_map(|key| {
                // The store is the owner; skip an image it no longer holds, or
                // holds a newer decode of (evicted or replaced between building
                // the list and uploading), rather than upload the wrong pixels.
                let img = store
                    .get(&key.image)
                    .filter(|_| store.generation(&key.image) == Some(key.generation))?;
                let (w, h) = (img.width, img.height);
                // A texture larger than the device allows cannot be created; the
                // picture stays undrawn rather than ending the program.
                if w == 0 || h == 0 || w > max || h > max {
                    return None;
                }
                // The copy reads `w * h` texels out of staging; a shorter buffer
                // would have it read whatever follows.
                (img.pixels.len() == w as usize * h as usize).then_some(WantedImage {
                    key,
                    width: w,
                    height: h,
                    argb: &img.pixels,
                })
            })
            .collect()
    }

    /// Reconcile the renderer's textures with this frame's data: (re)create
    /// atlas textures on generation changes, create textures and descriptor
    /// sets for the `wanted` images (evicting old ones first when they would not
    /// fit), and assemble the staging sources plus copy list.
    fn sync_frame_textures<'a>(
        &mut self,
        frame: &'a FrameData,
        wanted: Vec<WantedImage<'a>>,
    ) -> Result<(Staging<'a>, Vec<PendingUpload>)> {
        // Sources are collected, not copied: the bytes go straight from their owner
        // into mapped memory once every source is known.
        let mut staging = Staging::default();
        let mut uploads = Vec::new();

        // The dummy texture: 1x1, never written, bound whenever a batch has
        // no image. Contents are irrelevant (no mode samples it), only the
        // layout must be right, so its "upload" is a bare barrier.
        if self.renderer.dummy.image == 0 {
            let dummy = self.create_texture(VK_FORMAT_B8G8R8A8_UNORM, 1, 1)?;
            write_image_set(
                &self.fns,
                self.device.raw,
                self.renderer.set_dummy,
                0,
                self.renderer.sampler,
                dummy.view,
            );
            uploads.push(PendingUpload {
                image: dummy.image,
                was_initialized: false,
                staging_offset: 0,
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            });
            self.renderer.dummy = dummy;
        }

        // Atlas textures follow the CPU mirrors' generations, then take any
        // dirty-region upload for the frame. The shared descriptor set is
        // repointed once both textures exist, so it never names a missing one.
        let glyphs_changed = self.sync_atlas(AtlasSlot::Glyph, &frame.glyph_atlas)?;
        let emoji_changed = self.sync_atlas(AtlasSlot::Emoji, &frame.emoji_atlas)?;
        if glyphs_changed || emoji_changed {
            self.update_atlas_set()?;
        }
        self.stage_atlas_upload(
            AtlasSlot::Glyph,
            &frame.glyph_upload,
            &mut staging,
            &mut uploads,
        );
        self.stage_atlas_upload(
            AtlasSlot::Emoji,
            &frame.emoji_upload,
            &mut staging,
            &mut uploads,
        );

        // Stamp what this frame draws, which is what eviction must spare.
        self.renderer.frame += 1;
        let now = self.renderer.frame;
        for key in frame.batches.iter().filter_map(|b| b.image.as_ref()) {
            if let Some(image) = self.renderer.images.get_mut(key) {
                image.last_drawn = now;
            }
        }

        // Make room before allocating: the descriptor pool is finite and device
        // memory is not ours alone, so textures no longer on screen go first.
        let incoming_bytes = wanted.iter().map(|w| w.argb.len() as u64 * 4).sum();
        self.evict_for(wanted.len(), incoming_bytes, now);

        // New image textures, one per (key, native size). The pixels come from
        // the store rather than a copy carried in the frame, so the decoded
        // buffer reaches staging without an intermediate image-sized clone.
        for want in wanted {
            let (w, h) = (want.width, want.height);
            let Some((texture, set)) = self.create_image_texture(w, h, now) else {
                // The picture does not draw this frame (its batch finds no
                // texture and is skipped). The caller sees it has no texture
                // and presents again later.
                self.dropped_image_uploads += 1;
                continue;
            };
            uploads.push(PendingUpload {
                image: texture.image,
                was_initialized: false,
                staging_offset: staging.push(StagingSrc::Argb(want.argb)),
                x: 0,
                y: 0,
                w,
                h,
            });
            self.renderer.images.insert(
                want.key.clone(),
                ImageTexture {
                    texture: Texture {
                        initialized: true,
                        ..texture
                    },
                    set,
                    bytes: want.argb.len() as u64 * 4,
                    last_drawn: now,
                },
            );
        }

        // Uploads whose staging_offset was pushed before later bytes landed
        // stay valid: offsets only ever append.
        Ok((staging, uploads))
    }

    /// Rebuild an atlas texture when its CPU mirror's generation moved (or it
    /// does not exist yet), returning whether it did. A replacement may still be
    /// sampled by the other slot's in-flight frame, so the queue is drained
    /// before the old texture is destroyed; the caller repoints the shared atlas
    /// descriptor set once both textures exist. Shared by the glyph and emoji
    /// slots so that ordering lives in one place.
    fn sync_atlas(&mut self, slot: AtlasSlot, info: &AtlasInfo) -> Result<bool> {
        let tex = match slot {
            AtlasSlot::Glyph => &mut self.renderer.glyph_tex,
            AtlasSlot::Emoji => &mut self.renderer.emoji_tex,
        };
        if tex.as_ref().is_some_and(|(_, g)| *g == info.generation) {
            return Ok(false);
        }
        if let Some((old, _)) = tex.take() {
            self.wait_idle();
            destroy_texture(&self.fns, self.device.raw, old);
        }
        let tex = self.create_texture(slot.format(), info.width, info.height)?;
        match slot {
            AtlasSlot::Glyph => self.renderer.glyph_tex = Some((tex, info.generation)),
            AtlasSlot::Emoji => self.renderer.emoji_tex = Some((tex, info.generation)),
        }
        Ok(true)
    }

    /// Queue an atlas's dirty region for upload into its live texture, appending
    /// the bytes to `staging`. A no-op when the texture does not exist yet.
    fn stage_atlas_upload<'a>(
        &mut self,
        slot: AtlasSlot,
        upload: &'a Option<AtlasUpload>,
        staging: &mut Staging<'a>,
        uploads: &mut Vec<PendingUpload>,
    ) {
        let tex = match slot {
            AtlasSlot::Glyph => self.renderer.glyph_tex.as_mut(),
            AtlasSlot::Emoji => self.renderer.emoji_tex.as_mut(),
        };
        let Some((tex, _)) = tex else {
            return;
        };
        let Some(up) = upload else {
            // A fresh texture with nothing to upload is still bound and sampled as
            // SHADER_READ_ONLY, and the colour atlas is exactly that for as long as
            // no emoji is on screen. A bare barrier gives it the layout it is
            // bound in.
            if !tex.initialized {
                uploads.push(PendingUpload {
                    image: tex.image,
                    was_initialized: false,
                    staging_offset: 0,
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                });
                tex.initialized = true;
            }
            return;
        };
        uploads.push(PendingUpload {
            image: tex.image,
            was_initialized: tex.initialized,
            staging_offset: staging.push(StagingSrc::Bytes(&up.bytes)),
            x: up.x,
            y: up.y,
            w: up.w,
            h: up.h,
        });
        tex.initialized = true;
    }

    /// Evict image textures so that `incoming_count` new ones totalling
    /// `incoming_bytes` fit the limits, per [`eviction_plan`]. `now` is the
    /// frame being prepared; what it draws is never evicted.
    fn evict_for(&mut self, incoming_count: usize, incoming_bytes: u64, now: u64) {
        let (keys, residents): (Vec<&TextureKey>, Vec<Resident>) = self
            .renderer
            .images
            .iter()
            .map(|(key, image)| {
                let resident = Resident {
                    bytes: image.bytes,
                    last_drawn: image.last_drawn,
                    drawn: image.last_drawn == now,
                };
                (key, resident)
            })
            .unzip();
        let victims: Vec<TextureKey> =
            eviction_plan(&residents, incoming_count, incoming_bytes, IMAGE_LIMITS)
                .into_iter()
                .filter_map(|i| keys.get(i).map(|key| (*key).clone()))
                .collect();
        self.evict_images(&victims);
    }

    /// Evict every image texture the frame `now` does not draw: the last resort
    /// when an allocation fails. Returns whether anything was freed.
    fn evict_undrawn(&mut self, now: u64) -> bool {
        let victims: Vec<TextureKey> = self
            .renderer
            .images
            .iter()
            .filter(|(_, image)| image.last_drawn != now)
            .map(|(key, _)| key.clone())
            .collect();
        self.evict_images(&victims);
        !victims.is_empty()
    }

    /// Destroy the image textures under `keys` and return their descriptor sets
    /// to the pool. The other slot's in-flight frame may still sample them, so
    /// the queue is drained first.
    fn evict_images(&mut self, keys: &[TextureKey]) {
        if keys.is_empty() {
            return;
        }
        self.wait_idle();
        for key in keys {
            if let Some(image) = self.renderer.images.remove(key) {
                self.free_image_set(image.set);
                destroy_texture(&self.fns, self.device.raw, image.texture);
            }
        }
    }

    /// Return an image's descriptor set to the pool.
    fn free_image_set(&self, set: VkDescriptorSet) {
        // SAFETY: the pool was created with the free flag, the set came from it,
        // and no pending command buffer references the set (the caller drained
        // the queue, or the set was never bound).
        unsafe {
            (self.fns.free_descriptor_sets)(self.device.raw, self.renderer.descriptor_pool, 1, &set)
        };
    }

    /// A texture and descriptor set for one `w` x `h` image, or `None` when
    /// there is no room for it: the set pool is full of textures this frame
    /// draws, or the device is out of memory even after everything the frame
    /// `now` does not draw has been evicted. Never an error: a picture that
    /// cannot be uploaded is a blank cell, not a reason to exit.
    fn create_image_texture(
        &mut self,
        w: u32,
        h: u32,
        now: u64,
    ) -> Option<(Texture, VkDescriptorSet)> {
        if self.renderer.images.len() >= IMAGE_LIMITS.max_count {
            return None;
        }
        if let Some(made) = self.try_image_texture(w, h) {
            return Some(made);
        }
        if !self.evict_undrawn(now) {
            return None;
        }
        self.try_image_texture(w, h)
    }

    /// One attempt at an image's texture and descriptor set, leaving nothing
    /// behind when either half fails.
    fn try_image_texture(&self, w: u32, h: u32) -> Option<(Texture, VkDescriptorSet)> {
        // sRGB format: the sampler decodes and linear-filters when scaling the
        // image to its display size, matching the linear-space blend.
        let texture = self.create_texture(VK_FORMAT_B8G8R8A8_SRGB, w, h).ok()?;
        match self.allocate_image_set(texture.view) {
            Ok(set) => Some((texture, set)),
            Err(_) => {
                // Nothing was submitted with the texture, so it can go at once.
                destroy_texture(&self.fns, self.device.raw, texture);
                None
            }
        }
    }

    /// Point the shared atlas descriptor set at the two atlas textures. Called
    /// only after both have been synced, so both exist.
    fn update_atlas_set(&self) -> Result<()> {
        let (Some((glyphs, _)), Some((emoji, _))) =
            (&self.renderer.glyph_tex, &self.renderer.emoji_tex)
        else {
            return Err(Error::msg("atlas descriptor update before both atlases"));
        };
        write_image_set(
            &self.fns,
            self.device.raw,
            self.renderer.set_atlas,
            0,
            self.renderer.sampler,
            glyphs.view,
        );
        write_image_set(
            &self.fns,
            self.device.raw,
            self.renderer.set_atlas,
            1,
            self.renderer.sampler,
            emoji.view,
        );
        Ok(())
    }

    /// Allocate and fill a descriptor set for one image texture.
    fn allocate_image_set(&self, view: VkImageView) -> Result<VkDescriptorSet> {
        let set = allocate_set(
            &self.fns,
            self.device.raw,
            self.renderer.descriptor_pool,
            self.renderer.set_layout_image,
            "vkAllocateDescriptorSets",
        )?;
        write_image_set(
            &self.fns,
            self.device.raw,
            set,
            0,
            self.renderer.sampler,
            view,
        );
        Ok(set)
    }

    /// A sampled, transfer-destination 2D texture (an atlas or an image).
    fn create_texture(&self, format: u32, width: u32, height: u32) -> Result<Texture> {
        let d = self.device.raw;
        let f = &self.fns;
        let info = VkImageCreateInfo {
            s_type: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            p_next: core::ptr::null(),
            flags: 0,
            image_type: VK_IMAGE_TYPE_2D,
            format,
            extent: VkExtent3D {
                width,
                height,
                depth: 1,
            },
            mip_levels: 1,
            array_layers: 1,
            samples: VK_SAMPLE_COUNT_1_BIT,
            tiling: VK_IMAGE_TILING_OPTIMAL,
            usage: VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
            sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
            queue_family_index_count: 0,
            p_queue_family_indices: core::ptr::null(),
            initial_layout: VK_IMAGE_LAYOUT_UNDEFINED,
        };
        // The guard destroys whatever exists so far if any step below fails.
        let mut pending = PendingTexture {
            gpu: self,
            image: 0,
            memory: 0,
            view: 0,
        };
        // SAFETY: info outlives the call.
        check(
            unsafe { (f.create_image)(d, &info, core::ptr::null(), &mut pending.image) },
            "vkCreateImage (texture)",
        )?;
        let mut reqs = VkMemoryRequirements {
            size: 0,
            alignment: 0,
            memory_type_bits: 0,
        };
        // SAFETY: image is live.
        unsafe { (f.get_image_memory_requirements)(d, pending.image, &mut reqs) };
        let memory_type = pick_memory_type(&self.memory_type_flags, reqs.memory_type_bits, 0)
            .ok_or_else(|| Error::msg("no memory type fits a texture"))?;
        let alloc = VkMemoryAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            p_next: core::ptr::null(),
            allocation_size: reqs.size,
            memory_type_index: memory_type,
        };
        // SAFETY: alloc outlives the call.
        check(
            unsafe { (f.allocate_memory)(d, &alloc, core::ptr::null(), &mut pending.memory) },
            "vkAllocateMemory (texture)",
        )?;
        // SAFETY: image and memory are live and compatible.
        check(
            unsafe { (f.bind_image_memory)(d, pending.image, pending.memory, 0) },
            "vkBindImageMemory (texture)",
        )?;
        pending.view = self.create_view(pending.image, format)?;
        // Success: hand the handles to the Texture and disarm the guard.
        let texture = Texture {
            image: pending.image,
            memory: pending.memory,
            view: pending.view,
            initialized: false,
        };
        core::mem::forget(pending);
        Ok(texture)
    }

    /// A plain 2D color view of `image`.
    fn create_view(&self, image: VkImage, format: u32) -> Result<VkImageView> {
        let info = VkImageViewCreateInfo {
            s_type: VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
            p_next: core::ptr::null(),
            flags: 0,
            image,
            view_type: VK_IMAGE_VIEW_TYPE_2D,
            format,
            components: VkComponentMapping {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            },
            subresource_range: color_range(),
        };
        let mut view: VkImageView = 0;
        // SAFETY: image is live; info outlives the call.
        check(
            unsafe {
                (self.fns.create_image_view)(self.device.raw, &info, core::ptr::null(), &mut view)
            },
            "vkCreateImageView",
        )?;
        Ok(view)
    }

    /// A host-visible, host-coherent, persistently mapped buffer.
    fn create_host_buffer(&self, size: usize, usage: u32) -> Result<HostBuffer> {
        let d = self.device.raw;
        let f = &self.fns;
        let info = VkBufferCreateInfo {
            s_type: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
            p_next: core::ptr::null(),
            flags: 0,
            size: size as u64,
            usage,
            sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
            queue_family_index_count: 0,
            p_queue_family_indices: core::ptr::null(),
        };
        // The guard destroys whatever exists so far if any step below fails.
        let mut pending = PendingBuffer {
            gpu: self,
            buf: 0,
            memory: 0,
        };
        // SAFETY: info outlives the call.
        check(
            unsafe { (f.create_buffer)(d, &info, core::ptr::null(), &mut pending.buf) },
            "vkCreateBuffer",
        )?;
        let mut reqs = VkMemoryRequirements {
            size: 0,
            alignment: 0,
            memory_type_bits: 0,
        };
        // SAFETY: buf is live.
        unsafe { (f.get_buffer_memory_requirements)(d, pending.buf, &mut reqs) };
        let required = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
        let memory_type =
            pick_memory_type(&self.memory_type_flags, reqs.memory_type_bits, required)
                .ok_or_else(|| Error::msg("no host-visible memory type for a buffer"))?;
        let alloc = VkMemoryAllocateInfo {
            s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            p_next: core::ptr::null(),
            allocation_size: reqs.size,
            memory_type_index: memory_type,
        };
        // SAFETY: alloc outlives the call.
        check(
            unsafe { (f.allocate_memory)(d, &alloc, core::ptr::null(), &mut pending.memory) },
            "vkAllocateMemory (buffer)",
        )?;
        // SAFETY: buf and memory are live and compatible.
        let r = unsafe { (f.bind_buffer_memory)(d, pending.buf, pending.memory, 0) };
        let mut ptr: *mut c_void = core::ptr::null_mut();
        // SAFETY: memory is live, host-visible, and bound (checked below).
        let r2 = if r == VK_SUCCESS {
            unsafe { (f.map_memory)(d, pending.memory, 0, u64::MAX, 0, &mut ptr) }
        } else {
            r
        };
        if r2 != VK_SUCCESS || ptr.is_null() {
            return Err(Error::msg(format!("buffer bind/map failed: VkResult {r2}")));
        }
        // Success: hand the handles to the HostBuffer and disarm the guard.
        let buffer = HostBuffer {
            buf: pending.buf,
            memory: pending.memory,
            ptr: ptr.cast(),
            capacity: size,
        };
        core::mem::forget(pending);
        Ok(buffer)
    }

    /// Free a host buffer (a zeroed default is a no-op).
    fn destroy_host_buffer(&self, b: HostBuffer) {
        if b.buf == 0 {
            return;
        }
        // SAFETY: buf/memory are live and no submission references them (the
        // caller waited the slot's fence or the queue). Freeing unmaps.
        unsafe {
            (self.fns.destroy_buffer)(self.device.raw, b.buf, core::ptr::null());
            (self.fns.free_memory)(self.device.raw, b.memory, core::ptr::null());
        }
    }

    /// Grow `b` to at least `needed` bytes (doubling), preserving nothing: the
    /// caller rewrites the whole buffer each frame. For the vertex buffer; the
    /// staging buffer has its own policy ([`Self::fit_staging`]).
    fn grow_host_buffer(&self, b: &mut HostBuffer, needed: usize, usage: u32) -> Result<()> {
        if needed <= b.capacity {
            return Ok(());
        }
        let new_cap = needed.max(b.capacity * 2);
        let fresh = self.create_host_buffer(new_cap, usage)?;
        let old = std::mem::replace(b, fresh);
        self.destroy_host_buffer(old);
        Ok(())
    }

    /// Give `img`'s staging buffer the capacity [`staging_plan`] calls for, given
    /// that this frame stages `needed` bytes. An error means a larger buffer
    /// could not be made; failing to make a smaller one keeps the buffer as is.
    fn fit_staging(&self, img: &mut GpuImage, needed: usize) -> Result<()> {
        let (resize, quiet) = staging_plan(img.staging.capacity, needed, img.staging_quiet_frames);
        img.staging_quiet_frames = quiet;
        let Some(capacity) = resize else {
            return Ok(());
        };
        match self.create_host_buffer(capacity, STAGING_USAGE) {
            Ok(fresh) => {
                // The slot's fence was waited before this frame touched the
                // buffer, so nothing in flight reads the old one.
                let old = std::mem::replace(&mut img.staging, fresh);
                self.destroy_host_buffer(old);
                Ok(())
            }
            Err(e) if needed > img.staging.capacity => Err(e),
            Err(_) => Ok(()),
        }
    }
}

/// A staging buffer feeds texture copies and nothing else.
const STAGING_USAGE: u32 = VK_BUFFER_USAGE_TRANSFER_SRC_BIT;

/// What a staging buffer of `capacity` bytes should become for a frame staging
/// `needed`, and the slot's new count of quiet frames: `(Some(new capacity) to
/// recreate it, quiet frames)`.
///
/// A photograph is uploaded through this buffer once and never again, so a
/// buffer that only ever grew would hold a photograph's worth of host memory
/// (96 MB at 24 megapixels) per swap slot for the rest of the session. The
/// policy: up to [`STAGING_KEEP`] the buffer doubles and is kept; past that it
/// is sized to the need (in `STAGING_KEEP` steps, so a run of slightly larger
/// pictures does not recreate it each time), and once
/// [`STAGING_QUIET_FRAMES`] frames in a row have needed under a quarter of it,
/// it shrinks back to what that frame needs.
fn staging_plan(capacity: usize, needed: usize, quiet_frames: u32) -> (Option<usize>, u32) {
    let sized = |bytes: usize| {
        bytes
            .checked_next_multiple_of(STAGING_KEEP)
            .unwrap_or(bytes)
            .max(STAGING_KEEP)
    };
    if needed > capacity {
        let grown = if needed <= STAGING_KEEP {
            needed.max(capacity.saturating_mul(2)).min(STAGING_KEEP)
        } else {
            sized(needed)
        };
        return (Some(grown), 0);
    }
    if capacity <= STAGING_KEEP || needed >= capacity / 4 {
        return (None, 0);
    }
    let quiet = quiet_frames.saturating_add(1);
    if quiet < STAGING_QUIET_FRAMES {
        return (None, quiet);
    }
    (Some(sized(needed)), 0)
}

/// An image texture as the eviction policy sees it.
#[derive(Clone, Copy)]
struct Resident {
    bytes: u64,
    /// The frame that last drew it; older goes first.
    last_drawn: u64,
    /// Whether the frame being prepared draws it. Such a texture is never
    /// evicted: the frame's command buffer is about to bind it.
    drawn: bool,
}

/// The bounds [`eviction_plan`] keeps the image textures within.
#[derive(Clone, Copy)]
struct ImageLimits {
    max_count: usize,
    max_bytes: u64,
}

/// Which of `residents` (by index) to evict so that `incoming_count` new
/// textures totalling `incoming_bytes` fit within `limits`.
///
/// Nothing, while everything fits. Once it would not, the textures the current
/// frame does not draw go, least recently drawn first, until the total is down
/// to half of each limit (or nothing evictable is left). Stopping at half
/// rather than at the limit is what keeps eviction rare: each one drains the
/// queue, and scrolling a grid at the limit would otherwise drain it every
/// frame. What the frame draws always stays, so a frame larger than the limits
/// is allowed to exceed the byte budget; the count limit is the descriptor
/// pool's size and is enforced where the textures are created.
fn eviction_plan(
    residents: &[Resident],
    incoming_count: usize,
    incoming_bytes: u64,
    limits: ImageLimits,
) -> Vec<usize> {
    let mut count = residents.len() + incoming_count;
    let mut bytes = residents
        .iter()
        .fold(incoming_bytes, |sum, r| sum.saturating_add(r.bytes));
    if count <= limits.max_count && bytes <= limits.max_bytes {
        return Vec::new();
    }
    let mut candidates: Vec<(usize, &Resident)> = residents
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.drawn)
        .collect();
    candidates.sort_by_key(|(_, r)| r.last_drawn);
    let mut victims = Vec::new();
    for (index, resident) in candidates {
        if count <= limits.max_count / 2 && bytes <= limits.max_bytes / 2 {
            break;
        }
        count -= 1;
        bytes = bytes.saturating_sub(resident.bytes);
        victims.push(index);
    }
    victims
}

/// An image texture the frame wants created: its key, and the size and pixels
/// to upload, borrowed from the store.
struct WantedImage<'a> {
    key: &'a TextureKey,
    width: u32,
    height: u32,
    argb: &'a [u32],
}

/// One texture copy scheduled for this frame's command buffer: which image,
/// where its bytes sit in the staging buffer, and the destination region.
/// A zero-sized region is a bare layout-initialization barrier.
struct PendingUpload {
    image: VkImage,
    was_initialized: bool,
    staging_offset: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// One run of bytes bound for the staging buffer, borrowed from whoever owns it: a
/// frame's atlas dirty region, or an image's pixels in the store. Borrowing rather
/// than copying is the point — the bytes are written into mapped memory once,
/// straight from their owner.
enum StagingSrc<'a> {
    Bytes(&'a [u8]),
    Argb(&'a [u32]),
}

impl StagingSrc<'_> {
    fn len(&self) -> usize {
        match self {
            StagingSrc::Bytes(b) => b.len(),
            StagingSrc::Argb(w) => w.len() * 4,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        match self {
            StagingSrc::Bytes(b) => b,
            StagingSrc::Argb(w) => argb_as_bytes(w),
        }
    }
}

/// The frame's staging sources in write order, with the running total that gives
/// each its offset.
///
/// The offset recorded in a copy command and the position the bytes are actually
/// written at both come out of [`Staging::push`], so they cannot drift apart. That
/// is the hazard in writing straight into mapped memory rather than assembling a
/// buffer first, and keeping both derived from one list is what removes it.
#[derive(Default)]
struct Staging<'a> {
    /// Each source with the offset it is written at.
    sources: Vec<(usize, StagingSrc<'a>)>,
    total: usize,
}

/// A copy into an image must start at a multiple of the image's texel size. Four
/// bytes is the largest any texture here has, and a multiple of the coverage
/// atlas's one.
const STAGING_ALIGN: usize = 4;

impl<'a> Staging<'a> {
    /// Record `src` and return the offset it will be written at: the next whole
    /// texel, since a coverage upload before it can end on any byte.
    fn push(&mut self, src: StagingSrc<'a>) -> usize {
        let (at, end) = staging_place(self.total, src.len());
        self.total = end;
        self.sources.push((at, src));
        at
    }
}

/// Where a source of `len` bytes lands in a staging buffer already holding
/// `total`, and the total after it: `(offset, new total)`.
fn staging_place(total: usize, len: usize) -> (usize, usize) {
    let at = total.next_multiple_of(STAGING_ALIGN);
    (at, at + len)
}

/// The staging bytes sources of these lengths occupy when pushed in order: what
/// [`Staging::total`] will read once they all are.
fn staging_end(lens: impl Iterator<Item = usize>) -> usize {
    lens.fold(0, |total, len| staging_place(total, len).1)
}

/// View ARGB words as the bytes the GPU reads, without converting them. A
/// native-endian `u32` is already B,G,R,A in memory, which is exactly
/// `B8G8R8A8`, so the texture upload is a memcpy of the decoded buffer rather
/// than a per-pixel repack.
fn argb_as_bytes(words: &[u32]) -> &[u8] {
    // SAFETY: `u32` is four bytes and more strictly aligned than `u8`, so the
    // pointer is valid and aligned for `len * 4` bytes, and the borrow keeps the
    // words alive for the returned slice's lifetime.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 4) }
}

#[cfg(test)]
mod tests {
    use crate::render::vulkan::target::{
        eviction_plan, staging_end, staging_plan, ImageLimits, Resident, Staging, StagingSrc,
        STAGING_KEEP, STAGING_QUIET_FRAMES, STAGING_START,
    };

    const MIB: usize = 1024 * 1024;
    /// A 24-megapixel photograph's upload.
    const PHOTO: usize = 96_000_000;

    /// A coverage upload is one byte a texel and ends wherever its dirty region
    /// does. The colour upload after it copies four-byte texels, and a copy must
    /// start on a whole one.
    #[test]
    fn an_upload_after_an_odd_length_one_starts_on_a_whole_texel() {
        let glyphs = vec![0x11u8; 9 * 17];
        let photo = vec![0x2222_2222u32; 6];
        let mut staging = Staging::default();

        assert_eq!(staging.push(StagingSrc::Bytes(&glyphs)), 0);
        assert_eq!(staging.push(StagingSrc::Argb(&photo)), 156);
        assert_eq!(staging.total, 156 + 24);
        // The offsets the bytes are written at are the ones the copies were given.
        let written: Vec<usize> = staging.sources.iter().map(|(at, _)| *at).collect();
        assert_eq!(written, [0, 156]);
    }

    /// The staging buffer is sized before the sources are pushed. A size that
    /// came out smaller than what the pushes then occupy would have the copy
    /// write past the mapping.
    #[test]
    fn the_size_reserved_for_a_frames_uploads_is_what_pushing_them_occupies() {
        let glyphs = vec![0u8; 153];
        let emoji = vec![0u8; 4 * 7];
        let photo = vec![0u32; 6];
        let mut staging = Staging::default();
        staging.push(StagingSrc::Bytes(&glyphs));
        staging.push(StagingSrc::Bytes(&emoji));
        staging.push(StagingSrc::Argb(&photo));

        let reserved = staging_end([glyphs.len(), emoji.len(), photo.len() * 4].into_iter());
        assert_eq!(reserved, staging.total);
        assert_eq!(staging_end(std::iter::empty()), 0);
    }

    #[test]
    fn a_small_staging_buffer_doubles_and_is_never_given_back() {
        // A frame of glyphs outgrows the starting buffer: double, do not creep.
        assert_eq!(
            staging_plan(STAGING_START, STAGING_START + 1, 0),
            (Some(2 * STAGING_START), 0)
        );
        // A need past double is met exactly, and nothing grows beyond the keep
        // size on small needs.
        assert_eq!(staging_plan(STAGING_START, 3 * MIB, 0), (Some(3 * MIB), 0));
        assert_eq!(
            staging_plan(3 * MIB, 3 * MIB + 1, 0),
            (Some(STAGING_KEEP), 0)
        );
        // However long it sits idle, a buffer within the keep size stays.
        assert_eq!(staging_plan(STAGING_KEEP, 0, u32::MAX - 1), (None, 0));
    }

    /// The case this policy exists for: one photograph's upload must not keep its
    /// 96 MB staging buffer allocated in the slot for good.
    #[test]
    fn a_photographs_staging_buffer_is_released_once_frames_stop_needing_it() {
        let (grown, quiet) = staging_plan(STAGING_KEEP, PHOTO, 0);
        let capacity = grown.expect("the photograph needs a larger buffer");
        assert!((PHOTO..PHOTO + STAGING_KEEP).contains(&capacity));
        assert_eq!(quiet, 0);

        // Frames with nothing to upload: the buffer is kept for a while, in case
        // the next photograph is a keypress away.
        let mut quiet = 0;
        for _ in 1..STAGING_QUIET_FRAMES {
            let (resize, next) = staging_plan(capacity, 0, quiet);
            assert_eq!(resize, None, "kept while photographs may still come");
            quiet = next;
        }
        // Then it goes back to the size every slot keeps.
        assert_eq!(staging_plan(capacity, 0, quiet), (Some(STAGING_KEEP), 0));
    }

    #[test]
    fn paging_through_photographs_keeps_the_staging_buffer() {
        let (grown, _) = staging_plan(STAGING_KEEP, PHOTO, 0);
        let capacity = grown.expect("grown for the first photograph");
        // Almost quiet enough to shrink, then another photograph: the count
        // starts over, and the same buffer serves it.
        let quiet = STAGING_QUIET_FRAMES - 1;
        assert_eq!(staging_plan(capacity, PHOTO, quiet), (None, 0));
        // A slightly larger one fits the slack or steps the buffer up once.
        let (resize, _) = staging_plan(capacity, capacity + 1, 0);
        assert_eq!(resize, Some(capacity + STAGING_KEEP));
    }

    #[test]
    fn a_shrunk_staging_buffer_still_holds_the_frame_that_shrinks_it() {
        // The frame that trips the shrink stages a screenful of thumbnails.
        let needed = 6 * MIB;
        let (resize, _) = staging_plan(PHOTO, needed, STAGING_QUIET_FRAMES - 1);
        let capacity = resize.expect("far larger than needed for long enough");
        assert!(capacity >= needed, "{capacity} holds {needed}");
        assert!(capacity <= needed + STAGING_KEEP);
    }

    const LIMITS: ImageLimits = ImageLimits {
        max_count: 8,
        max_bytes: 1000,
    };

    fn resident(bytes: u64, last_drawn: u64, drawn: bool) -> Resident {
        Resident {
            bytes,
            last_drawn,
            drawn,
        }
    }

    #[test]
    fn nothing_is_evicted_while_the_new_textures_fit() {
        let residents = [resident(400, 1, false), resident(400, 2, true)];
        assert!(eviction_plan(&residents, 1, 200, LIMITS).is_empty());
        // Exactly at both limits still fits.
        let full: Vec<Resident> = (0..7).map(|i| resident(100, i, false)).collect();
        assert!(eviction_plan(&full, 1, 300, LIMITS).is_empty());
    }

    /// Paging through large photographs: each is one texture, and the count cap
    /// alone would let dozens of them (gigabytes) pile up.
    #[test]
    fn bytes_over_budget_evict_the_least_recently_drawn_down_to_half() {
        let residents = [
            resident(300, 3, false),
            resident(300, 1, false),
            resident(300, 2, false),
            resident(100, 4, true),
        ];
        // 1000 resident + 300 incoming is over 1000: evict oldest first until
        // the total is at most 500.
        assert_eq!(eviction_plan(&residents, 1, 300, LIMITS), [1, 2, 0]);
        // A smaller overshoot stops as soon as half the budget is reached.
        assert_eq!(eviction_plan(&residents, 1, 1, LIMITS), [1, 2]);
    }

    /// A page-scroll of the thumbnail grid: every new thumbnail needs a
    /// descriptor set from a fixed pool, so room is made before allocating.
    #[test]
    fn a_count_over_the_pool_evicts_down_to_half_of_it() {
        let residents: Vec<Resident> = (0..8).map(|i| resident(1, 10 - i, false)).collect();
        let victims = eviction_plan(&residents, 2, 2, LIMITS);
        // 10 would exceed 8: go down to 4 in all, oldest (highest index) first.
        assert_eq!(victims, [7, 6, 5, 4, 3, 2]);
    }

    /// The frame being prepared binds what it draws; evicting one of those
    /// would have the command buffer name a destroyed texture.
    #[test]
    fn what_the_frame_draws_is_never_evicted_even_over_both_limits() {
        let residents = [
            resident(900, 5, true),
            resident(900, 5, true),
            resident(50, 1, false),
        ];
        assert_eq!(eviction_plan(&residents, 20, 5000, LIMITS), [2]);
        let all_drawn = [resident(900, 5, true), resident(900, 5, true)];
        assert!(eviction_plan(&all_drawn, 20, 5000, LIMITS).is_empty());
    }
}
