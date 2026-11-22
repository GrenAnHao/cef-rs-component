//! Opt-in validation only. The clock brackets the real FrameInput copy,
//! not allocation, mailbox publication, presentation, or hardware GPU time.
use rust_cef_core::ports::{EngineError, FrameInfo, FrameInput};
use serde::Serialize;
use std::{cell::Cell, time::Instant};

pub const VIEW_COPY_SAMPLE_CAPACITY: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ViewCopySample {
    pub browser: (u64, u64),
    pub generation: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub ui_thread: u64,
    pub copy_ns: u64,
    /// DXGI LUID of this native receive device's adapter, not of the CEF
    /// producer or GPUI swapchain. Software and failed queries have no LUID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readback_adapter_luid: Option<(i32, u32)>,
    /// Process-local monotonic completion stamp; never serialized.
    #[serde(skip)]
    pub copy_finished_at: Instant,
}

impl ViewCopySample {
    pub(super) fn new(
        info: FrameInfo,
        ui_thread: u64,
        measurement: (u64, Instant),
        readback_adapter_luid: Option<(i32, u32)>,
    ) -> Self {
        let (copy_ns, copy_finished_at) = measurement;
        Self {
            browser: (info.browser.runtime().value(), info.browser.local()),
            generation: info.surface_generation,
            sequence: info.sequence,
            width: info.width,
            height: info.height,
            ui_thread,
            copy_ns,
            readback_adapter_luid,
            copy_finished_at,
        }
    }
}

pub(super) struct TimedCopy<'a> {
    pub input: &'a dyn FrameInput,
    pub measurement: Cell<Option<(u64, Instant)>>,
}

impl FrameInput for TimedCopy<'_> {
    fn copy_bgra(&self, destination: &mut [u8]) -> Result<(), EngineError> {
        let started = Instant::now();
        self.input.copy_bgra(destination)?;
        let completed = Instant::now();
        self.measurement.set(
            completed
                .checked_duration_since(started)
                .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok())
                .map(|ns| (ns, completed)),
        );
        Ok(())
    }
}
