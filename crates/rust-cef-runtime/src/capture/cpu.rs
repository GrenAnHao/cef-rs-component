//! Synchronous software capture: callback bytes never escape this scope.
use rust_cef_core::{
    frame_budget::{FrameBudget, FrameError},
    frame_layout::{FrameFormat, copy_rows},
    frame_store::frame_len,
    ports::{CaptureFailureKind, EngineError, FrameCapture, FrameInfo, FrameInput, OwnedFrame},
};

use super::frame_error;

pub(crate) struct BgraInput<'a> {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: &'a [u8],
}

impl FrameInput for BgraInput<'_> {
    fn copy_bgra(&self, destination: &mut [u8]) -> Result<(), EngineError> {
        let pitch = self.width.checked_mul(4).ok_or(EngineError::Capture {
            kind: CaptureFailureKind::InvalidDimensions,
            code: None,
        })?;
        if frame_len(self.width, self.height) != Some(self.pixels.len())
            || !copy_rows(
                self.pixels,
                pitch,
                self.width,
                self.height,
                FrameFormat::Bgra8,
                destination,
            )
        {
            return Err(EngineError::Capture {
                kind: CaptureFailureKind::InvalidDimensions,
                code: None,
            });
        }
        Ok(())
    }
}

/// Constructed with the thread identity established by the CEF UI endpoint.
/// The primitive has no native references; endpoint admission additionally
/// checks currently_on(UI). Tests of this pure copy do not initialize CEF.
pub(crate) struct CpuCapture {
    budget: FrameBudget,
    owner_thread: u64,
}

impl CpuCapture {
    pub(crate) fn new(budget: FrameBudget, owner_thread: u64) -> Self {
        Self {
            budget,
            owner_thread,
        }
    }
}

impl FrameCapture for CpuCapture {
    fn capture(
        &mut self,
        info: FrameInfo,
        input: &dyn FrameInput,
    ) -> Result<OwnedFrame, EngineError> {
        if crate::executor::current_thread_id() != self.owner_thread {
            return Err(EngineError::WrongThread);
        }
        let len = frame_len(info.width, info.height)
            .ok_or_else(|| frame_error(FrameError::InvalidDimensions))?;
        let mut output = self.budget.allocate(len).map_err(frame_error)?;
        input.copy_bgra(output.as_mut_slice())?;
        Ok(OwnedFrame {
            info,
            pixels: output.freeze(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::{
        contracts::{BrowserId, RuntimeGeneration},
        mailbox::{FrameMailbox, SurfaceBounds},
        ports::SurfaceKind,
    };
    use std::sync::Arc;

    fn mailbox() -> FrameMailbox {
        let source = FrameMailbox::with_budget(
            BrowserId::new(RuntimeGeneration::new(1), 1),
            FrameBudget::new(32),
        );
        source
            .configure_view(
                SurfaceBounds {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 1,
                },
                2,
                1,
            )
            .unwrap();
        source.take_update();
        source
    }

    #[test]
    fn real_capture_trait_copies_callback_bytes_and_uses_the_browser_ledger() {
        let mailbox = mailbox();
        let budget = mailbox.budget();
        let token = mailbox.capture_token(SurfaceKind::View, 2, 1).unwrap();
        let mut pixels = [1, 2, 3, 127, 4, 5, 6, 255];
        let mut capture = CpuCapture::new(budget.clone(), crate::executor::current_thread_id());
        let frame = FrameCapture::capture(
            &mut capture,
            token.info(),
            &BgraInput {
                width: 2,
                height: 1,
                pixels: &pixels,
            },
        )
        .unwrap();
        pixels.fill(0);
        assert_eq!(budget.stats().used, 8);
        assert_eq!(frame.pixels.as_slice(), &[1, 2, 3, 127, 4, 5, 6, 255]);
        mailbox.publish(frame).unwrap();
        assert_eq!(
            mailbox.latest(SurfaceKind::View).unwrap().info,
            token.info()
        );
    }

    #[test]
    fn partial_failed_input_and_invalid_geometry_do_not_replace_good_frame_or_leak_budget() {
        struct Partial;
        impl FrameInput for Partial {
            fn copy_bgra(&self, output: &mut [u8]) -> Result<(), EngineError> {
                output[..4].copy_from_slice(&[0; 4]);
                Err(EngineError::Capture {
                    kind: CaptureFailureKind::Map,
                    code: Some(-123),
                })
            }
        }
        let mailbox = mailbox();
        let budget = mailbox.budget();
        let mut capture = CpuCapture::new(budget.clone(), crate::executor::current_thread_id());
        let first = mailbox.capture_token(SurfaceKind::View, 2, 1).unwrap();
        mailbox
            .publish(
                capture
                    .capture(
                        first.info(),
                        &BgraInput {
                            width: 2,
                            height: 1,
                            pixels: &[1, 2, 3, 4, 5, 6, 7, 8],
                        },
                    )
                    .unwrap(),
            )
            .unwrap();
        let good = mailbox.latest(SurfaceKind::View).unwrap();
        let next = mailbox.capture_token(SurfaceKind::View, 2, 1).unwrap();
        assert!(matches!(
            capture.capture(next.info(), &Partial),
            Err(EngineError::Capture {
                kind: CaptureFailureKind::Map,
                code: Some(-123)
            })
        ));
        let mut invalid = next.info();
        invalid.width = 0;
        assert!(matches!(
            capture.capture(invalid, &Partial),
            Err(EngineError::Capture {
                kind: CaptureFailureKind::InvalidDimensions,
                ..
            })
        ));
        assert!(Arc::ptr_eq(
            &good,
            &mailbox.latest(SurfaceKind::View).unwrap()
        ));
        assert_eq!(budget.stats().used, 8);
        let reserved = budget.allocate(24).unwrap();
        assert!(matches!(
            capture.capture(next.info(), &Partial),
            Err(EngineError::Backpressure)
        ));
        assert!(Arc::ptr_eq(
            &good,
            &mailbox.latest(SurfaceKind::View).unwrap()
        ));
        assert_eq!(budget.stats().used, 32);
        drop(reserved);
        assert_eq!(budget.stats().used, 8);
    }

    #[test]
    fn wrong_owner_thread_is_refused_before_copy_or_allocation() {
        let mailbox = mailbox();
        let token = mailbox.capture_token(SurfaceKind::View, 2, 1).unwrap();
        let budget = mailbox.budget();
        let mut capture = CpuCapture::new(budget.clone(), crate::executor::current_thread_id());
        std::thread::spawn(move || {
            assert!(matches!(
                capture.capture(
                    token.info(),
                    &BgraInput {
                        width: 2,
                        height: 1,
                        pixels: &[0; 8]
                    }
                ),
                Err(EngineError::WrongThread)
            ));
            assert_eq!(budget.stats().used, 0);
        })
        .join()
        .unwrap();
    }
}
