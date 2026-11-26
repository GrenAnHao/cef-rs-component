//! Per-tab Find toolbar controller, standalone direct Find/View Source sinks
//! (used when native context menus are disabled or in NativeHwnd mode), and
//! bounded GPUI View Source window rendering.

use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    num::NonZeroUsize,
    rc::Rc,
    sync::{Arc, Mutex, Weak},
};

use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, EntityInputHandler,
    FocusHandle, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};
use rust_cef_core::menu::ContextMenuRequestId;
use rust_cef_gpui::{
    contracts::BrowserId,
    dispatcher::UiWakeSender,
    find::{
        FindFailure, FindOutcome, FindPhase, FindReceipt, FindRequest, FindResult, FindResultSink,
        FindStatus,
    },
    find_view::FindUiUpdate,
    ports::{EngineError, UiDispatcher as _},
    source::{
        SourceFailure, SourceOperationId, SourceOperationStatus, SourcePhase, SourceText,
        SourceViewControl, SourceViewRequest, SourceViewSink,
    },
};

use super::{
    model::{ExampleTabModel, PendingFindFollowUp, TabFindState},
    text_input::{
        TextFieldLayoutCache, TextFieldRenderSpec, TextFieldTheme, render_text_field_box,
    },
};

#[allow(dead_code)]
pub const MAX_DIRECT_SOURCE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_FIND_QUERY_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindCommandKind {
    StopClearSelection,
    Start {
        query: String,
        forward: bool,
        match_case: bool,
        find_next: bool,
    },
}

/// Closes the Find bar UI and defers `StopClearSelection` until any in-flight
/// search finishes so runtime `Finds` is never wedged in `Cancelling`.
pub fn plan_close_find_bar(state: &mut TabFindState) -> Option<FindCommandKind> {
    state.set_open(false);
    state.active_ordinal = 0;
    state.matches = 0;
    state.last_submitted_query.clear();
    if !state.quarantined {
        state.last_error = None;
    }
    if state.search_in_progress {
        state.pending_after_finish = Some(PendingFindFollowUp::StopClearSelection);
        None
    } else {
        state.pending_after_finish = None;
        Some(FindCommandKind::StopClearSelection)
    }
}

/// Computes the next strict-serial Page Find command for a tab without violating
/// `FindLimits`, issuing empty searches, or wedging an in-flight search.
pub fn plan_find_command(
    state: &mut TabFindState,
    forward: bool,
    force_find_next: bool,
) -> Option<FindCommandKind> {
    if state.quarantined {
        state.last_error = Some("Quarantined".into());
        return None;
    }
    if state.query.text.is_empty() {
        state.active_ordinal = 0;
        state.matches = 0;
        state.last_error = None;
        state.last_submitted_query.clear();
        if state.search_in_progress {
            state.pending_after_finish = Some(PendingFindFollowUp::StopClearSelection);
            return None;
        }
        state.pending_after_finish = None;
        return Some(FindCommandKind::StopClearSelection);
    }
    if state.query.text.len() > MAX_FIND_QUERY_BYTES {
        state.last_error = Some(format!("Query too long (>{MAX_FIND_QUERY_BYTES} B)"));
        return None;
    }
    if state.search_in_progress {
        state.pending_after_finish = Some(PendingFindFollowUp::Search {
            forward,
            force_find_next,
        });
        if force_find_next {
            state.last_error = Some("Busy (strict serial)".into());
        } else if state.last_error.as_deref() == Some("Busy (strict serial)") {
            state.last_error = None;
        }
        return None;
    }
    let same_query = state.query.text == state.last_submitted_query
        && state.match_case == state.last_submitted_match_case;
    if !force_find_next && same_query && state.last_error.is_none() {
        return None;
    }
    let find_next = force_find_next && same_query && state.matches > 1;
    state.search_in_progress = true;
    state.last_error = None;
    state.last_submitted_query = state.query.text.clone();
    state.last_submitted_match_case = state.match_case;
    Some(FindCommandKind::Start {
        query: state.query.text.clone(),
        forward,
        match_case: state.match_case,
        find_next,
    })
}

