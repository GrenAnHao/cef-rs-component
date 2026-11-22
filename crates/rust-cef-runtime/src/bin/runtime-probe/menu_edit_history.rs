//! Opt-in CEF-local input through the public command and menu response ports.
//! Never modifies the native menu model or selects a clipboard command.
use rust_cef_core::{
    contracts::{BrowserId, ThreadMode},
    input::{InputEvent, InputKind, MouseButton},
    menu::{ContextMenuResponse, MenuLimits},
    menu_context::MenuContextLimits,
    menu_lease::MenuLeaseOwner,
    menu_model::MenuItem,
    ports::{
        BrowserCommand, BrowserHandle, ContextMenuTarget, EngineError, MouseClick, WindowBinding,
    },
};
use rust_cef_runtime::{
    ContextMenuConfig, ContextMenuRegistration,
    menu_bridge_fixture::{BridgeDelivery, MenuBridgeFixture},
    runtime::Runtime,
    test_support::{EditHistoryMenu, EditHistoryReport},
};
use std::{
    num::{NonZeroU8, NonZeroUsize},
    sync::Arc,
    time::{Duration, Instant},
};

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;
const POINT: (i32, i32) = (20, 20);
const TITLES: [&str; 3] = [
    "edit-history:1:insertText:1",
    "edit-history:2:historyUndo:0",
    "edit-history:3:historyRedo:1",
];

#[derive(Default)]
pub struct EditHistory {
    owner: Option<MenuLeaseOwner>,
    registration: Option<ContextMenuRegistration>,
    inbox: Option<Arc<MenuBridgeFixture>>,
    report: EditHistoryReport,
}

struct Run<'a> {
    rt: &'a mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    handle: &'a BrowserHandle,
    browser: BrowserId,
    inbox: &'a MenuBridgeFixture,
    registration: &'a ContextMenuRegistration,
    deadline: Instant,
}

impl Run<'_> {
    fn wait(&mut self, label: &str, predicate: impl Fn(&Self) -> bool) -> Result<(), String> {
        loop {
            if let Some(e) = self.registration.status().first_failure {
                return Err(format!("native edit menu {label}: {e}"));
            }
            let s = self.handle.snapshot();
            if s.browser != self.browser || !s.is_ready() {
                return Err(format!("original edit Browser lost at {label}"));
            }
            if s.navigation.title == "edit-history:invalid" {
                return Err("original editor saw unexpected input or lost focus".into());
            }
            if predicate(self) {
                return Ok(());
            }
            if Instant::now() >= self.deadline {
                return Err(format!(
                    "native edit menu {label} timeout, title={}",
                    s.navigation.title
                ));
            }
            (self.tick)(self.rt, self.mode)?;
        }
    }

    fn title(&mut self, expected: &str) -> Result<(), String> {
        self.wait(expected, |r| {
            r.handle.snapshot().navigation.title == expected
        })
    }

    fn submit(&self, command: BrowserCommand) -> Result<(), String> {
        self.handle
            .submit(command)
            .map(|_| ())
            .map_err(|e| format!("original edit input admission: {e:?}"))
    }

    fn input(&self, kind: InputKind) -> Result<(), String> {
        let epoch = self
            .handle
            .input_snapshot()
            .map_err(|e| format!("edit input epoch: {e:?}"))?
            .epoch;
        self.submit(BrowserCommand::Input(InputEvent { epoch, kind }))
    }

    fn open(&mut self, sequence: u64) -> Result<BridgeDelivery, String> {
        for up in [false, true] {
            self.input(InputKind::MouseButton {
                x: POINT.0,
                y: POINT.1,
                button: MouseButton::Right,
                up,
                click_count: 1,
                modifiers: Default::default(),
            })?;
        }
        self.wait("original menu delivery and callback return", |r| {
            let d = r.rt.diagnostics();
            r.inbox.snapshot().last_sequence == sequence
                && d.tasks.pending == 0
                && d.pending_requests == 1
        })?;
        let d = self
            .inbox
            .take()
            .ok_or("original edit menu delivery missing")?;
        if d.presentation.request.browser != self.browser
            || d.presentation.request.sequence != sequence
        {
            return Err("original edit menu request identity mismatch".into());
        }
        Ok(d)
    }

    fn terminal(&mut self, sequence: u64) -> Result<(), String> {
        self.wait("exact terminal", |r| {
            let d = r.rt.diagnostics();
            let s = r.registration.status();
            d.tasks.pending == 0
                && d.pending_requests == 0
                && s.cancelled == sequence
                && s.native_dismissed == sequence
                && r.inbox.snapshot().terminal == sequence
        })
    }
}

