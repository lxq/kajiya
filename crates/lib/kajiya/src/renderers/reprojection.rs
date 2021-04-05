use kajiya_backend::{
    ash::{version::DeviceV1_0, vk},
    vk_sync::AccessType,
    vulkan::image::*,
};
use kajiya_rg::{self as rg, GetOrCreateTemporal, SimpleRenderPass};

use super::GbufferDepth;

pub fn copy_depth(
    rg: &mut rg::RenderGraph,
    input: &rg::Handle<Image>,
    output: &mut rg::Handle<Image>,
) {
    let mut pass = rg.add_pass("copy depth");
    let input_ref = pass.read(input, AccessType::TransferRead);
    let output_ref = pass.write(output, AccessType::TransferWrite);

    pass.render(move |api| {
        let raw_device = &api.device().raw;
        let cb = api.cb;

        let input = api.resources.image(input_ref);
        let output = api.resources.image(output_ref);

        let input_extent = input_ref.desc().extent;

        unsafe {
            raw_device.cmd_copy_image(
                cb.raw,
                input.raw,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                output.raw,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::ImageCopy::builder()
                    .src_subresource(
                        vk::ImageSubresourceLayers::builder()
                            .aspect_mask(vk::ImageAspectFlags::DEPTH)
                            .layer_count(1)
                            .mip_level(0)
                            .build(),
                    )
                    .dst_subresource(
                        vk::ImageSubresourceLayers::builder()
                            .aspect_mask(vk::ImageAspectFlags::DEPTH)
                            .layer_count(1)
                            .mip_level(0)
                            .build(),
                    )
                    .extent(vk::Extent3D {
                        width: input_extent[0],
                        height: input_extent[1],
                        depth: input_extent[2],
                    })
                    .build()],
            );
        }
    });
}

pub fn calculate_reprojection_map(
    rg: &mut rg::TemporalRenderGraph,
    gbuffer_depth: &GbufferDepth,
    velocity_img: &rg::Handle<Image>,
) -> rg::Handle<Image> {
    //let mut output_tex = rg.create(depth.desc().format(vk::Format::R16G16B16A16_SFLOAT));
    //let mut output_tex = rg.create(depth.desc().format(vk::Format::R32G32B32A32_SFLOAT));
    let mut output_tex = rg.create(
        gbuffer_depth
            .depth
            .desc()
            .format(vk::Format::R16G16B16A16_SNORM),
    );

    let mut prev_depth = rg
        .get_or_create_temporal(
            "reprojection.prev_depth",
            gbuffer_depth
                .depth
                .desc()
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST),
        )
        .unwrap();

    SimpleRenderPass::new_compute(
        rg.add_pass("reprojection map"),
        "/shaders/calculate_reprojection_map.hlsl",
    )
    .read_aspect(&gbuffer_depth.depth, vk::ImageAspectFlags::DEPTH)
    .read(&gbuffer_depth.geometric_normal)
    .read_aspect(&prev_depth, vk::ImageAspectFlags::DEPTH)
    .read(velocity_img)
    .write(&mut output_tex)
    .constants(output_tex.desc().extent_inv_extent_2d())
    .dispatch(output_tex.desc().extent);

    copy_depth(rg, &gbuffer_depth.depth, &mut prev_depth);

    output_tex
}