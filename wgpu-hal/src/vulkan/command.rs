use super::conv;

use ash::{version::DeviceV1_0, vk};
use inplace_it::inplace_or_alloc_from_iter;

use std::ops::Range;

const ALLOCATION_GRANULARITY: u32 = 16;
const DST_IMAGE_LAYOUT: vk::ImageLayout = vk::ImageLayout::TRANSFER_DST_OPTIMAL;

impl super::Texture {
    fn map_buffer_copies<T>(&self, regions: T) -> impl Iterator<Item = vk::BufferImageCopy>
    where
        T: Iterator<Item = crate::BufferTextureCopy>,
    {
        let dim = self.dim;
        let aspects = self.aspects;
        let fi = self.format_info;
        regions.map(move |r| {
            let (layer_count, image_extent) = conv::map_extent(r.size, dim);
            let (image_subresource, image_offset) =
                conv::map_subresource_layers(&r.texture_base, dim, aspects, layer_count);
            vk::BufferImageCopy {
                buffer_offset: r.buffer_layout.offset,
                buffer_row_length: r
                    .buffer_layout
                    .bytes_per_row
                    .map_or(0, |bpr| bpr.get() / fi.block_size as u32),
                buffer_image_height: r
                    .buffer_layout
                    .rows_per_image
                    .map_or(0, |rpi| rpi.get() * fi.block_dimensions.1 as u32),
                image_subresource,
                image_offset,
                image_extent,
            }
        })
    }
}

impl crate::CommandEncoder<super::Api> for super::CommandEncoder {
    unsafe fn begin_encoding(&mut self, label: crate::Label) -> Result<(), crate::DeviceError> {
        if self.free.is_empty() {
            let vk_info = vk::CommandBufferAllocateInfo::builder()
                .command_buffer_count(ALLOCATION_GRANULARITY)
                .build();
            let cmd_buf_vec = self.device.raw.allocate_command_buffers(&vk_info)?;
            self.free.extend(cmd_buf_vec);
        }
        let raw = self.free.pop().unwrap();

        let vk_info = vk::CommandBufferBeginInfo::builder()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)
            .build();
        self.device.raw.begin_command_buffer(raw, &vk_info)?;
        self.active = raw;