fn selection(command_id: i32) -> ContextMenuResponse {
    ContextMenuResponse::Select { command_id }
}

impl EditHistory {
    pub fn run(
        &mut self,
        rt: &mut Runtime,
        mode: ThreadMode,
        handle: &BrowserHandle,
        tick: Tick,
    ) -> Result<(), String> {
        let browser = handle.snapshot().browser;
        let binding = WindowBinding {
            browser,
            window: browser.local(),
        };
        let nz = |n| NonZeroUsize::new(n).unwrap();
        rt.configure_context_menus(ContextMenuConfig {
            capacity: nz(1),
            model_limits: MenuLimits {
                max_entries: nz(32),
                max_label_bytes: nz(256),
                max_total_label_bytes: nz(2048),
            },
            context_limits: MenuContextLimits {
                max_frame_id_bytes: nz(512),
                max_text_bytes: nz(256),
                max_suggestions: nz(5),
                max_total_bytes: nz(1024),
            },
        })
        .map_err(|e| format!("edit history menu config: {e:?}"))?;
        let owner = MenuLeaseOwner::new(binding, nz(1))
            .map_err(|e| format!("metadata edit owner: {e:?}"))?;
        let inbox = MenuBridgeFixture::new(owner.lease());
        let registration = rt
            .bind_context_menu(ContextMenuTarget {
                lease: owner.lease(),
                delivery: inbox.clone(),
            })
            .map_err(|e| format!("bind edit history: {e:?}"))?;
        self.owner = Some(owner);
        let registration = self.registration.insert(registration);
        self.inbox = Some(inbox.clone());
        self.report.browser = Some((browser.runtime().value(), browser.local()));
        let mut r = Run {
            rt,
            mode,
            tick,
            handle,
            browser,
            inbox: &inbox,
            registration,
            deadline: Instant::now() + Duration::from_secs(20),
        };
        r.title("edit-history:ready")?;
        r.submit(BrowserCommand::SetFocus(true))?;
        r.submit(BrowserCommand::MouseClick(MouseClick {
            x: POINT.0,
            y: POINT.1,
            modifiers: Default::default(),
            click_count: NonZeroU8::new(1).unwrap(),
        }))?;
        r.title("edit-history:focused")?;
        let undo = cef::MenuId::UNDO.get_raw();
        let redo = cef::MenuId::REDO.get_raw();
        let mut frame = None;
        let mut previous: Option<BridgeDelivery> = None;
        for (seq, enabled, action) in [
            (1, [false, false], None),
            (2, [true, false], Some(undo)),
            (3, [false, true], Some(redo)),
        ] {
            let d = r.open(seq)?;
            let p = &d.presentation;
            let c = &p.context;
            let flags = [undo, redo].map(|cmd| {
                p.model.entries.iter().find_map(|row| match row {
                    MenuItem::Command(item)
                        if item.command_id == cmd && item.visible && item.checked.is_none() =>
                    {
                        Some(item.enabled)
                    }
                    _ => None,
                })
            });
            let separators = p
                .model
                .entries
                .iter()
                .filter(|row| matches!(row, MenuItem::Separator { visible: true }))
                .count();
            if c.binding != binding
                || c.binding_revision == 0
                || (c.x, c.y) != POINT
                || (p.model.x, p.model.y) != POINT
                || c.frame_id.is_empty()
                || frame.as_ref().is_some_and(|id| id != &c.frame_id)
                || flags != enabled.map(Some)
                || separators != 2
            {
                return Err(format!(
                    "stock edit menu {seq}: flags={flags:?} separators={separators}, original context={c:?}"
                ));
            }
            frame.get_or_insert_with(|| c.frame_id.clone());
            let posted = r.rt.diagnostics().tasks.posted;
            let mut refused = Vec::with_capacity(2);
            for (cmd, available) in [undo, redo].into_iter().zip(enabled) {
                if !available {
                    let reply = d.response.submit(p.request, selection(cmd));
                    if !matches!(reply, Err(EngineError::InvalidRequest)) {
                        return Err(format!("disabled edit command {cmd}: {reply:?}"));
                    }
                    refused.push(cmd);
                }
            }
            let stale_refused = if let Some(old) = previous.as_ref() {
                let reply = old
                    .response
                    .submit(old.presentation.request, ContextMenuResponse::Cancel);
                if !matches!(reply, Err(EngineError::InvalidRequest)) {
                    return Err(format!("original old edit menu reply: {reply:?}"));
                }
                true
            } else {
                false
            };
            let after = r.rt.diagnostics();
            if after.tasks.posted != posted
                || after.pending_requests != 1
                || r.inbox.snapshot().terminal != seq - 1
            {
                return Err("refused edit actions changed original pending state".into());
            }
            self.report.menus.push(EditHistoryMenu {
                request: seq,
                enabled,
                separators,
                refused,
                stale_refused,
                action,
                frame_id_bytes: c.frame_id.len(),
                anchor: [c.x, c.y],
                terminal: false,
            });
            d.response
                .submit(
                    p.request,
                    action.map_or(ContextMenuResponse::Cancel, selection),
                )
                .map_err(|e| format!("original edit menu response {seq}: {e:?}"))?;
            r.terminal(seq)?;
            self.report.menus.last_mut().unwrap().terminal = true;
            previous = Some(d);
            if seq == 1 {
                r.input(InputKind::Text("x".into()))?;
            }
            let expected = TITLES[(seq - 1) as usize];
            r.title(expected)?;
            self.report
                .renderer_witnesses
                .push(r.handle.snapshot().navigation.title);
        }
        Ok(())
    }

