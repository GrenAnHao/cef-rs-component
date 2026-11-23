//! Per-Browser latest-frame mailbox with independent VIEW/POPUP surfaces.
//!
//! Each CEF client owns exactly one mailbox. All callbacks publish into it and
//! every host query reads the same instance; there is no global registry and no
//! producer-native handle is exposed. The mailbox retains only the newest frame
//! per surface. It never queues video frames.
//!
//! The ledger behind [`FrameMailbox::budget`] covers both latest slots, every
//! in-flight capture copy, presentation copies and capture scratch. A single
//! frame-size check cannot substitute for that total.
//!
//! Wakes are merged, not queued: `dirty` records "a host update is pending" and
//! `wake_armed` records "an irreversible wake ticket was handed to the host".
//! Only the first change while unarmed creates a ticket. `take_update` clears
//! both atomically after cloning both surface snapshots. `wake_failed` only
//! disarms the matching ticket and keeps `dirty`, so a failed host wake is
//! retried on the next update instead of losing data.

use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use crate::{
    contracts::{BrowserId, Viewport},
    frame_budget::{FrameBudget, FrameError},
    frame_layout::{FrameFormat, copy_rows},
    ports::{
        FrameInfo, FramePayload, GpuSharedTextureFrame, OwnedFrame, PixelFormat, SurfaceKind,
        TextureFormat,
    },
};

/// VIEW/POPUP rectangle in DIP, with the surface origin CEF reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurfaceBounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Immutable state of one surface at the moment an update was taken.
#[derive(Clone, Debug)]
pub struct SurfaceSnapshot {
    /// The generation a frame must carry to be current. A retained frame whose
    /// `FrameInfo::surface_generation` differs from this is explicitly stale.
    pub generation: u64,
    pub bounds: SurfaceBounds,
    /// Effective visibility: the view's visibility gates the popup.
    pub visible: bool,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub frame: Option<Arc<OwnedFrame>>,
}

/// One host-facing update: both surfaces plus the terminal closed flag.
#[derive(Clone, Debug)]
pub struct FrameUpdate {
    pub browser: BrowserId,
    /// Exact producer-applied Runtime target, committed with generations.
    /// None is an unlabelled legacy producer, never proof of a scale match.
    /// This is not a CEF renderer/callback epoch or a rendered-frame ACK.
    pub viewport: Option<Viewport>,
    pub view: SurfaceSnapshot,
    pub popup: SurfaceSnapshot,
    pub closed: bool,
}

/// Metadata for exactly one capture attempt. It is cheap to copy, but its
/// sequence can be copied at most once and published at most once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureToken {
    info: FrameInfo,
}

impl CaptureToken {
    pub fn info(self) -> FrameInfo {
        self.info
    }

    pub fn surface(self) -> SurfaceKind {
        self.info.surface
    }

    pub fn generation(self) -> u64 {
        self.info.surface_generation
    }

    pub fn sequence(self) -> u64 {
        self.info.sequence
    }

    pub fn width(self) -> u32 {
        self.info.width
    }

    pub fn height(self) -> u32 {
        self.info.height
    }
}

/// One host wake. A failed `UiDispatcher::wake` returns this exact ticket to
/// [`FrameMailbox::wake_failed`]; a stale ticket cannot disarm a newer wake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeTicket {
    browser: BrowserId,
    id: u64,
}

impl WakeTicket {
    pub fn browser(self) -> BrowserId {
        self.browser
    }

    pub fn id(self) -> u64 {
        self.id
    }
}

/// A successfully stored frame plus the wake decision made while publishing.
/// The caller wakes the host only if `wake` is `Some`; the observer's
/// `PublishResult` does not decide whether the host is woken.
#[derive(Clone, Debug)]
pub struct FramePublication {
    pub frame: Arc<OwnedFrame>,
    pub wake: Option<WakeTicket>,
}

struct Surface {
    generation: u64,
    bounds: SurfaceBounds,
    visible: bool,
    pixel_width: u32,
    pixel_height: u32,
    configured: bool,
    frame: Option<Arc<OwnedFrame>>,
    /// Last sequence handed out by `capture_token`; never reset by a
    /// generation change and never wraps.
    next_sequence: u64,
    /// Highest sequence accepted into the latest slot.
    published_sequence: u64,
    /// Highest token sequence whose copy was claimed, so a token cannot be
    /// copied twice (including concurrently).
    copied_sequence: u64,
}

