use image::ImageFormat;
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
pub enum DownloadFormat {
    #[default]
    Jpeg,
    Png,
    Webp,
    Original,
}

impl DownloadFormat {
    pub fn extension(self) -> Option<&'static str> {
        match self {
            DownloadFormat::Jpeg => Some("jpg"),
            DownloadFormat::Png => Some("png"),
            DownloadFormat::Webp => Some("webp"),
            DownloadFormat::Original => None,
        }
    }

    pub fn to_image_format(self) -> Option<ImageFormat> {
        match self {
            DownloadFormat::Jpeg => Some(ImageFormat::Jpeg),
            DownloadFormat::Png => Some(ImageFormat::Png),
            DownloadFormat::Webp => Some(ImageFormat::WebP),
            DownloadFormat::Original => None,
        }
    }
}
