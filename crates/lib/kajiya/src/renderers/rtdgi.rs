use std::sync::Arc;

use kajiya_backend::{
    ash::vk,
    vk_sync,
    vulkan::{buffer::*, image::*, ray_tracing::RayTracingAcceleration},
    Device,
};
use kajiya_rg::{self as rg, SimpleRenderPass};

use super::{
    surfel_gi::SurfelGiRenderState, wrc::WrcRenderState, GbufferDepth, PingPongTemporalResource,
};

use blue_noise_sampler::spp64::*;

pub struct RtdgiRenderer {
    temporal_irradiance_tex: PingPongTemporalResource,
    temporal_ray_tex: PingPongTemporalResource,
    temporal_reservoir_tex: PingPongTemporalResource,
    temporal_candidate_tex: PingPongTemporalResource,

    temporal_tex: PingPongTemporalResource,
    temporal2_tex: PingPongTemporalResource,
    temporal2_variance_tex: PingPongTemporalResource,
    temporal_hit_normal_tex: PingPongTemporalResource,

    ranking_tile_buf: Arc<Buffer>,
    scambling_tile_buf: Arc<Buffer>,
    sobol_buf: Arc<Buffer>,

    pub spatial_reuse_pass_count: u32,
}

fn as_byte_slice_unchecked<T: Copy>(v: &[T]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * std::mem::size_of::<T>())
    }
}

fn make_lut_buffer<T: Copy>(device: &Device, v: &[T]) -> Arc<Buffer> {
    Arc::new(
        device
            .create_buffer(
                BufferDesc::new(
                    v.len() * std::mem::size_of::<T>(),
                    vk::BufferUsageFlags::STORAGE_BUFFER,
                ),
                Some(as_byte_slice_unchecked(v)),
            )
            .unwrap(),
    )
}

impl RtdgiRenderer {
    pub fn new(device: &Device) -> Self {
        Self {
            temporal_irradiance_tex: PingPongTemporalResource::new("rtdgi.irradiance"),
            temporal_ray_tex: PingPongTemporalResource::new("rtdgi.ray"),
            temporal_reservoir_tex: PingPongTemporalResource::new("rtdgi.reservoir"),
            temporal_candidate_tex: PingPongTemporalResource::new("rtdgi.candidate"),
            temporal_tex: PingPongTemporalResource::new("rtdgi.temporal"),
            temporal2_tex: PingPongTemporalResource::new("rtdgi.temporal2"),
            temporal2_variance_tex: PingPongTemporalResource::new("rtdgi.temporal2_var"),
            temporal_hit_normal_tex: PingPongTemporalResource::new("rtdgi.hit_normal"),
            ranking_tile_buf: make_lut_buffer(device, RANKING_TILE),
            scambling_tile_buf: make_lut_buffer(device, SCRAMBLING_TILE),
            sobol_buf: make_lut_buffer(device, SOBOL),
            spatial_reuse_pass_count: 2,
        }
    }
}

