//! Legacy single-VIEW frame facade over the shared mailbox.
//!
//! The demo's `src/browser/frame_store.rs` re-exports this type, so old call
//! sites keep one message shape while the storage, budget and validation are
//! the same code the runtime uses. This is a compatibility VIEW facade only:
//! its sentinel Browser identity cannot masquerade as a real Runtime browser,
//! because the Runtime never reads this store.
//!
//! `publish_owned(Vec<u8>)` is gone. Accelerated capture reserves a budgeted
//! scratch with [`FrameStore::allocate`] and transfers it with
//! [`FrameStore::publish_buffer`] without another pixel copy.

use std::sync::Arc;

use crate::{
    contracts::{BrowserId, RuntimeGeneration},
    frame_budget::{BudgetedPixels, FrameBudget, FrameBuffer, FrameError},
    mailbox::{FrameMailbox, SurfaceBounds},
    ports::{OwnedFrame, SurfaceKind},
};

/// A practical per-frame cap; the total per-Browser budget is also enforced by
/// the shared ledger behind every `FrameStore`.
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

pub fn frame_len(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 {
        return None;
    }
    let bytes = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    (bytes <= MAX_FRAME_BYTES).then_some(bytes)
}

#[derive(Clone, Debug)]
pub struct BrowserFrame {
    pub width: u32,
    pub height: u32,
    /// CEF supplies premultiplied BGRA. GPUI RenderImage also expects BGRA.
    ///
    /// The allocation is still owned by the store's CPU budget: consumers can
    /// read it or make a charged copy, but cannot detach the underlying `Vec`.
    pub bgra: Arc<BudgetedPixels>,
    pub sequence: u64,
}

pub struct FrameStore {
    budget: FrameBudget,
    mailbox: FrameMailbox,
}

impl Default for FrameStore {
    fn default() -> Self {
        Self::with_budget(FrameBudget::default())
    }
}

impl FrameStore {
    /// The sentinel identity keeps this facade from colliding with a live
    /// Runtime browser; the Runtime never consumes this store.
    fn sentinel() -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(0), 0)
    }

    pub fn with_budget(budget: FrameBudget) -> Self {
        Self {
            mailbox: FrameMailbox::with_budget(Self::sentinel(), budget.clone()),
            budget,
        }
    }

    pub fn budget(&self) -> FrameBudget {
        self.budget.clone()
    }

    /// Reserves CPU scratch (D3D11 read-back) from the same total budget. The
    /// caller fills it and transfers ownership with `publish_buffer`.
    pub fn allocate(&self, width: u32, height: u32) -> Result<FrameBuffer, FrameError> {
        let len = frame_len(width, height).ok_or(FrameError::InvalidDimensions)?;
        self.budget.allocate(len)
    }

    /// Copies callback-owned bytes. Only the latest frame is retained.
    /// Invalid frames and poisoned locks are rejected without panicking in FFI.
    pub fn publish_bgra(&self, width: u32, height: u32, bytes: &[u8]) -> bool {
        if frame_len(width, height) != Some(bytes.len()) {
            return false;
        }
        self.configure_view(width, height).is_ok()
            && self
                .mailbox
                .capture_token(SurfaceKind::View, width, height)
                .ok()
                .and_then(|token| self.mailbox.copy_frame(&token, bytes).ok())
                .is_some_and(|frame| self.mailbox.publish(frame).is_ok())
    }

    /// Transfers an already-filled budgeted buffer, which is how the
    /// accelerated path avoids an extra copy.
    pub fn publish_buffer(&self, width: u32, height: u32, buffer: FrameBuffer) -> bool {
        if frame_len(width, height) != Some(buffer.len()) {
            return false;
        }
        if self.configure_view(width, height).is_err() {
            return false;
        }
        let Ok(token) = self.mailbox.capture_token(SurfaceKind::View, width, height) else {
            return false;
        };
        let frame = OwnedFrame::from_cpu_pixels(token.info(), buffer.freeze());
        self.mailbox.publish(frame).is_ok()
    }

    /// The legacy API has no separate resize surface. Updating the VIEW target
    /// to the published frame keeps its single-size behaviour; the retained
    /// snapshot is untouched until `publish` stores the new frame.
    fn configure_view(&self, width: u32, height: u32) -> Result<(), FrameError> {
        let bounds = SurfaceBounds {
            x: 0,
            y: 0,
            width: i32::try_from(width).map_err(|_| FrameError::InvalidDimensions)?,
            height: i32::try_from(height).map_err(|_| FrameError::InvalidDimensions)?,
        };
        self.mailbox
            .configure_view(bounds, width, height)
            .map(|_| ())
    }

    pub fn latest(&self) -> Option<BrowserFrame> {
        let frame = self.mailbox.latest(SurfaceKind::View)?;
        let bgra = frame.cpu_pixels()?.clone();
        Some(BrowserFrame {
            width: frame.info.width,
            height: frame.info.height,
            bgra,
            sequence: frame.info.sequence,
        })
    }

    /// Cheap fingerprint of the current frame: size plus a few sampled pixels.
    /// Retained for legacy software/accelerated sample comparisons. Sparse
    /// equality alone does not prove full-image, aligned-channel or alpha equality.
    pub fn fingerprint(&self) -> Option<(u32, u32, [u8; 12])> {
        let frame = self.latest()?;
        Some((
            frame.width,
            frame.height,
            fingerprint_bytes(frame.bgra.as_slice())?,
        ))
    }

    pub fn sequence(&self) -> u64 {
        self.latest().map_or(0, |frame| frame.sequence)
    }
}

