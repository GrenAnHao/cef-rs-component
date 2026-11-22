//! Pure frame-format and row-layout helpers shared by software capture and
//! accelerated read-back.
//!
//! These helpers used to live in the demo's `src/browser/shared_texture.rs`.
//! They are deliberately std-only: CEF supplies premultiplied BGRA, GPUI's
//! `RenderImage` expects BGRA, and D3D staging rows are often padded. All tests
//! for these real layout helpers live here; they are not native GPU evidence.

/// Whether content fills the visible aperture rather than a letterboxed
/// intermediate resize frame. Coded-buffer padding is not letterboxing.
pub fn is_full_frame(visible: (i32, i32, i32, i32), content: (i32, i32, i32, i32)) -> bool {
    visible.2 > 0 && visible.3 > 0 && content == visible
}

/// Pixel layout of an accelerated frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameFormat {
    /// 8-bit BGRA, the default for CEF's accelerated paint on Windows.
    Bgra8,
    /// 8-bit RGBA.
    Rgba8,
}

impl FrameFormat {
    /// Raw `cef_color_type_t` discriminants, so this module stays free of the
    /// `cef` crate and can be tested in the std-only target. Values are from
    /// `include/internal/cef_types_color.h`: RGBA is 0, BGRA is 1 (the enum has
    /// no explicit initialisers, so order is the value).
    pub const CEF_RGBA_8888: i32 = 0;
    pub const CEF_BGRA_8888: i32 = 1;

    /// Maps CEF's colour type. Returns `None` for formats this demo does not
    /// handle. Required acceleration must reject these, not silently fall back
    /// to another backend or interpret unknown bytes as BGRA.
    pub fn from_cef_raw(color: i32) -> Option<Self> {
        match color {
            Self::CEF_BGRA_8888 => Some(Self::Bgra8),
            Self::CEF_RGBA_8888 => Some(Self::Rgba8),
            _ => None,
        }
    }

    pub fn bytes_per_pixel(self) -> usize {
        4
    }

    /// Whether the bytes are already in the order gpui's `RenderImage` expects.
    ///
    /// gpui 0.2.2 documents `RenderImage` as BGRA, and CEF's software path hands
    /// us BGRA, so RGBA frames need a channel swap.
    pub fn matches_render_image(self) -> bool {
        matches!(self, Self::Bgra8)
    }
}

/// `row_pitch` is what D3D11 reports for the mapped staging texture. It is often
/// larger than `width * bytes_per_pixel` because drivers align rows, so copying
/// `height * width * 4` bytes straight out of it shears the image.
pub fn plane_size(width: u32, height: u32, row_pitch: u32, format: FrameFormat) -> Option<usize> {
    if width == 0 || height == 0 {
        return None;
    }
    let min_pitch = (width as usize).checked_mul(format.bytes_per_pixel())?;
    if (row_pitch as usize) < min_pitch {
        return None;
    }
    (row_pitch as usize).checked_mul(height as usize)
}

/// Copies tightly packed rows out of a pitched buffer.
///
/// `dst` must be exactly `width * height * 4` bytes. Returns `false` on a size
/// mismatch rather than panicking inside a paint callback.
pub fn copy_rows(
    src: &[u8],
    row_pitch: u32,
    width: u32,
    height: u32,
    format: FrameFormat,
    dst: &mut [u8],
) -> bool {
    let Some(needed) = plane_size(width, height, row_pitch, format) else {
        return false;
    };
    if src.len() < needed {
        return false;
    }
    let Some(row_bytes) = (width as usize).checked_mul(format.bytes_per_pixel()) else {
        return false;
    };
    let Some(expected_dst) = row_bytes.checked_mul(height as usize) else {
        return false;
    };
    if dst.len() != expected_dst {
        return false;
    }
    for row in 0..height as usize {
        let Some(start) = row.checked_mul(row_pitch as usize) else {
            return false;
        };
        let Some(target) = row.checked_mul(row_bytes) else {
            return false;
        };
        dst[target..target + row_bytes].copy_from_slice(&src[start..start + row_bytes]);
    }
    true
}

