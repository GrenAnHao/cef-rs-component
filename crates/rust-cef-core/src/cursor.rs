//! Platform-independent owned cursor payloads and coalescing cursor slot.
//!
//! `NativeCursorData` carries standard system cursor kinds, an explicit hidden
//! cursor (`CT_NONE`), or bounded CPU-owned BGRA / monochrome cursor bitmaps
//! copied synchronously inside CEF's `OnCursorChange` callback before any
//! native `HCURSOR` or CEF pixel buffer expires.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub const MAX_CUSTOM_CURSOR_DIMENSION: u32 = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StandardCursor {
    #[default]
    Arrow,
    IBeam,
    Wait,
    Cross,
    SizeNwse,
    SizeNesw,
    SizeWe,
    SizeNs,
    SizeAll,
    No,
    Hand,
    AppStarting,
    Help,
}

#[derive(Clone, PartialEq, Eq)]
pub enum NativeCursorData {
    Standard(StandardCursor),
    Hidden,
    BgraBitmap {
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        pixels: Arc<[u8]>,
    },
    MonochromeMask {
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        and_mask: Arc<[u8]>,
        xor_mask: Arc<[u8]>,
    },
}

impl Default for NativeCursorData {
    fn default() -> Self {
        Self::Standard(StandardCursor::Arrow)
    }
}

impl fmt::Debug for NativeCursorData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Standard(kind) => formatter.debug_tuple("Standard").field(kind).finish(),
            Self::Hidden => formatter.write_str("Hidden"),
            Self::BgraBitmap {
                width,
                height,
                hotspot_x,
                hotspot_y,
                pixels,
            } => formatter
                .debug_struct("BgraBitmap")
                .field("width", width)
                .field("height", height)
                .field("hotspot_x", hotspot_x)
                .field("hotspot_y", hotspot_y)
                .field("byte_len", &pixels.len())
                .finish(),
            Self::MonochromeMask {
                width,
                height,
                hotspot_x,
                hotspot_y,
                and_mask,
                xor_mask,
            } => formatter
                .debug_struct("MonochromeMask")
                .field("width", width)
                .field("height", height)
                .field("hotspot_x", hotspot_x)
                .field("hotspot_y", hotspot_y)
                .field("and_bytes", &and_mask.len())
                .field("xor_bytes", &xor_mask.len())
                .finish(),
        }
    }
}

impl NativeCursorData {
    pub fn from_bgra(
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        pixels: Vec<u8>,
    ) -> Option<Self> {
        if !(1..=MAX_CUSTOM_CURSOR_DIMENSION).contains(&width)
            || !(1..=MAX_CUSTOM_CURSOR_DIMENSION).contains(&height)
        {
            return None;
        }
        let expected = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if pixels.len() != expected {
            return None;
        }
        Some(Self::BgraBitmap {
            width,
            height,
            hotspot_x: hotspot_x.clamp(0, width.saturating_sub(1) as i32),
            hotspot_y: hotspot_y.clamp(0, height.saturating_sub(1) as i32),
            pixels: Arc::from(pixels),
        })
    }

    pub fn from_monochrome(
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        and_mask: Vec<u8>,
        xor_mask: Vec<u8>,
    ) -> Option<Self> {
        if !(1..=MAX_CUSTOM_CURSOR_DIMENSION).contains(&width)
            || !(1..=MAX_CUSTOM_CURSOR_DIMENSION).contains(&height)
        {
            return None;
        }
        // Win32 CreateBitmap 1-bit masks require word-aligned (16-pixel / 2-byte) row stride.
        let stride = ((width as usize + 15) / 16) * 2;
        let expected = stride.checked_mul(height as usize)?;
        if and_mask.len() != expected || xor_mask.len() != expected {
            return None;
        }
        Some(Self::MonochromeMask {
            width,
            height,
            hotspot_x: hotspot_x.clamp(0, width.saturating_sub(1) as i32),
            hotspot_y: hotspot_y.clamp(0, height.saturating_sub(1) as i32),
            and_mask: Arc::from(and_mask),
            xor_mask: Arc::from(xor_mask),
        })
    }
}

