//! Test-only no-job print panels in the two original pressure windows.
use super::*;
use crate::windows::print_panel_probe::PanelCase;
use rust_cef_gpui::{
    ports::{BrowserHandle, WindowBinding},
    print::{PrintOutcome, PrintPhase, PrintStatus},
    test_support::{CommandPressure, WindowPrintCut},
};

pub(super) struct Owner<'a> {
    pub handle: &'a BrowserHandle,
    pub view: &'a Entity<CefBrowser>,
    pub root: WindowHandle<HostRoot>,
    pub window: u64,
}

struct Slot {
    owner: BrowserHandle,
    root: WindowHandle<HostRoot>,
    panel: PanelCase,
    reaped: bool,
}

pub(crate) struct Held {
    cut: WindowPrintCut,
    slots: [Slot; 2],
    initial_paints: [u64; 2],
    stale_owner_paints: [u64; 2],
    b_paints_after_a_close: u64,
    b_paints_before_panel_refresh: u64,
    b_paints_after_its_view_repaint: u64,
    pending_counts: Vec<usize>,
    cleanup_attempted: bool,
}

impl Held {
    pub(super) fn arm(rt: &Runtime, owners: &[Owner<'_>; 2]) -> Result<Self, String> {
        let bindings = owners.each_ref().map(|o| WindowBinding {
            browser: o.handle.snapshot().browser,
            window: o.window,
        });
        // Qualify both real sources before installing either row.
        let source = |binding: WindowBinding| {
            rt.frame_source(binding.browser)
                .map_err(|e| format!("original panel source: {e:?}"))
        };
        let a = source(bindings[0])?;
        let b = source(bindings[1])?;
        let cut = rt
            .probe_arm_window_print_pair(bindings)
            .map_err(|e| format!("arm the two original no-job panels: {e:?}"))?;
        let [slot_a, slot_b] = owners;
        let slots =
            [(slot_a, cut.ids[0], a), (slot_b, cut.ids[1], b)].map(|(o, id, source)| Slot {
                owner: o.handle.clone(),
                root: o.root,
                panel: PanelCase::bind_armed(id, o.window, o.view, source),
                reaped: false,
            });
        Ok(Self {
            cut,
            slots,
            initial_paints: [0; 2],
            stale_owner_paints: [0; 2],
            b_paints_after_a_close: 0,
            b_paints_before_panel_refresh: 0,
            b_paints_after_its_view_repaint: 0,
            pending_counts: Vec::with_capacity(6),
            cleanup_attempted: false,
        })
    }

    pub(super) async fn present(
        &mut self,
        cx: &mut AsyncApp,
        rt: &Runtime,
        owners: &[Owner<'_>; 2],
    ) -> Result<(), String> {
        self.observe_count(rt, 2)?;
        for (i, owner) in owners.iter().enumerate() {
            let status = self.awaiting(rt, i, false)?;
            let slot = &mut self.slots[i];
            slot.panel
                .present_original(cx, slot.root, status, owner.view)
                .await?;
            self.initial_paints[i] = slot.panel.original_panel_paints(cx, slot.root, true)?;
        }
        Ok(())
    }

    pub(super) fn cut(&self) -> WindowPrintCut {
        self.cut
    }

    fn observe_count(&mut self, rt: &Runtime, expected: usize) -> Result<(), String> {
        let actual = self
            .cut
            .ids
            .iter()
            .filter(|&&id| rt.probe_print_status(id).is_some())
            .count();
        self.pending_counts.push(actual);
        if actual != expected {
            return Err(format!(
                "original print count {actual}, expected {expected}"
            ));
        }
        Ok(())
    }

    fn status(&self, rt: &Runtime, index: usize) -> Result<PrintStatus, String> {
        let id = self.cut.ids[index];
        rt.probe_print_status(id)
            .filter(|s| {
                s.operation == id
                    && s.origin.binding
                        == WindowBinding {
                            browser: id.browser,
                            window: self.cut.windows[index],
                        }
                    && !s.resources_retired
                    && s.accepted_job.is_none()
            })
            .ok_or_else(|| format!("print row {index} is not the original no-job row"))
    }

    fn awaiting(&self, rt: &Runtime, index: usize, cancelled: bool) -> Result<PrintStatus, String> {
        let status = self.status(rt, index)?;
        if status.phase != PrintPhase::AwaitingConfirmation
            || status.cancel_requested != cancelled
            || status.outcome.is_some()
        {
            return Err(format!(
                "original panel {index} did not remain in its required confirmation phase"
            ));
        }
        Ok(status)
    }

    pub(super) fn first_closed(
        &mut self,
        cx: &mut AsyncApp,
        rt: &Runtime,
        pressure: &CommandPressure,
    ) -> Result<(), String> {
        if pressure.held_window_prints_at_close != Some(self.cut.identities())
            || !self.slots[1].owner.snapshot().is_ready()
        {
            return Err("pressure Close missed original print IDs or closed the peer".into());
        }
        self.awaiting(rt, 0, true)?;
        self.awaiting(rt, 1, false)?;
        let b = &self.slots[1];
        self.b_paints_after_a_close = b.panel.original_panel_paints(cx, b.root, true)?;
        if self.b_paints_after_a_close < self.initial_paints[1] || self.initial_paints.contains(&0)
        {
            return Err("B original panel lost its paint witness at A close".into());
        }
        Ok(())
    }

    async fn retire_one(
        &mut self,
        cx: &mut AsyncApp,
        rt: &Runtime,
        index: usize,
        proof: bool,
    ) -> Result<(), String> {
        let slot = &self.slots[index];
        if slot.reaped {
            if !proof && rt.probe_print_status(self.cut.ids[index]).is_none() {
                return Ok(());
            }
            return Err("unexpected repeat original panel retirement".into());
        }
        if !slot.panel.owner_is_closed(&slot.owner) {
            return Err(
                "do not retire print resources before actual original Browser/View close".into(),
            );
        }
        if proof {
            self.awaiting(rt, index, true)?;
            self.stale_owner_paints[index] =
                slot.panel.original_panel_paints(cx, slot.root, false)?;
            if self.stale_owner_paints[index] < self.initial_paints[index] {
                return Err("original stale panel lost the paint witness".into());
            }
        }
        let id = self.cut.ids[index];
        let status = self.status(rt, index)?;
        if status.cancel_requested
            && status.phase == PrintPhase::AwaitingConfirmation
            && status.outcome.is_none()
        {
            rt.probe_print_cancelled(id)
                .map_err(|e| format!("original metadata no-job result: {e:?}"))?;
        }
        let status = self.status(rt, index)?;
        if !status.cancel_requested
            || status.phase != PrintPhase::Terminal
            || status.outcome != Some(PrintOutcome::Cancelled)
        {
            return Err("cannot retire an unknown or replaced print outcome".into());
        }
        // The no-job result above does not release or stand in for this entity.
        if proof {
            self.observe_count(rt, 2 - index)?;
        }
        let slot = &mut self.slots[index];
        if !slot.panel.release_panel_resources(cx, slot.root).await? && proof {
            return Err(
                "expected the actual original panel to be removed during retirement".into(),
            );
        }
        let receipt = rt
            .probe_print_retired(id)
            .map_err(|e| format!("original panel release receipt: {e:?}"))?;
        if receipt.operation != id
            || !receipt.resources_retired
            || receipt.accepted_job.is_some()
            || receipt.outcome != Some(PrintOutcome::Cancelled)
            || rt.probe_print_status(id).is_some()
        {
            return Err("print API did not retire the exact original panel row".into());
        }
        slot.reaped = true;
        if proof {
            self.observe_count(rt, 1 - index)?;
        }
        Ok(())
    }

    pub(super) async fn reap_first(
        &mut self,
        cx: &mut AsyncApp,
        rt: &Runtime,
    ) -> Result<(), String> {
        if self.b_paints_after_a_close == 0 {
            return Err("missing original A Closed witness".into());
        }
        self.awaiting(rt, 1, false)?;
        self.retire_one(cx, rt, 0, true).await?;
        self.awaiting(rt, 1, false)?;
        Ok(())
    }

    pub(super) async fn observe_b_repaint(
        &mut self,
        cx: &mut AsyncApp,
        rt: &Runtime,
        deadline: Instant,
    ) -> Result<(), String> {
        if !self.slots[0].reaped || rt.probe_print_status(self.cut.ids[0]).is_some() {
            return Err("B repaint preceded original A panel retirement".into());
        }
        self.awaiting(rt, 1, false)?;
        let b = &self.slots[1];
        self.b_paints_before_panel_refresh = b.panel.original_panel_paints(cx, b.root, true)?;
        if self.b_paints_before_panel_refresh < self.b_paints_after_a_close {
            return Err("B original panel replaced or invalidated at B View repaint".into());
        }
        // The View has a new frame; its sibling panel may still have cached paint.
        // Request one original panel draw and wait for a fully contained witness.
        b.panel.request_original_panel_paint(cx, b.root)?;
        let until = deadline.min(Instant::now() + Duration::from_secs(5));
        loop {
            if Instant::now() >= until {
                return Err("no fresh fully-contained B panel paint".into());
            }
            rendered(cx, b.root).await?;
            let painted = b.panel.original_panel_paints(cx, b.root, true)?;
            if painted > self.b_paints_before_panel_refresh {
                self.b_paints_after_its_view_repaint = painted;
                break;
            }
        }
        self.awaiting(rt, 1, false)?;
        Ok(())
    }

    pub(super) async fn reap_second(
        &mut self,
        cx: &mut AsyncApp,
        rt: &Runtime,
    ) -> Result<String, String> {
        if self.b_paints_after_its_view_repaint == 0 || !self.slots[0].reaped {
            return Err("missing original B View repaint cut".into());
        }
        self.retire_one(cx, rt, 1, true).await?;
        let [a, b] = self.cut.identities();
        Ok(object(vec![
            ("ids", format!("[{a:?},{b:?}]")),
            ("windows", format!("{:?}", self.cut.windows)),
            ("first_only_cancelled", bool_json(true)),
            ("second_pending_after_first_reap", bool_json(true)),
            ("both_reaped", bool_json(true)),
            ("metadata_only", bool_json(true)),
            ("native_job_submitted", bool_json(false)),
            ("initial_panel_paints", format!("{:?}", self.initial_paints)),
            (
                "b_panel_paints_after_a_closed",
                self.b_paints_after_a_close.to_string(),
            ),
            (
                "b_panel_paints_before_refresh",
                self.b_paints_before_panel_refresh.to_string(),
            ),
            (
                "b_panel_paints_at_repaint",
                self.b_paints_after_its_view_repaint.to_string(),
            ),
            (
                "stale_owner_panel_paints",
                format!("{:?}", self.stale_owner_paints),
            ),
            ("original_panel_entities_released", "[true,true]".into()),
            ("pending_print_rows", format!("{:?}", self.pending_counts)),
        ]))
    }

    /// Advance the original resource rows once and only after actual owner close.
    /// A missing/unqualified resource witness keeps the Runtime fence intact.
    pub(crate) async fn cleanup_closed(
        &mut self,
        cx: &mut AsyncApp,
        rt: &Runtime,
    ) -> Result<(), String> {
        if self.cleanup_attempted || !self.slots.iter().all(|s| s.panel.owner_is_closed(&s.owner)) {
            return Ok(());
        }
        self.cleanup_attempted = true;
        let a = self.retire_one(cx, rt, 0, false).await;
        let b = self.retire_one(cx, rt, 1, false).await;
        a.and(b)
    }
}
