//! Per-Browser capture endpoint. Native resources never leave the CEF UI
//! callback/retirement sequence; only immutable budget-owned pixels do.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

mod cpu;
mod d3d11;
#[cfg(test)]
mod tests;
#[cfg(feature = "test-support")]
pub(crate) mod timing;

use cpu::{BgraInput, CpuCapture};
use d3d11::{D3d11Capture, D3dMetrics};
use rust_cef_core::{
    contracts::RenderPreference,
    frame_budget::{FrameBudget, FrameError},
    frame_store::frame_len,
    mailbox::FrameMailbox,
    ports::{
        CaptureFailureKind, EngineError, FrameCapture, FrameInfo, FrameInput, FrameSink,
        OwnedFrame, PublishResult, SurfaceKind, UiDispatcher,
    },
};
use serde::Serialize;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureFault {
    pub(crate) kind: CaptureFailureKind,
    pub(crate) code: Option<i32>,
}
impl CaptureFault {
    pub(crate) fn new(kind: CaptureFailureKind) -> Self {
        Self { kind, code: None }
    }
    pub(crate) fn native(kind: CaptureFailureKind, code: i32) -> Self {
        Self {
            kind,
            code: Some(code),
        }
    }
    fn engine(self) -> EngineError {
        EngineError::Capture {
            kind: self.kind,
            code: self.code,
        }
    }
}

pub(crate) fn frame_error(error: FrameError) -> EngineError {
    match error {
        FrameError::BudgetExceeded => EngineError::Backpressure,
        FrameError::Closed => EngineError::Closed,
        FrameError::AllocationFailed => CaptureFault::new(CaptureFailureKind::Allocation).engine(),
        FrameError::InvalidDimensions => {
            CaptureFault::new(CaptureFailureKind::InvalidDimensions).engine()
        }
        _ => CaptureFault::new(CaptureFailureKind::SourceRejected).engine(),
    }
}

/// Pure owned diagnostic data; requested backend is never an observed success.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CaptureDiagnostics {
    pub owner_thread: u64,
    pub retirement_thread: u64,
    pub requested_backend: Option<&'static str>,
    pub observed_backend: Option<&'static str>,
    pub software_captures: u64,
    pub d3d11_captures: u64,
    pub failures: u64,
    pub last_failure: Option<String>,
    pub last_failure_code: Option<i32>,
    pub opens: u64,
    pub copies: u64,
    pub maps: u64,
    pub unmaps: u64,
    pub staging_bytes: u64,
    pub staging_peak: u64,
    pub live_imports: u64,
    pub native_resources_live: bool,
    pub retired: bool,
    pub abandoned: bool,
}

struct State {
    owner: u64,
    backend: Option<RenderPreference>,
    retired: bool,
    in_flight: bool,
    on_idle: Option<Box<dyn FnOnce() + Send + 'static>>,
    gpu: Option<D3d11Capture>,
    diagnostics: CaptureDiagnostics,
    #[cfg(feature = "test-support")]
    view_copy_samples: Option<Vec<timing::ViewCopySample>>,
    #[cfg(test)]
    adapter_test: bool,
}

/// Callback-scoped input. RefCell supplies the trait's synchronous `&self`
/// adapter without allowing this native borrower to become a Send/Sync task.
type CopyAccelerated = fn(
    &mut D3d11Capture,
    SurfaceKind,
    &cef::AcceleratedPaintInfo,
    &mut [u8],
) -> Result<(), CaptureFault>;

struct AcceleratedInput<'a> {
    gpu: std::cell::RefCell<&'a mut D3d11Capture>,
    copy: CopyAccelerated,
    native: &'a cef::AcceleratedPaintInfo,
    surface: SurfaceKind,
    abandoned: &'a AtomicBool,
    _ui: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl FrameInput for AcceleratedInput<'_> {
    fn copy_bgra(&self, destination: &mut [u8]) -> Result<(), EngineError> {
        if self.abandoned.load(Ordering::Acquire) {
            return Err(EngineError::Closed);
        }
        let mut gpu = self
            .gpu
            .try_borrow_mut()
            .map_err(|_| CaptureFault::new(CaptureFailureKind::Busy).engine())?;
        (self.copy)(&mut gpu, self.surface, self.native, destination).map_err(CaptureFault::engine)
    }
}

