//! Synchronously extracts owned cursor payloads from CEF `OnCursorChange` inputs on Windows.
//!
//! Reference anchors:
//! - `D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\Internals\ClientAdapter.cpp:345-379`
//! - `D:\System Dir\Downloads\CefSharp\CefSharp.Wpf\ChromiumWebBrowser.cs:1121-1154`
//! - `.cef\156.0.31_windows_x86_64\include\internal\cef_types.h:2712-2776`

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use cef::{CursorInfo, CursorType};
use rust_cef_core::cursor::{MAX_CUSTOM_CURSOR_DIMENSION, NativeCursorData, StandardCursor};

#[repr(C)]
#[derive(Default)]
struct IconInfo {
    f_icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    hbm_mask: isize,
    hbm_color: isize,
}

#[repr(C)]
#[derive(Default)]
struct WinBitmap {
    bm_type: i32,
    bm_width: i32,
    bm_height: i32,
    bm_width_bytes: i32,
    bm_planes: u16,
    bm_bits_pixel: u16,
    bm_bits: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[repr(C)]
struct BitmapInfo2 {
    bmi_header: BitmapInfoHeader,
    bmi_colors: [u32; 2],
}

#[allow(clashing_extern_declarations)]
#[link(name = "user32")]
unsafe extern "system" {
    fn GetIconInfo(h_icon: isize, piconinfo: *mut IconInfo) -> i32;
    fn GetDC(hwnd: isize) -> isize;
    fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
}

#[allow(clashing_extern_declarations)]
#[link(name = "gdi32")]
unsafe extern "system" {
    fn GetObjectW(h: isize, c: i32, pv: *mut c_void) -> i32;
    fn GetDIBits(
        hdc: isize,
        hbm: isize,
        start: u32,
        c_lines: u32,
        lpv_bits: *mut c_void,
        lpbmi: *mut BitmapInfoHeader,
        usage: u32,
    ) -> i32;
    fn DeleteObject(ho: isize) -> i32;
}

struct OwnedGdiObject(isize);

impl Drop for OwnedGdiObject {
    fn drop(&mut self) {
        if self.0 != 0 {
            let _ = unsafe { DeleteObject(self.0) };
        }
    }
}

struct ScreenDc(isize);

impl ScreenDc {
    fn get() -> Option<Self> {
        let hdc = unsafe { GetDC(0) };
        (hdc != 0).then_some(Self(hdc))
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        if self.0 != 0 {
            let _ = unsafe { ReleaseDC(0, self.0) };
        }
    }
}

fn read_bitmap_dims(hbm: isize) -> Option<(i32, i32)> {
    let mut bmp = WinBitmap::default();
    let got = unsafe {
        GetObjectW(
            hbm,
            std::mem::size_of::<WinBitmap>() as i32,
            (&mut bmp as *mut WinBitmap).cast(),
        )
    };
    if got == 0 {
        return None;
    }
    Some((bmp.bm_width, bmp.bm_height))
}

pub(crate) fn extract_hcursor_data(hcursor: isize) -> Option<NativeCursorData> {
    if hcursor == 0 {
        return None;
    }
    let mut info = IconInfo::default();
    if unsafe { GetIconInfo(hcursor, &mut info) } == 0 {
        return None;
    }
    let _mask_guard = OwnedGdiObject(info.hbm_mask);
    let _color_guard = OwnedGdiObject(info.hbm_color);
    let dc = ScreenDc::get()?;

    if info.hbm_color != 0 {
        let (bw, bh) = read_bitmap_dims(info.hbm_color)?;
        if bw <= 0 || bh <= 0 {
            return None;
        }
        let w = bw as u32;
        let h = bh as u32;
        if w > MAX_CUSTOM_CURSOR_DIMENSION || h > MAX_CUSTOM_CURSOR_DIMENSION {
            return None;
        }
        let byte_len = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
        let mut bgra = vec![0u8; byte_len];
        let mut bih = BitmapInfoHeader {
            bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            bi_width: bw,
            bi_height: -bh, // Top-down DIB matching upper-left origin.
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: 0, // BI_RGB
            ..Default::default()
        };
        let lines = unsafe {
            GetDIBits(
                dc.0,
                info.hbm_color,
                0,
                h,
                bgra.as_mut_ptr().cast(),
                &mut bih,
                0,
            )
        };
        if lines != bh {
            return None;
        }
        // Legacy non-alpha color cursors use all-zero alpha + the 1-bit AND mask.
        if bgra.chunks_exact(4).all(|px| px[3] == 0) && info.hbm_mask != 0 {
            let (mw, mh) = read_bitmap_dims(info.hbm_mask)?;
            if mw == bw && mh >= bh {
                let mut mask_bgra = vec![0u8; byte_len];
                let mut mbih = BitmapInfoHeader {
                    bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                    bi_width: bw,
                    bi_height: -bh,
                    bi_planes: 1,
                    bi_bit_count: 32,
                    bi_compression: 0,
                    ..Default::default()
                };
                let mlines = unsafe {
                    GetDIBits(
                        dc.0,
                        info.hbm_mask,
                        0,
                        h,
                        mask_bgra.as_mut_ptr().cast(),
                        &mut mbih,
                        0,
                    )
                };
                if mlines == bh {
                    for (px, mpx) in bgra.chunks_exact_mut(4).zip(mask_bgra.chunks_exact(4)) {
                        if mpx[0] == 0 && mpx[1] == 0 && mpx[2] == 0 {
                            px[3] = 255;
                        } else {
                            px.copy_from_slice(&[0, 0, 0, 0]);
                        }
                    }
                }
            }
        }
        return NativeCursorData::from_bgra(
            w,
            h,
            info.x_hotspot as i32,
            info.y_hotspot as i32,
            bgra,
        );
    }

    if info.hbm_mask == 0 {
        return None;
    }
    let (bw, bh) = read_bitmap_dims(info.hbm_mask)?;
    if bw <= 0 || bh < 2 || bh % 2 != 0 {
        return None;
    }
    let w = bw as u32;
    let cursor_h = (bh / 2) as u32;
    if w > MAX_CUSTOM_CURSOR_DIMENSION || cursor_h > MAX_CUSTOM_CURSOR_DIMENSION {
        return None;
    }
    let dib_stride = ((w as usize + 31) / 32) * 4;
    let total_dib_bytes = dib_stride.checked_mul(bh as usize)?;
    let mut dib = vec![0u8; total_dib_bytes];
    let mut bmi = BitmapInfo2 {
        bmi_header: BitmapInfoHeader {
            bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            bi_width: bw,
            bi_height: -bh,
            bi_planes: 1,
            bi_bit_count: 1,
            bi_compression: 0,
            bi_clr_used: 2,
            ..Default::default()
        },
        bmi_colors: [0x0000_0000, 0x00FF_FFFF],
    };
    let lines = unsafe {
        GetDIBits(
            dc.0,
            info.hbm_mask,
            0,
            bh as u32,
            dib.as_mut_ptr().cast(),
            (&mut bmi as *mut BitmapInfo2).cast(),
            0,
        )
    };
    if lines != bh {
        return None;
    }
    let mask_stride = ((w as usize + 15) / 16) * 2;
    let plane_bytes = mask_stride.checked_mul(cursor_h as usize)?;
    let mut and_mask = Vec::with_capacity(plane_bytes);
    let mut xor_mask = Vec::with_capacity(plane_bytes);
    for row in 0..cursor_h as usize {
        let start = row * dib_stride;
        and_mask.extend_from_slice(&dib[start..start + mask_stride]);
    }
    for row in (cursor_h as usize)..(bh as usize) {
        let start = row * dib_stride;
        xor_mask.extend_from_slice(&dib[start..start + mask_stride]);
    }
    NativeCursorData::from_monochrome(
        w,
        cursor_h,
        info.x_hotspot as i32,
        info.y_hotspot as i32,
        and_mask,
        xor_mask,
    )
}

fn direct_standard_cursor(type_: CursorType) -> Option<StandardCursor> {
    match type_ {
        CursorType::POINTER => Some(StandardCursor::Arrow),
        CursorType::IBEAM => Some(StandardCursor::IBeam),
        CursorType::WAIT => Some(StandardCursor::Wait),
        CursorType::CROSS => Some(StandardCursor::Cross),
        CursorType::HAND => Some(StandardCursor::Hand),
        CursorType::HELP => Some(StandardCursor::Help),
        CursorType::PROGRESS => Some(StandardCursor::AppStarting),
        CursorType::MOVE => Some(StandardCursor::SizeAll),
        CursorType::NOTALLOWED | CursorType::NODROP | CursorType::DND_NONE => {
            Some(StandardCursor::No)
        }
        CursorType::EASTRESIZE | CursorType::WESTRESIZE | CursorType::EASTWESTRESIZE => {
            Some(StandardCursor::SizeWe)
        }
        CursorType::NORTHRESIZE | CursorType::SOUTHRESIZE | CursorType::NORTHSOUTHRESIZE => {
            Some(StandardCursor::SizeNs)
        }
        CursorType::NORTHWESTRESIZE
        | CursorType::SOUTHEASTRESIZE
        | CursorType::NORTHWESTSOUTHEASTRESIZE => Some(StandardCursor::SizeNwse),
        CursorType::NORTHEASTRESIZE
        | CursorType::SOUTHWESTRESIZE
        | CursorType::NORTHEASTSOUTHWESTRESIZE => Some(StandardCursor::SizeNesw),
        _ => None,
    }
}

fn fallback_resource_cursor(type_: CursorType) -> NativeCursorData {
    let std = match type_ {
        CursorType::GRAB | CursorType::GRABBING => StandardCursor::Hand,
        CursorType::COLUMNRESIZE => StandardCursor::SizeWe,
        CursorType::ROWRESIZE => StandardCursor::SizeNs,
        CursorType::VERTICALTEXT => StandardCursor::IBeam,
        CursorType::CELL => StandardCursor::Cross,
        CursorType::MIDDLEPANNING
        | CursorType::EASTPANNING
        | CursorType::NORTHPANNING
        | CursorType::NORTHEASTPANNING
        | CursorType::NORTHWESTPANNING
        | CursorType::SOUTHPANNING
        | CursorType::SOUTHEASTPANNING
        | CursorType::SOUTHWESTPANNING
        | CursorType::WESTPANNING
        | CursorType::MIDDLE_PANNING_VERTICAL
        | CursorType::MIDDLE_PANNING_HORIZONTAL
        | CursorType::DND_MOVE => StandardCursor::SizeAll,
        _ => StandardCursor::Arrow,
    };
    NativeCursorData::Standard(std)
}

fn resource_cursor_cache() -> &'static Mutex<BTreeMap<i32, NativeCursorData>> {
    static CACHE: OnceLock<Mutex<BTreeMap<i32, NativeCursorData>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(crate) fn resolve_cef_cursor(
    cursor: cef::sys::HCURSOR,
    type_: CursorType,
    custom_info: Option<&CursorInfo>,
) -> NativeCursorData {
    if type_ == CursorType::NONE {
        return NativeCursorData::Hidden;
    }
    if type_ == CursorType::CUSTOM {
        if let Some(info) = custom_info
            && info.size.width > 0
            && info.size.height > 0
            && !info.buffer.is_null()
        {
            let w = info.size.width as u32;
            let h = info.size.height as u32;
            if w <= MAX_CUSTOM_CURSOR_DIMENSION
                && h <= MAX_CUSTOM_CURSOR_DIMENSION
                && let Some(byte_len) = (w as usize)
                    .checked_mul(h as usize)
                    .and_then(|p| p.checked_mul(4))
            {
                // SAFETY: CEF guarantees `custom_cursor_info.buffer` points to
                // `width * height * 4` BGRA bytes for the duration of OnCursorChange.
                let slice =
                    unsafe { std::slice::from_raw_parts(info.buffer as *const u8, byte_len) };
                if let Some(data) = NativeCursorData::from_bgra(
                    w,
                    h,
                    info.hotspot.x,
                    info.hotspot.y,
                    slice.to_vec(),
                ) {
                    return data;
                }
            }
        }
        if !cursor.is_null()
            && let Some(data) = extract_hcursor_data(cursor as isize)
        {
            return data;
        }
        return NativeCursorData::Standard(StandardCursor::Arrow);
    }
    if let Some(standard) = direct_standard_cursor(type_) {
        return NativeCursorData::Standard(standard);
    }

    let raw = type_.get_raw() as i32;
    if let Some(cached) = resource_cursor_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&raw)
        .cloned()
    {
        return cached;
    }
    if !cursor.is_null()
        && let Some(data) = extract_hcursor_data(cursor as isize)
    {
        resource_cursor_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(raw, data.clone());
        return data;
    }
    fallback_resource_cursor(type_)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clashing_extern_declarations)]
    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateIconIndirect(piconinfo: *const IconInfo) -> isize;
        fn DestroyIcon(h_icon: isize) -> i32;
    }

    #[allow(clashing_extern_declarations)]
    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn CreateBitmap(
            n_width: i32,
            n_height: i32,
            n_planes: u32,
            n_bit_count: u32,
            lp_bits: *const c_void,
        ) -> isize;
        fn CreateDIBSection(
            hdc: isize,
            lpbmi: *const BitmapInfoHeader,
            usage: u32,
            ppv_bits: *mut *mut c_void,
            h_section: isize,
            offset: u32,
        ) -> isize;
    }

    struct OwnedTestCursor(isize);
    impl Drop for OwnedTestCursor {
        fn drop(&mut self) {
            if self.0 != 0 {
                let _ = unsafe { DestroyIcon(self.0) };
            }
        }
    }

    #[test]
    fn resolves_custom_cursor_info_bgra_without_hcursor() {
        let mut pixels = vec![
            10u8, 20, 30, 255, 40, 50, 60, 128, 1, 2, 3, 255, 7, 8, 9, 255,
        ];
        let info = CursorInfo {
            hotspot: cef::Point { x: 1, y: 1 },
            image_scale_factor: 1.0,
            buffer: pixels.as_mut_ptr().cast(),
            size: cef::Size {
                width: 2,
                height: 2,
            },
        };
        let resolved = resolve_cef_cursor(std::ptr::null_mut(), CursorType::CUSTOM, Some(&info));
        assert_eq!(
            resolved,
            NativeCursorData::from_bgra(2, 2, 1, 1, pixels).unwrap()
        );
        assert_eq!(
            resolve_cef_cursor(std::ptr::null_mut(), CursorType::NONE, None),
            NativeCursorData::Hidden
        );
    }

    #[test]
    fn extracts_alpha_bgra_and_legacy_color_and_monochrome_hcursors() {
        // 1. 32-bit BGRA alpha cursor
        let expected_bgra = vec![
            10, 20, 30, 255, 40, 50, 60, 128, 70, 80, 90, 255, 0, 0, 0, 0,
        ];
        let bih = BitmapInfoHeader {
            bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            bi_width: 2,
            bi_height: -2,
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: 0,
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let hcolor = OwnedGdiObject(unsafe { CreateDIBSection(0, &bih, 0, &mut bits, 0, 0) });
        assert_ne!(hcolor.0, 0);
        unsafe {
            std::ptr::copy_nonoverlapping(expected_bgra.as_ptr(), bits.cast(), expected_bgra.len());
        }
        let mask_zeros = [0u8; 4];
        let hmask = OwnedGdiObject(unsafe { CreateBitmap(2, 2, 1, 1, mask_zeros.as_ptr().cast()) });
        assert_ne!(hmask.0, 0);
        let hcur = OwnedTestCursor(unsafe {
            CreateIconIndirect(&IconInfo {
                f_icon: 0,
                x_hotspot: 1,
                y_hotspot: 0,
                hbm_mask: hmask.0,
                hbm_color: hcolor.0,
            })
        });
        assert_ne!(hcur.0, 0);
        let extracted = extract_hcursor_data(hcur.0).expect("extract alpha bgra cursor");
        assert_eq!(
            extracted,
            NativeCursorData::from_bgra(2, 2, 1, 0, expected_bgra).unwrap()
        );

        // 2. Legacy 0-alpha color+mask cursor (top row opaque = mask bit 0, bottom row transparent = mask bit 1)
        let mut legacy_bits: *mut c_void = std::ptr::null_mut();
        let hcolor2 =
            OwnedGdiObject(unsafe { CreateDIBSection(0, &bih, 0, &mut legacy_bits, 0, 0) });
        assert_ne!(hcolor2.0, 0);
        let legacy_pixels = vec![11u8, 22, 33, 0, 44, 55, 66, 0, 77, 88, 99, 0, 12, 34, 56, 0];
        unsafe {
            std::ptr::copy_nonoverlapping(
                legacy_pixels.as_ptr(),
                legacy_bits.cast(),
                legacy_pixels.len(),
            );
        }
        // 2x2 word-aligned 1bpp rows: row 0 = 0x0000 (all opaque), row 1 = 0xC000 (both pixels 1 = transparent)
        let legacy_mask: [u8; 4] = [0x00, 0x00, 0xC0, 0x00];
        let hmask2 =
            OwnedGdiObject(unsafe { CreateBitmap(2, 2, 1, 1, legacy_mask.as_ptr().cast()) });
        let hcur2 = OwnedTestCursor(unsafe {
            CreateIconIndirect(&IconInfo {
                f_icon: 0,
                x_hotspot: 0,
                y_hotspot: 1,
                hbm_mask: hmask2.0,
                hbm_color: hcolor2.0,
            })
        });
        assert_ne!(hcur2.0, 0);
        let extracted_legacy = extract_hcursor_data(hcur2.0).expect("extract legacy cursor");
        assert_eq!(
            extracted_legacy,
            NativeCursorData::from_bgra(
                2,
                2,
                0,
                1,
                vec![11, 22, 33, 255, 44, 55, 66, 255, 0, 0, 0, 0, 0, 0, 0, 0],
            )
            .unwrap()
        );

        // 3. Monochrome cursor (2x2 => 2x4 1bpp combined AND + XOR planes)
        let mono_planes: [u8; 8] = [
            0xC0, 0x00, 0x40, 0x00, // AND
            0x80, 0x00, 0x00, 0x00, // XOR
        ];
        let hmono =
            OwnedGdiObject(unsafe { CreateBitmap(2, 4, 1, 1, mono_planes.as_ptr().cast()) });
        let hcur3 = OwnedTestCursor(unsafe {
            CreateIconIndirect(&IconInfo {
                f_icon: 0,
                x_hotspot: 1,
                y_hotspot: 1,
                hbm_mask: hmono.0,
                hbm_color: 0,
            })
        });
        assert_ne!(hcur3.0, 0);
        let extracted_mono = extract_hcursor_data(hcur3.0).expect("extract monochrome cursor");
        assert_eq!(
            extracted_mono,
            NativeCursorData::from_monochrome(
                2,
                2,
                1,
                1,
                vec![0xC0, 0x00, 0x40, 0x00],
                vec![0x80, 0x00, 0x00, 0x00],
            )
            .unwrap()
        );
    }
}
