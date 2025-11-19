use crate::image::{Image, ImageFormat, ImageType, TextureError};
use bevy_asset::{io::Reader, AssetLoader, LoadContext, RenderAssetUsages};
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
    pub immediate_upload: bool,
}

impl Default for ImageLoaderSettings {
    fn default() -> Self {
        Self {
            format: ImageFormatSetting::default(),
            is_srgb: true,
            sampler: ImageSampler::Default,
            asset_usage: RenderAssetUsages::default(),
            immediate_upload: false,
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

        #[cfg(target_arch = "wasm32")]
        let image = if image.texture_descriptor.format == wgpu_types::TextureFormat::Rgba16Unorm {
            let data = image
                .data
                .unwrap()
                .chunks_exact(2)
                .map(|pair| {
                    (u16::from_le_bytes([pair[0], pair[1]]) as f32 / u16::MAX as f32
                        * u8::MAX as f32) as u8
                })
                .collect::<Vec<_>>();
            Image::new(
                image.texture_descriptor.size,
                image.texture_descriptor.dimension,
                data,
                wgpu_types::TextureFormat::Rgba8Unorm,
                image.asset_usage,
            )
        } else {
            image
        };

        #[cfg(feature = "reduce_image_sizes")]
        let image = if image.data.as_ref().unwrap().len() > 1024 * 1024 * 4 {
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
                immediate_upload: image.immediate_upload,
            };

            match image.try_into_dynamic() {
                Ok(dyn_image) => {

                    let dyn_image = dyn_image.resize(
                        1024 * 4 / pixel_size,
                        1024 * 4 / pixel_size,
                        image::imageops::FilterType::CatmullRom,
                    );
                    let resized_image = Image::from_dynamic(dyn_image, is_srgb, asset_usage);
                    let mut texture_descriptor = resized_image.texture_descriptor;
                    texture_descriptor.usage = empty_image.texture_descriptor.usage;
                    texture_descriptor.view_formats = empty_image.texture_descriptor.view_formats;
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
        };

        let image = Image {
            immediate_upload: settings.immediate_upload,
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
