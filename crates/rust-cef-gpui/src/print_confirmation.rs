//! In-window confirmation UI pinned to the original CefBrowser entity. This
//! scaffold never enables submission without a Windows provider. The UI emits
//! only an owned cancellation decision; it never calls Runtime while borrowed.
use crate::{CefBrowser, binding::WindowToken};
use gpui::{
    AnyWindowHandle, App, Bounds, ClickEvent, Context, Entity, ParentElement, Pixels, Render,
    Styled, WeakEntity, Window, canvas, div, point, prelude::*, px, rgb,
};
use rust_cef_core::{
    menu::ContextMenuRequestId,
    print::{PrintId, PrintOrigin, PrintPhase, PrintStatus},
};
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintPanelError {
    WrongOwner,
    NotAwaitingConfirmation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dismissal {
    Open,
    Queued,
    Taken,
    Invalidated,
}

pub struct PrintConfirmation {
    owner: WeakEntity<CefBrowser>,
    window: AnyWindowHandle,
    token: WindowToken,
    operation: PrintId,
    menu: ContextMenuRequestId,
    origin: PrintOrigin,
    dismissal: Rc<Cell<Dismissal>>,
    paints: Rc<Cell<u64>>,
}

impl PrintConfirmation {
    pub fn new(
        view: &Entity<CefBrowser>,
        window: &Window,
        app: &App,
        status: PrintStatus,
    ) -> Result<Self, PrintPanelError> {
        let (token, original, _, _) = view.read(app).menu_attachment();
        if original != window.window_handle()
            || !token.is_live()
            || token.binding() != status.origin.binding
            || status.operation.browser != token.binding().browser
            || status.menu.browser != token.binding().browser
            || status.menu.sequence == 0
        {
            return Err(PrintPanelError::WrongOwner);
        }
        if status.phase != PrintPhase::AwaitingConfirmation
            || status.cancel_requested
            || status.resources_retired
            || status.outcome.is_some()
            || status.accepted_job.is_some()
        {
            return Err(PrintPanelError::NotAwaitingConfirmation);
        }
        Ok(Self {
            owner: view.downgrade(),
            window: original,
            token,
            operation: status.operation,
            menu: status.menu,
            origin: status.origin,
            dismissal: Rc::new(Cell::new(Dismissal::Open)),
            paints: Rc::new(Cell::new(0)),
        })
    }

    pub fn owner_current(&self, window: &Window) -> bool {
        window.window_handle() == self.window
            && self.token.is_live()
            && self.owner.upgrade().is_some()
    }

    pub fn open_in_owner(&self, window: &Window) -> bool {
        self.owner_current(window) && self.dismissal.get() == Dismissal::Open
    }

    pub fn operation(&self) -> PrintId {
        self.operation
    }

    pub fn owner_paints(&self) -> u64 {
        self.paints.get()
    }

    /// Apply a fresh cancellation snapshot from the original Runtime, fetched
    /// outside GPUI borrows. A navigation may revoke the original document
    /// while the Browser and GPUI owner remain live. An invalidated panel never
    /// returns to Open, even if an older snapshot is delivered afterwards.
    pub fn observe_revocation(&mut self, status: PrintStatus, window: &Window) -> bool {
        if !self.owner_current(window)
            || !status.cancel_requested
            || status.operation != self.operation
            || status.menu != self.menu
            || status.origin != self.origin
            || self.dismissal.get() == Dismissal::Invalidated
        {
            return false;
        }
        self.dismissal.set(Dismissal::Invalidated);
        true
    }

    /// Drain the owned cancel decision on the original GPUI window; the caller
    /// may dispatch it only after releasing all App/Entity/window borrows.
    /// A stale owner cannot send a new UI response. Owner retirement belongs
    /// to the independent lifecycle path, not a fabricated button press.
    pub fn take_cancel(&mut self, window: &Window) -> Option<PrintId> {
        if self.dismissal.get() != Dismissal::Queued || !self.owner_current(window) {
            return None;
        }
        self.dismissal.set(Dismissal::Taken);
        Some(self.operation)
    }
}

fn fully_in_drawable_area(bounds: Bounds<Pixels>, window: &Window) -> bool {
    let viewport = Bounds {
        origin: point(px(0.), px(0.)),
        size: window.viewport_size(),
    };
    let end = bounds.bottom_right();
    let within = |outer: Bounds<Pixels>| {
        let limit = outer.bottom_right();
        bounds.origin.x >= outer.origin.x
            && bounds.origin.y >= outer.origin.y
            && end.x <= limit.x
            && end.y <= limit.y
    };
    bounds.size.width > px(0.)
        && bounds.size.height > px(0.)
        && within(viewport)
        && within(window.content_mask().bounds)
}

impl Render for PrintConfirmation {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open_in_owner(window) {
            return div().into_any_element();
        }
        let token = self.token.clone();
        let owner = self.owner.clone();
        let handle = self.window;
        let paints = self.paints.clone();
        let dismissal = self.dismissal.clone();
        div()
            .relative()
            .p(px(12.))
            .w_full()
            .bg(rgb(0x272a32))
            .text_color(rgb(0xf2f3f5))
            .child(div().text_size(px(16.)).child("Print this page?"))
            .child(
                div()
                    .text_size(px(13.))
                    .child("A managed Windows printer is not configured."),
            )
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .pt(px(8.))
                    .child(div().text_color(rgb(0x91939b)).child("Print unavailable"))
                    .child(
                        div()
                            .id("print-confirmation-cancel")
                            .child("Cancel")
                            .on_click(cx.listener(|this, _: &ClickEvent, w, cx| {
                                if this.dismissal.get() == Dismissal::Open && this.owner_current(w)
                                {
                                    this.dismissal.set(Dismissal::Queued);
                                    cx.notify();
                                }
                            })),
                    ),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        if dismissal.get() == Dismissal::Open
                            && window.window_handle() == handle
                            && token.is_live()
                            && owner.upgrade().is_some()
                            && fully_in_drawable_area(bounds, window)
                        {
                            paints.set(paints.get().saturating_add(1));
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }
}