/// Copies a visible aperture from a pitched coded buffer directly into tight
/// premultiplied BGRA storage. It allocates no CPU scratch. Format conversion
/// uses the same channel-order helper as the original accelerated path.
pub fn copy_bgra_aperture(
    src: &[u8],
    row_pitch: u32,
    coded_width: u32,
    coded_height: u32,
    visible: (i32, i32, i32, i32),
    format: FrameFormat,
    dst: &mut [u8],
) -> bool {
    let Some(needed) = plane_size(coded_width, coded_height, row_pitch, format) else {
        return false;
    };
    let (x, y, width, height) = visible;
    if x < 0 || y < 0 || width <= 0 || height <= 0 || src.len() < needed {
        return false;
    }
    let (x, y, width, height) = (x as u32, y as u32, width as u32, height as u32);
    if x.checked_add(width).is_none_or(|right| right > coded_width)
        || y.checked_add(height)
            .is_none_or(|bottom| bottom > coded_height)
    {
        return false;
    }
    let Some(row_bytes) = (width as usize).checked_mul(4) else {
        return false;
    };
    if row_bytes.checked_mul(height as usize) != Some(dst.len()) {
        return false;
    }
    for row in 0..height as usize {
        let Some(start) = (y as usize + row)
            .checked_mul(row_pitch as usize)
            .and_then(|offset| {
                (x as usize)
                    .checked_mul(4)
                    .and_then(|x| offset.checked_add(x))
            })
        else {
            return false;
        };
        let Some(end) = start.checked_add(row_bytes) else {
            return false;
        };
        let Some(source) = src.get(start..end) else {
            return false;
        };
        let target = &mut dst[row * row_bytes..(row + 1) * row_bytes];
        if format.matches_render_image() {
            target.copy_from_slice(source);
        } else if !rgba_to_bgra(source, target) {
            return false;
        }
    }
    true
}