impl Surface {
    fn new(visible: bool) -> Self {
        Self {
            generation: 0,
            bounds: SurfaceBounds::default(),
            visible,
            pixel_width: 0,
            pixel_height: 0,
            configured: false,
            frame: None,
            next_sequence: 0,
            published_sequence: 0,
            copied_sequence: 0,
        }
    }
}

struct Inner {
    browser: BrowserId,
    budget: FrameBudget,
    view_scale: Option<f32>,
    view: Surface,
    popup: Surface,
    closed: bool,
    dirty: bool,
    wake_armed: bool,
    armed: Option<WakeTicket>,
    next_wake_id: u64,
}

impl Inner {
    fn surface(&self, surface: SurfaceKind) -> &Surface {
        match surface {
            SurfaceKind::View => &self.view,
            SurfaceKind::Popup => &self.popup,
        }
    }

    fn surface_mut(&mut self, surface: SurfaceKind) -> &mut Surface {
        match surface {
            SurfaceKind::View => &mut self.view,
            SurfaceKind::Popup => &mut self.popup,
        }
    }

    fn effective_visible(&self, surface: SurfaceKind) -> bool {
        self.view.visible
            && match surface {
                SurfaceKind::View => true,
                SurfaceKind::Popup => self.popup.visible,
            }
    }

    fn snapshot(&self, surface: SurfaceKind) -> SurfaceSnapshot {
        let state = self.surface(surface);
        SurfaceSnapshot {
            generation: state.generation,
            bounds: state.bounds,
            visible: self.effective_visible(surface),
            pixel_width: state.pixel_width,
            pixel_height: state.pixel_height,
            frame: state.frame.clone(),
        }
    }

    fn update(&self) -> FrameUpdate {
        FrameUpdate {
            browser: self.browser,
            viewport: self.view_scale.map(|scale| Viewport {
                width: self.view.bounds.width,
                height: self.view.bounds.height,
                scale,
            }),
            view: self.snapshot(SurfaceKind::View),
            popup: self.snapshot(SurfaceKind::Popup),
            closed: self.closed,
        }
    }

    fn advance_generation(generation: u64) -> Result<u64, FrameError> {
        generation
            .checked_add(1)
            .ok_or(FrameError::GenerationExhausted)
    }

    /// Marks the mailbox dirty and emits a ticket only if no wake is armed.
    fn touch(&mut self) -> Option<WakeTicket> {
        self.dirty = true;
        if self.wake_armed {
            return None;
        }
        let id = self.next_wake_id.checked_add(1)?;
        self.next_wake_id = id;
        let ticket = WakeTicket {
            browser: self.browser,
            id,
        };
        self.armed = Some(ticket);
        self.wake_armed = true;
        Some(ticket)
    }
}

/// One Browser's authoritative latest-frame slots and CPU pixel budget.
pub struct FrameMailbox {
    inner: Mutex<Inner>,
}

impl FrameMailbox {
    pub fn new(browser: BrowserId) -> Self {
        Self::with_budget(browser, FrameBudget::default())
    }

    /// A smaller ledger is useful for tests and for hosts that choose their own
    /// per-Browser CPU cap.
    pub fn with_budget(browser: BrowserId, budget: FrameBudget) -> Self {
        Self {
            inner: Mutex::new(Inner {
                browser,
                budget,
                view_scale: None,
                view: Surface::new(true),
                popup: Surface::new(false),
                closed: false,
                dirty: false,
                wake_armed: false,
                armed: None,
                next_wake_id: 0,
            }),
        }
    }

    pub fn browser(&self) -> BrowserId {
        self.lock().browser
    }

