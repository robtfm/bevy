use crate::image::{Image, ImageFormat, ImageType, TextureError};
use bevy_asset::{
    io::Reader, AssetLoader, LoadContext, RenderAssetTransferPriority, RenderAssetUsages,
};
use thiserror::Error;

use super::{CompressedImageFormats, ImageSampler};
use serde::{Deserialize, Serialize};

/// Loader for images that can be read by the `image` crate.
#[derive(Clone)]
pub struct ImageLoader {
    supported_compressed_formats: CompressedImageFormats,
}

impl ImageLoader {
    /// Full list of supported formats.
    pub const SUPPORTED_FORMATS: &'static [ImageFormat] = &[
        #[cfg(feature = "basis-universal")]
        ImageFormat::Basis,
        #[cfg(feature = "bmp")]
        ImageFormat::Bmp,
        #[cfg(feature = "dds")]
        ImageFormat::Dds,
        #[cfg(feature = "ff")]
        ImageFormat::Farbfeld,
        #[cfg(feature = "gif")]
        ImageFormat::Gif,
        #[cfg(feature = "ico")]
        ImageFormat::Ico,
        #[cfg(feature = "jpeg")]
        ImageFormat::Jpeg,
        #[cfg(feature = "ktx2")]
        ImageFormat::Ktx2,
        #[cfg(feature = "png")]
        ImageFormat::Png,
        #[cfg(feature = "pnm")]
        ImageFormat::Pnm,
        #[cfg(feature = "qoi")]
        ImageFormat::Qoi,
        #[cfg(feature = "tga")]
        ImageFormat::Tga,
        #[cfg(feature = "tiff")]
        ImageFormat::Tiff,
        #[cfg(feature = "webp")]
        ImageFormat::WebP,
    ];

    /// Total count of file extensions, for computing supported file extensions list.
    const COUNT_FILE_EXTENSIONS: usize = {
        let mut count = 0;
        let mut idx = 0;
        while idx < Self::SUPPORTED_FORMATS.len() {
            count += Self::SUPPORTED_FORMATS[idx].to_file_extensions().len();
            idx += 1;
        }
        count
    };

    /// Gets the list of file extensions for all formats.
    pub const SUPPORTED_FILE_EXTENSIONS: &'static [&'static str] = &{
        let mut exts = [""; Self::COUNT_FILE_EXTENSIONS];
        let mut ext_idx = 0;
        let mut fmt_idx = 0;
        while fmt_idx < Self::SUPPORTED_FORMATS.len() {
            let mut off = 0;
            let fmt_exts = Self::SUPPORTED_FORMATS[fmt_idx].to_file_extensions();
            while off < fmt_exts.len() {
                exts[ext_idx] = fmt_exts[off];
                off += 1;
                ext_idx += 1;
            }
            fmt_idx += 1;
        }
        exts
    };

    /// Creates a new image loader that supports the provided formats.
    pub fn new(supported_compressed_formats: CompressedImageFormats) -> Self {
        Self {
            supported_compressed_formats,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub enum ImageFormatSetting {
    FromExtension,
    Format(ImageFormat),
    #[default]
    Guess,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ImageLoaderSettings {
    pub format: ImageFormatSetting,
    pub is_srgb: bool,
    pub sampler: ImageSampler,
    pub asset_usage: RenderAssetUsages,
    pub transfer_priority: RenderAssetTransferPriority,
}

impl Default for ImageLoaderSettings {
    fn default() -> Self {
        Self {
            format: ImageFormatSetting::default(),
            is_srgb: true,
            sampler: ImageSampler::Default,
            asset_usage: RenderAssetUsages::default(),
            transfer_priority: RenderAssetTransferPriority::default(),
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ImageLoaderError {
    #[error("Could load shader: {0}")]
    Io(#[from] std::io::Error),
    #[error("Could not load texture file: {0}")]
    FileTexture(#[from] FileTextureError),
}

impl AssetLoader for ImageLoader {
    type Asset = Image;
    type Settings = ImageLoaderSettings;
    type Error = ImageLoaderError;
    async fn load(
        &self,
        reader: &mut dyn Reader,
        settings: &ImageLoaderSettings,
        load_context: &mut LoadContext<'_>,
    ) -> Result<Image, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        let image_type = match settings.format {
            ImageFormatSetting::FromExtension => {
                // use the file extension for the image type
                let ext = load_context.path().extension().unwrap().to_str().unwrap();
                ImageType::Extension(ext)
            }
            ImageFormatSetting::Format(format) => ImageType::Format(format),
            ImageFormatSetting::Guess => {
                let format = image::guess_format(&bytes).map_err(|err| FileTextureError {
                    error: err.into(),
                    path: format!("{}", load_context.path().display()),
                })?;
                ImageType::Format(ImageFormat::from_image_crate_format(format).ok_or_else(
                    || FileTextureError {
                        error: TextureError::UnsupportedTextureFormat(format!("{format:?}")),
                        path: format!("{}", load_context.path().display()),
                    },
                )?)
            }
        };

        let image = Image::from_buffer(
            #[cfg(all(debug_assertions, feature = "dds"))]
            load_context.path().display().to_string(),
            &bytes,
            image_type,
            self.supported_compressed_formats,
            settings.is_srgb,
            settings.sampler.clone(),
            settings.asset_usage,
        )
        .map_err(|err| FileTextureError {
            error: err,
            path: format!("{}", load_context.path().display()),
        })?;

        // wgpu on web can't sample Rgba16Unorm, so wasm downconverts every 16-bit
        // texture to 8-bit. Native keeps 16-bit for precision EXCEPT sRGB (colour)
        // textures: there is no Rgba16UnormSrgb format, so a 16-bit sRGB texture
        // would otherwise sit in a linear format and render too bright.
        let convert_16bit = image.texture_descriptor.format
            == wgpu_types::TextureFormat::Rgba16Unorm
            && (cfg!(target_arch = "wasm32") || settings.is_srgb);
        let image = if convert_16bit {
            // Image::new resets sampler/view-descriptor to defaults; preserve them
            // across the 16->8 bit rebuild.
            let sampler = image.sampler.clone();
            let texture_view_descriptor = image.texture_view_descriptor.clone();
            let data = image
                .data
                .unwrap()
                .chunks_exact(2)
                .map(|pair| {
                    (u16::from_le_bytes([pair[0], pair[1]]) as f32 / u16::MAX as f32
                        * u8::MAX as f32) as u8
                })
                .collect::<Vec<_>>();
            // Mirror `from_dynamic`: sRGB textures keep their sRGB-encoded data in
            // an *Srgb format (the GPU decodes on sample). Hardcoding Rgba8Unorm
            // would leave sRGB data in a linear format -> washed-out colors.
            let format = if settings.is_srgb {
                wgpu_types::TextureFormat::Rgba8UnormSrgb
            } else {
                wgpu_types::TextureFormat::Rgba8Unorm
            };
            let mut image = Image::new(
                image.texture_descriptor.size,
                image.texture_descriptor.dimension,
                data,
                format,
                image.asset_usage,
            );
            image.sampler = sampler;
            image.texture_view_descriptor = texture_view_descriptor;
            image
        } else {
            image
        };

        #[cfg(feature = "reduce_image_sizes")]
        let image = {
            // wasm shrinks aggressively for memory; native shrinks only to
            // dodge wgpu's per-device texture-dim cap (downstream BC7
            // reprocess replaces with <=1024 anyway).
            #[cfg(target_arch = "wasm32")]
            const MAX_BASE_SIZE: u32 = 1024;
            #[cfg(not(target_arch = "wasm32"))]
            const MAX_BASE_SIZE: u32 = 8192;
            let size = image.texture_descriptor.size;
            if size.width > MAX_BASE_SIZE || size.height > MAX_BASE_SIZE {
                use crate::image::TextureFormatPixelInfo;

                let is_srgb = image.texture_descriptor.format.is_srgb();
                let asset_usage = image.asset_usage;
                let pixel_size = image.texture_descriptor.format.pixel_size() as u32;

                let empty_image = Image {
                    data: None,
                    texture_descriptor: image.texture_descriptor.clone(),
                    sampler: image.sampler.clone(),
                    texture_view_descriptor: image.texture_view_descriptor.clone(),
                    asset_usage: image.asset_usage,
                    transfer_priority: image.transfer_priority,
                };

                match image.try_into_dynamic() {
                    Ok(dyn_image) => {
                        let dyn_image = dyn_image.resize(
                            MAX_BASE_SIZE * 4 / pixel_size,
                            MAX_BASE_SIZE * 4 / pixel_size,
                            image::imageops::FilterType::CatmullRom,
                        );
                        let resized_image = Image::from_dynamic(dyn_image, is_srgb, asset_usage);
                        let mut texture_descriptor = resized_image.texture_descriptor;
                        texture_descriptor.usage = empty_image.texture_descriptor.usage;
                        texture_descriptor.view_formats =
                            empty_image.texture_descriptor.view_formats;
                        Image {
                            data: resized_image.data,
                            texture_descriptor,
                            ..empty_image
                        }
                    }
                    Err(e) => {
                        let image = e.image();
                        tracing::warn!("failed to resize {:?}", image.texture_descriptor.format);
                        image
                    }
                }
            } else {
                image
            }
        };

        let image = Image {
            transfer_priority: settings.transfer_priority,
            ..image
        };

        Ok(image)
    }

    fn extensions(&self) -> &[&str] {
        Self::SUPPORTED_FILE_EXTENSIONS
    }
}

/// An error that occurs when loading a texture from a file.
#[derive(Error, Debug)]
#[error("Error reading image file {path}: {error}, this is an error in `bevy_render`.")]
pub struct FileTextureError {
    error: TextureError,
    path: String,
}
