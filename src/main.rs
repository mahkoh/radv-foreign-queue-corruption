use ash::Device;
use ash::Entry;
use ash::Instance;
use ash::vk;
use std::fs::File;
use std::io::BufWriter;
use std::ptr;
use std::slice;

const WIDTH: u32 = 1024;
const HEIGHT: u32 = 1024;
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
const LAYOUT: vk::ImageLayout = vk::ImageLayout::ATTACHMENT_FEEDBACK_LOOP_OPTIMAL_EXT;
// const LAYOUT: vk::ImageLayout = vk::ImageLayout::GENERAL;
const AMD_MODIFIER_DCC: u64 = 1 << 13;

struct Pixels {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

fn write_png(path: &str, pixels: &Pixels) {
    let file = File::create(path).expect("could not create the output image");
    let mut encoder = png::Encoder::new(BufWriter::new(file), pixels.width, pixels.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .expect("could not write the output image");
    writer
        .write_image_data(&pixels.data)
        .expect("could not write the output image");
}

struct Context {
    instance: Instance,
    physical_device: vk::PhysicalDevice,
    device: Device,
    queue: vk::Queue,
    family: u32,
    pool: vk::CommandPool,
}

impl Context {
    unsafe fn new(entry: &Entry) -> Self {
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let instance_info = vk::InstanceCreateInfo::default().application_info(&app);
        let instance = unsafe {
            entry
                .create_instance(&instance_info, None)
                .expect("could not create the instance")
        };
        let physical_devices = unsafe {
            instance
                .enumerate_physical_devices()
                .expect("could not enumerate the physical devices")
        };
        let physical_device = *physical_devices
            .first()
            .expect("there is no physical device");
        let families =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        let family = families
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .expect("there is no graphics queue") as u32;
        let priority = 1.0;
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(slice::from_ref(&priority));
        let mut feedback_loop =
            vk::PhysicalDeviceAttachmentFeedbackLoopLayoutFeaturesEXT::default()
                .attachment_feedback_loop_layout(true);
        let mut features13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);
        let extensions = [
            ash::ext::image_drm_format_modifier::NAME.as_ptr(),
            ash::ext::queue_family_foreign::NAME.as_ptr(),
            ash::ext::attachment_feedback_loop_layout::NAME.as_ptr(),
        ];
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(slice::from_ref(&queue_info))
            .enabled_extension_names(&extensions)
            .push_next(&mut features13)
            .push_next(&mut feedback_loop);
        let device = unsafe {
            instance
                .create_device(physical_device, &device_info, None)
                .expect("could not create the device")
        };
        let queue = unsafe { device.get_device_queue(family, 0) };
        let pool_info = vk::CommandPoolCreateInfo::default().queue_family_index(family);
        let pool = unsafe {
            device
                .create_command_pool(&pool_info, None)
                .expect("could not create the command pool")
        };
        Self {
            instance,
            physical_device,
            device,
            queue,
            family,
            pool,
        }
    }

    fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> u32 {
        let properties = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical_device)
        };
        (0..properties.memory_type_count)
            .find(|&i| {
                bits & (1 << i) != 0
                    && properties.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .expect("there is no suitable memory type")
    }

    unsafe fn allocate(
        &self,
        requirements: vk::MemoryRequirements,
        flags: vk::MemoryPropertyFlags,
    ) -> vk::DeviceMemory {
        let info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(self.memory_type(requirements.memory_type_bits, flags));
        unsafe {
            self.device
                .allocate_memory(&info, None)
                .expect("could not allocate memory")
        }
    }

    unsafe fn begin(&self) -> vk::CommandBuffer {
        let info = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            let cmd = self
                .device
                .allocate_command_buffers(&info)
                .expect("could not allocate a command buffer")[0];
            self.device
                .begin_command_buffer(cmd, &begin_info)
                .expect("could not begin the command buffer");
            cmd
        }
    }