        Ok(())
    }

    unsafe fn end_encoding(&mut self) -> Result<super::CommandBuffer, crate::DeviceError> {
        let raw = self.active;
        self.active = vk::CommandBuffer::null();
        self.device.raw.end_command_buffer(raw)?;
        Ok(super::CommandBuffer { raw })
    }

    unsafe fn discard_encoding(&mut self) {
        self.discarded.push(self.active);
        self.active = vk::CommandBuffer::null();
    }

    unsafe fn reset_all<I>(&mut self, cmd_bufs: I)
    where
        I: Iterator<Item = super::CommandBuffer>,
    {
        self.free
            .extend(cmd_bufs.into_iter().map(|cmd_buf| cmd_buf.raw));
        self.free.extend(self.discarded.drain(..));
        let _ = self
            .device
            .raw
            .reset_command_pool(self.raw, vk::CommandPoolResetFlags::RELEASE_RESOURCES);
    }

    unsafe fn transition_buffers<'a, T>(&mut self, barriers: T)
    where
        T: Iterator<Item = crate::BufferBarrier<'a, super::Api>>,
    {
        let mut src_stages = vk::PipelineStageFlags::empty();
        let mut dst_stages = vk::PipelineStageFlags::empty();
        let vk_barrier_iter = barriers.map(move |bar| {
            let (src_stage, src_access) = conv::map_buffer_usage_to_barrier(bar.usage.start);
            src_stages |= src_stage;
            let (dst_stage, dst_access) = conv::map_buffer_usage_to_barrier(bar.usage.end);
            dst_stages |= dst_stage;

            vk::BufferMemoryBarrier::builder()
                .buffer(bar.buffer.raw)
                .size(vk::WHOLE_SIZE)
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .build()
        });

        inplace_or_alloc_from_iter(vk_barrier_iter, |vk_barriers| {
            self.device.raw.cmd_pipeline_barrier(
                self.active,
                src_stages,
                dst_stages,
                vk::DependencyFlags::empty(),
                &[],
                vk_barriers,
                &[],
            );
        });
    }

    unsafe fn transition_textures<'a, T>(&mut self, barriers: T)
    where
        T: Iterator<Item = crate::TextureBarrier<'a, super::Api>>,
    {
        let mut src_stages = vk::PipelineStageFlags::empty();
        let mut dst_stages = vk::PipelineStageFlags::empty();
        let vk_barrier_iter = barriers.map(move |bar| {
            let range = conv::map_subresource_range(&bar.range, bar.texture.aspects);
            let (src_stage, src_access) = conv::map_texture_usage_to_barrier(bar.usage.start);
            let src_layout = conv::derive_image_layout(bar.usage.start);
            src_stages |= src_stage;
            let (dst_stage, dst_access) = conv::map_texture_usage_to_barrier(bar.usage.end);
            let dst_layout = conv::derive_image_layout(bar.usage.end);
            dst_stages |= dst_stage;

            vk::ImageMemoryBarrier::builder()
                .image(bar.texture.raw)
                .subresource_range(range)
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .old_layout(src_layout)
                .new_layout(dst_layout)
                .build()
        });

        inplace_or_alloc_from_iter(vk_barrier_iter, |vk_barriers| {
            self.device.raw.cmd_pipeline_barrier(
                self.active,
                src_stages,
                dst_stages,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                vk_barriers,
            );
        });
    }

    unsafe fn fill_buffer(&mut self, buffer: &super::Buffer, range: crate::MemoryRange, value: u8) {
        self.device.raw.cmd_fill_buffer(
            self.active,
            buffer.raw,
            range.start,
            range.end - range.start,
            (value as u32) * 0x01010101,
        );
    }

    unsafe fn copy_buffer_to_buffer<T>(
        &mut self,
        src: &super::Buffer,
        dst: &super::Buffer,
        regions: T,
    ) where
        T: Iterator<Item = crate::BufferCopy>,
    {
        let vk_regions_iter = regions.map(|r| vk::BufferCopy {
            src_offset: r.src_offset,
            dst_offset: r.dst_offset,
            size: r.size.get(),
        });

        inplace_or_alloc_from_iter(vk_regions_iter, |vk_regions| {
            self.device
                .raw
                .cmd_copy_buffer(self.active, src.raw, dst.raw, vk_regions)
        })
    }

    unsafe fn copy_texture_to_texture<T>(
        &mut self,
        src: &super::Texture,
        src_usage: crate::TextureUse,
        dst: &super::Texture,
        regions: T,
    ) where
        T: Iterator<Item = crate::TextureCopy>,
    {
        let src_layout = conv::derive_image_layout(src_usage);

        let vk_regions_iter = regions.map(|r| {
            let (layer_count, extent) = conv::map_extent(r.size, src.dim);
            let (src_subresource, src_offset) =
                conv::map_subresource_layers(&r.src_base, src.dim, src.aspects, layer_count);
            let (dst_subresource, dst_offset) =
                conv::map_subresource_layers(&r.dst_base, dst.dim, dst.aspects, layer_count);
            vk::ImageCopy {
                src_subresource,
                src_offset,
                dst_subresource,
                dst_offset,
                extent,
            }
        });

        inplace_or_alloc_from_iter(vk_regions_iter, |vk_regions| {
            self.device.raw.cmd_copy_image(
                self.active,
                src.raw,
                src_layout,
                dst.raw,
                DST_IMAGE_LAYOUT,
                vk_regions,
            );
        });
    }

    unsafe fn copy_buffer_to_texture<T>(
        &mut self,
        src: &super::Buffer,
        dst: &super::Texture,
        regions: T,
    ) where
        T: Iterator<Item = crate::BufferTextureCopy>,
    {
        let vk_regions_iter = dst.map_buffer_copies(regions);

        inplace_or_alloc_from_iter(vk_regions_iter, |vk_regions| {
            self.device.raw.cmd_copy_buffer_to_image(
                self.active,
                src.raw,
                dst.raw,
                DST_IMAGE_LAYOUT,
                vk_regions,
            );
        });
    }

    unsafe fn copy_texture_to_buffer<T>(
        &mut self,
        src: &super::Texture,
        src_usage: crate::TextureUse,
        dst: &super::Buffer,
        regions: T,
    ) where
        T: Iterator<Item = crate::BufferTextureCopy>,
    {
        let src_layout = conv::derive_image_layout(src_usage);
        let vk_regions_iter = src.map_buffer_copies(regions);

        inplace_or_alloc_from_iter(vk_regions_iter, |vk_regions| {
            self.device.raw.cmd_copy_image_to_buffer(
                self.active,
                src.raw,
                src_layout,
                dst.raw,
                vk_regions,
            );
        });
    }

    unsafe fn begin_query(&mut self, set: &super::QuerySet, index: u32) {}
    unsafe fn end_query(&mut self, set: &super::QuerySet, index: u32) {}
    unsafe fn write_timestamp(&mut self, set: &super::QuerySet, index: u32) {}
    unsafe fn reset_queries(&mut self, set: &super::QuerySet, range: Range<u32>) {}
    unsafe fn copy_query_results(
        &mut self,
        set: &super::QuerySet,
        range: Range<u32>,
        buffer: &super::Buffer,
        offset: wgt::BufferAddress,
    ) {
    }

    // render

    unsafe fn begin_render_pass(&mut self, desc: &crate::RenderPassDescriptor<super::Api>) {}
    unsafe fn end_render_pass(&mut self) {}

    unsafe fn set_bind_group(
        &mut self,
        layout: &super::PipelineLayout,
        index: u32,
        group: &super::BindGroup,
        dynamic_offsets: &[wgt::DynamicOffset],
    ) {
    }
    unsafe fn set_push_constants(
        &mut self,
        layout: &super::PipelineLayout,
        stages: wgt::ShaderStage,
        offset: u32,
        data: &[u32],
    ) {
    }

    unsafe fn insert_debug_marker(&mut self, label: &str) {}
    unsafe fn begin_debug_marker(&mut self, group_label: &str) {}
    unsafe fn end_debug_marker(&mut self) {}

    unsafe fn set_render_pipeline(&mut self, pipeline: &super::RenderPipeline) {
        self.device.raw.cmd_bind_pipeline(
            self.active,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline.raw,
        );
    }

    unsafe fn set_index_buffer<'a>(
        &mut self,
        binding: crate::BufferBinding<'a, super::Api>,
        format: wgt::IndexFormat,
    ) {
    }
    unsafe fn set_vertex_buffer<'a>(
        &mut self,
        index: u32,
        binding: crate::BufferBinding<'a, super::Api>,
    ) {
    }
    unsafe fn set_viewport(&mut self, rect: &crate::Rect<f32>, depth_range: Range<f32>) {}
    unsafe fn set_scissor_rect(&mut self, rect: &crate::Rect<u32>) {}
    unsafe fn set_stencil_reference(&mut self, value: u32) {}
    unsafe fn set_blend_constants(&mut self, color: &wgt::Color) {}

    unsafe fn draw(
        &mut self,
        start_vertex: u32,
        vertex_count: u32,
        start_instance: u32,
        instance_count: u32,
    ) {
    }
    unsafe fn draw_indexed(
        &mut self,
        start_index: u32,
        index_count: u32,
        base_vertex: i32,
        start_instance: u32,
        instance_count: u32,
    ) {
    }
    unsafe fn draw_indirect(
        &mut self,
        buffer: &super::Buffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    ) {
    }
    unsafe fn draw_indexed_indirect(
        &mut self,
        buffer: &super::Buffer,
        offset: wgt::BufferAddress,
        draw_count: u32,
    ) {
    }
    unsafe fn draw_indirect_count(
        &mut self,
        buffer: &super::Buffer,
        offset: wgt::BufferAddress,
        count_buffer: &super::Buffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    ) {
    }
    unsafe fn draw_indexed_indirect_count(
        &mut self,
        buffer: &super::Buffer,
        offset: wgt::BufferAddress,
        count_buffer: &super::Buffer,
        count_offset: wgt::BufferAddress,
        max_count: u32,
    ) {
    }

    // compute

    unsafe fn begin_compute_pass(&mut self, desc: &crate::ComputePassDescriptor) {}
    unsafe fn end_compute_pass(&mut self) {}

    unsafe fn set_compute_pipeline(&mut self, pipeline: &super::ComputePipeline) {
        self.device.raw.cmd_bind_pipeline(
            self.active,
            vk::PipelineBindPoint::COMPUTE,
            pipeline.raw,
        );
    }

    unsafe fn dispatch(&mut self, count: [u32; 3]) {}
    unsafe fn dispatch_indirect(&mut self, buffer: &super::Buffer, offset: wgt::BufferAddress) {}
}

#[test]
fn check_dst_image_layout() {
    assert_eq!(
        conv::derive_image_layout(crate::TextureUse::COPY_DST),
        DST_IMAGE_LAYOUT
    );
}