/// Thread-safe latest-cursor slot shared between CEF UI callbacks and the GPUI view.
#[derive(Clone, Debug, Default)]
pub struct CursorSlot {
    inner: Arc<Mutex<(u64, NativeCursorData)>>,
}

impl CursorSlot {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, (u64, NativeCursorData)> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set(&self, cursor: NativeCursorData) -> bool {
        let mut guard = self.lock();
        if guard.1 == cursor {
            return false;
        }
        guard.0 = guard.0.saturating_add(1).max(1);
        guard.1 = cursor;
        true
    }

    pub fn snapshot(&self) -> (u64, NativeCursorData) {
        self.lock().clone()
    }

    pub fn clear(&self) {
        let _ = self.set(NativeCursorData::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_validation_rejects_bad_dimensions_or_lengths_and_clamps_hotspot() {
        assert!(NativeCursorData::from_bgra(0, 4, 0, 0, vec![]).is_none());
        assert!(NativeCursorData::from_bgra(4, 0, 0, 0, vec![]).is_none());
        assert!(
            NativeCursorData::from_bgra(
                MAX_CUSTOM_CURSOR_DIMENSION + 1,
                1,
                0,
                0,
                vec![0; (MAX_CUSTOM_CURSOR_DIMENSION as usize + 1) * 4],
            )
            .is_none()
        );
        assert!(NativeCursorData::from_bgra(2, 2, 0, 0, vec![0; 12]).is_none());

        let valid =
            NativeCursorData::from_bgra(4, 3, -5, 99, vec![0xAA; 4 * 3 * 4]).expect("valid bgra");
        assert_eq!(
            valid,
            NativeCursorData::BgraBitmap {
                width: 4,
                height: 3,
                hotspot_x: 0,
                hotspot_y: 2,
                pixels: Arc::from(vec![0xAA; 48]),
            }
        );
        let dbg = format!("{valid:?}");
        assert!(dbg.contains("byte_len: 48"));
        assert!(!dbg.contains("170"));
    }

    #[test]
    fn monochrome_validation_checks_word_stride_and_clamps_hotspot() {
        // 1x2 cursor -> 16-bit aligned stride is 2 bytes per row => 4 bytes per plane.
        assert!(NativeCursorData::from_monochrome(1, 2, 0, 0, vec![0; 2], vec![0; 4]).is_none());
        assert!(NativeCursorData::from_monochrome(0, 2, 0, 0, vec![], vec![]).is_none());

        let valid = NativeCursorData::from_monochrome(1, 2, 9, -2, vec![0xFF; 4], vec![0x00; 4])
            .expect("valid monochrome");
        assert_eq!(
            valid,
            NativeCursorData::MonochromeMask {
                width: 1,
                height: 2,
                hotspot_x: 0,
                hotspot_y: 0,
                and_mask: Arc::from(vec![0xFF; 4]),
                xor_mask: Arc::from(vec![0x00; 4]),
            }
        );
        let dbg = format!("{valid:?}");
        assert!(dbg.contains("and_bytes: 4"));
        assert!(dbg.contains("xor_bytes: 4"));
        assert!(!dbg.contains("255"));
    }

    #[test]
    fn cursor_slot_deduplicates_and_advances_revision_monotonically() {
        let slot = CursorSlot::new();
        assert_eq!(
            slot.snapshot(),
            (0, NativeCursorData::Standard(StandardCursor::Arrow))
        );
        assert!(!slot.set(NativeCursorData::Standard(StandardCursor::Arrow)));
        assert_eq!(slot.snapshot().0, 0);

        assert!(slot.set(NativeCursorData::Hidden));
        assert_eq!(slot.snapshot(), (1, NativeCursorData::Hidden));
        assert!(!slot.set(NativeCursorData::Hidden));
        assert_eq!(slot.snapshot().0, 1);

        let bmp = NativeCursorData::from_bgra(1, 1, 0, 0, vec![1, 2, 3, 255]).unwrap();
        assert!(slot.set(bmp.clone()));
        assert_eq!(slot.snapshot(), (2, bmp));

        slot.clear();
        assert_eq!(
            slot.snapshot(),
            (3, NativeCursorData::Standard(StandardCursor::Arrow))
        );
    }
}
