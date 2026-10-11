//! Removes hidden metadata from uploaded pictures before they are stored and published. Phone
//! photos carry EXIF (GPS position, camera, time), XMP and comments; product photos taken in the
//! shop or at home would otherwise publish where they were taken. Only the picture data is kept,
//! plus a JPEG's rotation, so a photo taken sideways still shows the right way up.
//!
//! The image itself isn't decoded or re-encoded: the file's metadata blocks are dropped and the
//! rest is copied byte for byte. A file whose structure doesn't add up is refused.

use crate::services::storage::ImageKind;

#[derive(Debug, PartialEq, Eq)]
pub struct Malformed;

pub fn strip(kind: ImageKind, bytes: &[u8]) -> Result<Vec<u8>, Malformed> {
    match kind {
        ImageKind::Jpeg => strip_jpeg(bytes),
        ImageKind::Png => strip_png(bytes),
        ImageKind::Webp => strip_webp(bytes),
    }
}

fn be16(b: &[u8], at: usize) -> Option<usize> {
    Some(usize::from(u16::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
    ])))
}

/* ───────────────────────── JPEG ───────────────────────── */

fn strip_jpeg(b: &[u8]) -> Result<Vec<u8>, Malformed> {
    if !b.starts_with(&[0xFF, 0xD8]) {
        return Err(Malformed);
    }
    let mut kept: Vec<&[u8]> = Vec::new();
    let mut orientation = None;
    let mut i = 2;
    loop {
        // Markers may be preceded by any number of 0xFF fill bytes.
        if b.get(i) != Some(&0xFF) {
            return Err(Malformed);
        }
        while b.get(i) == Some(&0xFF) {
            i += 1;
        }
        let marker = *b.get(i).ok_or(Malformed)?;
        i += 1;
        match marker {
            // Start of scan: the compressed picture follows; everything from here is kept as is.
            0xDA => {
                let mut out = Vec::with_capacity(b.len());
                out.extend_from_slice(&[0xFF, 0xD8]);
                let jfif_first = kept.first().is_some_and(|s| s[1] == 0xE0);
                let mut segs = kept.into_iter();
                if jfif_first {
                    out.extend_from_slice(segs.next().unwrap_or_default());
                }
                if let Some(o) = orientation.filter(|o| (2..=8).contains(o)) {
                    out.extend_from_slice(&exif_orientation_only(o));
                }
                for s in segs {
                    out.extend_from_slice(s);
                }
                out.extend_from_slice(&b[i - 2..]);
                return Ok(out);
            }
            // Markers without a length.
            0x01 | 0xD0..=0xD7 => kept.push(&b[i - 2..i]),
            0xD9 => return Err(Malformed), // end of image before any picture data
            _ => {
                let len = be16(b, i).ok_or(Malformed)?;
                if len < 2 || i + len > b.len() {
                    return Err(Malformed);
                }
                let seg = &b[i - 2..i + len];
                let payload = &b[i + 2..i + len];
                let keep = match marker {
                    0xE0 => true,                                  // JFIF / JFXX
                    0xE2 => payload.starts_with(b"ICC_PROFILE\0"), // colour profile
                    0xEE => payload.starts_with(b"Adobe"),         // colour transform
                    0xE1 => {
                        if payload.starts_with(b"Exif\0\0") {
                            orientation = orientation.or_else(|| exif_orientation(&payload[6..]));
                        }
                        false // EXIF (GPS, camera, time) and XMP
                    }
                    0xE3..=0xEF | 0xFE => false, // other application data, comments
                    _ => true,                   // tables, frame header…: the picture itself
                };
                if keep {
                    kept.push(seg);
                }
                i += len;
            }
        }
    }
}

/// Reads the Orientation tag (0x0112) from IFD0 of an EXIF TIFF block.
fn exif_orientation(tiff: &[u8]) -> Option<u16> {
    let little = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b = [*tiff.get(at)?, *tiff.get(at + 1)?];
        Some(if little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b = [
            *tiff.get(at)?,
            *tiff.get(at + 1)?,
            *tiff.get(at + 2)?,
            *tiff.get(at + 3)?,
        ];
        Some(if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let count = usize::from(u16_at(ifd)?);
    (0..count.min(256)).find_map(|n| {
        let e = ifd + 2 + n * 12;
        (u16_at(e)? == 0x0112 && u16_at(e + 2)? == 3)
            .then(|| u16_at(e + 8))
            .flatten()
    })
}

/// A minimal EXIF block holding only the orientation (big-endian TIFF, one IFD0 entry).
fn exif_orientation_only(o: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(36);
    p.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x22]); // APP1, length 34
    p.extend_from_slice(b"Exif\0\0");
    p.extend_from_slice(&[b'M', b'M', 0x00, 0x2A, 0x00, 0x00, 0x00, 0x08]); // TIFF header, IFD0 at 8
    p.extend_from_slice(&[0x00, 0x01]); // one entry
    p.extend_from_slice(&[0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01]); // Orientation, SHORT, 1
    p.extend_from_slice(&o.to_be_bytes());
    p.extend_from_slice(&[0x00, 0x00]); // value padding
    p.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // no next IFD
    p
}