/// Applies a runtime `FindStatus` update to `TabFindState` and optionally returns
/// a deferred `Start` or `StopClearSelection` command queued while the previous
/// strict-serial search was still in progress.
pub fn apply_find_status(state: &mut TabFindState, status: &FindStatus) -> Option<FindCommandKind> {
    if status.operation.sequence < state.latest_operation_seq {
        return None;
    }
    state.latest_operation_seq = status.operation.sequence;
    let suppress_stale_counts = !state.open
        || state.query.text.is_empty()
        || matches!(
            state.pending_after_finish,
            Some(PendingFindFollowUp::StopClearSelection)
        );
    if !suppress_stale_counts && let Some(res) = &status.last_result {
        state.matches = res.matches.max(0);
        state.active_ordinal = res.active_ordinal.max(0);
    }
    if status.phase != FindPhase::Finished {
        return None;
    }
    state.search_in_progress = false;
    if matches!(status.first_failure, Some(FindFailure::Uncorrelated)) {
        state.quarantined = true;
        state.last_error = Some("Uncorrelated ID (quarantined)".into());
        state.pending_after_finish = None;
        return None;
    }
    if let Some(failure) = &status.first_failure {
        state.last_error = Some(format!("Find error: {failure:?}"));
    } else if suppress_stale_counts
        || (matches!(
            status.outcome,
            Some(FindOutcome::EmptySelection | FindOutcome::Cancelled)
        ) && status.last_result.is_none())
    {
        state.matches = 0;
        state.active_ordinal = 0;
    } else if status.outcome == Some(FindOutcome::Result) && state.matches == 0 {
        state.active_ordinal = 0;
    }
    if state.last_error.as_deref() == Some("Busy (strict serial)") {
        state.last_error = None;
    }
    match state.pending_after_finish.take() {
        Some(PendingFindFollowUp::StopClearSelection) => {
            state.matches = 0;
            state.active_ordinal = 0;
            state.last_submitted_query.clear();
            Some(FindCommandKind::StopClearSelection)
        }
        Some(PendingFindFollowUp::Search {
            forward,
            force_find_next,
        }) if state.open => plan_find_command(state, forward, force_find_next),
        _ if !state.open || state.query.text.is_empty() => {
            state.matches = 0;
            state.active_ordinal = 0;
            state.last_submitted_query.clear();
            Some(FindCommandKind::StopClearSelection)
        }
        _ => None,
    }
}

pub fn handle_find_dispatch_error(state: &mut TabFindState, err: &EngineError) {
    state.search_in_progress = false;
    state.last_error = Some(match err {
        EngineError::Backpressure => "Busy (strict serial)".into(),
        EngineError::InvalidRequest => "Invalid query".into(),
        other => format!("{other:?}"),
    });
}

pub fn format_find_counter(state: &TabFindState) -> String {
    if state.quarantined {
        return "Quarantined".into();
    }
    if let Some(err) = &state.last_error {
        return err.clone();
    }
    if state.search_in_progress {
        return "Searching...".into();
    }
    format!("{} / {}", state.active_ordinal.max(0), state.matches.max(0))
}