/// Original byte-sample fingerprint, preserved for compatibility. `len/3`
/// need not be pixel-aligned, so this is not an alpha/full-image validator.
pub fn fingerprint_bytes(bytes: &[u8]) -> Option<[u8; 12]> {
    if bytes.len() < 4 {
        return None;
    }
    let mut sample = [0u8; 12];
    let picks = [0usize, bytes.len() / 3, bytes.len() / 2, bytes.len() - 4];
    for (slot, index) in picks.iter().enumerate() {
        let base = (*index).min(bytes.len().saturating_sub(4));
        sample[slot * 3..slot * 3 + 3].copy_from_slice(&bytes[base..base + 3]);
    }
    Some(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_empty() {
        let store = FrameStore::default();
        assert!(store.latest().is_none());
        assert_eq!(store.sequence(), 0);
    }

    #[test]
    fn copies_buffer_and_preserves_bgra() {
        let store = FrameStore::default();
        let mut pixels = [0_u8, 0, 255, 255, 255, 0, 0, 255];
        assert!(store.publish_bgra(2, 1, &pixels));
        pixels.fill(0);
        let frame = store.latest().unwrap();
        assert_eq!((frame.width, frame.height, frame.sequence), (2, 1, 1));
        assert_eq!(frame.bgra.as_slice(), &[0, 0, 255, 255, 255, 0, 0, 255]);
        // Legacy len/3=2 deliberately lands inside a pixel. Preserve all
        // twelve bytes, but do not mistake them for aligned RGBA samples.
        assert_eq!(
            store.fingerprint(),
            Some((2, 1, [0, 0, 255, 255, 255, 255, 255, 0, 0, 255, 0, 0]))
        );
        assert_eq!(fingerprint_bytes(&[1, 2, 3]), None);
    }

    #[test]
    fn replaces_latest_without_mutating_existing_snapshot() {
        let store = FrameStore::default();
        assert!(store.publish_bgra(1, 1, &[1, 2, 3, 255]));
        let old = store.latest().unwrap();
        assert!(store.publish_bgra(1, 1, &[4, 5, 6, 255]));
        assert_eq!(store.sequence(), 2);
        assert_eq!(old.bgra.as_slice(), &[1, 2, 3, 255]);
        assert_eq!(store.latest().unwrap().bgra.as_slice(), &[4, 5, 6, 255]);
    }

    #[test]
    fn rejects_zero_wrong_lengths_and_huge_dimensions() {
        let store = FrameStore::default();
        assert!(!store.publish_bgra(0, 1, &[]));
        assert!(!store.publish_bgra(1, 0, &[]));
        assert!(!store.publish_bgra(1, 1, &[0; 3]));
        assert!(!store.publish_bgra(1, 1, &[0; 5]));
        assert!(!store.publish_bgra(u32::MAX, u32::MAX, &[]));
        assert!(store.latest().is_none());
    }

    #[test]
    fn invalid_frame_does_not_replace_good_frame() {
        let store = FrameStore::default();
        assert!(store.publish_bgra(1, 1, &[1, 2, 3, 4]));
        assert!(!store.publish_bgra(2, 1, &[0; 4]));
        assert_eq!(store.sequence(), 1);
        assert_eq!(store.latest().unwrap().bgra.as_slice(), &[1, 2, 3, 4]);
    }

    #[test]
    fn producer_and_consumer_share_owned_frames() {
        let store = Arc::new(FrameStore::default());
        let producer = store.clone();
        std::thread::spawn(move || {
            for i in 0..100_u8 {
                assert!(producer.publish_bgra(1, 1, &[i, 0, 0, 255]));
            }
        })
        .join()
        .unwrap();
        assert_eq!(store.sequence(), 100);
        assert_eq!(store.latest().unwrap().bgra.as_slice()[0], 99);
    }
}
