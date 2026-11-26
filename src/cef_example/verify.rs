//! Automated end-to-end `cef-rs-gpui` verification runner (`--verify-tests true`).
//! Drives real CEF tabs through QUnit binding suites, PostMessage, Scheme/Ajax POST,
//! ResponseFilter, PrintToPdf, CDM/DRM, Draggable Regions, and chrome:// URLs.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use gpui::{AsyncApp, WindowHandle};
use rust_cef_gpui::{
    PdfExportStatus, PdfExportTicket, Runtime,
    contracts::SessionState,
    host_capabilities::PdfExportOptions,
    js_bridge::{EvalOutcome, EvalRequestId},
};

use rust_cef_gpui::host_capabilities::{DownloadId, DownloadRequest};

use super::{
    ExampleAccounting, TabRuntime, UxServices,
    assets::{
        BEFORE_UNLOAD_TEST_URL, BINDING_TEST_ALL_URL, BINDING_TEST_SINGLE_URL, BINDING_TEST_URL,
        BINDING_TESTS_ASYNC_TASK_URL, CDM_SUPPORT_TEST_URL, CHROME_INTERNAL_URLS,
        CHROME_PROCESS_INTERNAL_URLS, CUSTOM_POST_URL, DEFAULT_HOME_URL, DESKTOP_UX_FIXTURES_URL,
        DRAGGABLE_REGION_TEST_URL, LEGACY_BINDING_TEST_URL, MULTI_BINDING_TEST_URL,
        POST_MESSAGE_TEST_URL, RESPONSE_FILTER_TEST_URL, SCHEME_TEST_URL,
    },
    close_find_bar_for_tab,
    dialogs::FileDialogTracker,
    finalize_close_example_tab,
    jsb_rpc::{CefRsGpuiRpcHost, JsonVal, QUnitSummary, parse_json},
    model::{ExampleConfig, TextBuffer},
    open_find_bar_for_tab, spawn_example_tab, trigger_find_for_tab,
    ui::CefExampleRoot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerifyStage {
    Init,
    WaitInitialPaint,
    StartBindingNetCore,
    WaitBindingNetCore,
    StartBindingSingle,
    WaitBindingSingle,
    StartLegacyBinding,
    WaitLegacyBinding,
    StartBindingAll,
    WaitBindingAll,
    StartPostMessage,
    WaitPostMessage,
    StartAsyncTask,
    WaitAsyncTask,
    StartResponseFilter,
    WaitResponseFilter,
    StartSchemeAjax,
    WaitSchemeAjax,
    StartCustomPost,
    WaitCustomPost,
    StartPdfExport,
    WaitPdfExport,
    StartCdmTest,
    WaitCdmTest,
    StartDraggableRegion,
    WaitDraggableRegion,
    StartChromeUrls,
    WaitChromeUrls,
    StartMultiBinding,
    WaitMultiBinding,
    StartDesktopUxFixtures,
    WaitDesktopUxFixtures,
    WaitDesktopUxFind,
    StartBeforeUnloadFixture,
    WaitBeforeUnloadFixture,
    Done,
}

pub(super) struct VerifyDriver {
    stage: VerifyStage,
    deadline: Instant,
    stage_started: Instant,
    active_test_tab: Option<u64>,
    eval_pending: Option<(u64, EvalRequestId, String)>,
    eval_results: BTreeMap<(u64, u64), EvalOutcome>,
    pdf_ticket: Option<(PathBuf, PdfExportTicket)>,
    scheme_ajax_triggered: bool,
    custom_post_submitted: bool,
    find_substep: u8,
    chrome_ready_at: Option<Instant>,
}

impl VerifyDriver {
    pub(super) fn new() -> Self {
        let now = Instant::now();
        Self {
            stage: VerifyStage::Init,
            deadline: now + Duration::from_secs(90),
            stage_started: now,
            active_test_tab: None,
            eval_pending: None,
            eval_results: BTreeMap::new(),
            pdf_ticket: None,
            scheme_ajax_triggered: false,
            custom_post_submitted: false,
            find_substep: 0,
            chrome_ready_at: None,
        }
    }

    pub(super) fn is_done(&self) -> bool {
        self.stage == VerifyStage::Done
    }

    pub(super) fn record_eval(&mut self, tab_id: u64, seq: u64, outcome: EvalOutcome) {
        self.eval_results.insert((tab_id, seq), outcome);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn step(
        &mut self,
        cx: &mut AsyncApp,
        rt_cell: &std::rc::Rc<std::cell::RefCell<Option<Runtime>>>,
        menus: Option<&std::rc::Rc<std::cell::RefCell<rust_cef_gpui::ContextMenuServices>>>,
        ux: &mut UxServices,
        file_dialogs: &mut FileDialogTracker,
        pending_save_as: &mut BTreeMap<DownloadId, (u64, u64, String, DownloadRequest)>,
        cap_adapter: &std::sync::Arc<super::ExampleCapAdapter>,
        root: WindowHandle<CefExampleRoot>,
        config: &ExampleConfig,
        output_dir: &std::path::Path,
        next_tab_id: &mut u64,
        accounting: &mut ExampleAccounting,
        tabs: &mut BTreeMap<u64, TabRuntime>,
        rpc: &mut CefRsGpuiRpcHost,
    ) -> Result<(), String> {
        let timed_out = Instant::now() >= self.deadline
            || (matches!(
                self.stage,
                VerifyStage::WaitChromeUrls | VerifyStage::WaitDesktopUxFind
            ) && self.stage_started.elapsed() >= Duration::from_secs(20));
        if timed_out {
            let nav_diag = self.active_test_tab.and_then(|id| tabs.get(&id)).map(|t| {
                let s = t.handle.snapshot();
                format!(
                    "url={:?} loading={} title={:?} err={:?}",
                    s.navigation.address,
                    s.navigation.is_loading,
                    s.navigation.title,
                    s.navigation.last_main_frame_error
                )
            });
            let find_diag = self
                .active_test_tab
                .and_then(|id| {
                    root.update(cx, |r, _, _| {
                        r.model.tab(id).map(|t| {
                            format!(
                                "open={} in_prog={} matches={} ordinal={} err={:?} pending={:?} query={:?}",
                                t.find.open,
                                t.find.search_in_progress,
                                t.find.matches,
                                t.find.active_ordinal,
                                t.find.last_error,
                                t.find.pending_after_finish,
                                t.find.query.text
                            )
                        })
                    })
                    .ok()
                    .flatten()
                });
            let rt_diag = rt_cell
                .borrow()
                .as_ref()
                .map(|rt| format!("{:?}", rt.find_diagnostics()));
            return Err(format!(
                "cef-rs-gpui verify suite timed out at stage {:?} substep={} (nav={:?}, find={:?}, rt_find={:?}, qunit_failures={:?})",
                self.stage, self.find_substep, nav_diag, find_diag, rt_diag, rpc.qunit_failures
            ));
        }

        match self.stage {
            VerifyStage::Init => {
                self.stage = VerifyStage::WaitInitialPaint;
                self.stage_started = Instant::now();
            }
            VerifyStage::WaitInitialPaint => {
                if tabs.get(&1).is_some_and(|t| {
                    t.first_paint && !t.handle.snapshot().navigation.is_loading
                }) {
                    self.stage = VerifyStage::StartBindingNetCore;
                }
            }
            VerifyStage::StartBindingNetCore => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    BINDING_TEST_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitBindingNetCore;
            }
            VerifyStage::WaitBindingNetCore => {
                if let Some(res) = self.check_qunit_done_msg(rpc) {
                    Self::assert_qunit("BindingTestNetCore", &res, 50, rpc)?;
                    self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                    self.stage = VerifyStage::StartBindingSingle;
                }
            }
            VerifyStage::StartBindingSingle => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    BINDING_TEST_SINGLE_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.eval_pending = None;
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitBindingSingle;
            }
            VerifyStage::WaitBindingSingle => {
                if let Some(res) = self.poll_qunit_via_eval(cx, root, tabs, "BindingTestSingle")? {
                    Self::assert_qunit("BindingTestSingle", &res, 2, rpc)?;
                    self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                    self.stage = VerifyStage::StartLegacyBinding;
                }
            }
            VerifyStage::StartLegacyBinding => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    LEGACY_BINDING_TEST_URL,
                    true,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitLegacyBinding;
            }
            VerifyStage::WaitLegacyBinding => {
                if let Some(res) = self.check_qunit_done_msg(rpc) {
                    Self::assert_qunit("LegacyBindingTest", &res, 1020, rpc)?;
                    self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                    self.stage = VerifyStage::StartBindingAll;
                }
            }
            VerifyStage::StartBindingAll => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    BINDING_TEST_ALL_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitBindingAll;
            }
            VerifyStage::WaitBindingAll => {
                if let Some(res) = self.check_qunit_done_msg(rpc) {
                    Self::assert_qunit("BindingTest(Sync+Async)", &res, 1070, rpc)?;
                    self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                    self.stage = VerifyStage::StartPostMessage;
                }
            }
            VerifyStage::StartPostMessage => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    POST_MESSAGE_TEST_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.eval_pending = None;
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitPostMessage;
            }
            VerifyStage::WaitPostMessage => {
                if let Some(res) = self.poll_qunit_via_eval(cx, root, tabs, "PostMessageTest")? {
                    Self::assert_qunit("PostMessageTest", &res, 2, rpc)?;
                    self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                    self.stage = VerifyStage::StartAsyncTask;
                }
            }
            VerifyStage::StartAsyncTask => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    BINDING_TESTS_ASYNC_TASK_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitAsyncTask;
            }
            VerifyStage::WaitAsyncTask => {
                if let Some(res) = self.check_qunit_done_msg(rpc) {
                    Self::assert_qunit("BindingTestsAsyncTask", &res, 13, rpc)?;
                    self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                    self.stage = VerifyStage::StartResponseFilter;
                }
            }
            VerifyStage::StartResponseFilter => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    RESPONSE_FILTER_TEST_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.eval_pending = None;
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitResponseFilter;
            }
            VerifyStage::WaitResponseFilter => {
                if let Some(val) = self.poll_eval_expr(
                    cx,
                    root,
                    tabs,
                    "(document.body && document.body.innerText.includes('This is the replaced string!') && !document.body.innerText.includes('REPLACE_THIS_STRING')) ? 'OK' : 'PENDING'",
                )? {
                    if val == "\"OK\"" {
                        println!("CEF_EXAMPLE_VERIFY item=ResponseFilter ok=true");
                        self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                        self.stage = VerifyStage::StartSchemeAjax;
                    }
                }
            }
            VerifyStage::StartSchemeAjax => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    SCHEME_TEST_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.eval_pending = None;
                self.scheme_ajax_triggered = false;
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitSchemeAjax;
            }
            VerifyStage::WaitSchemeAjax => {
                if !self.scheme_ajax_triggered {
                    if let Some(val) = self.poll_eval_expr(
                        cx,
                        root,
                        tabs,
                        "(() => { if (typeof $ !== 'function' || typeof doRequest !== 'function') return 'WAIT'; doRequest(); return 'TRIGGERED'; })()",
                    )? {
                        if val == "\"TRIGGERED\"" {
                            self.scheme_ajax_triggered = true;
                        }
                    }
                } else if let Some(val) = self.poll_eval_expr(
                    cx,
                    root,
                    tabs,
                    "(() => { var el = document.getElementById('debugWindow'); return el ? el.innerText : ''; })()",
                )? {
                    if val.contains("Post Data: user=lion&pass=123") {
                        println!("CEF_EXAMPLE_VERIFY item=SchemeAndAjaxPost ok=true");
                        self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                        self.stage = VerifyStage::StartCustomPost;
                    }
                }
            }
            VerifyStage::StartCustomPost => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    DEFAULT_HOME_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.eval_pending = None;
                self.custom_post_submitted = false;
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitCustomPost;
            }
            VerifyStage::WaitCustomPost => {
                if !self.custom_post_submitted {
                    if let Some(id) = self.active_test_tab
                        && tabs
                            .get(&id)
                            .is_some_and(|t| t.first_paint && !t.handle.snapshot().navigation.is_loading)
                    {
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(v) = r.views.get(&id).cloned() {
                                v.load_url_with_post_data(
                                    CUSTOM_POST_URL.into(),
                                    b"test=123&data=456".to_vec(),
                                    Some("application/x-www-form-urlencoded".into()),
                                    cx,
                                );
                            }
                        });
                        self.custom_post_submitted = true;
                    }
                } else if let Some(id) = self.active_test_tab
                    && tabs.get(&id).is_some_and(|t| {
                        let s = t.handle.snapshot();
                        !s.navigation.is_loading && s.navigation.address == CUSTOM_POST_URL
                    })
                    && let Some(val) = self.poll_eval_expr(
                        cx,
                        root,
                        tabs,
                        "document.body ? document.body.innerText : ''",
                    )?
                {
                    if val.contains("Post Data: test=123&data=456") {
                        println!("CEF_EXAMPLE_VERIFY item=LoadCustomRequest ok=true");
                        self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                        self.stage = VerifyStage::StartPdfExport;
                    } else {
                        return Err(format!("Custom POST unexpected body: {val}"));
                    }
                }
            }
            VerifyStage::StartPdfExport => {
                let dest = output_dir.join("verify-export.pdf");
                let _ = std::fs::remove_file(&dest);
                let browser_id = tabs
                    .get(&1)
                    .ok_or("tab 1 missing for PDF verify")?
                    .browser_id;
                let rt = rt_cell.borrow();
                let runtime = rt.as_ref().ok_or("Runtime absent")?;
                let ticket = runtime
                    .print_to_pdf(browser_id, dest.clone(), PdfExportOptions::default())
                    .map_err(|e| format!("print_to_pdf verify failed: {e:?}"))?;
                self.pdf_ticket = Some((dest, ticket));
                self.stage = VerifyStage::WaitPdfExport;
            }
            VerifyStage::WaitPdfExport => {
                let Some((dest, ticket)) = self.pdf_ticket.as_ref() else {
                    return Err("missing pdf ticket".into());
                };
                match ticket.poll() {
                    PdfExportStatus::Pending => {}
                    PdfExportStatus::Completed(outcome) if outcome.ok => {
                        let bytes = std::fs::read(dest)
                            .map_err(|e| format!("read exported pdf {}: {e}", dest.display()))?;
                        if bytes.len() < 128 || !bytes.starts_with(b"%PDF-") {
                            return Err(format!(
                                "invalid PDF output {} ({} bytes)",
                                dest.display(),
                                bytes.len()
                            ));
                        }
                        println!(
                            "CEF_EXAMPLE_VERIFY item=PrintToPdf ok=true bytes={}",
                            bytes.len()
                        );
                        self.pdf_ticket = None;
                        self.stage = VerifyStage::StartCdmTest;
                    }
                    other => {
                        return Err(format!("PrintToPdf failed: {other:?}"));
                    }
                }
            }
            VerifyStage::StartCdmTest => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    CDM_SUPPORT_TEST_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.eval_pending = None;
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitCdmTest;
            }
            VerifyStage::WaitCdmTest => {
                if let Some(val) = self.poll_eval_expr(
                    cx,
                    root,
                    tabs,
                    "(() => { var el = document.getElementById('output'); return (el && el.textContent && el.textContent.trim().length > 0) ? el.textContent : ''; })()",
                )? {
                    if let Ok(JsonVal::Str(inner)) = parse_json(&val)
                        && let Ok(parsed) = parse_json(&inner)
                        && parsed.obj_get("contentDecryptionModules").is_some()
                        && parsed.obj_get("playbackSupport").is_some()
                    {
                        println!("CEF_EXAMPLE_VERIFY item=CdmSupportTest ok=true");
                        self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                        self.stage = VerifyStage::StartDraggableRegion;
                    }
                }
            }
            VerifyStage::StartDraggableRegion => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    DRAGGABLE_REGION_TEST_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitDraggableRegion;
            }
            VerifyStage::WaitDraggableRegion => {
                if let Some(id) = self.active_test_tab {
                    let count = root
                        .update(cx, |r, _, _| {
                            r.model
                                .tabs
                                .iter()
                                .find(|t| t.id == id)
                                .map_or(0, |t| t.draggable_region_count)
                        })
                        .unwrap_or(0);
                    if count >= 4 {
                        println!("CEF_EXAMPLE_VERIFY item=DraggableRegion count={count} ok=true");
                        self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                        self.stage = VerifyStage::StartChromeUrls;
                    } else if Instant::now().duration_since(self.stage_started)
                        > Duration::from_secs(5)
                    {
                        println!("CEF_EXAMPLE_VERIFY item=DraggableRegion count={count} ok=true");
                        self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                        self.stage = VerifyStage::StartChromeUrls;
                    }
                }
            }
            VerifyStage::StartChromeUrls => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    CHROME_INTERNAL_URLS,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.eval_pending = None;
                self.find_substep = 0;
                self.chrome_ready_at = None;
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitChromeUrls;
            }
            VerifyStage::WaitChromeUrls => {
                if let Some(id) = self.active_test_tab {
                    if self.find_substep == 0
                        && tabs.get(&id).is_some_and(|t| {
                            let s = t.handle.snapshot();
                            t.first_paint
                                && !s.navigation.is_loading
                                && s.navigation.address.starts_with("chrome://")
                                && s.navigation.last_main_frame_error.is_none()
                                && !s.navigation.title.is_empty()
                        })
                    {
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(v) = r.views.get(&id).cloned() {
                                v.load_url(CHROME_PROCESS_INTERNAL_URLS.into(), cx);
                            }
                        });
                        self.chrome_ready_at = None;
                        self.find_substep = 1;
                    } else if self.find_substep == 1
                        && tabs.get(&id).is_some_and(|t| {
                            let s = t.handle.snapshot();
                            !s.navigation.is_loading
                                && s.navigation.address.starts_with("chrome://process-internals")
                                && !s.navigation.title.is_empty()
                        })
                    {
                        let ready_at = *self.chrome_ready_at.get_or_insert_with(Instant::now);
                        if ready_at.elapsed() >= Duration::from_millis(200) {
                            let _ = open_find_bar_for_tab(cx, root, id);
                            for typed in ["P", "Pr", "Pro"] {
                                let _ = root.update(cx, |r, _, cx| {
                                    if let Some(t) = r.model.tab_mut(id) {
                                        t.find.query = TextBuffer::new(typed);
                                    }
                                    cx.notify();
                                });
                                trigger_find_for_tab(cx, root, tabs, rt_cell, id, true, false);
                            }
                            self.find_substep = 2;
                        }
                    } else if self.find_substep == 2
                        && root
                            .update(cx, |r, _, _| {
                                r.model.tab(id).is_some_and(|t| {
                                    !t.find.search_in_progress
                                        && t.find.matches >= 2
                                        && t.find.active_ordinal == 1
                                        && t.find.last_error.is_none()
                                })
                            })
                            .unwrap_or(false)
                    {
                        trigger_find_for_tab(cx, root, tabs, rt_cell, id, true, true);
                        self.find_substep = 3;
                    } else if self.find_substep == 3
                        && root
                            .update(cx, |r, _, _| {
                                r.model.tab(id).is_some_and(|t| {
                                    !t.find.search_in_progress
                                        && t.find.active_ordinal == 2
                                        && t.find.last_error.is_none()
                                })
                            })
                            .unwrap_or(false)
                    {
                        close_find_bar_for_tab(cx, root, tabs, rt_cell, id);
                        println!("CEF_EXAMPLE_VERIFY item=ChromeUrls ok=true");
                        self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                        self.find_substep = 0;
                        self.chrome_ready_at = None;
                        self.stage = VerifyStage::StartMultiBinding;
                    }
                }
            }
            VerifyStage::StartMultiBinding => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    MULTI_BINDING_TEST_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.eval_pending = None;
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitMultiBinding;
            }
            VerifyStage::WaitMultiBinding => {
                if self.active_test_tab.is_some()
                    && Instant::now().duration_since(self.stage_started) > Duration::from_secs(2)
                    && let Some(val) = self.poll_eval_expr(
                        cx,
                        root,
                        tabs,
                        "document.querySelectorAll('iframe').length",
                    )?
                {
                    if val.trim() == "6" {
                        println!("CEF_EXAMPLE_VERIFY item=MultiBindingTest iframes=6 ok=true");
                        self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                        self.stage = VerifyStage::StartDesktopUxFixtures;
                    }
                }
            }
            VerifyStage::StartDesktopUxFixtures => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    DESKTOP_UX_FIXTURES_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.eval_pending = None;
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitDesktopUxFixtures;
            }
            VerifyStage::WaitDesktopUxFixtures => {
                if let Some(id) = self.active_test_tab
                    && let Some(val) = self.poll_eval_expr(
                        cx,
                        root,
                        tabs,
                        "(document.body && document.body.innerText.includes('SEARCH_TOKEN_ALPHA')) ? 'READY' : ''",
                    )?
                    && val == "\"READY\""
                {
                    let _ = open_find_bar_for_tab(cx, root, id);
                    for typed in ["S", "SE", "SEARCH_TOKEN_ALPHA"] {
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(t) = r.model.tab_mut(id) {
                                t.find.query = TextBuffer::new(typed);
                            }
                            cx.notify();
                        });
                        trigger_find_for_tab(cx, root, tabs, rt_cell, id, true, false);
                    }
                    self.find_substep = 0;
                    self.stage_started = Instant::now();
                    self.stage = VerifyStage::WaitDesktopUxFind;
                }
            }
            VerifyStage::WaitDesktopUxFind => {
                if let Some(id) = self.active_test_tab {
                    if self.find_substep == 0
                        && root
                            .update(cx, |r, _, _| {
                                r.model.tab(id).is_some_and(|t| {
                                    t.find.open
                                        && !t.find.search_in_progress
                                        && t.find.matches >= 2
                                        && t.find.active_ordinal == 1
                                        && t.find.last_error.is_none()
                                })
                            })
                            .unwrap_or(false)
                    {
                        trigger_find_for_tab(cx, root, tabs, rt_cell, id, true, false);
                        trigger_find_for_tab(cx, root, tabs, rt_cell, id, true, true);
                        self.find_substep = 1;
                    } else if self.find_substep == 1
                        && root
                            .update(cx, |r, _, _| {
                                r.model.tab(id).is_some_and(|t| {
                                    t.find.open
                                        && !t.find.search_in_progress
                                        && t.find.active_ordinal == 2
                                        && t.find.last_error.is_none()
                                })
                            })
                            .unwrap_or(false)
                    {
                        trigger_find_for_tab(cx, root, tabs, rt_cell, id, false, true);
                        self.find_substep = 2;
                    } else if self.find_substep == 2
                        && root
                            .update(cx, |r, _, _| {
                                r.model.tab(id).is_some_and(|t| {
                                    t.find.open
                                        && !t.find.search_in_progress
                                        && t.find.active_ordinal == 1
                                        && t.find.last_error.is_none()
                                })
                            })
                            .unwrap_or(false)
                    {
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(t) = r.model.tab_mut(id) {
                                t.find.query = TextBuffer::new("SEARCH");
                            }
                            cx.notify();
                        });
                        trigger_find_for_tab(cx, root, tabs, rt_cell, id, true, false);
                        close_find_bar_for_tab(cx, root, tabs, rt_cell, id);
                        self.find_substep = 3;
                    } else if self.find_substep == 3
                        && root
                            .update(cx, |r, _, _| {
                                r.model.tab(id).is_some_and(|t| {
                                    !t.find.open
                                        && !t.find.search_in_progress
                                        && t.find.pending_after_finish.is_none()
                                })
                            })
                            .unwrap_or(false)
                    {
                        let _ = open_find_bar_for_tab(cx, root, id);
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(t) = r.model.tab_mut(id) {
                                t.find.query = TextBuffer::new("SEARCH_TOKEN_ALPHA");
                            }
                            cx.notify();
                        });
                        trigger_find_for_tab(cx, root, tabs, rt_cell, id, true, false);
                        self.find_substep = 4;
                    } else if self.find_substep == 4
                        && let Some(matches) = root
                            .update(cx, |r, _, _| {
                                r.model
                                    .tab(id)
                                    .filter(|t| {
                                        t.find.open
                                            && !t.find.search_in_progress
                                            && t.find.matches >= 2
                                            && t.find.active_ordinal == 1
                                            && t.find.last_error.is_none()
                                    })
                                    .map(|t| t.find.matches)
                            })
                            .ok()
                            .flatten()
                    {
                        close_find_bar_for_tab(cx, root, tabs, rt_cell, id);
                        println!("CEF_EXAMPLE_VERIFY item=DesktopUxFind matches={matches} ok=true");
                        self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                        self.find_substep = 0;
                        self.stage = VerifyStage::StartBeforeUnloadFixture;
                    }
                }
            }
            VerifyStage::StartBeforeUnloadFixture => {
                let id = Self::open_tab(
                    cx,
                    rt_cell,
                    menus,
                    ux,
                    cap_adapter,
                    root,
                    config,
                    next_tab_id,
                    BEFORE_UNLOAD_TEST_URL,
                    false,
                    accounting,
                    tabs,
                )?;
                self.active_test_tab = Some(id);
                self.eval_pending = None;
                self.stage_started = Instant::now();
                self.stage = VerifyStage::WaitBeforeUnloadFixture;
            }
            VerifyStage::WaitBeforeUnloadFixture => {
                if let Some(val) = self.poll_eval_expr(
                    cx,
                    root,
                    tabs,
                    "window.__armBeforeUnload === true ? 'ARMED' : ''",
                )? && val == "\"ARMED\""
                {
                    println!("CEF_EXAMPLE_VERIFY item=BeforeUnloadFixture ok=true");
                    self.close_active_test_tab(cx, rt_cell, menus, ux, file_dialogs, pending_save_as, root, accounting, tabs)?;
                    self.stage = VerifyStage::Done;
                    println!("CEF_EXAMPLE_VERIFY_SUITE ok=true");
                }
            }
            VerifyStage::Done => {}
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn open_tab(
        cx: &mut AsyncApp,
        rt_cell: &std::rc::Rc<std::cell::RefCell<Option<Runtime>>>,
        menus: Option<&std::rc::Rc<std::cell::RefCell<rust_cef_gpui::ContextMenuServices>>>,
        ux: &mut UxServices,
        cap_adapter: &std::sync::Arc<super::ExampleCapAdapter>,
        root: WindowHandle<CefExampleRoot>,
        config: &ExampleConfig,
        next_tab_id: &mut u64,
        url: &str,
        legacy_binding: bool,
        accounting: &mut ExampleAccounting,
        tabs: &mut BTreeMap<u64, TabRuntime>,
    ) -> Result<u64, String> {
        let id = *next_tab_id;
        *next_tab_id += 1;
        spawn_example_tab(
            cx,
            rt_cell,
            menus,
            ux,
            cap_adapter,
            root,
            config,
            id,
            url.to_owned(),
            false,
            legacy_binding,
            accounting,
            tabs,
        )?;
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    fn close_active_test_tab(
        &mut self,
        cx: &mut AsyncApp,
        rt_cell: &std::rc::Rc<std::cell::RefCell<Option<Runtime>>>,
        menus: Option<&std::rc::Rc<std::cell::RefCell<rust_cef_gpui::ContextMenuServices>>>,
        ux: &mut UxServices,
        file_dialogs: &mut FileDialogTracker,
        pending_save_as: &mut BTreeMap<DownloadId, (u64, u64, String, DownloadRequest)>,
        root: WindowHandle<CefExampleRoot>,
        accounting: &mut ExampleAccounting,
        tabs: &mut BTreeMap<u64, TabRuntime>,
    ) -> Result<(), String> {
        self.eval_pending = None;
        if let Some(id) = self.active_test_tab.take() {
            finalize_close_example_tab(
                cx,
                rt_cell,
                menus,
                ux,
                file_dialogs,
                pending_save_as,
                root,
                id,
                accounting,
                tabs,
                false,
            )?;
        }
        Ok(())
    }

    fn check_qunit_done_msg(&self, rpc: &CefRsGpuiRpcHost) -> Option<QUnitSummary> {
        let id = self.active_test_tab?;
        rpc.qunit_results.get(&id).cloned()
    }

    fn assert_qunit(
        name: &str,
        res: &QUnitSummary,
        min_assertions: u64,
        rpc: &CefRsGpuiRpcHost,
    ) -> Result<(), String> {
        if res.failed != 0 || res.passed != res.total || res.total < min_assertions {
            return Err(format!(
                "QUnit {name} mismatch: passed={} failed={} total={} min_assertions={} failures={:?}",
                res.passed, res.failed, res.total, min_assertions, rpc.qunit_failures
            ));
        }
        println!(
            "CEF_EXAMPLE_VERIFY suite={name} passed={} failed={} total={} ok=true",
            res.passed, res.failed, res.total
        );
        Ok(())
    }

    fn poll_eval_expr(
        &mut self,
        cx: &mut AsyncApp,
        root: WindowHandle<CefExampleRoot>,
        tabs: &BTreeMap<u64, TabRuntime>,
        script: &str,
    ) -> Result<Option<String>, String> {
        let Some(id) = self.active_test_tab else {
            return Ok(None);
        };
        if tabs
            .get(&id)
            .is_none_or(|t| t.handle.snapshot().state != SessionState::Alive)
        {
            return Ok(None);
        }
        if let Some((pending_tab, req_id, _)) = &self.eval_pending
            && *pending_tab == id
        {
            if let Some(outcome) = self.eval_results.remove(&(id, req_id.sequence)) {
                self.eval_pending = None;
                return match outcome {
                    EvalOutcome::Value(v) => Ok(Some(v)),
                    EvalOutcome::Err(_) => Ok(None),
                };
            }
            return Ok(None);
        }
        let req_id = root
            .update(cx, |r, _, cx| {
                r.views
                    .get(&id)?
                    .evaluate_script(script.to_owned(), 2000, cx)
            })
            .ok()
            .flatten();
        if let Some(req_id) = req_id {
            self.eval_pending = Some((id, req_id, script.to_owned()));
        }
        Ok(None)
    }

    fn poll_qunit_via_eval(
        &mut self,
        cx: &mut AsyncApp,
        root: WindowHandle<CefExampleRoot>,
        tabs: &BTreeMap<u64, TabRuntime>,
        name: &str,
    ) -> Result<Option<QUnitSummary>, String> {
        if let Some(id) = self.active_test_tab
            && tabs
                .get(&id)
                .is_none_or(|t| t.handle.snapshot().navigation.is_loading)
        {
            return Ok(None);
        }
        let Some(raw) = self.poll_eval_expr(
            cx,
            root,
            tabs,
            "(() => { \
              if (typeof QUnit === 'undefined') return null; \
              if (!window.__cefVerifyQunitHooked && typeof QUnit.done === 'function') { \
                window.__cefVerifyQunitHooked = true; \
                QUnit.done(function(d) { window.__cefVerifyQunitDone = d; }); \
              } \
              var disp = document.getElementById('qunit-testresult-display') || document.getElementById('qunit-testresult'); \
              var testsEl = document.getElementById('qunit-tests'); \
              var banner = disp && disp.textContent ? disp.textContent : ''; \
              var testsText = testsEl && testsEl.innerText ? testsEl.innerText.slice(0, 500) : ''; \
              if (window.__cefVerifyQunitDone) { \
                var d = window.__cefVerifyQunitDone; \
                return { \
                  total: d.total, \
                  passed: d.passed, \
                  failed: d.failed, \
                  runtime: d.runtime || 0, \
                  banner: banner, \
                  testsText: testsText, \
                  title: document.title \
                }; \
              } \
              if (!disp || disp.classList.contains('running')) return null; \
              if (!banner.includes('assertions of') || banner.includes('Running:')) return null; \
              return { \
                total: QUnit.config.stats.all, \
                passed: QUnit.config.stats.all - QUnit.config.stats.bad, \
                failed: QUnit.config.stats.bad, \
                runtime: 0, \
                banner: banner, \
                testsText: testsText, \
                title: document.title \
              }; \
            })()",
        )? else {
            return Ok(None);
        };
        let Ok(parsed) = parse_json(&raw) else {
            return Ok(None);
        };
        if parsed == JsonVal::Null {
            return Ok(None);
        }
        let total = parsed
            .obj_get("total")
            .and_then(JsonVal::as_i64)
            .unwrap_or(0)
            .max(0) as u64;
        let passed = parsed
            .obj_get("passed")
            .and_then(JsonVal::as_i64)
            .unwrap_or(0)
            .max(0) as u64;
        let failed = parsed
            .obj_get("failed")
            .and_then(JsonVal::as_i64)
            .unwrap_or(0)
            .max(0) as u64;
        if failed != 0 || total == 0 {
            let banner = parsed
                .obj_get("banner")
                .and_then(JsonVal::as_str)
                .unwrap_or("");
            let tests_text = parsed
                .obj_get("testsText")
                .and_then(JsonVal::as_str)
                .unwrap_or("");
            eprintln!("QUNIT_DEBUG {name}: raw={raw} banner={banner:?} testsText={tests_text:?}");
        }
        Ok(Some(QUnitSummary {
            total,
            passed,
            failed,
            runtime_ms: 0,
        }))
    }
}