pub(crate) fn render_find_bar<V: EntityInputHandler>(
    tab: &ExampleTabModel,
    find_focused: bool,
    host: Entity<V>,
    focus_handle: FocusHandle,
    query_cache: Rc<RefCell<TextFieldLayoutCache>>,
    on_focus_query: impl Fn(&mut V, &MouseDownEvent, &mut Window, &mut Context<V>) + 'static,
    on_toggle_case: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    on_prev: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    on_next: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    on_close: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let find = &tab.find;
    let counter = format_find_counter(find);
    let case_active = find.match_case;

    let toolbar_btn = |id: &'static str,
                       label: &'static str,
                       active: bool,
                       handler: Box<dyn Fn(&mut V, &mut Window, &mut Context<V>) + 'static>,
                       cx: &mut Context<V>| {
        let bg = if active { rgb(0x5e35b1) } else { rgb(0xffffff) };
        let fg = if active { rgb(0xffffff) } else { rgb(0x333333) };
        let border = if active { rgb(0x5e35b1) } else { rgb(0xbdbdbd) };
        div()
            .id(id)
            .px(px(8.0))
            .h(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .bg(bg)
            .border_1()
            .border_color(border)
            .rounded(px(3.0))
            .text_size(px(12.0))
            .text_color(fg)
            .cursor_pointer()
            .hover(move |s| s.bg(if active { rgb(0x512da8) } else { rgb(0xede7f6) }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    handler(this, window, cx);
                }),
            )
            .child(label)
    };

    div()
        .id("find-bar")
        .w_full()
        .px(px(10.0))
        .py(px(5.0))
        .bg(rgb(0xfafafa))
        .border_b_1()
        .border_color(rgb(0xd6d6d6))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(0x424242))
                .child("Find:"),
        )
        .child(div().w(px(220.0)).child(render_text_field_box(
            TextFieldRenderSpec {
                id: "find-query-input",
                host,
                focus_handle,
                buffer: &find.query,
                cache: query_cache,
                placeholder: "Find in page...",
                focused: find_focused,
                multiline: false,
                font_size_px: 12.0,
                pad_x_px: 8.0,
                pad_y_px: 3.0,
                height_px: Some(26.0),
                rounded_px: 3.0,
                flex_1: false,
                theme: TextFieldTheme::example_light(),
            },
            on_focus_query,
            cx,
        )))
        .child(toolbar_btn(
            "find-match-case",
            "Aa",
            case_active,
            Box::new(on_toggle_case),
            cx,
        ))
        .child(toolbar_btn(
            "find-prev-btn",
            "Prev",
            false,
            Box::new(on_prev),
            cx,
        ))
        .child(toolbar_btn(
            "find-next-btn",
            "Next",
            false,
            Box::new(on_next),
            cx,
        ))
        .child(
            div()
                .px(px(8.0))
                .text_size(px(12.0))
                .text_color(if find.quarantined || find.last_error.is_some() {
                    rgb(0xc62828)
                } else {
                    rgb(0x616161)
                })
                .child(counter),
        )
        .child(div().flex_1())
        .child(toolbar_btn(
            "find-close-btn",
            "×",
            false,
            Box::new(on_close),
            cx,
        ))
        .into_any_element()
}

struct DirectFindSlot {
    receipt: Option<Arc<dyn FindReceipt>>,
    latest: Option<FindStatus>,
    dirty: bool,
}

#[derive(Default)]
struct DirectFindState {
    slots: BTreeMap<BrowserId, DirectFindSlot>,
}

/// Direct per-browser `FindResultSink` registry used when context menus are
/// disabled or when running in `NativeHwnd` mode.
pub struct DirectFindBook {
    state: Arc<Mutex<DirectFindState>>,
    wake: Arc<UiWakeSender>,
}

struct DirectFindSink {
    browser: BrowserId,
    state: Weak<Mutex<DirectFindState>>,
    wake: Arc<UiWakeSender>,
}

impl FindResultSink for DirectFindSink {
    fn started(
        &self,
        request: FindRequest,
        receipt: Arc<dyn FindReceipt>,
    ) -> Result<(), EngineError> {
        let snapshot = receipt.status();
        let Some(state) = self.state.upgrade() else {
            return Err(EngineError::Closed);
        };
        {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            guard.slots.insert(
                request.operation.browser,
                DirectFindSlot {
                    receipt: Some(receipt),
                    latest: Some(snapshot),
                    dirty: true,
                },
            );
        }
        let _ = self.wake.wake();
        Ok(())
    }

    fn update(&self, result: FindResult) -> Result<(), EngineError> {
        let Some(state) = self.state.upgrade() else {
            return Err(EngineError::Closed);
        };
        let receipt = {
            let guard = state.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .slots
                .get(&result.operation.browser)
                .and_then(|s| s.receipt.clone())
        };
        let snapshot = receipt.as_ref().map(|r| r.status());
        {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = guard.slots.get_mut(&self.browser) {
                if let Some(s) = snapshot {
                    slot.latest = Some(s);
                    slot.dirty = true;
                }
            }
        }
        let _ = self.wake.wake();
        Ok(())
    }

    fn finished(&self, status: FindStatus) -> Result<(), EngineError> {
        let Some(state) = self.state.upgrade() else {
            return Ok(());
        };
        {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            let slot = guard.slots.entry(self.browser).or_insert(DirectFindSlot {
                receipt: None,
                latest: None,
                dirty: false,
            });
            slot.latest = Some(status);
            slot.dirty = true;
        }
        let _ = self.wake.wake();
        Ok(())
    }
}