    pub fn snapshot(&self) -> EditHistoryReport {
        let mut report = self.report.clone();
        if let Some(s) = &self.registration {
            report.status = s.status();
        }
        if let Some(i) = &self.inbox {
            report.fixture = i.snapshot();
        }
        report
    }
}

pub fn fixture_url() -> String {
    // Page makes no edits itself. Unexpected extra or untrusted edits stick as
    // invalid; menu dismissal and native renderer effects are separate signals.
    let html = r#"<!doctype html><body style='margin:0;background:#1a4c7e;font:24px Arial'><div id='editor' contenteditable='true' spellcheck='false' style='width:260px;height:100px'></div><script>
const e = document.getElementById('editor');
const focused = () => document.hasFocus() && document.activeElement === e;
const types = ['insertText', 'historyUndo', 'historyRedo'];
const values = ['x', '', 'x'];
let changes = 0;
let invalid = false;
document.title = 'edit-history:ready';
e.addEventListener('click', event => {
  invalid = !event.isTrusted || !focused();
  document.title = invalid ? 'edit-history:invalid' : 'edit-history:focused';
}, {once: true});
e.addEventListener('input', event => {
  const i = changes++;
  invalid ||= !event.isTrusted || !focused() || event.inputType !== types[i] || e.textContent !== values[i];
  document.title = invalid ? 'edit-history:invalid' : `edit-history:${changes}:${event.inputType}:${e.textContent.length}`;
});
</script></body>"#;
    let mut url = String::from("data:text/html,");
    for b in html.bytes() {
        use std::fmt::Write;
        write!(url, "%{b:02X}").expect("String write");
    }
    url
}