/// A temporary UI-only checkout. Even an unwinding native adapter error must
/// restore/abandon ownership and keep the native-reference gate conservative.
struct GpuCheckout<'a> {
    endpoint: &'a CaptureEndpoint,
    gpu: Option<D3d11Capture>,
    _ui: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Drop for GpuCheckout<'_> {
    fn drop(&mut self) {
        let Some(gpu) = self.gpu.take() else {
            return;
        };
        let mut state = self.endpoint.lock();
        state.in_flight = false;
        if self.endpoint.abandoned.load(Ordering::Acquire) || state.retired {
            state.retired = true;
            state.diagnostics.abandoned = true;
            let idle = state.on_idle.take();
            drop(state);
            std::mem::forget(gpu);
            drop(idle); // Abandon never schedules another task.
        } else {
            state.gpu = Some(gpu);
            let idle = state.on_idle.take();
            drop(state);
            if let Some(idle) = idle {
                idle();
            }
        }
    }
}

pub(crate) struct CaptureEndpoint {
    budget: FrameBudget,
    state: Mutex<State>,
    native_live: AtomicBool,
    abandoned: AtomicBool,
    #[cfg(feature = "test-support")]
    view_copy_timing_active: AtomicBool,
}
impl CaptureEndpoint {
    pub(crate) fn new(budget: FrameBudget) -> Self {
        Self {
            budget,
            state: Mutex::new(State {
                owner: 0,
                backend: None,
                retired: false,
                in_flight: false,
                on_idle: None,
                gpu: None,
                diagnostics: CaptureDiagnostics::default(),
                #[cfg(feature = "test-support")]
                view_copy_samples: None,
                #[cfg(test)]
                adapter_test: false,
            }),
            native_live: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            view_copy_timing_active: AtomicBool::new(false),
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn check(&self, state: &State, backend: RenderPreference) -> Result<(), EngineError> {
        if self.abandoned.load(Ordering::Acquire) {
            return Err(EngineError::Closed);
        }
        if state.retired {
            return Err(EngineError::Closed);
        }
        if state.owner == 0 {
            return Err(EngineError::NotReady);
        }
        if state.owner != crate::executor::current_thread_id() {
            return Err(EngineError::WrongThread);
        }
        #[cfg(test)]
        let test = state.adapter_test;
        #[cfg(not(test))]
        let test = false;
        if !test && cef::currently_on(cef::ThreadId::UI) == 0 {
            return Err(EngineError::WrongThread);
        }
        if state.backend != Some(backend) {
            return Err(CaptureFault::new(CaptureFailureKind::WrongBackend).engine());
        }
        Ok(())
    }
    pub(crate) fn prepare(&self, backend: RenderPreference) -> Result<(), EngineError> {
        self.prepare_with(backend, D3d11Capture::new)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn prepare_device_failure_probe(
        &self,
        backend: RenderPreference,
    ) -> Result<(), EngineError> {
        if backend != RenderPreference::D3d11Required {
            return Err(CaptureFault::new(CaptureFailureKind::WrongBackend).engine());
        }
        self.prepare_with(backend, |owner| {
            D3d11Capture::new_with_fault(owner, Some(d3d11::D3dFaultPoint::DeviceCreation))
        })
    }

    fn prepare_with(
        &self,
        backend: RenderPreference,
        create_device: impl FnOnce(u64) -> Result<D3d11Capture, CaptureFault>,
    ) -> Result<(), EngineError> {
        if cef::currently_on(cef::ThreadId::UI) == 0 {
            return Err(EngineError::WrongThread);
        }
        if self.abandoned.load(Ordering::Acquire) {
            return Err(EngineError::Closed);
        }
        let owner = crate::executor::current_thread_id();
        {
            let mut state = self.lock();
            if state.retired {
                return Err(EngineError::Closed);
            }
            if state.owner != 0 {
                return Err(EngineError::InvalidRequest);
            }
            state.owner = owner;
            state.diagnostics.owner_thread = owner;
            state.backend = Some(backend);
            state.diagnostics.requested_backend = Some(match backend {
                RenderPreference::SoftwareRequired => "Software",
                RenderPreference::D3d11Required => "D3d11",
                RenderPreference::Auto => "Auto",
            });
        }
        let gpu = match backend {
            RenderPreference::SoftwareRequired => None,
            RenderPreference::D3d11Required => match create_device(owner) {
                Ok(gpu) => Some(gpu),
                Err(fault) => {
                    self.record_failure(&fault.engine());
                    return Err(fault.engine());
                }
            },
            RenderPreference::Auto => {
                return Err(CaptureFault::new(CaptureFailureKind::WrongBackend).engine());
            }
        };
        self.commit_gpu(gpu)
    }
    fn commit_gpu(&self, gpu: Option<D3d11Capture>) -> Result<(), EngineError> {
        // Native construction/drop is outside the state lock. Preparation is a
        // single CEF UI task, but main-thread abandon can interleave here.
        let mut state = self.lock();
        if state.retired || self.abandoned.load(Ordering::Acquire) {
            // Main-thread abandon can race in-flight D3D construction. Commit
            // neither a ready endpoint nor a subsequent native Browser create.
            if let Some(gpu) = gpu {
                self.native_live.store(true, Ordering::Release);
                std::mem::forget(gpu);
            }
            state.retired = true;
            state.diagnostics.abandoned = true;
            return Err(EngineError::Closed);
        }
        self.native_live.store(gpu.is_some(), Ordering::Release);
        state.gpu = gpu;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn prepare_adapter_test(&self) {
        let mut state = self.lock();
        state.owner = crate::executor::current_thread_id();
        state.backend = Some(RenderPreference::SoftwareRequired);
        state.adapter_test = true;
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn arm_view_copy_timing(&self) -> Result<(), EngineError> {
        let samples = Vec::with_capacity(timing::VIEW_COPY_SAMPLE_CAPACITY);
        let mut state = self.lock();
        if state.retired || self.is_abandoned() {
            return Err(EngineError::Closed);
        }
        if state.owner == 0 || state.view_copy_samples.is_some() {
            return Err(EngineError::InvalidRequest);
        }
        state.view_copy_samples = Some(samples);
        self.view_copy_timing_active.store(true, Ordering::Release);
        Ok(())
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn view_copy_samples(&self) -> Option<Vec<timing::ViewCopySample>> {
        self.lock().view_copy_samples.clone()
    }

    fn capture_cpu(
        &self,
        info: FrameInfo,
        owner: u64,
        input: &dyn FrameInput,
        #[cfg(feature = "test-support")] readback_adapter_luid: Option<(i32, u32)>,
    ) -> Result<OwnedFrame, EngineError> {
        let mut capture = CpuCapture::new(self.budget.clone(), owner);
        #[cfg(feature = "test-support")]
        if info.surface == SurfaceKind::View && self.view_copy_timing_active.load(Ordering::Acquire)
        {
            let timed = timing::TimedCopy {
                input,
                measurement: std::cell::Cell::new(None),
            };
            // No instrumentation lock spans allocation or the native copy.
            let frame = capture.capture(info, &timed)?;
            if let Some(measurement) = timed.measurement.get() {
                let mut state = self.lock();
                if !state.retired
                    && !self.is_abandoned()
                    && let Some(samples) = state.view_copy_samples.as_mut()
                    && samples.len() < timing::VIEW_COPY_SAMPLE_CAPACITY
                {
                    samples.push(timing::ViewCopySample::new(
                        info,
                        owner,
                        measurement,
                        readback_adapter_luid,
                    ));
                    if samples.len() == timing::VIEW_COPY_SAMPLE_CAPACITY {
                        self.view_copy_timing_active.store(false, Ordering::Release);
                    }
                }
            }
            return Ok(frame);
        }
        capture.capture(info, input)
    }

    pub(crate) fn software(
        &self,
        info: FrameInfo,
        pixels: &[u8],
    ) -> Result<OwnedFrame, EngineError> {
        let owner = {
            let state = self.lock();
            self.check(&state, RenderPreference::SoftwareRequired)?;
            state.owner
        };
        let frame = self.capture_cpu(
            info,
            owner,
            &BgraInput {
                width: info.width,
                height: info.height,
                pixels,
            },
            #[cfg(feature = "test-support")]
            None,
        )?;
        let mut state = self.lock();
        state.diagnostics.software_captures += 1;
        state.diagnostics.observed_backend = Some("Software");
        Ok(frame)
    }
    pub(crate) fn accelerated(
        &self,
        info: FrameInfo,
        input: &cef::AcceleratedPaintInfo,
    ) -> Result<OwnedFrame, EngineError> {
        self.accelerated_with(info, input, D3d11Capture::copy)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn accelerated_after_map_probe(
        &self,
        info: FrameInfo,
        input: &cef::AcceleratedPaintInfo,
    ) -> Result<OwnedFrame, EngineError> {
        self.accelerated_with(info, input, D3d11Capture::copy_after_map_probe)
    }

    fn accelerated_with(
        &self,
        info: FrameInfo,
        input: &cef::AcceleratedPaintInfo,
        copy: CopyAccelerated,
    ) -> Result<OwnedFrame, EngineError> {
        {
            self.check(&self.lock(), RenderPreference::D3d11Required)?;
        }
        validate_accelerated(input)?;
        if input.extra.visible_rect.width != info.width as i32
            || input.extra.visible_rect.height != info.height as i32
        {
            return Err(CaptureFault::new(CaptureFailureKind::InvalidDimensions).engine());
        }
        let gpu = {
            let mut state = self.lock();
            self.check(&state, RenderPreference::D3d11Required)?;
            if state.in_flight {
                return Err(CaptureFault::new(CaptureFailureKind::Busy).engine());
            }
            let gpu = state.gpu.take().ok_or(EngineError::NotReady)?;
            state.in_flight = true;
            gpu
        };
        // UI serialization, not a mutex held across a driver wait. native_live
        // remains true while checked out so neither retire nor the barrier can
        // mistake a temporary empty Option for resource retirement.
        let mut checkout = GpuCheckout {
            endpoint: self,
            gpu: Some(gpu),
            _ui: std::marker::PhantomData,
        };
        let gpu = checkout.gpu.as_mut().expect("one GPU per UI checkout");
        // The same real FrameCapture builder pre-reserves owned CPU output for
        // both inputs, outside every endpoint/registry/budget lock and before
        // Open/Copy/Map. There is no extra readback/swap scratch allocation.
        #[cfg(feature = "test-support")]
        let readback_adapter_luid = gpu.readback_adapter_luid();
        let result = {
            let input = AcceleratedInput {
                gpu: std::cell::RefCell::new(&mut *gpu),
                copy,
                native: input,
                surface: info.surface,
                abandoned: &self.abandoned,
                _ui: std::marker::PhantomData,
            };
            self.capture_cpu(
                info,
                crate::executor::current_thread_id(),
                &input,
                #[cfg(feature = "test-support")]
                readback_adapter_luid,
            )
        };
        let metrics = gpu.metrics();
        update_metrics(&mut self.lock().diagnostics, metrics);
        drop(checkout);
        if self.is_abandoned() {
            return Err(EngineError::Closed);
        }
        let frame = result?;
        let mut state = self.lock();
        state.diagnostics.d3d11_captures += 1;
        state.diagnostics.observed_backend = Some("D3d11");
        drop(state);
        Ok(frame)
    }
    pub(crate) fn record_failure(&self, error: &EngineError) {
        let mut state = self.lock();
        state.diagnostics.failures += 1;
        match error {
            EngineError::Capture { kind, code } => {
                state.diagnostics.last_failure = Some(format!("{kind:?}"));
                state.diagnostics.last_failure_code = *code;
            }
            _ => {
                state.diagnostics.last_failure = Some(format!("{error:?}"));
                state.diagnostics.last_failure_code = None;
            }
        }
    }
    pub(crate) fn diagnostics(&self) -> CaptureDiagnostics {
        let mut diagnostics = self.lock().diagnostics.clone();
        diagnostics.native_resources_live = self.native_live.load(Ordering::Acquire);
        diagnostics.abandoned |= self.abandoned.load(Ordering::Acquire);
        diagnostics
    }
    pub(crate) fn native_refs_clear(&self) -> bool {
        !self.native_live.load(Ordering::Acquire)
    }
    pub(crate) fn retire(&self) -> Result<(), EngineError> {
        let gpu = {
            let mut state = self.lock();
            if self.abandoned.load(Ordering::Acquire) {
                return Err(EngineError::Closed);
            }
            if state.in_flight {
                return Err(CaptureFault::new(CaptureFailureKind::Busy).engine());
            }
            if state.retired {
                return Ok(());
            }
            // No native object exists for an unprepared/software endpoint.
            if state.gpu.is_some() {
                self.check(&state, RenderPreference::D3d11Required)?;
            }
            state.retired = true;
            state.diagnostics.retired = true;
            state.diagnostics.retirement_thread = crate::executor::current_thread_id();
            state.gpu.take()
        };
        drop(gpu); // Context/device/staging released synchronously on CEF UI.
        self.native_live.store(false, Ordering::Release);
        self.lock().diagnostics.staging_bytes = 0;
        Ok(())
    }
    /// One coalesced pure-data continuation for a reentrant close. It only
    /// posts tracked UI work after the native checkout has ended.
    pub(crate) fn when_idle(&self, callback: impl FnOnce() + Send + 'static) {
        let callback: Box<dyn FnOnce() + Send> = Box::new(callback);
        let mut state = self.lock();
        if self.is_abandoned() {
            return;
        }
        if state.in_flight {
            if state.on_idle.is_none() {
                state.on_idle = Some(callback);
            }
        } else {
            drop(state);
            callback();
        }
    }
    pub(crate) fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }
    pub(crate) fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
        let mut state = self.lock();
        state.retired = true;
        state.diagnostics.abandoned = true;
        if let Some(gpu) = state.gpu.take() {
            std::mem::forget(gpu);
        }
        // The in-flight UI checkout sees the atomic flag before returning its
        // GPU; an empty Option is never permission to clear native_live.
    }
}
impl Drop for CaptureEndpoint {
    fn drop(&mut self) {
        // A native endpoint must have been explicitly retired. Even an owner
        // thread Drop is not permission to release an unfinished CEF cohort.
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(gpu) = state.gpu.take() {
            std::mem::forget(gpu);
        }
    }
}
fn update_metrics(d: &mut CaptureDiagnostics, m: D3dMetrics) {
    d.opens = m.opens;
    d.copies = m.copies;
    d.maps = m.maps;
    d.unmaps = m.unmaps;
    d.staging_bytes = m.staging_bytes;
    d.staging_peak = m.staging_peak;
    d.live_imports = m.live_imports;
}

/// The callback witness and the D3D11 import path use the same classification.
/// A bounded content rectangle smaller than the visible aperture is refused
/// as incomplete; malformed geometry is not evidence of letterboxing.
fn accelerated_frame_bytes(extra: &cef::AcceleratedPaintInfoCommon) -> Result<usize, CaptureFault> {
    let v = &extra.visible_rect;
    let c = &extra.content_rect;
    let end = |origin: i32, length: i32| i64::from(origin) + i64::from(length);
    let right = end(v.x, v.width);
    let bottom = end(v.y, v.height);
    let invalid = || CaptureFault::new(CaptureFailureKind::InvalidDimensions);
    if v.x < 0
        || v.y < 0
        || v.width <= 0
        || v.height <= 0
        || right > i64::from(extra.coded_size.width)
        || bottom > i64::from(extra.coded_size.height)
        || c.x < v.x
        || c.y < v.y
        || c.width <= 0
        || c.height <= 0
        || end(c.x, c.width) > right
        || end(c.y, c.height) > bottom
    {
        return Err(invalid());
    }
    let bytes = frame_len(v.width as u32, v.height as u32).ok_or_else(invalid)?;
    if (v.x, v.y, v.width, v.height) != (c.x, c.y, c.width, c.height) {
        return Err(CaptureFault::new(CaptureFailureKind::IncompleteFrame));
    }
    Ok(bytes)
}

pub(crate) fn validate_accelerated(info: &cef::AcceleratedPaintInfo) -> Result<(), EngineError> {
    use rust_cef_core::frame_layout::FrameFormat;
    if info.shared_texture_handle.is_null() {
        return Err(CaptureFault::new(CaptureFailureKind::InvalidHandle).engine());
    }
    if FrameFormat::from_cef_raw(info.format.get_raw()).is_none() {
        return Err(CaptureFault::new(CaptureFailureKind::UnsupportedFormat).engine());
    }
    accelerated_frame_bytes(&info.extra)
        .map(|_| ())
        .map_err(CaptureFault::engine)
}

/// Actual FrameSink adapter: authoritative latest slots and wake tickets are
/// committed before the optional host observation. There is no video queue.
pub(crate) struct CaptureFrameSink {
    pub(crate) mailbox: Arc<FrameMailbox>,
    pub(crate) observer: Arc<dyn FrameSink>,
    pub(crate) ui: Arc<dyn UiDispatcher>,
    pub(crate) recovery: Option<Arc<crate::resize::ResizeState>>,
}
impl CaptureFrameSink {
    pub(crate) fn publish_checked(&self, frame: OwnedFrame) -> Result<PublishResult, FrameError> {
        let publication = self.mailbox.publish(frame)?;
        // Ack only the successfully committed current VIEW before a host wake
        // can synchronously submit another viewport/close.
        if let Some(recovery) = &self.recovery {
            recovery.frame_received(publication.frame.info);
        }
        let result = if publication.wake.is_some() {
            PublishResult::Published
        } else {
            PublishResult::Coalesced
        };
        crate::registry::wake_frames(&self.mailbox, &self.ui, publication.wake);
        let _ = self.observer.publish((*publication.frame).clone());
        Ok(result)
    }
}
impl FrameSink for CaptureFrameSink {
    fn publish(&self, frame: OwnedFrame) -> PublishResult {
        self.publish_checked(frame)
            .unwrap_or(PublishResult::Rejected)
    }
}