impl RtdgiRenderer {
    fn temporal_tex_desc(extent: [u32; 2]) -> ImageDesc {
        ImageDesc::new_2d(vk::Format::R32G32B32A32_SFLOAT, extent)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::STORAGE)
    }

    fn temporal(
        &mut self,
        rg: &mut rg::TemporalRenderGraph,
        input_color: &rg::Handle<Image>,
        gbuffer_depth: &GbufferDepth,
        reprojection_map: &rg::Handle<Image>,
        sky_cube: &rg::Handle<Image>,
    ) -> rg::Handle<Image> {
        let half_view_normal_tex = gbuffer_depth.half_view_normal(rg);
        let half_depth_tex = gbuffer_depth.half_depth(rg);
        let half_res_extent = half_view_normal_tex.desc().extent_2d();

        let (mut temporal_output_tex, history_tex) = self
            .temporal_tex
            .get_output_and_history(rg, Self::temporal_tex_desc(half_res_extent));

        let mut temporal_filtered_tex = rg.create(
            gbuffer_depth
                .gbuffer
                .desc()
                .half_res()
                .usage(vk::ImageUsageFlags::empty())
                .format(vk::Format::R16G16B16A16_SFLOAT),
        );

        SimpleRenderPass::new_compute(
            rg.add_pass("rtdgi temporal"),
            "/shaders/rtdgi/temporal_filter.hlsl",
        )
        .read(input_color)
        .read(&history_tex)
        .read(reprojection_map)
        .read(&*half_view_normal_tex)
        .read(&*half_depth_tex)
        .read(sky_cube)
        .write(&mut temporal_output_tex)
        .write(&mut temporal_filtered_tex)
        .constants((
            temporal_output_tex.desc().extent_inv_extent_2d(),
            gbuffer_depth.gbuffer.desc().extent_inv_extent_2d(),
        ))
        .dispatch(temporal_output_tex.desc().extent);

        temporal_filtered_tex
    }

    fn temporal2(
        &mut self,
        rg: &mut rg::TemporalRenderGraph,
        input_color: &rg::Handle<Image>,
        gbuffer_depth: &GbufferDepth,
        reprojection_map: &rg::Handle<Image>,
        reprojected_history_tex: &rg::Handle<Image>,
        mut temporal_output_tex: rg::Handle<Image>,
    ) -> rg::Handle<Image> {
        let (mut temporal_variance_output_tex, variance_history_tex) =
            self.temporal2_variance_tex.get_output_and_history(
                rg,
                ImageDesc::new_2d(vk::Format::R16G16_SFLOAT, input_color.desc().extent_2d())
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::STORAGE),
            );

        let mut temporal_filtered_tex = rg.create(
            gbuffer_depth
                .gbuffer
                .desc()
                .usage(vk::ImageUsageFlags::empty())
                .format(vk::Format::R16G16B16A16_SFLOAT),
        );

        SimpleRenderPass::new_compute(
            rg.add_pass("rtdgi temporal2"),
            "/shaders/rtdgi/temporal_filter2.hlsl",
        )
        .read(input_color)
        .read(reprojected_history_tex)
        .read(&variance_history_tex)
        .read(reprojection_map)
        .write(&mut temporal_filtered_tex)
        .write(&mut temporal_output_tex)
        .write(&mut temporal_variance_output_tex)
        .constants((
            temporal_output_tex.desc().extent_inv_extent_2d(),
            gbuffer_depth.gbuffer.desc().extent_inv_extent_2d(),
        ))
        .dispatch(temporal_output_tex.desc().extent);

        temporal_filtered_tex
    }

    fn spatial(
        rg: &mut rg::TemporalRenderGraph,
        input_color: &rg::Handle<Image>,
        gbuffer_depth: &GbufferDepth,
        ssao_img: &rg::Handle<Image>,
    ) -> rg::Handle<Image> {
        let half_view_normal_tex = gbuffer_depth.half_view_normal(rg);
        let half_depth_tex = gbuffer_depth.half_depth(rg);

        let mut spatial_filtered_tex = rg.create(Self::temporal_tex_desc(
            half_view_normal_tex.desc().extent_2d(),
        ));

        SimpleRenderPass::new_compute(
            rg.add_pass("rtdgi spatial"),
            "/shaders/rtdgi/spatial_filter.hlsl",
        )
        .read(input_color)
        .read(&*half_view_normal_tex)
        .read(&*half_depth_tex)
        .read(ssao_img)
        .write(&mut spatial_filtered_tex)
        .constants((
            spatial_filtered_tex.desc().extent_inv_extent_2d(),
            super::rtr::SPATIAL_RESOLVE_OFFSETS,
        ))
        .dispatch(spatial_filtered_tex.desc().extent);

        spatial_filtered_tex
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        rg: &mut rg::TemporalRenderGraph,
        gbuffer_depth: &GbufferDepth,
        reprojection_map: &rg::Handle<Image>,
        sky_cube: &rg::Handle<Image>,
        bindless_descriptor_set: vk::DescriptorSet,
        surfel_gi: &SurfelGiRenderState,
        wrc: &WrcRenderState,
        tlas: &rg::Handle<RayTracingAcceleration>,
        ssao_img: &rg::Handle<Image>,
        ussao_img: &rg::Handle<Image>,
    ) -> rg::ReadOnlyHandle<Image> {
        let gbuffer_desc = gbuffer_depth.gbuffer.desc();

        let (temporal_output_tex, history_tex) = self
            .temporal2_tex
            .get_output_and_history(rg, Self::temporal_tex_desc(gbuffer_desc.extent_2d()));

        let mut reprojected_history_tex =
            rg.create(Self::temporal_tex_desc(gbuffer_desc.extent_2d()));

        SimpleRenderPass::new_compute(
            rg.add_pass("rtdgi reproject"),
            "/shaders/rtdgi/fullres_reproject.hlsl",
        )
        .read(&history_tex)
        .read(reprojection_map)
        .write(&mut reprojected_history_tex)
        .constants((reprojected_history_tex.desc().extent_inv_extent_2d(),))
        .dispatch(reprojected_history_tex.desc().extent);

        let (mut hit_normal_output_tex, hit_normal_history_tex) =
            self.temporal_hit_normal_tex.get_output_and_history(
                rg,
                Self::temporal_tex_desc(
                    gbuffer_desc
                        .format(vk::Format::R32G32B32A32_SFLOAT)
                        .half_res()
                        .extent_2d(),
                ),
            );

        let ranking_tile_buf = rg.import(
            self.ranking_tile_buf.clone(),
            vk_sync::AccessType::ComputeShaderReadSampledImageOrUniformTexelBuffer,
        );
        let scambling_tile_buf = rg.import(
            self.scambling_tile_buf.clone(),
            vk_sync::AccessType::ComputeShaderReadSampledImageOrUniformTexelBuffer,
        );
        let sobol_buf = rg.import(
            self.sobol_buf.clone(),
            vk_sync::AccessType::ComputeShaderReadSampledImageOrUniformTexelBuffer,
        );

        let (mut candidate_output_tex, candidate_history_tex) =
            self.temporal_candidate_tex.get_output_and_history(
                rg,
                ImageDesc::new_2d(
                    vk::Format::R32G32B32A32_SFLOAT,
                    gbuffer_desc.half_res().extent_2d(),
                )
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::STORAGE),
            );

        let (irradiance_tex, ray_tex, mut temporal_reservoir_tex) = {
            let (mut irradiance_output_tex, irradiance_history_tex) =
                self.temporal_irradiance_tex.get_output_and_history(
                    rg,
                    Self::temporal_tex_desc(gbuffer_desc.half_res().extent_2d()),
                );

            let (mut ray_output_tex, ray_history_tex) =
                self.temporal_ray_tex.get_output_and_history(
                    rg,
                    Self::temporal_tex_desc(gbuffer_desc.half_res().extent_2d()),
                );

            let (mut reservoir_output_tex, reservoir_history_tex) =
                self.temporal_reservoir_tex.get_output_and_history(
                    rg,
                    ImageDesc::new_2d(
                        vk::Format::R32G32B32A32_SFLOAT,
                        gbuffer_desc.half_res().extent_2d(),
                    )
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::STORAGE),
                );

            let half_view_normal_tex = gbuffer_depth.half_view_normal(rg);

            let mut candidate_irradiance_tex = rg.create(
                gbuffer_desc
                    .half_res()
                    .format(vk::Format::R16G16B16A16_SFLOAT),
            );

            let mut candidate_hit_tex = rg.create(
                gbuffer_desc
                    .half_res()
                    .format(vk::Format::R16G16B16A16_SFLOAT),
            );

            SimpleRenderPass::new_rt(
                rg.add_pass("rtdgi trace"),
                "/shaders/rtdgi/trace_diffuse.rgen.hlsl",
                &[
                    "/shaders/rt/gbuffer.rmiss.hlsl",
                    "/shaders/rt/shadow.rmiss.hlsl",
                ],
                &["/shaders/rt/gbuffer.rchit.hlsl"],
            )
            .read(&*half_view_normal_tex)
            .read_aspect(&gbuffer_depth.depth, vk::ImageAspectFlags::DEPTH)
            .read(&reprojected_history_tex)
            .read(ssao_img)
            .read(&ranking_tile_buf)
            .read(&scambling_tile_buf)
            .read(&sobol_buf)
            .read(reprojection_map)
            .bind(surfel_gi)
            .bind(wrc)
            .read(sky_cube)
            .write(&mut candidate_irradiance_tex)
            .write(&mut candidate_hit_tex)
            .constants((gbuffer_desc.extent_inv_extent_2d(),))
            .raw_descriptor_set(1, bindless_descriptor_set)
            .trace_rays(tlas, candidate_irradiance_tex.desc().extent);

            SimpleRenderPass::new_compute(
                rg.add_pass("restir temporal"),
                "/shaders/rtdgi/restir_temporal.hlsl",
            )
            .read(&*half_view_normal_tex)
            .read_aspect(&gbuffer_depth.depth, vk::ImageAspectFlags::DEPTH)
            .read(&candidate_irradiance_tex)
            .read(&candidate_hit_tex)
            .read(&ranking_tile_buf)
            .read(&scambling_tile_buf)
            .read(&sobol_buf)
            .read(&irradiance_history_tex)
            .read(&ray_history_tex)
            .read(&reservoir_history_tex)
            .read(reprojection_map)
            .read(&hit_normal_history_tex)
            .read(&candidate_history_tex)
            .write(&mut irradiance_output_tex)
            .write(&mut ray_output_tex)
            .write(&mut hit_normal_output_tex)
            .write(&mut reservoir_output_tex)
            .write(&mut candidate_output_tex)
            .constants((gbuffer_desc.extent_inv_extent_2d(),))
            .raw_descriptor_set(1, bindless_descriptor_set)
            .dispatch(irradiance_output_tex.desc().extent);

            (irradiance_output_tex, ray_output_tex, reservoir_output_tex)
        };

        let irradiance_tex = {
            let half_view_normal_tex = gbuffer_depth.half_view_normal(rg);
            let half_depth_tex = gbuffer_depth.half_depth(rg);

            let mut irradiance_output_tex = rg.create(
                gbuffer_desc
                    .usage(vk::ImageUsageFlags::empty())
                    .half_res()
                    .format(vk::Format::R32G32B32A32_SFLOAT),
            );

            let mut reservoir_output_tex0 = rg.create(
                gbuffer_desc
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::STORAGE)
                    .half_res()
                    .format(vk::Format::R32G32B32A32_SFLOAT),
            );
            let mut reservoir_output_tex1 = rg.create(
                gbuffer_desc
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::STORAGE)
                    .half_res()
                    .format(vk::Format::R32G32B32A32_SFLOAT),
            );

            let mut reservoir_input_tex = &mut temporal_reservoir_tex;

            for spatial_reuse_pass_idx in 0..self.spatial_reuse_pass_count {
                SimpleRenderPass::new_compute(
                    rg.add_pass("restir spatial"),
                    "/shaders/rtdgi/restir_spatial.hlsl",
                )
                .read(&irradiance_tex)
                .read(&hit_normal_output_tex)
                .read(&ray_tex)
                .read(reservoir_input_tex)
                .read(&gbuffer_depth.gbuffer)
                .read(&*half_view_normal_tex)
                .read(&*half_depth_tex)
                .read(ssao_img)
                .read(&candidate_output_tex)
                .write(&mut reservoir_output_tex0)
                .constants((
                    gbuffer_desc.extent_inv_extent_2d(),
                    reservoir_output_tex0.desc().extent_inv_extent_2d(),
                    spatial_reuse_pass_idx as u32,
                ))
                .dispatch(reservoir_output_tex0.desc().extent);

                std::mem::swap(&mut reservoir_output_tex0, &mut reservoir_output_tex1);
                reservoir_input_tex = &mut reservoir_output_tex1;
            }

            SimpleRenderPass::new_compute(
                rg.add_pass("restir resolve"),
                "/shaders/rtdgi/restir_resolve.hlsl",
            )
            .read(&irradiance_tex)
            .read(&hit_normal_output_tex)
            .read(&ray_tex)
            .read(reservoir_input_tex)
            .read(&gbuffer_depth.gbuffer)
            .read(&*half_view_normal_tex)
            .read(&*half_depth_tex)
            .read(ssao_img)
            .read(ussao_img)
            .write(&mut irradiance_output_tex)
            .constants((
                gbuffer_desc.extent_inv_extent_2d(),
                irradiance_output_tex.desc().extent_inv_extent_2d(),
            ))
            .dispatch(irradiance_output_tex.desc().extent);

            irradiance_output_tex
        };

        let filtered_tex = self.temporal(
            rg,
            &irradiance_tex,
            gbuffer_depth,
            reprojection_map,
            sky_cube,
        );
        let filtered_tex = Self::spatial(rg, &filtered_tex, gbuffer_depth, ssao_img);

        let half_view_normal_tex = gbuffer_depth.half_view_normal(rg);
        let half_depth_tex = gbuffer_depth.half_depth(rg);

        let mut upsampled_tex = rg.create(gbuffer_desc.format(vk::Format::R16G16B16A16_SFLOAT));
        SimpleRenderPass::new_compute(
            rg.add_pass("rtdgi upsample"),
            "/shaders/rtdgi/upsample.hlsl",
        )
        .read(&filtered_tex)
        .read_aspect(&gbuffer_depth.depth, vk::ImageAspectFlags::DEPTH)
        .read(&gbuffer_depth.gbuffer)
        .read(&*half_view_normal_tex)
        .read(&*half_depth_tex)
        .read(ssao_img)
        .write(&mut upsampled_tex)
        .constants((
            upsampled_tex.desc().extent_inv_extent_2d(),
            super::rtr::SPATIAL_RESOLVE_OFFSETS,
        ))
        .raw_descriptor_set(1, bindless_descriptor_set)
        .dispatch(upsampled_tex.desc().extent);

        let filtered_tex = self.temporal2(
            rg,
            &upsampled_tex,
            gbuffer_depth,
            reprojection_map,
            &reprojected_history_tex,
            temporal_output_tex,
        );

        filtered_tex.into()
    }
}