impl DirectFindBook {
    pub fn new(wake: Arc<UiWakeSender>) -> Self {
        Self {
            state: Arc::new(Mutex::new(DirectFindState::default())),
            wake,
        }
    }

    pub fn sink_for(&self, browser: BrowserId) -> Arc<dyn FindResultSink> {
        Arc::new(DirectFindSink {
            browser,
            state: Arc::downgrade(&self.state),
            wake: self.wake.clone(),
        })
    }

    pub fn drain_updates(&self) -> Vec<FindUiUpdate> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut updates = Vec::new();
        for (&browser, slot) in &mut guard.slots {
            if slot.dirty
                && let Some(status) = slot.latest.clone()
            {
                slot.dirty = false;
                updates.push(FindUiUpdate { browser, status });
            }
        }
        updates
    }

    pub fn unregister(&self, browser: BrowserId) {
        let old = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .remove(&browser);
        drop(old);
    }
}

pub fn format_source_failure(op: SourceOperationId, failure: &SourceFailure) -> (String, String) {
    match failure {
        SourceFailure::Budget => (
            "Page source too large".into(),
            format!(
                "Page source for browser {:?} exceeded the configured byte budget.",
                op.browser
            ),
        ),
        other => (
            "View Source failed".into(),
            format!("Unable to load page source ({other:?})."),
        ),
    }
}

#[allow(dead_code)]
pub fn validate_source_payload_bounds(
    text: &str,
    max_bytes: usize,
) -> Result<SourceText, SourceFailure> {
    let limit = NonZeroUsize::new(max_bytes).ok_or(SourceFailure::Budget)?;
    SourceText::new(text.to_owned(), limit)
}

enum DirectSourceEvent {
    Open {
        request: SourceViewRequest,
        control: Arc<dyn SourceViewControl>,
    },
    Close {
        operation: SourceOperationId,
        control: Arc<dyn SourceViewControl>,
    },
}

struct OpenDirectViewer {
    browser: BrowserId,
    handle: AnyWindowHandle,
    control: Arc<dyn SourceViewControl>,
}

struct DirectSourceState {
    max_source_bytes: usize,
    active_ops: BTreeMap<SourceOperationId, Arc<dyn SourceViewControl>>,
    events: VecDeque<DirectSourceEvent>,
    open_windows: BTreeMap<SourceOperationId, OpenDirectViewer>,
}

/// Direct per-browser `SourceViewSink` manager for View Source when context menus
/// are disabled or running in `NativeHwnd` mode.
pub struct DirectSourceBook {
    state: Arc<Mutex<DirectSourceState>>,
    wake: Arc<UiWakeSender>,
}

struct DirectSourceViewerWindow {
    text: SharedString,
    bytes: usize,
}

impl Render for DirectSourceViewerWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x171b22))
            .text_color(rgb(0xe7edf3))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(0x2d333b))
                    .text_sm()
                    .child(format!("Page source — read only ({} bytes)", self.bytes)),
            )
            .child(
                div()
                    .id("direct-source-text")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .p_3()
                    .font_family("monospace")
                    .text_sm()
                    .whitespace_nowrap()
                    .child(self.text.clone()),
            )
    }
}

struct DirectSourceSink {
    browser: BrowserId,
    state: Weak<Mutex<DirectSourceState>>,
    wake: Arc<UiWakeSender>,
}

impl SourceViewSink for DirectSourceSink {
    fn started(
        &self,
        id: SourceOperationId,
        _menu: ContextMenuRequestId,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        if id.browser != self.browser {
            return Err(EngineError::InvalidRequest);
        }
        let Some(state) = self.state.upgrade() else {
            return Err(EngineError::Closed);
        };
        {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            guard.active_ops.insert(id, control);
        }
        let _ = self.wake.wake();
        Ok(())
    }

    fn publish(
        &self,
        request: SourceViewRequest,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        let Some(state) = self.state.upgrade() else {
            return Err(EngineError::Closed);
        };
        {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            if request.source.as_str().len() > guard.max_source_bytes {
                return Err(EngineError::InvalidRequest);
            }
            guard
                .events
                .push_back(DirectSourceEvent::Open { request, control });
        }
        let _ = self.wake.wake();
        Ok(())
    }

