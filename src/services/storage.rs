//! Product and category pictures in S3-compatible storage (Supabase Storage).
//!
//! Only real JPEG, PNG and WebP files are accepted, checked from the file's own first bytes, not
//! from its name or the browser's claim. SVG and everything else is refused (SVG can carry
//! scripts). Files get random names, so nobody can guess or overwrite another file.

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{
    BehaviorVersion, Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use uuid::Uuid;

use crate::config::StorageConfig;

pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Folders the admin may upload into.
pub const FOLDERS: [&str; 4] = ["products", "categories", "offers", "reviews"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Jpeg,
    Png,
    Webp,
}

impl ImageKind {
    /// Identify a picture from its first bytes ("magic numbers").
    pub fn sniff(b: &[u8]) -> Option<Self> {
        if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            Some(Self::Png)
        } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Webp => "webp",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
        }
    }
}

/// `products/2026/10/<uuid>.webp`
pub fn new_key(folder: &str, kind: ImageKind, now: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "{folder}/{}/{}.{}",
        now.format("%Y/%m"),
        Uuid::new_v4(),
        kind.ext()
    )
}

/// Only keys this server could have made: a known folder, then year/month/uuid.ext.
pub fn valid_key(key: &str) -> bool {
    let parts: Vec<&str> = key.split('/').collect();
    let [folder, year, month, file] = parts.as_slice() else {
        return false;
    };
    let Some((stem, ext)) = file.rsplit_once('.') else {
        return false;
    };
    FOLDERS.contains(folder)
        && year.len() == 4
        && year.bytes().all(|c| c.is_ascii_digit())
        && month.len() == 2
        && month.bytes().all(|c| c.is_ascii_digit())
        && Uuid::parse_str(stem).is_ok()
        && matches!(ext, "jpg" | "png" | "webp")
}

#[derive(Clone)]
pub struct Storage {
    client: Client,
    bucket: String,
    public_base_url: String,
}

impl Storage {
    pub fn new(cfg: &StorageConfig) -> Self {
        let creds = Credentials::new(
            &cfg.access_key_id,
            &cfg.secret_access_key,
            None,
            None,
            "env",
        );
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(cfg.region.clone()))
            .endpoint_url(&cfg.endpoint)
            .credentials_provider(creds)
            // Supabase uses https://host/storage/v1/s3/<bucket>/<key>, not bucket.host.
            .force_path_style(true)
            // Newer SDK checksum headers aren't understood by every S3-compatible store.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .build();
        Self {
            client: Client::from_conf(conf),
            bucket: cfg.bucket.clone(),
            public_base_url: cfg.public_base_url.clone(),
        }
    }

    pub fn public_url(&self, key: &str) -> String {
        format!("{}/{key}", self.public_base_url)
    }

    pub async fn put_image(&self, key: &str, kind: ImageKind, body: Bytes) -> anyhow::Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(kind.mime())
            // Names are never reused, so browsers and CDNs may keep a copy for a year.
            .cache_control("public, max-age=31536000, immutable")
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "upload failed: {}",
                    aws_sdk_s3::error::DisplayErrorContext(e)
                )
            })?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "delete failed: {}",
                    aws_sdk_s3::error::DisplayErrorContext(e)
                )
            })?;
        Ok(())
    }

    /// Reads a file back (used by the live storage check).
    pub async fn get(&self, key: &str) -> anyhow::Result<Bytes> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!("read failed: {}", aws_sdk_s3::error::DisplayErrorContext(e))
            })?;
        Ok(out.body.collect().await?.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_real_formats_and_refuses_the_rest() {
        assert_eq!(
            ImageKind::sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(
            ImageKind::sniff(b"\x89PNG\r\n\x1a\n...."),
            Some(ImageKind::Png)
        );
        assert_eq!(
            ImageKind::sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageKind::Webp)
        );
        assert_eq!(
            ImageKind::sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\">"),
            None
        );
        assert_eq!(ImageKind::sniff(b"GIF89a"), None);
        assert_eq!(ImageKind::sniff(b"RIFF\0\0\0\0WAVE"), None);
        assert_eq!(ImageKind::sniff(b""), None);
    }

    #[test]
    fn keys_are_random_dated_and_validated() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-04T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let a = new_key("products", ImageKind::Webp, now);
        let b = new_key("products", ImageKind::Webp, now);
        assert!(a.starts_with("products/2026/10/") && a.ends_with(".webp"));
        assert_ne!(a, b);
        assert!(valid_key(&a));
        assert!(!valid_key("products/2026/10/../../secret.webp"));
        assert!(!valid_key(
            "other/2026/10/2f1c1a5e-0b8e-4a0c-9d2a-4f7f4a1b2c3d.webp"
        ));
        assert!(!valid_key("products/2026/10/not-a-uuid.webp"));
        assert!(!valid_key(
            "products/2026/10/2f1c1a5e-0b8e-4a0c-9d2a-4f7f4a1b2c3d.svg"
        ));
    }
}
