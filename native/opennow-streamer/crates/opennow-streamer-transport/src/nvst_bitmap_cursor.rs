//! Native ServerControl BMP cursors, normalized to the existing cursor callback.
//! Cache ownership is one bundle/stream; sender IDs are never narrowed for lookup.
use base64::{Engine, engine::general_purpose::STANDARD};
use std::collections::VecDeque;

const MAX_CURSORS: usize = 16;
const MAX_IMAGE_BYTES: usize = 49_000; // base64 must fit the callback's u16 length

#[derive(Default)]
pub(crate) struct BitmapCursors {
    entries: VecDeque<(u32, Vec<u8>)>,
}

impl BitmapCursors {
    pub(crate) fn normalize(&mut self, message: &[u8]) -> Option<Vec<u8>> {
        if read_u16(message, 0)? != 0x0110 {
            return None;
        }
        let size = usize::from(read_u16(message, 2)?);
        if message.len() != size + 4 {
            return None;
        }
        let body = &message[4..];
        let id = read_u32(body, 0)?;
        let image_size = usize::from(read_u16(body, 4)?);
        let (mut result, suffix_offset) = if image_size == 0 {
            let index = self.entries.iter().position(|entry| entry.0 == id)?;
            let entry = self.entries.remove(index)?;
            let result = entry.1.clone();
            self.entries.push_back(entry);
            (result, 6)
        } else {
            let image = body.get(12..12usize.checked_add(image_size)?)?;
            let (bmp, width, height) = normalized_bmp(image)?;
            let hotspot_x = read_u16(body, 8)?;
            let hotspot_y = read_u16(body, 10)?;
            if hotspot_x >= width || hotspot_y >= height {
                return None;
            }
            let encoded = STANDARD.encode(bmp);
            let length = u16::try_from(encoded.len()).ok()?;
            // Type 1 is a custom image, including when the callback ID is zero.
            // The full server ID belongs to the cache, not this legacy u8 field.
            let mut result = vec![1, 0, hotspot_x as u8, hotspot_y as u8, 9];
            result.extend_from_slice(b"image/bmp");
            result.extend_from_slice(&length.to_le_bytes());
            result.extend_from_slice(encoded.as_bytes());
            self.entries.retain(|entry| entry.0 != id);
            if self.entries.len() == MAX_CURSORS {
                self.entries.pop_front();
            }
            self.entries.push_back((id, result.clone()));
            (result, 12 + image_size)
        };
        // Native optional suffix: x/y (4), visibility (1), scale percent (2).
        // Existing callback suffix: x/y (4), scale percent (2). Shape/type own
        // relative mode; do not turn the visibility metadata into system ID 0.
        let suffix = body.get(suffix_offset..)?;
        if suffix.len() >= 4 {
            result.extend_from_slice(&suffix[..4]);
            if suffix.len() >= 7 {
                result.extend_from_slice(&suffix[5..7]);
            }
        }
        Some(result)
    }
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}
fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Emit an explicit-alpha BMP v4 so Qt does not discard cursor transparency.
/// Accept bounded, uncompressed 32-bit BMPs and standard BGRA bitfield BMPs.
fn normalized_bmp(image: &[u8]) -> Option<(Vec<u8>, u16, u16)> {
    if image.len() > MAX_IMAGE_BYTES || image.get(..2)? != b"BM" {
        return None;
    }
    let header = usize::try_from(read_u32(image, 14)?).ok()?;
    if !matches!(header, 40 | 108 | 124) || read_u16(image, 26)? != 1 || read_u16(image, 28)? != 32
    {
        return None;
    }
    let width = read_u32(image, 18)? as i32;
    let signed_height = read_u32(image, 22)? as i32;
    let height = signed_height.checked_abs()?;
    if !(1..=256).contains(&width) || !(1..=256).contains(&height) {
        return None;
    }
    let compression = read_u32(image, 30)?;
    if compression != 0 && compression != 3 {
        return None;
    }
    let masks = [0x00ff0000, 0x0000ff00, 0x000000ff, 0xff000000];
    if compression == 3
        && (header < 108 || (0..4).any(|i| read_u32(image, 54 + i * 4) != Some(masks[i])))
    {
        return None;
    }
    let offset = usize::try_from(read_u32(image, 10)?).ok()?;
    if offset < 14 + header {
        return None;
    }
    let pixels_len = width as usize * height as usize * 4;
    let pixels = image.get(offset..offset.checked_add(pixels_len)?)?;
    if 122 + pixels_len > MAX_IMAGE_BYTES {
        return None;
    }
    let mut bmp = vec![0; 122];
    bmp[..54].copy_from_slice(image.get(..54)?);
    bmp[2..6].copy_from_slice(&((122 + pixels_len) as u32).to_le_bytes());
    bmp[10..14].copy_from_slice(&122_u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&108_u32.to_le_bytes());
    bmp[30..34].copy_from_slice(&3_u32.to_le_bytes());
    bmp[34..38].copy_from_slice(&(pixels_len as u32).to_le_bytes());
    bmp[46..54].fill(0);
    for (i, mask) in masks.into_iter().enumerate() {
        bmp[54 + i * 4..58 + i * 4].copy_from_slice(&mask.to_le_bytes());
    }
    bmp[70..74].copy_from_slice(&0x73524742_u32.to_le_bytes()); // LCS_sRGB
    bmp.extend_from_slice(pixels);
    // BMP v3 can use the fourth byte as padding. Match SDL's opaque fallback
    // when every alpha byte is zero; otherwise preserve every alpha byte.
    if header == 40 && compression == 0 && pixels.chunks_exact(4).all(|p| p[3] == 0) {
        for pixel in bmp[122..].chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }
    Some((bmp, width as u16, height as u16))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bmp() -> Vec<u8> {
        let mut image = vec![0; 54];
        image[..2].copy_from_slice(b"BM");
        image[2..6].copy_from_slice(&62_u32.to_le_bytes());
        image[10..14].copy_from_slice(&54_u32.to_le_bytes());
        image[14..18].copy_from_slice(&40_u32.to_le_bytes());
        image[18..22].copy_from_slice(&2_u32.to_le_bytes());
        image[22..26].copy_from_slice(&1_u32.to_le_bytes());
        image[26..28].copy_from_slice(&1_u16.to_le_bytes());
        image[28..30].copy_from_slice(&32_u16.to_le_bytes());
        image.extend_from_slice(&[3, 2, 1, 255, 6, 5, 4, 0]);
        image
    }
    fn message(id: u32, image: Option<&[u8]>, suffix: &[u8]) -> Vec<u8> {
        let mut body = id.to_le_bytes().to_vec();
        body.extend_from_slice(&(image.map_or(0, |v| v.len()) as u16).to_le_bytes());
        if let Some(image) = image {
            body.extend_from_slice(&[0, 0, 1, 0, 0, 0]);
            body.extend_from_slice(image);
        }
        body.extend_from_slice(suffix);
        let mut wire = vec![0x10, 1];
        wire.extend_from_slice(&(body.len() as u16).to_le_bytes());
        wire.extend(body);
        wire
    }
    #[test]
    fn full_bitmap_and_six_byte_cache_reference_preserve_alpha_and_hotspot() {
        let mut cache = BitmapCursors::default();
        let full = cache.normalize(&message(1000, Some(&bmp()), &[])).unwrap();
        assert_eq!(&full[..5], &[1, 0, 1, 0, 9]);
        assert!(crate::nvst_cursor::valid_cursor_channel_message(&full));
        let image = STANDARD.decode(&full[16..]).unwrap();
        assert_eq!(read_u32(&image, 14), Some(108));
        assert_eq!(read_u32(&image, 66), Some(0xff000000));
        assert_eq!(&image[122..], &[3, 2, 1, 255, 6, 5, 4, 0]);
        assert_eq!(
            cache.normalize(&message(1000, None, &[])),
            Some(full.clone())
        );
        assert!(cache.normalize(&message(744, None, &[])).is_none());
        let positioned = cache
            .normalize(&message(1000, None, &[1, 2, 3, 4, 0, 150, 0]))
            .unwrap();
        assert_eq!(&positioned[full.len()..], &[1, 2, 3, 4, 150, 0]);
        assert!(crate::nvst_cursor::valid_cursor_channel_message(
            &positioned
        ));
    }
    #[test]
    fn malformed_updates_cannot_replace_cached_image_and_cache_is_bounded() {
        let mut cache = BitmapCursors::default();
        let wire = message(1000, Some(&bmp()), &[]);
        let expected = cache.normalize(&wire).unwrap();
        for length in 0..wire.len() {
            assert!(cache.normalize(&wire[..length]).is_none());
        }
        let mut bad = bmp();
        bad[18..22].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(cache.normalize(&message(1000, Some(&bad), &[])).is_none());
        assert_eq!(cache.normalize(&message(1000, None, &[])), Some(expected));
        for id in 1001..1100 {
            cache.normalize(&message(id, Some(&bmp()), &[])).unwrap();
        }
        assert_eq!(cache.entries.len(), MAX_CURSORS);
        assert!(cache.normalize(&message(1000, None, &[])).is_none());
        assert!(
            BitmapCursors::default()
                .normalize(&message(1099, None, &[]))
                .is_none()
        );
    }
    #[test]
    fn invalid_hotspots_and_oversized_dimensions_are_rejected() {
        let mut wire = message(1000, Some(&bmp()), &[]);
        wire[12..14].copy_from_slice(&256_u16.to_le_bytes());
        assert!(BitmapCursors::default().normalize(&wire).is_none());
        let mut image = bmp();
        image[22..26].copy_from_slice(&i32::MIN.to_le_bytes());
        assert!(normalized_bmp(&image).is_none());
        image[22..26].copy_from_slice(&257_i32.to_le_bytes());
        assert!(normalized_bmp(&image).is_none());
    }
}