    unsafe fn submit_and_wait_idle(&self, cmd: vk::CommandBuffer) {
        unsafe {
            self.device
                .end_command_buffer(cmd)
                .expect("could not end the command buffer");
            let submit = vk::SubmitInfo::default().command_buffers(slice::from_ref(&cmd));
            self.device
                .queue_submit(self.queue, slice::from_ref(&submit), vk::Fence::null())
                .expect("could not submit");
            self.device
                .queue_wait_idle(self.queue)
                .expect("could not wait for the queue");
        }
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

unsafe fn dcc_modifier(ctx: &Context, usage: vk::ImageUsageFlags) -> u64 {
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut properties = vk::FormatProperties2::default().push_next(&mut list);
    let modifiers = unsafe {
        ctx.instance.get_physical_device_format_properties2(
            ctx.physical_device,
            FORMAT,
            &mut properties,
        );
        let mut modifiers = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut modifiers);
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        ctx.instance.get_physical_device_format_properties2(
            ctx.physical_device,
            FORMAT,
            &mut properties,
        );
        modifiers
    };
    modifiers
        .iter()
        .map(|m| m.drm_format_modifier)
        .filter(|&m| m & AMD_MODIFIER_DCC != 0)
        .find(|&modifier| {
            let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(FORMAT)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(usage)
                .push_next(&mut modifier_info);
            let mut properties = vk::ImageFormatProperties2::default();
            unsafe {
                ctx.instance
                    .get_physical_device_image_format_properties2(
                        ctx.physical_device,
                        &format_info,
                        &mut properties,
                    )
                    .is_ok()
            }
        })
        .expect("there is no DCC modifier")
}

unsafe fn run(ctx: &Context) -> Pixels {
    let device = &ctx.device;
    let width = WIDTH;
    let height = HEIGHT;
    let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::SAMPLED
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::TRANSFER_DST
        | vk::ImageUsageFlags::ATTACHMENT_FEEDBACK_LOOP_EXT;
    let modifier = unsafe { dcc_modifier(ctx, usage) };
    let mut modifier_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
        .drm_format_modifiers(slice::from_ref(&modifier));
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(FORMAT)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut modifier_list);
    let image = unsafe {
        device
            .create_image(&image_info, None)
            .expect("could not create the image")
    };
    unsafe {
        let image_memory = ctx.allocate(
            device.get_image_memory_requirements(image),
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        );
        device
            .bind_image_memory(image, image_memory, 0)
            .expect("could not bind the image memory");
    }
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(FORMAT)
        .subresource_range(color_range());
    let view = unsafe {
        device
            .create_image_view(&view_info, None)
            .expect("could not create the image view")
    };

    let size = u64::from(width) * u64::from(height) * 4;
    let buffer_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_DST);
    let buffer = unsafe {
        device
            .create_buffer(&buffer_info, None)
            .expect("could not create the buffer")
    };
    let mapped = unsafe {
        let buffer_memory = ctx.allocate(
            device.get_buffer_memory_requirements(buffer),
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        );
        device
            .bind_buffer_memory(buffer, buffer_memory, 0)
            .expect("could not bind the buffer memory");
        let mapped = device
            .map_memory(buffer_memory, 0, size, vk::MemoryMapFlags::empty())
            .expect("could not map the buffer");
        ptr::slice_from_raw_parts_mut(mapped.cast::<u8>(), size as usize)
    };

    unsafe {
        let cmd = ctx.begin();
        // STEP 1: transition UNDEFINED -> GENERAL
        let to_general = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::CLEAR)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(color_range());
        device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(slice::from_ref(&to_general)),
        );
        // STEP 2: clear to all white
        let white = vk::ClearColorValue {
            float32: [1.0, 1.0, 1.0, 1.0],
        };
        device.cmd_clear_color_image(
            cmd,
            image,
            vk::ImageLayout::GENERAL,
            &white,
            slice::from_ref(&color_range()),
        );
        // STEP 3: transition GENERAL -> LAYOUT + FOREIGN queue
        let release = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::NONE)
            .dst_access_mask(vk::AccessFlags2::NONE)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(LAYOUT)
            .src_queue_family_index(ctx.family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .image(image)
            .subresource_range(color_range());
        device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(slice::from_ref(&release)),
        );
        ctx.submit_and_wait_idle(cmd);
        let cmd = ctx.begin();
        // STEP 4: transition GENERAL + FOREIGN queue -> LAYOUT
        let acquire = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(
                vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            )
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(LAYOUT)
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(ctx.family)
            .image(image)
            .subresource_range(color_range());
        let info = vk::DependencyInfo::default().image_memory_barriers(slice::from_ref(&acquire));
        device.cmd_pipeline_barrier2(cmd, &info);
        // STEP 5: clear to all black
        let black = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        };
        let attachment = vk::RenderingAttachmentInfo::default()
            .image_view(view)
            .image_layout(LAYOUT)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(black);
        let rendering_info = vk::RenderingInfo::default()
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D { width, height },
            })
            .layer_count(1)
            .color_attachments(slice::from_ref(&attachment));
        device.cmd_begin_rendering(cmd, &rendering_info);
        device.cmd_end_rendering(cmd);
        // STEP 6: transition LAYOUT -> TRANSFER_SRC_OPTIMAL
        let to_transfer_src = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .old_layout(LAYOUT)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(color_range());
        let info =
            vk::DependencyInfo::default().image_memory_barriers(slice::from_ref(&to_transfer_src));
        device.cmd_pipeline_barrier2(cmd, &info);
        // STEP 7: read back the image
        let region = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        device.cmd_copy_image_to_buffer(
            cmd,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            buffer,
            slice::from_ref(&region),
        );
        ctx.submit_and_wait_idle(cmd);
    }

    Pixels {
        width,
        height,
        data: unsafe { (*mapped).to_vec() },
    }
}

fn main() {
    let output = unsafe {
        let entry = Entry::load().expect("could not load the Vulkan loader");
        let ctx = Context::new(&entry);
        run(&ctx)
    };
    write_png("output.png", &output);
}