    pub fn budget(&self) -> FrameBudget {
        self.lock().budget.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Issues a non-zero, per-surface monotonic sequence for exactly one paint.
    ///
    /// Rejects a closed mailbox, an effectively hidden surface, malformed or
    /// stale pixel dimensions, and sequence exhaustion. Generation changes do
    /// not reset the sequence.
    pub fn capture_token(
        &self,
        surface: SurfaceKind,
        width: u32,
        height: u32,
    ) -> Result<CaptureToken, FrameError> {
        validate_pixel_size(width, height)?;
        let mut inner = self.lock();
        if inner.closed {
            return Err(FrameError::Closed);
        }
        if !inner.effective_visible(surface) {
            return Err(FrameError::Hidden);
        }
        let state = inner.surface(surface);
        if !state.configured || state.pixel_width == 0 || state.pixel_height == 0 {
            return Err(FrameError::InvalidDimensions);
        }
        if (width, height) != (state.pixel_width, state.pixel_height) {
            // A callback with the previous layout is not a fault; it is stale.
            return Err(FrameError::StaleGeneration);
        }
        let surface_generation = state.generation;
        let info = FrameInfo {
            browser: inner.browser,
            surface,
            surface_generation,
            sequence: 0,
            width,
            height,
            format: PixelFormat::PremultipliedBgra8,
        };
        let state = inner.surface_mut(surface);
        let sequence = state
            .next_sequence
            .checked_add(1)
            .ok_or(FrameError::SequenceExhausted)?;
        state.next_sequence = sequence;
        Ok(CaptureToken {
            info: FrameInfo { sequence, ..info },
        })
    }

    /// Copies callback-owned bytes into a frame charged to this mailbox's
    /// ledger. The row copy uses the shared frame-layout helper, so software
    /// capture and accelerated read-back share one validated layout path.
    ///
    /// A token can claim one copy. A later resize/close is still rejected by
    /// `publish`, which revalidates generation and visibility.
    pub fn copy_frame(&self, token: &CaptureToken, bytes: &[u8]) -> Result<OwnedFrame, FrameError> {
        let info = token.info;
        let expected = validate_pixel_size(info.width, info.height)?;
        if bytes.len() != expected {
            return Err(FrameError::InvalidDimensions);
        }
        let budget = {
            let mut inner = self.lock();
            if inner.closed {
                return Err(FrameError::Closed);
            }
            if info.browser != inner.browser {
                return Err(FrameError::WrongIdentity);
            }
            if !inner.effective_visible(info.surface) {
                return Err(FrameError::Hidden);
            }
            let state = inner.surface_mut(info.surface);
            if !state.configured
                || (info.width, info.height) != (state.pixel_width, state.pixel_height)
                || info.surface_generation != state.generation
            {
                return Err(FrameError::StaleGeneration);
            }
            if info.sequence == 0 || info.sequence <= state.copied_sequence {
                return Err(FrameError::OutOfOrder);
            }
            state.copied_sequence = info.sequence;
            inner.budget.clone()
        };
        let mut buffer = budget.allocate(expected)?;
        let row_pitch = info
            .width
            .checked_mul(4)
            .ok_or(FrameError::InvalidDimensions)?;
        if !copy_rows(
            bytes,
            row_pitch,
            info.width,
            info.height,
            FrameFormat::Bgra8,
            buffer.as_mut_slice(),
        ) {
            return Err(FrameError::InvalidDimensions);
        }
        Ok(OwnedFrame::from_cpu_pixels(info, buffer.freeze()))
    }

    /// Validates the whole frame identity and stores it as the newest slot.
    ///
    /// The check happens under this mailbox lock, so a resize or close that
    /// completed after `copy_frame` is rejected instead of reviving old pixels.
    pub fn publish(&self, mut frame: OwnedFrame) -> Result<FramePublication, FrameError> {
        let info = frame.info;
        let expected = validate_pixel_size(info.width, info.height)?;
        match &mut frame.payload {
            FramePayload::CpuBgra { buffer, .. } => {
                if buffer.len() != expected {
                    return Err(FrameError::InvalidDimensions);
                }
            }
            FramePayload::GpuSharedTexture(gpu) => {
                if gpu.width != info.width
                    || gpu.height != info.height
                    || gpu.format != TextureFormat::Bgra8888
                {
                    return Err(FrameError::InvalidDimensions);
                }
                gpu.frame_seq = info.sequence;
            }
        }
        let mut inner = self.lock();
        if inner.closed {
            return Err(FrameError::Closed);
        }
        if info.browser != inner.browser {
            return Err(FrameError::WrongIdentity);
        }
        if let FramePayload::CpuBgra { buffer, .. } = &frame.payload {
            // Foreign budget pixels cannot be charged to this Browser's ledger.
            if !buffer.shares_ledger(&inner.budget) {
                return Err(FrameError::WrongIdentity);
            }
        }
        if info.format != PixelFormat::PremultipliedBgra8 {
            return Err(FrameError::WrongIdentity);
        }
        if !inner.effective_visible(info.surface) {
            return Err(FrameError::Hidden);
        }
        let state = inner.surface_mut(info.surface);
        if !state.configured
            || (info.width, info.height) != (state.pixel_width, state.pixel_height)
            || info.surface_generation != state.generation
        {
            return Err(FrameError::StaleGeneration);
        }
        if info.sequence == 0 || info.sequence <= state.published_sequence {
            return Err(FrameError::OutOfOrder);
        }
        if let FramePayload::GpuSharedTexture(gpu) = &frame.payload {
            gpu.lease.tracker().publish_ready(gpu.slot_index);
        }
        let frame = Arc::new(frame);
        state.frame = Some(frame.clone());
        state.published_sequence = info.sequence;
        let wake = inner.touch();
        Ok(FramePublication { frame, wake })
    }

    /// Publishes an external GPU shared-texture slot captured on `token`.
    pub fn publish_gpu_frame(
        &self,
        token: &CaptureToken,
        mut gpu: GpuSharedTextureFrame,
    ) -> Result<FramePublication, FrameError> {
        let info = token.info();
        gpu.frame_seq = info.sequence;
        let frame = OwnedFrame {
            info,
            payload: FramePayload::GpuSharedTexture(gpu),
        };
        self.publish(frame)
    }

    /// Reclaims an unconsumed GPU shared-texture frame in `surface`'s mailbox slot,
    /// freeing its [`SharedTextureLease`] without clearing visibility or generation.
    pub fn try_reclaim_unconsumed_gpu_slot(&self, surface: SurfaceKind) -> Option<usize> {
        let mut inner = self.lock();
        let state = inner.surface_mut(surface);
        let reclaimed_slot = state
            .frame
            .as_ref()
            .and_then(|frame| frame.gpu_texture().map(|gpu| gpu.slot_index));
        if reclaimed_slot.is_some() {
            state.frame = None;
        }
        reclaimed_slot
    }

    /// Reconfigures the VIEW target. An actual change advances the VIEW
    /// generation; an unchanged call does not wake. The last good VIEW frame is
    /// retained but becomes stale through its generation.
    pub fn configure_view(
        &self,
        bounds: SurfaceBounds,
        pixel_width: u32,
        pixel_height: u32,
    ) -> Result<Option<WakeTicket>, FrameError> {
        self.configure(bounds, pixel_width, pixel_height, SurfaceKind::View, None)
    }

    /// Applies an exact Runtime viewport context. Scale changes revoke both
    /// generations and clear POPUP atomically with this metadata, including
    /// distinct scales that ceil to the same pixel dimensions.
    pub fn configure_view_with_scale(
        &self,
        bounds: SurfaceBounds,
        pixel_width: u32,
        pixel_height: u32,
        scale: f32,
    ) -> Result<Option<WakeTicket>, FrameError> {
        let width = (bounds.width as f32 * scale).ceil();
        let height = (bounds.height as f32 * scale).ceil();
        if !scale.is_finite()
            || scale <= 0.
            || !width.is_finite()
            || !height.is_finite()
            || width < 1.
            || height < 1.
            || width > u32::MAX as f32
            || height > u32::MAX as f32
            || width as u32 != pixel_width
            || height as u32 != pixel_height
        {
            return Err(FrameError::InvalidDimensions);
        }
        self.configure(
            bounds,
            pixel_width,
            pixel_height,
            SurfaceKind::View,
            Some(scale),
        )
    }

    /// Reconfigures the POPUP target. Any actual change clears the popup slot:
    /// resizing or moving a popup must not resurrect old pixels.
    pub fn configure_popup(
        &self,
        bounds: SurfaceBounds,
        pixel_width: u32,
        pixel_height: u32,
    ) -> Result<Option<WakeTicket>, FrameError> {
        self.configure(bounds, pixel_width, pixel_height, SurfaceKind::Popup, None)
    }

    fn configure(
        &self,
        bounds: SurfaceBounds,
        pixel_width: u32,
        pixel_height: u32,
        surface: SurfaceKind,
        view_scale: Option<f32>,
    ) -> Result<Option<WakeTicket>, FrameError> {
        if bounds.width <= 0 || bounds.height <= 0 {
            return Err(FrameError::InvalidDimensions);
        }
        validate_pixel_size(pixel_width, pixel_height)?;
        let mut inner = self.lock();
        if inner.closed {
            return Err(FrameError::Closed);
        }
        let state = inner.surface(surface);
        let geometry_changed = state.bounds != bounds
            || state.pixel_width != pixel_width
            || state.pixel_height != pixel_height;
        let context_changed = surface == SurfaceKind::View && inner.view_scale != view_scale;
        if state.configured && !geometry_changed && !context_changed {
            return Ok(None);
        }
        let scale_changed = context_changed && state.configured;
        let mut generation = Inner::advance_generation(state.generation)?;
        // Preserve the former configure+invalidate generation steps, but commit
        // them together so no consumer can see new scale with old POPUP pixels.
        if scale_changed && geometry_changed {
            generation = Inner::advance_generation(generation)?;
        }
        let popup_generation = if scale_changed {
            Some(Inner::advance_generation(inner.popup.generation)?)
        } else {
            None
        };
        let state = inner.surface_mut(surface);
        state.generation = generation;
        state.bounds = bounds;
        state.pixel_width = pixel_width;
        state.pixel_height = pixel_height;
        state.configured = true;
        if surface == SurfaceKind::Popup {
            state.frame = None;
        }
        if surface == SurfaceKind::View {
            inner.view_scale = view_scale;
        }
        if let Some(generation) = popup_generation {
            inner.popup.generation = generation;
            inner.popup.frame = None;
        }
        Ok(inner.touch())
    }

    /// Hides or shows the whole page. Hiding forces the popup hidden, advances
    /// both generations and clears the popup slot; the VIEW frame is retained
    /// but becomes stale. Repeating the same state is a no-op.
    pub fn set_visible(&self, visible: bool) -> Result<Option<WakeTicket>, FrameError> {
        let mut inner = self.lock();
        if inner.closed {
            return Err(FrameError::Closed);
        }
        if inner.view.visible == visible {
            return Ok(None);
        }
        let view_generation = Inner::advance_generation(inner.view.generation)?;
        let popup_generation = if visible {
            inner.popup.generation
        } else {
            Inner::advance_generation(inner.popup.generation)?
        };
        inner.view.visible = visible;
        inner.view.generation = view_generation;
        if !visible {
            inner.popup.visible = false;
            inner.popup.generation = popup_generation;
            inner.popup.frame = None;
        }
        Ok(inner.touch())
    }

    /// Toggles the popup layer. A hidden view rejects a popup show outright.
    /// Any actual change advances the popup generation and clears its slot.
    pub fn set_popup_visible(&self, visible: bool) -> Result<Option<WakeTicket>, FrameError> {
        let mut inner = self.lock();
        if inner.closed {
            return Err(FrameError::Closed);
        }
        if visible && !inner.view.visible {
            return Err(FrameError::Hidden);
        }
        if inner.popup.visible == visible {
            return Ok(None);
        }
        let generation = Inner::advance_generation(inner.popup.generation)?;
        inner.popup.visible = visible;
        inner.popup.generation = generation;
        inner.popup.frame = None;
        Ok(inner.touch())
    }

    /// Invalidates a renderer generation without replacing the mailbox.
    ///
    /// VIEW invalidation keeps the last good VIEW frame (now stale) and retires
    /// the popup; POPUP invalidation clears only the popup slot. Both revoke
    /// outstanding capture tokens through the advanced generation.
    pub fn invalidate(&self, surface: SurfaceKind) -> Result<Option<WakeTicket>, FrameError> {
        let mut inner = self.lock();
        if inner.closed {
            return Err(FrameError::Closed);
        }
        match surface {
            SurfaceKind::View => {
                let view_generation = Inner::advance_generation(inner.view.generation)?;
                let popup_generation = Inner::advance_generation(inner.popup.generation)?;
                inner.view.generation = view_generation;
                inner.popup.generation = popup_generation;
                inner.popup.visible = false;
                inner.popup.frame = None;
            }
            SurfaceKind::Popup => {
                let popup_generation = Inner::advance_generation(inner.popup.generation)?;
                inner.popup.generation = popup_generation;
                inner.popup.visible = false;
                inner.popup.frame = None;
            }
        }
        Ok(inner.touch())
    }

    /// Advances both surface generations for a layout/DPI change that does not
    /// change the pixel target (for example 2x2 DIP cells that both ceil to
    /// 3x3 pixels).
    ///
    /// Visible flags, bounds and pixel targets are retained, so an already
    /// shown POPUP stays shown and CEF does not have to repeat `OnPopupShow`.
    /// The VIEW keeps its last good frame (now stale by generation) while the
    /// POPUP frame is cleared. Both outstanding token sets are revoked and one
    /// wake is emitted. Unlike renderer-fault [`FrameMailbox::invalidate`], this
    /// never hides the popup.
    pub fn invalidate_layout(&self) -> Result<Option<WakeTicket>, FrameError> {
        let mut inner = self.lock();
        if inner.closed {
            return Err(FrameError::Closed);
        }
        let view_generation = Inner::advance_generation(inner.view.generation)?;
        let popup_generation = Inner::advance_generation(inner.popup.generation)?;
        inner.view.generation = view_generation;
        inner.popup.generation = popup_generation;
        inner.popup.frame = None;
        Ok(inner.touch())
    }

    /// Permanently seals this very mailbox instance, clears both slots and
    /// publishes one terminal wake. Closing an already closed mailbox returns
    /// `None`; there is no replacement store that could fake a revocation.
    pub fn close(&self) -> Option<WakeTicket> {
        let mut inner = self.lock();
        if inner.closed {
            return None;
        }
        inner.closed = true;
        inner.view.visible = false;
        inner.popup.visible = false;
        inner.view.frame = None;
        inner.popup.frame = None;
        inner.touch()
    }

    /// Latest frame for one surface. A pure query: it neither acknowledges the
    /// update nor disarms a wake.
    pub fn latest(&self, surface: SurfaceKind) -> Option<Arc<OwnedFrame>> {
        self.lock().surface(surface).frame.clone()
    }

    /// Clones both surface snapshots and atomically clears `dirty`/`wake_armed`.
    /// A closed mailbox still yields its terminal update once.
    pub fn take_update(&self) -> Option<FrameUpdate> {
        let mut inner = self.lock();
        if !inner.dirty {
            return None;
        }
        inner.dirty = false;
        inner.wake_armed = false;
        inner.armed = None;
        Some(inner.update())
    }

    /// Reads the same state without consuming a pending update. Hosts use this
    /// to resynchronize after a missed or failed wake.
    pub fn snapshot(&self) -> FrameUpdate {
        self.lock().update()
    }

    /// Disarms the matching ticket but keeps `dirty`, so the next update can
    /// retry the host wake (or the host can call `take_update` explicitly).
    pub fn wake_failed(&self, ticket: WakeTicket) -> bool {
        let mut inner = self.lock();
        if inner.armed == Some(ticket) {
            inner.armed = None;
            inner.wake_armed = false;
            true
        } else {
            false
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl crate::ports::FrameSource for FrameMailbox {
    fn latest(&self, surface: SurfaceKind) -> Option<Arc<OwnedFrame>> {
        FrameMailbox::latest(self, surface)
    }

    fn take_update(&self) -> Option<FrameUpdate> {
        FrameMailbox::take_update(self)
    }

    fn snapshot(&self) -> FrameUpdate {
        FrameMailbox::snapshot(self)
    }
}

impl fmt::Debug for FrameMailbox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();
        formatter
            .debug_struct("FrameMailbox")
            .field("browser", &inner.browser)
            .field("view_generation", &inner.view.generation)
            .field("popup_generation", &inner.popup.generation)
            .field("closed", &inner.closed)
            .field("dirty", &inner.dirty)
            .field("wake_armed", &inner.wake_armed)
            .finish()
    }
}

/// Bytes for a tightly packed premultiplied BGRA frame.
fn validate_pixel_size(width: u32, height: u32) -> Result<usize, FrameError> {
    if width == 0 || height == 0 {
        return Err(FrameError::InvalidDimensions);
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(FrameError::InvalidDimensions)
}