/// Converts RGBA to BGRA in place-adjacent form.
pub fn rgba_to_bgra(src: &[u8], dst: &mut [u8]) -> bool {
    if src.len() != dst.len() || src.len() % 4 != 0 {
        return false;
    }
    for (chunk_in, chunk_out) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        chunk_out[0] = chunk_in[2];
        chunk_out[1] = chunk_in[1];
        chunk_out[2] = chunk_in[0];
        chunk_out[3] = chunk_in[3];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intermediate_letterboxed_capture_is_not_a_full_frame() {
        assert!(!is_full_frame((0, 0, 947, 594), (0, 12, 947, 569)));
        assert!(!is_full_frame((0, 0, 1160, 848), (0, 0, 595, 213)));
        assert!(!is_full_frame((0, 0, 0, 0), (0, 0, 0, 0)));
        assert!(is_full_frame((0, 0, 1160, 848), (0, 0, 1160, 848)));
        // A clean aperture may be inset within a larger coded allocation.
        assert!(is_full_frame((2, 1, 1279, 719), (2, 1, 1279, 719)));
    }

    #[test]
    fn formats_map_from_cef_and_keep_channel_order() {
        assert_eq!(
            FrameFormat::from_cef_raw(FrameFormat::CEF_BGRA_8888),
            Some(FrameFormat::Bgra8)
        );
        assert_eq!(
            FrameFormat::from_cef_raw(FrameFormat::CEF_RGBA_8888),
            Some(FrameFormat::Rgba8)
        );
        assert_eq!(FrameFormat::from_cef_raw(99), None);
        // Guard against the two constants silently swapping: RGBA must be 0.
        assert_eq!(FrameFormat::CEF_RGBA_8888, 0);
        assert_eq!(FrameFormat::CEF_BGRA_8888, 1);
        assert!(FrameFormat::Bgra8.matches_render_image());
        assert!(!FrameFormat::Rgba8.matches_render_image());
    }

    #[test]
    fn plane_size_rejects_bad_geometry() {
        assert_eq!(plane_size(0, 10, 40, FrameFormat::Bgra8), None);
        assert_eq!(plane_size(10, 0, 40, FrameFormat::Bgra8), None);
        // 10 px * 4 bytes = 40; a smaller pitch cannot hold a row.
        assert_eq!(plane_size(10, 10, 39, FrameFormat::Bgra8), None);
        assert_eq!(
            plane_size(10, 10, 64, FrameFormat::Bgra8),
            Some(640),
            "padded pitch is allowed"
        );
    }

    #[test]
    fn copy_rows_strips_padding() {
        // 2x2 BGRA with a 3-byte pad per row: 8 bytes of pixels, 11 pitch.
        let width = 2;
        let height = 2;
        let pitch: u32 = 11;
        let mut src = vec![0u8; pitch as usize * height as usize];
        for row in 0..height as usize {
            src[row * pitch as usize] = 0x10 + row as u8;
            src[row * pitch as usize + 7] = 0x20 + row as u8;
        }
        let mut dst = vec![0u8; (width * height * 4) as usize];
        assert!(copy_rows(
            &src,
            pitch,
            width,
            height,
            FrameFormat::Bgra8,
            &mut dst
        ));
        assert_eq!(dst[0], 0x10);
        assert_eq!(dst[7], 0x20);
        // Second row starts at the compact offset 8, not 11.
        assert_eq!(dst[8], 0x11);
        assert_eq!(dst[15], 0x21);
    }

    #[test]
    fn copy_rows_refuses_mismatched_buffers() {
        let src = vec![0u8; 16];
        let mut too_small = vec![0u8; 4];
        assert!(!copy_rows(
            &src,
            8,
            2,
            2,
            FrameFormat::Bgra8,
            &mut too_small
        ));
        let mut ok = vec![0u8; 16];
        assert!(copy_rows(&src, 8, 2, 2, FrameFormat::Bgra8, &mut ok));
        // Source shorter than one full pitched image.
        assert!(!copy_rows(&src[..8], 8, 2, 2, FrameFormat::Bgra8, &mut ok));
    }

    #[test]
    fn aperture_copy_strips_coded_padding_and_preserves_alpha_without_scratch() {
        let mut source = [0xee; 48]; // 3x3 pixels, sixteen-byte pitched rows.
        for row in 0..3 {
            for column in 0..3 {
                let offset = row * 16 + column * 4;
                source[offset..offset + 4].copy_from_slice(&[row as u8, column as u8, 7, 127]);
            }
        }
        let mut bgra = [0; 16];
        assert!(copy_bgra_aperture(
            &source,
            16,
            3,
            3,
            (1, 1, 2, 2),
            FrameFormat::Bgra8,
            &mut bgra
        ));
        assert_eq!(
            bgra,
            [1, 1, 7, 127, 1, 2, 7, 127, 2, 1, 7, 127, 2, 2, 7, 127]
        );
        let mut converted = [0; 16];
        assert!(copy_bgra_aperture(
            &source,
            16,
            3,
            3,
            (1, 1, 2, 2),
            FrameFormat::Rgba8,
            &mut converted
        ));
        assert_eq!(
            converted,
            [7, 1, 1, 127, 7, 2, 1, 127, 7, 1, 2, 127, 7, 2, 2, 127]
        );
    }

    #[test]
    fn aperture_copy_rejects_outside_rect_stride_and_lengths_before_writing() {
        let source = [0; 48];
        let mut output = [99; 16];
        for rect in [
            (-1, 0, 2, 2),
            (0, -1, 2, 2),
            (0, 0, 0, 2),
            (2, 2, 2, 2),
            (i32::MAX, 0, 2, 2),
        ] {
            assert!(!copy_bgra_aperture(
                &source,
                16,
                3,
                3,
                rect,
                FrameFormat::Bgra8,
                &mut output
            ));
            assert_eq!(output, [99; 16]);
        }
        assert!(!copy_bgra_aperture(
            &source,
            11,
            3,
            3,
            (0, 0, 2, 2),
            FrameFormat::Bgra8,
            &mut output
        ));
        assert!(!copy_bgra_aperture(
            &source[..47],
            16,
            3,
            3,
            (0, 0, 2, 2),
            FrameFormat::Bgra8,
            &mut output
        ));
        assert!(!copy_bgra_aperture(
            &source,
            16,
            3,
            3,
            (0, 0, 2, 2),
            FrameFormat::Bgra8,
            &mut output[..15]
        ));
        assert_eq!(output, [99; 16]);
    }

    #[test]
    fn rgba_to_bgra_swaps_red_blue_only() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut dst = [0u8; 8];
        assert!(rgba_to_bgra(&src, &mut dst));
        assert_eq!(dst, [3, 2, 1, 4, 7, 6, 5, 8]);
        assert!(!rgba_to_bgra(&src, &mut [0u8; 4]));
        assert!(!rgba_to_bgra(&[1, 2, 3], &mut [0; 3]));
    }
}