    fn close(
        &self,
        id: SourceOperationId,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        let Some(state) = self.state.upgrade() else {
            return Ok(());
        };
        {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            guard.events.push_back(DirectSourceEvent::Close {
                operation: id,
                control,
            });
        }
        let _ = self.wake.wake();
        Ok(())
    }
}

impl DirectSourceBook {
    pub fn new(wake: Arc<UiWakeSender>, max_source_bytes: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(DirectSourceState {
                max_source_bytes,
                active_ops: BTreeMap::new(),
                events: VecDeque::new(),
                open_windows: BTreeMap::new(),
            })),
            wake,
        }
    }

    pub fn viewer_for(&self, browser: BrowserId) -> Arc<dyn SourceViewSink> {
        Arc::new(DirectSourceSink {
            browser,
            state: Arc::downgrade(&self.state),
            wake: self.wake.clone(),
        })
    }

    /// Drains pending open/close events on GPUI and collects any terminal source
    /// failures (including pre-publish `SourceFailure::Budget`) for inline UI notices.
    pub fn drain_and_apply(&self, cx: &mut App) -> Vec<(BrowserId, SourceOperationStatus)> {
        let (events, tracked) = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let events: Vec<_> = guard.events.drain(..).collect();
            let tracked: Vec<_> = guard
                .active_ops
                .iter()
                .map(|(&id, c)| (id, c.clone()))
                .collect();
            (events, tracked)
        };

        for event in events {
            match event {
                DirectSourceEvent::Open { request, control } => {
                    self.open_window(request, control, cx);
                }
                DirectSourceEvent::Close { operation, control } => {
                    let removed = {
                        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                        guard.active_ops.remove(&operation);
                        guard.open_windows.remove(&operation)
                    };
                    if let Some(win) = removed {
                        let _ = win.handle.update(cx, |_, window, _| {
                            window.remove_window();
                        });
                    }
                    let _ = control.closed();
                }
            }
        }

        let mut failures = Vec::new();
        let mut finished_ids = Vec::new();
        for (id, control) in tracked {
            let status = control.status();
            if status.phase == SourcePhase::Finished {
                finished_ids.push(id);
                if status.first_failure.is_some() {
                    failures.push((id.browser, status));
                }
            }
        }
        if !finished_ids.is_empty() {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            for id in finished_ids {
                guard.active_ops.remove(&id);
            }
        }
        failures
    }

    fn open_window(
        &self,
        request: SourceViewRequest,
        control: Arc<dyn SourceViewControl>,
        cx: &mut App,
    ) {
        let op = request.operation;
        let bytes = request.source.as_str().len();
        let text: SharedString = request.source.as_str().to_owned().into();
        let state_ref = Arc::downgrade(&self.state);
        let control_for_close = control.clone();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(900.0), px(640.0)),
                cx,
            ))),
            focus: true,
            show: true,
            titlebar: Some(TitlebarOptions {
                title: Some("Page source".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let result = cx.open_window(options, move |window, cx| {
            let handle = window.window_handle();
            let state_weak = state_ref.clone();
            let ctrl_weak = control_for_close.clone();
            let _sub = cx.on_window_closed(move |_, closed_id| {
                if closed_id != handle.window_id() {
                    return;
                }
                if let Some(state) = state_weak.upgrade() {
                    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                    guard.open_windows.remove(&op);
                    guard.active_ops.remove(&op);
                }
                let _ = ctrl_weak.closed();
            });
            std::mem::forget(_sub);
            cx.new(|_| DirectSourceViewerWindow { text, bytes })
        });

        match result {
            Ok(win) => {
                let handle: AnyWindowHandle = win.into();
                if control.is_current() && !control.status().cancelled {
                    if control.presented().is_ok() {
                        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                        guard.open_windows.insert(
                            op,
                            OpenDirectViewer {
                                browser: op.browser,
                                handle,
                                control,
                            },
                        );
                        return;
                    }
                }
                let _ = handle.update(cx, |_, window, _| window.remove_window());
                let _ = control.closed();
            }
            Err(_) => {
                let _ = control.closed();
            }
        }
    }

    pub fn unregister_browser(&self, browser: BrowserId, cx: &mut App) {
        let to_close: Vec<OpenDirectViewer> = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.active_ops.retain(|id, _| id.browser != browser);
            let ids: Vec<_> = guard
                .open_windows
                .iter()
                .filter(|(_, v)| v.browser == browser)
                .map(|(k, _)| *k)
                .collect();
            ids.into_iter()
                .filter_map(|id| guard.open_windows.remove(&id))
                .collect()
        };
        for win in to_close {
            let _ = win.handle.update(cx, |_, window, _| {
                window.remove_window();
            });
            let _ = win.control.closed();
        }
    }

    #[allow(dead_code)]
    pub fn active_window_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open_windows
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::menu_context::MenuContext;
    use rust_cef_core::ports::WindowBinding;
    use rust_cef_gpui::{contracts::RuntimeGeneration, dispatcher, find::FindLimits};

    fn test_browser(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), local)
    }

    struct StubReceipt(Mutex<FindStatus>);
    impl FindReceipt for StubReceipt {
        fn status(&self) -> FindStatus {
            self.0.lock().unwrap().clone()
        }
        fn cancel(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    struct StubSourceControl {
        status: Mutex<SourceOperationStatus>,
    }
    impl SourceViewControl for StubSourceControl {
        fn status(&self) -> SourceOperationStatus {
            self.status.lock().unwrap().clone()
        }
        fn is_current(&self) -> bool {
            true
        }
        fn presented(&self) -> Result<(), EngineError> {
            Ok(())
        }
        fn closed(&self) -> Result<(), EngineError> {
            Ok(())
        }
        fn request_close(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    mod find_bar_tests {
        use super::*;

        #[test]
        fn find_bar_dispatch_find_next_busy_backpressure_and_close_stop_finding() {
            let mut state = TabFindState::default();
            state.set_open(true);
            assert!(state.open_flag.load(std::sync::atomic::Ordering::Acquire));

            // Empty query does not start Find; emits StopClearSelection cleanly.
            assert_eq!(
                plan_find_command(&mut state, true, false),
                Some(FindCommandKind::StopClearSelection)
            );
            assert!(!state.search_in_progress);

            state.query.set_from_external("alpha");
            assert_eq!(
                plan_find_command(&mut state, true, false),
                Some(FindCommandKind::Start {
                    query: "alpha".into(),
                    forward: true,
                    match_case: false,
                    find_next: false,
                })
            );
            assert!(state.search_in_progress);

            // Overlapping Prev/Next while search_in_progress queues deferred step with busy badge.
            assert_eq!(plan_find_command(&mut state, false, true), None);
            assert_eq!(state.last_error.as_deref(), Some("Busy (strict serial)"));

            let op = rust_cef_gpui::find::FindOperationId {
                browser: test_browser(1),
                sequence: 1,
            };
            let menu = ContextMenuRequestId {
                browser: test_browser(1),
                sequence: 1,
            };
            let finished = FindStatus {
                operation: op,
                menu,
                phase: FindPhase::Finished,
                query_bytes: 5,
                issued: true,
                cancel_requested: false,
                stop_attempted: false,
                stop_submitted: false,
                last_result: Some(FindResult {
                    operation: op,
                    cef_identifier: 7,
                    matches: 4,
                    active_ordinal: 1,
                    selection_rect: None,
                    final_update: true,
                }),
                outcome: Some(FindOutcome::Result),
                first_failure: None,
            };
            let deferred = apply_find_status(&mut state, &finished);
            assert_eq!(state.matches, 4);
            assert_eq!(state.active_ordinal, 1);
            assert_eq!(
                deferred,
                Some(FindCommandKind::Start {
                    query: "alpha".into(),
                    forward: false,
                    match_case: false,
                    find_next: true,
                })
            );

            // Runtime Backpressure resets in-progress flag and exposes readable status.
            handle_find_dispatch_error(&mut state, &EngineError::Backpressure);
            assert!(!state.search_in_progress);
            assert_eq!(format_find_counter(&state), "Busy (strict serial)");

            // Uncorrelated failure quarantines the tab.
            let quarantined = FindStatus {
                first_failure: Some(FindFailure::Uncorrelated),
                outcome: Some(FindOutcome::Failed),
                ..finished
            };
            assert!(apply_find_status(&mut state, &quarantined).is_none());
            assert!(state.quarantined);
            assert_eq!(plan_find_command(&mut state, true, true), None);

            // Closing clears open_flag cleanly.
            state.set_open(false);
            assert!(!state.open_flag.load(std::sync::atomic::Ordering::Acquire));
        }

        #[test]
        fn incremental_typing_and_close_while_in_flight_defer_without_busy_error() {
            let make_finished = |seq: u64, matches: i32, active_ordinal: i32| {
                let op = rust_cef_gpui::find::FindOperationId {
                    browser: test_browser(2),
                    sequence: seq,
                };
                FindStatus {
                    operation: op,
                    menu: ContextMenuRequestId {
                        browser: test_browser(2),
                        sequence: seq,
                    },
                    phase: FindPhase::Finished,
                    query_bytes: 1,
                    issued: true,
                    cancel_requested: false,
                    stop_attempted: false,
                    stop_submitted: false,
                    last_result: Some(FindResult {
                        operation: op,
                        cef_identifier: seq as i32,
                        matches,
                        active_ordinal,
                        selection_rect: None,
                        final_update: true,
                    }),
                    outcome: Some(FindOutcome::Result),
                    first_failure: None,
                }
            };

            let mut state = TabFindState::default();
            state.set_open(true);
            state.query.set_from_external("P");
            assert_eq!(
                plan_find_command(&mut state, true, false),
                Some(FindCommandKind::Start {
                    query: "P".into(),
                    forward: true,
                    match_case: false,
                    find_next: false,
                })
            );

            // Rapid typing while in-flight shows Searching... (not Busy) and coalesces to "Pro".
            state.query.set_from_external("Pr");
            assert_eq!(plan_find_command(&mut state, true, false), None);
            assert!(state.last_error.is_none());
            assert_eq!(format_find_counter(&state), "Searching...");

            state.query.set_from_external("Pro");
            assert_eq!(plan_find_command(&mut state, true, false), None);
            assert!(state.last_error.is_none());
            assert_eq!(
                state.pending_after_finish,
                Some(PendingFindFollowUp::Search {
                    forward: true,
                    force_find_next: false,
                })
            );

            let follow_up = apply_find_status(&mut state, &make_finished(1, 5, 1));
            assert_eq!(
                follow_up,
                Some(FindCommandKind::Start {
                    query: "Pro".into(),
                    forward: true,
                    match_case: false,
                    find_next: false,
                })
            );

            // Completing "Pro" clears search_in_progress; duplicate force_find_next=false is a no-op.
            assert_eq!(apply_find_status(&mut state, &make_finished(2, 3, 1)), None);
            assert_eq!(state.matches, 3);
            assert_eq!(state.active_ordinal, 1);
            assert_eq!(plan_find_command(&mut state, true, false), None);
            assert!(!state.search_in_progress);

            // Clearing query while in flight defers StopClearSelection until Finished.
            state.query.set_from_external("SEARCH");
            assert!(plan_find_command(&mut state, true, false).is_some());
            state.query.set_from_external("");
            assert_eq!(plan_find_command(&mut state, true, false), None);
            assert!(state.search_in_progress);
            assert_eq!(
                apply_find_status(&mut state, &make_finished(3, 4, 2)),
                Some(FindCommandKind::StopClearSelection)
            );
            assert_eq!(state.matches, 0);
            assert_eq!(state.active_ordinal, 0);

            // Closing Find bar while in flight suppresses stale counts and defers StopClearSelection.
            state.query.set_from_external("SEARCH");
            assert!(plan_find_command(&mut state, true, false).is_some());
            assert_eq!(plan_close_find_bar(&mut state), None);
            assert!(!state.open);
            assert!(state.search_in_progress);
            assert_eq!(
                apply_find_status(&mut state, &make_finished(4, 5, 2)),
                Some(FindCommandKind::StopClearSelection)
            );
            assert!(!state.search_in_progress);
            assert_eq!(state.matches, 0);
            assert_eq!(state.active_ordinal, 0);
        }

        #[test]
        fn direct_find_book_delivers_updates_and_unregisters_cleanly() {
            let (tx, _rx) = dispatcher::channel();
            let book = DirectFindBook::new(Arc::new(tx));
            let browser = test_browser(5);
            let sink = book.sink_for(browser);
            let op = rust_cef_gpui::find::FindOperationId {
                browser,
                sequence: 1,
            };
            let menu = ContextMenuRequestId {
                browser,
                sequence: 1,
            };
            let initial = FindStatus {
                operation: op,
                menu,
                phase: FindPhase::Searching,
                query_bytes: 3,
                issued: true,
                cancel_requested: false,
                stop_attempted: false,
                stop_submitted: false,
                last_result: None,
                outcome: None,
                first_failure: None,
            };
            let receipt = Arc::new(StubReceipt(Mutex::new(initial.clone())));
            let ctx = Arc::new(MenuContext {
                binding: WindowBinding { browser, window: 1 },
                binding_revision: 1,
                x: 0,
                y: 0,
                frame_id: String::new(),
                selection: "abc".into(),
                misspelled_word: String::new(),
                dictionary_suggestions: Vec::new(),
            });
            sink.started(
                FindRequest {
                    operation: op,
                    menu,
                    context: ctx,
                },
                receipt,
            )
            .unwrap();
            let drained = book.drain_updates();
            assert_eq!(drained.len(), 1);
            assert_eq!(drained[0].browser, browser);
            assert_eq!(drained[0].status.phase, FindPhase::Searching);

            let _ = FindLimits {
                capacity: NonZeroUsize::new(1).unwrap(),
                max_query_bytes: NonZeroUsize::new(64).unwrap(),
            };
            book.unregister(browser);
            assert!(book.drain_updates().is_empty());
        }
    }

    mod source_viewer_tests {
        use super::*;

        #[test]
        fn direct_source_viewer_enforces_bounds_and_surfaces_budget_failure() {
            let too_big = "x".repeat(MAX_DIRECT_SOURCE_BYTES + 1);
            assert_eq!(
                validate_source_payload_bounds(&too_big, MAX_DIRECT_SOURCE_BYTES),
                Err(SourceFailure::Budget)
            );
            let ok = validate_source_payload_bounds("<html><body>ok</body></html>", 1024).unwrap();
            assert_eq!(ok.as_str(), "<html><body>ok</body></html>");

            let (tx, _rx) = dispatcher::channel();
            let book = DirectSourceBook::new(Arc::new(tx), 64);
            let browser = test_browser(9);
            let viewer = book.viewer_for(browser);
            let op = SourceOperationId {
                browser,
                sequence: 2,
            };
            let menu = ContextMenuRequestId {
                browser,
                sequence: 2,
            };
            let ctrl = Arc::new(StubSourceControl {
                status: Mutex::new(SourceOperationStatus {
                    operation: op,
                    menu,
                    phase: SourcePhase::Finished,
                    native_live: false,
                    host_live: false,
                    callback_received: true,
                    cancelled: false,
                    source_bytes: 0,
                    first_failure: Some(SourceFailure::Budget),
                    first_close_failure: None,
                    outcome: Some(rust_cef_gpui::source::SourceOutcome::Failed),
                }),
            });
            viewer.started(op, menu, ctrl.clone()).unwrap();
            let (title, msg) = format_source_failure(op, &SourceFailure::Budget);
            assert_eq!(title, "Page source too large");
            assert!(msg.contains("byte budget"));

            // Over-budget publish payload is rejected defensively even on DirectSourceSink.
            let bad_req = SourceViewRequest {
                operation: op,
                menu,
                context: Arc::new(MenuContext {
                    binding: WindowBinding { browser, window: 1 },
                    binding_revision: 1,
                    x: 0,
                    y: 0,
                    frame_id: String::new(),
                    selection: String::new(),
                    misspelled_word: String::new(),
                    dictionary_suggestions: Vec::new(),
                }),
                source: SourceText::new("z".repeat(128), NonZeroUsize::new(128).unwrap()).unwrap(),
            };
            assert_eq!(
                viewer.publish(bad_req, ctrl),
                Err(EngineError::InvalidRequest)
            );
            assert_eq!(book.active_window_count(), 0);
        }
    }
}
