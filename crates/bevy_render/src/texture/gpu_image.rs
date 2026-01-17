use crate::{
    render_asset::{PrepareAssetError, RenderAsset, RenderAssetUsages},
    render_resource::{DefaultImageSampler, Sampler, Texture, TextureView},
    renderer::{RenderDevice, RenderQueue},
};
use bevy_asset::{AssetId, RenderAssetTransferPriority};
use bevy_ecs::system::{lifetimeless::SRes, SystemParamItem};
use bevy_image::{Image, ImageSampler, TextureFormatPixelInfo};
use bevy_math::{AspectRatio, UVec2};
use tracing::{error, warn};
use wgpu::{Extent3d, TexelCopyBufferLayout, TextureFormat};
use wgpu_types::{TextureDescriptor, TextureViewDescriptor};

/// The GPU-representation of an [`Image`].
/// Consists of the [`Texture`], its [`TextureView`] and the corresponding [`Sampler`], and the texture's size.
#[derive(Debug, Clone)]
pub struct GpuImage {
    pub texture: Texture,
    pub texture_view: TextureView,
    pub texture_format: TextureFormat,
    pub sampler: Sampler,
    pub size: Extent3d,
    pub mip_level_count: u32,
    pub had_data: bool,
    pub texture_descriptor: TextureDescriptor<Option<&'static str>, &'static [TextureFormat]>,
    pub texture_view_descriptor: Option<TextureViewDescriptor<Option<&'static str>>>,
}

impl RenderAsset for GpuImage {
    type SourceAsset = Image;
    type Param = (
        SRes<RenderDevice>,
        SRes<RenderQueue>,
        SRes<DefaultImageSampler>,
    );

    #[inline]
    fn asset_usage(image: &Self::SourceAsset) -> RenderAssetUsages {
        image.asset_usage
    }

    #[inline]
    fn transfer_priority(
        image: &Self::SourceAsset,
    ) -> (RenderAssetTransferPriority, Option<usize>) {
        (image.transfer_priority, image.data.as_ref().map(Vec::len))
    }

    fn take_gpu_copy(
        source: &mut Self::SourceAsset,
        previous_gpu_asset: Option<&Self>,
    ) -> Option<Self::SourceAsset> {
        let data = source.data.take();

        let upload = data.is_some() || previous_gpu_asset.is_none_or(|prev| !prev.had_data);

        upload.then(|| Image {
            data,
            ..source.clone()
        })
    }

    /// Converts the extracted image into a [`GpuImage`].
    fn prepare_asset(
        image: Self::SourceAsset,
        _id: AssetId<Self::SourceAsset>,
        (render_device, render_queue, default_sampler): &mut SystemParamItem<Self::Param>,
        previous_gpu_asset: Option<&Self>,
    ) -> Result<Self, PrepareAssetError<Self::SourceAsset>> {
        let had_data = image.data.is_some();

        let texture = if let Some(prev) = previous_gpu_asset
            && prev.texture_descriptor == image.texture_descriptor
            && let Some(block_bytes) = image.texture_descriptor.format.block_copy_size(None)
        {
            if let Some(ref data) = image.data {
                let pixels_per_block = image.texture_descriptor.format.block_dimensions();
                if pixels_per_block.0 > 1 {
                    error!("using block size {pixels_per_block:?}");
                }

                // queue copy
                render_queue.write_texture(
                    prev.texture.as_image_copy(),
                    data,
                    TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(image.width() / pixels_per_block.0 * block_bytes),
                        rows_per_image: Some(image.height() / pixels_per_block.1),
                    },
                    image.texture_descriptor.size,
                );
            }
            // TODO else could clear here? probably not necessary as textures without data are only useful as render
            // targets and will normally be overwritten immediately anyway

            // reuse previous texture
            prev.texture.clone()
        } else {
            if let Some(ref data) = image.data {
                render_device.create_texture_with_data(
                    render_queue,
                    &image.texture_descriptor,
                    // TODO: Is this correct? Do we need to use `MipMajor` if it's a ktx2 file?
                    wgpu::util::TextureDataOrder::default(),
                    data,
                )
            } else {
                render_device.create_texture(&image.texture_descriptor)
            }
        };

        let texture_view = if let Some(prev) = previous_gpu_asset.as_ref()
            && prev.texture_descriptor == image.texture_descriptor
            && prev.texture_view_descriptor == prev.texture_view_descriptor
        {
            prev.texture_view.clone()
        } else {
            image
                .texture_view_descriptor
                .as_ref()
                .map(|desc| texture.create_view(desc))
                .unwrap_or_else(|| texture.create_view(&TextureViewDescriptor::default()))
        };
        let sampler = match image.sampler {
            ImageSampler::Default => (***default_sampler).clone(),
            ImageSampler::Descriptor(descriptor) => {
                render_device.create_sampler(&descriptor.as_wgpu())
            }
        };

        Ok(GpuImage {
            texture,
            texture_view,
            texture_format: image.texture_descriptor.format,
            sampler,
            size: image.texture_descriptor.size,
            mip_level_count: image.texture_descriptor.mip_level_count,
            had_data,
            texture_descriptor: image.texture_descriptor,
            texture_view_descriptor: image.texture_view_descriptor,
        })
    }
}

impl GpuImage {
    /// Returns the aspect ratio (width / height) of a 2D image.
    #[inline]
    pub fn aspect_ratio(&self) -> AspectRatio {
        AspectRatio::try_from_pixels(
            self.texture_descriptor.size.width,
            self.texture_descriptor.size.height,
        )
        .expect(
            "Failed to calculate aspect ratio: Image dimensions must be positive, non-zero values",
        )
    }

    /// Returns the size of a 2D image.
    #[inline]
    pub fn size_2d(&self) -> UVec2 {
        UVec2::new(
            self.texture_descriptor.size.width,
            self.texture_descriptor.size.height,
        )
    }
}
