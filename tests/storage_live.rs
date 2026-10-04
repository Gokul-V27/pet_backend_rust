//! Round trip against the real bucket: upload a tiny PNG, read it back, delete it.
//! Uses the S3_* and STORAGE_PUBLIC_URL settings from the environment (your .env). Run with:
//!   set -a; source .env; set +a; cargo test --test storage_live -- --ignored --nocapture

use bytes::Bytes;
use wagwell_api::config::Config;
use wagwell_api::services::storage::{ImageKind, Storage, new_key, valid_key};

/// The smallest valid PNG: one transparent pixel.
const PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

#[tokio::test]
#[ignore = "talks to the real storage bucket"]
async fn upload_read_delete_round_trip() {
    let cfg = Config::from_env().expect("settings from .env");
    let s = cfg.storage.as_ref().expect("S3 settings present");
    let store = Storage::new(s);

    let kind = ImageKind::sniff(PIXEL_PNG).expect("a PNG");
    let key = new_key("products", kind, chrono::Utc::now());
    assert!(valid_key(&key));

    store
        .put_image(&key, kind, Bytes::from_static(PIXEL_PNG))
        .await
        .expect("upload");
    let back = store.get(&key).await.expect("read back");
    assert_eq!(&back[..], PIXEL_PNG);
    println!(
        "uploaded and read back {key}\npublic url: {}",
        store.public_url(&key)
    );

    store.delete(&key).await.expect("delete");
    assert!(store.get(&key).await.is_err(), "file should be gone");
    println!("deleted {key}");
}