/* ───────────────────────── PNG ───────────────────────── */

const PNG_SIG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn strip_png(b: &[u8]) -> Result<Vec<u8>, Malformed> {
    if !b.starts_with(PNG_SIG) {
        return Err(Malformed);
    }
    let mut out = PNG_SIG.to_vec();
    let mut i = PNG_SIG.len();
    loop {
        let len = b.get(i..i + 4).ok_or(Malformed)?;
        let len = usize::try_from(u32::from_be_bytes([len[0], len[1], len[2], len[3]]))
            .map_err(|_| Malformed)?;
        let end = i
            .checked_add(12)
            .and_then(|x| x.checked_add(len))
            .ok_or(Malformed)?;
        let chunk = b.get(i..end).ok_or(Malformed)?;
        let kind = &chunk[4..8];
        // Text, EXIF and timestamp chunks go; everything that draws the picture stays.
        if !matches!(kind, b"tEXt" | b"zTXt" | b"iTXt" | b"eXIf" | b"tIME") {
            out.extend_from_slice(chunk);
        }
        i = end;
        if kind == b"IEND" {
            return Ok(out);
        }
    }
}

/* ───────────────────────── WebP ───────────────────────── */

fn strip_webp(b: &[u8]) -> Result<Vec<u8>, Malformed> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return Err(Malformed);
    }
    let riff_end = usize::try_from(u32::from_le_bytes([b[4], b[5], b[6], b[7]]))
        .ok()
        .and_then(|n| n.checked_add(8))
        .ok_or(Malformed)?;
    let body_end = riff_end.min(b.len());
    let mut chunks = Vec::new();
    let mut i = 12;
    while i + 8 <= body_end {
        let size = usize::try_from(u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]))
            .map_err(|_| Malformed)?;
        let padded = size + (size & 1);
        let end = i.checked_add(8 + padded).ok_or(Malformed)?;
        if end > b.len() {
            return Err(Malformed);
        }
        let fourcc = &b[i..i + 4];
        if !matches!(fourcc, b"EXIF" | b"XMP ") {
            let mut chunk = b[i..end].to_vec();
            if fourcc == b"VP8X" && chunk.len() > 8 {
                chunk[8] &= !(0x08 | 0x04); // the "has EXIF" and "has XMP" flags
            }
            chunks.push(chunk);
        }
        i = end;
    }
    if chunks.is_empty() {
        return Err(Malformed);
    }
    let body: usize = chunks.iter().map(Vec::len).sum();
    let riff_size = u32::try_from(4 + body).map_err(|_| Malformed)?;
    let mut out = Vec::with_capacity(12 + body);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    for c in chunks {
        out.extend_from_slice(&c);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
        s.extend_from_slice(payload);
        s
    }

    /// EXIF (little-endian) with Orientation = 6 and a GPS tag pointer.
    fn exif_with_gps_and_orientation() -> Vec<u8> {
        let mut p = b"Exif\0\0".to_vec();
        p.extend_from_slice(b"II\x2A\x00\x08\x00\x00\x00"); // TIFF, IFD0 at 8
        p.extend_from_slice(&[0x02, 0x00]); // two entries
        p.extend_from_slice(&[
            0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00,
        ]); // orientation 6
        p.extend_from_slice(&[
            0x25, 0x88, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x26, 0x00, 0x00, 0x00,
        ]); // GPS IFD pointer
        p.extend_from_slice(&[0, 0, 0, 0]);
        p.extend_from_slice(b"GPS 13.0012N 80.2565E");
        p
    }

    fn jpeg(extra: &[Vec<u8>]) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        j.extend(seg(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
        for e in extra {
            j.extend(e);
        }
        j.extend(seg(0xDB, &[0u8; 65])); // quantisation table
        j.extend(seg(0xC0, &[8, 0, 1, 0, 1, 1, 1, 0x11, 0])); // frame header
        j.extend(seg(0xDA, &[1, 1, 0, 0, 63, 0])); // start of scan
        j.extend_from_slice(&[0x12, 0x34, 0xFF, 0x00, 0x56, 0xFF, 0xD9]); // scan data + end
        j
    }

    #[test]
    fn jpeg_loses_gps_comments_and_xmp_but_keeps_rotation_and_picture() {
        let dirty = jpeg(&[
            seg(0xE1, &exif_with_gps_and_orientation()),
            seg(
                0xE1,
                b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>Shop on Lake View Rd</x:xmpmeta>",
            ),
            seg(0xFE, b"Taken at home"),
            seg(0xE2, b"ICC_PROFILE\0\x01\x01profile"),
        ]);
        let clean = strip(ImageKind::Jpeg, &dirty).unwrap();
        let has = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
        assert!(!has(&clean, b"GPS 13.0012N"));
        assert!(!has(&clean, b"Lake View"));
        assert!(!has(&clean, b"Taken at home"));
        assert!(has(&clean, b"ICC_PROFILE"), "colour profile kept");
        assert!(has(&clean, b"JFIF"));
        assert_eq!(
            exif_orientation(
                &clean[clean.windows(6).position(|w| w == b"Exif\0\0").unwrap() + 6..]
            ),
            Some(6)
        );
        assert!(
            clean.ends_with(&[0x12, 0x34, 0xFF, 0x00, 0x56, 0xFF, 0xD9]),
            "picture data untouched"
        );
        assert!(
            strip(ImageKind::Jpeg, &clean).unwrap() == clean,
            "stripping twice changes nothing"
        );
    }

    #[test]
    fn jpeg_without_metadata_is_unchanged() {
        let plain = jpeg(&[]);
        assert_eq!(strip(ImageKind::Jpeg, &plain).unwrap(), plain);
    }

    #[test]
    fn broken_files_are_refused() {
        assert_eq!(
            strip(ImageKind::Jpeg, &[0xFF, 0xD8, 0xFF, 0xE1, 0xFF, 0xFF]),
            Err(Malformed)
        );
        assert_eq!(strip(ImageKind::Jpeg, b"not a jpeg"), Err(Malformed));
        assert_eq!(strip(ImageKind::Png, PNG_SIG), Err(Malformed));
        assert_eq!(
            strip(ImageKind::Webp, b"RIFF\x04\0\0\0WEBP"),
            Err(Malformed)
        );
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        c.extend_from_slice(kind);
        c.extend_from_slice(data);
        c.extend_from_slice(&[0, 0, 0, 0]); // CRC (not checked here)
        c
    }

    #[test]
    fn png_loses_text_and_exif_chunks() {
        let mut p = PNG_SIG.to_vec();
        p.extend(png_chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]));
        p.extend(png_chunk(b"tEXt", b"Comment\0Taken at home"));
        p.extend(png_chunk(b"eXIf", b"MM\0*GPS"));
        p.extend(png_chunk(b"IDAT", &[1, 2, 3]));
        p.extend(png_chunk(b"IEND", &[]));
        let clean = strip(ImageKind::Png, &p).unwrap();
        assert!(!clean.windows(4).any(|w| w == b"tEXt" || w == b"eXIf"));
        assert!(clean.windows(4).any(|w| w == b"IDAT"));
        assert!(clean.ends_with(&png_chunk(b"IEND", &[])));
    }

    #[test]
    fn webp_loses_exif_and_xmp_and_their_flags() {
        let chunk = |fourcc: &[u8; 4], data: &[u8]| {
            let mut c = fourcc.to_vec();
            c.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
            c.extend_from_slice(data);
            if data.len() % 2 == 1 {
                c.push(0);
            }
            c
        };
        let mut body = b"WEBP".to_vec();
        body.extend(chunk(b"VP8X", &[0x0C, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        body.extend(chunk(b"VP8 ", &[9, 9, 9]));
        body.extend(chunk(b"EXIF", b"GPS here"));
        body.extend(chunk(b"XMP ", b"<xmp/>"));
        let mut w = b"RIFF".to_vec();
        w.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        w.extend(body);
        let clean = strip(ImageKind::Webp, &w).unwrap();
        assert!(!clean.windows(4).any(|x| x == b"EXIF" || x == b"XMP "));
        assert_eq!(clean[20] & 0x0C, 0, "flags cleared");
        let size = u32::from_le_bytes([clean[4], clean[5], clean[6], clean[7]]) as usize;
        assert_eq!(size + 8, clean.len(), "RIFF size matches");
    }
}
