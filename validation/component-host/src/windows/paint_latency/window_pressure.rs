//! Explicit paired-window pressure only. The sink retains genuine title and
//! optionally B reload callbacks in one lane. Closed bypasses the full lane.
use super::*;
use rust_cef_gpui::ports::{BrowserCommand, BrowserHandle, CommandId};
use rust_cef_gpui::test_support::{
    CommandPressure, PRESSURE_EVENT_CAPACITY, PRESSURE_TITLE_PREFIX, ProbeBrowserEvent,
    WindowPeerFlush,
};

#[derive(Default)]
pub(crate) struct EventLane {
    armed: AtomicBool,
    state: Mutex<Option<Lane>>,
}

struct Lane {
    a: BrowserId,
    b: BrowserId,
    ordinary: Vec<BrowserEventEnvelope>,
    rejected: Option<u64>,
    terminals: Vec<ProbeBrowserEvent>,
    depth_at_a_closed: Option<usize>,
    failed: bool,
    mixed: Option<ReloadSeed>,
}

const SEED_EVENTS: usize = 4;
const SEED_MASK: u8 = 0x0f;

#[derive(Default)]
struct ReloadSeed {
    mask: u8,
    closed: bool,
    duplicate_observations: u64,
    command: Option<CommandId>,
}

fn seed_kind(e: &BrowserEvent) -> Option<(u8, &'static str)> {
    match e {
        BrowserEvent::LoadingChanged { loading: true, .. } => Some((1, "loading-start")),
        BrowserEvent::LoadStarted {
            is_main_frame: true,
        } => Some((2, "main-load-start")),
        BrowserEvent::LoadFinished {
            is_main_frame: true,
            ..
        } => Some((4, "main-load-end")),
        BrowserEvent::LoadingChanged { loading: false, .. } => Some((8, "loading-end")),
        _ => None,
    }
}

impl Lane {
    fn ordered(&self, e: &BrowserEventEnvelope) -> bool {
        e.sequence != 0
            && self
                .ordinary
                .last()
                .is_none_or(|prev| prev.sequence < e.sequence)
    }
    fn seed_count(&self) -> usize {
        self.mixed
            .as_ref()
            .map_or(0, |s| s.mask.count_ones() as usize)
    }
}

impl EventLane {
    fn arm(&self, a: BrowserId, b: BrowserId, mixed: bool) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if a == b || a.runtime() != b.runtime() || state.is_some() {
            return Err("invalid or already armed pressure pair".into());
        }
        *state = Some(Lane {
            a,
            b,
            ordinary: Vec::with_capacity(PRESSURE_EVENT_CAPACITY),
            rejected: None,
            terminals: Vec::with_capacity(2),
            depth_at_a_closed: None,
            failed: false,
            mixed: mixed.then(ReloadSeed::default),
        });
        self.armed.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn observe(&self, event: &BrowserEventEnvelope) -> Option<PublishResult> {
        if !self.armed.load(Ordering::Acquire) {
            return None;
        }
        let mut state = self.state.lock().unwrap();
        let lane = state.as_mut()?;
        if matches!(event.event, BrowserEvent::Closed) {
            if event.browser == lane.a || event.browser == lane.b {
                if lane.terminals.len() == 2 {
                    lane.failed = true;
                } else {
                    lane.terminals.push((
                        (event.browser.runtime().value(), event.browser.local()),
                        event.sequence,
                    ));
                    if event.browser == lane.a {
                        lane.depth_at_a_closed = Some(lane.ordinary.len());
                    }
                }
            }
            return None;
        }
        if event.browser == lane.b && lane.mixed.as_ref().is_some_and(|s| !s.closed) {
            if matches!(
                event.event,
                BrowserEvent::LoadCancelled {
                    is_main_frame: true,
                    ..
                } | BrowserEvent::LoadFailed {
                    is_main_frame: true,
                    ..
                }
            ) {
                lane.failed = true;
                return None;
            }
            if let Some((bit, _)) = seed_kind(&event.event) {
                if lane.failed || !lane.ordered(event) {
                    lane.failed = true;
                    return Some(PublishResult::Rejected);
                }
                let seed = lane.mixed.as_mut().expect("armed mixed lane");
                if seed.mask & bit != 0 {
                    seed.duplicate_observations = seed.duplicate_observations.saturating_add(1);
                    return None;
                }
                seed.mask |= bit;
                lane.ordinary.push(event.clone());
                return Some(PublishResult::Published);
            }
        }
        let BrowserEvent::TitleChanged(title) = &event.event else {
            return None;
        };
        if event.browser != lane.a {
            return None;
        }
        let Some(step) = title.strip_prefix(PRESSURE_TITLE_PREFIX) else {
            return None;
        };
        if lane.failed
            || lane.rejected.is_some()
            || lane.mixed.as_ref().is_some_and(|s| !s.closed)
            || step.parse::<usize>().ok() != Some(lane.ordinary.len() + 1 - lane.seed_count())
            || !lane.ordered(event)
        {
            lane.failed = true;
            return Some(PublishResult::Rejected);
        }
        if lane.ordinary.len() == PRESSURE_EVENT_CAPACITY {
            lane.rejected = Some(event.sequence);
            return Some(PublishResult::Rejected);
        }
        lane.ordinary.push(event.clone());
        Some(PublishResult::Published)
    }

    pub(super) fn both_closed(&self) -> Result<bool, String> {
        let state = self.state.lock().unwrap();
        let lane = state
            .as_ref()
            .filter(|l| !l.failed)
            .ok_or("pressure ledger faulted")?;
        Ok(lane.terminals.len() == 2)
    }

    fn finish_reload_seed(&self, id: CommandId, ready: bool) -> Result<bool, String> {
        let mut state = self.state.lock().unwrap();
        let lane = state
            .as_mut()
            .filter(|l| !l.failed)
            .ok_or("mixed pressure reload seed faulted")?;
        if id.browser != lane.b || id.sequence == 0 {
            return Err("mixed seed command belongs to another Browser".into());
        }
        let seed = lane.mixed.as_mut().ok_or("missing mixed seed")?;
        if seed.closed {
            return Err("mixed seed already frozen".into());
        }
        if seed.mask != SEED_MASK || !ready {
            return Ok(false);
        }
        if lane.ordinary.len() != SEED_EVENTS {
            return Err("mixed seed must contribute four real events".into());
        }
        seed.command = Some(id);
        seed.closed = true;
        Ok(true)
    }

    fn title_goal(&self, a: BrowserId, b: BrowserId) -> Result<usize, String> {
        let state = self.state.lock().unwrap();
        let lane = state
            .as_ref()
            .filter(|l| !l.failed && l.a == a && l.b == b)
            .ok_or("missing original event pair")?;
        if lane
            .mixed
            .as_ref()
            .is_some_and(|m| !m.closed || m.command.is_none())
        {
            return Err("mixed reload seed not frozen".into());
        }
        Ok(PRESSURE_EVENT_CAPACITY + 1 - lane.seed_count())
    }

    fn seen(&self) -> Result<usize, String> {
        let state = self.state.lock().unwrap();
        let lane = state
            .as_ref()
            .filter(|l| !l.failed)
            .ok_or("title pressure ledger faulted")?;
        Ok(lane.ordinary.len() - lane.seed_count() + usize::from(lane.rejected.is_some()))
    }

    pub(super) fn receipt(
        &self,
        command: CommandPressure,
        authoritative_title: usize,
        terminal_image_counts: [u64; 3],
        menu: Option<window_menu::Retired>,
        peer_flush: Option<WindowPeerFlush>,
    ) -> Result<String, String> {
        let state = self.state.lock().unwrap();
        let lane = state
            .as_ref()
            .filter(|l| !l.failed)
            .ok_or("pressure ledger faulted")?;
        let rejected = lane.rejected.ok_or("missing refused title sequence")?;
        let [((ar, al), first), ((br, bl), second)] = lane.terminals.as_slice() else {
            return Err("expected both original Closed envelopes".into());
        };
        if (*ar, *al) != (lane.a.runtime().value(), lane.a.local())
            || (*br, *bl) != (lane.b.runtime().value(), lane.b.local())
            || !(0 < rejected && rejected < *first && *first < *second)
            || lane.ordinary.len() != PRESSURE_EVENT_CAPACITY
            || lane.depth_at_a_closed != Some(PRESSURE_EVENT_CAPACITY)
            || authoritative_title != PRESSURE_EVENT_CAPACITY + 1 - lane.seed_count()
            || !menu.as_ref().map_or_else(
                || command.complete_without_menu(),
                |r| command.complete_with_window_menu(r.cut, peer_flush),
            )
        {
            return Err("original pressure/terminal receipts did not correlate".into());
        }
        let (r, n) = command.close_task.ok_or("missing pressured Close task")?;
        if r != *ar {
            return Err("pressured Close belongs to another Runtime".into());
        }
        let mut fields = vec![
            ("a_browser", identity(lane.a)),
            ("b_browser", identity(lane.b)),
            ("command_thread", command.thread.to_string()),
            ("accepted_commands", command.accepted.to_string()),
            (
                "command_pending_at_close",
                command.pending_at_close.to_string(),
            ),
            ("dispatched_commands", command.dispatched.to_string()),
            ("close_task", format!("[{r},{n}]")),
            ("overflow_refused", bool_json(command.overflow_refused)),
            (
                "new_command_refused_after_intent",
                bool_json(command.new_command_refused_after_intent),
            ),
            (
                "menu_pending_at_close",
                command.menu_pending_at_close.to_string(),
            ),
            (
                "retained_title_events",
                (lane.ordinary.len() - lane.seed_count()).to_string(),
            ),
            (
                "first_title_sequence",
                lane.ordinary[lane.seed_count()].sequence.to_string(),
            ),
            (
                "last_retained_title_sequence",
                lane.ordinary[PRESSURE_EVENT_CAPACITY - 1]
                    .sequence
                    .to_string(),
            ),
            ("rejected_title_sequence", rejected.to_string()),
            (
                "resynced_title_before_close",
                authoritative_title.to_string(),
            ),
            (
                "event_depth_at_a_closed",
                PRESSURE_EVENT_CAPACITY.to_string(),
            ),
            // receipt() is reached only after these owner-thread checks in body().
            ("a_b_hwnds_live_after_first_closed", "true".into()),
            ("a_view_consumed_terminal_in_original_window", "true".into()),
            (
                "a_terminal_images_created_released_before_released_after",
                format!("{terminal_image_counts:?}"),
            ),
            (
                "closed_events",
                format!("[[[{ar},{al}],{first}],[[{br},{bl}],{second}]]"),
            ),
        ];
        if let Some(seed) = &lane.mixed {
            let id = seed.command.ok_or("no original B reload command")?;
            if !seed.closed || seed.mask != SEED_MASK || id.browser != lane.b {
                return Err("original mixed reload seed incomplete".into());
            }
            let mut rows = Vec::with_capacity(SEED_EVENTS);
            for e in &lane.ordinary[..SEED_EVENTS] {
                let (_, kind) = seed_kind(&e.event).ok_or("foreign seed event kind")?;
                if e.browser != lane.b {
                    return Err("event seed owner changed".into());
                }
                rows.push(format!("[{},{}]", quote(kind), e.sequence));
            }
            fields.push((
                "mixed_events",
                object(vec![
                    ("seed_browser", identity(lane.b)),
                    (
                        "seed_reload_command",
                        format!("[{br},{bl},{}]", id.sequence),
                    ),
                    ("seed_events", format!("[{}]", rows.join(","))),
                    (
                        "duplicate_seed_observations_ignored",
                        seed.duplicate_observations.to_string(),
                    ),
                    ("retained_total", lane.ordinary.len().to_string()),
                    ("rejected_ordinary", "1".into()),
                ]),
            ));
        }
        if let Some(menu) = menu {
            fields.push(("managed_menu", menu.json));
        }
        if let Some(source) = command.held_window_menu_at_close.and_then(|c| c.source) {
            fields.push(("source_operation_at_close", format!("{source:?}")));
        }
        if let Some(find) = command.held_window_menu_at_close.and_then(|c| c.find) {
            fields.push(("find_operation_at_close", format!("{find:?}")));
        }
        if let Some(prints) = command.held_window_prints_at_close {
            fields.push(("print_operations_at_close", format!("{prints:?}")));
        }
        if let Some([runtime, profile, sequence]) = command.held_cookie_flush_at_close {
            let (owner_runtime, owner) = command
                .peer_flush_browser_at_close
                .ok_or("missing B flush Browser at pressured Close")?;
            fields.push((
                "peer_flush_marker_at_close",
                format!("[{runtime},{profile},{sequence}]"),
            ));
            fields.push((
                "peer_flush_browser_at_close",
                format!("[{owner_runtime},{owner}]"),
            ));
        }
        Ok(object(fields))
    }
}

pub(super) struct FirstClose {
    pub(super) command: CommandPressure,
    pub(super) title: usize,
    pub(super) menu: Option<window_menu::Retired>,
    pub(super) peer_flush: Option<WindowPeerFlush>,
}

pub(super) async fn seed_from_live_reload(
    s: &mut Scene<'_>,
    a: BrowserId,
    peer: &BrowserHandle,
    deadline: Instant,
) -> Result<(), String> {
    let before = peer.snapshot();
    if !before.is_ready()
        || before.navigation.is_loading
        || before.navigation.address.is_empty()
        || before.navigation.title != "menu-bridge-ready"
    {
        return Err("original B fixture is not settled before mixed reload".into());
    }
    s.stats.window_pressure.arm(a, before.browser, true)?;
    let command = peer
        .submit(BrowserCommand::Reload)
        .map_err(|e| format!("original B reload: {e:?}"))?;
    loop {
        let after = peer.snapshot();
        if after.browser != before.browser || !after.is_ready() {
            return Err("B changed during mixed reload seed".into());
        }
        let settled = !after.navigation.is_loading
            && after.navigation.address == before.navigation.address
            && after.navigation.title == "menu-bridge-ready";
        if s.stats
            .window_pressure
            .finish_reload_seed(command, settled)?
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("B mixed reload seed timed out".into());
        }
        s.service(false, None).await?;
    }
}

pub(super) async fn close_first(
    s: &mut Scene<'_>,
    first: &rust_cef_gpui::ports::BrowserHandle,
    peer: BrowserId,
    menu_window: Option<u64>,
    peer_flush: Option<WindowPeerFlush>,
    find: Option<rust_cef_gpui::find::FindOperationId>,
    mixed: bool,
    print_owners: Option<[super::window_print::Owner<'_>; 2]>,
    deadline: Instant,
) -> Result<FirstClose, String> {
    let a = first.snapshot().browser;
    if !mixed {
        s.stats.window_pressure.arm(a, peer, false)?;
    }
    let goal = s.stats.window_pressure.title_goal(a, peer)?;
    for step in 1..=goal {
        s.rt.probe_pressure_title(a, step as u16)
            .map_err(|e| format!("real pressure title {step}: {e:?}"))?;
        while s.stats.window_pressure.seen()? < step {
            if Instant::now() >= deadline {
                return Err(format!("pressure title {step} timeout"));
            }
            s.service(false, None).await?;
        }
    }
    let menu = if let Some(window) = menu_window {
        Some(window_menu::Pending::open(s, a, window, find, deadline).await?)
    } else {
        None
    };
    let snapshot = first.snapshot();
    let title = snapshot
        .navigation
        .title
        .strip_prefix(PRESSURE_TITLE_PREFIX)
        .and_then(|s| s.parse::<usize>().ok());
    if !snapshot.is_ready() || title != Some(goal) {
        return Err("first Browser did not resync to the refused title".into());
    }
    if let Some(owners) = print_owners {
        if menu.is_none() || peer_flush.is_none() || !mixed {
            return Err("print cut requires the original mixed menu and flush cut".into());
        }
        let print = s
            .out
            .window_print
            .insert(super::window_print::Held::arm(s.rt, &owners)?);
        print.present(s.cx, s.rt, &owners).await?;
    }
    let prints = s
        .out
        .window_print
        .as_ref()
        .map(super::window_print::Held::cut);
    let posted = match &menu {
        Some(m) => s.rt.probe_command_pressure_close_with_window_menu(
            m.request(),
            m.cut().gpui_window,
            peer_flush,
            m.source,
            m.find,
            prints,
        ),
        None => s.rt.probe_command_pressure_close_without_menu(a),
    };
    posted.map_err(|e| format!("two-window pressure close: {e:?}"))?;
    // Consume lifecycle wakes without dispatching until a single explicit
    // consume on A's actual owner can check closed-source image retirement.
    loop {
        if let Some(c) = s.rt.probe_command_pressure_status(a) {
            if let Some(error) = &c.error {
                return Err(format!("pressure task: {error}"));
            }
            if first.snapshot().state == SessionState::Closed
                && s.stats.closed.load(Ordering::Acquire) == 1
            {
                let retired = if let Some(m) = menu {
                    Some(m.retire(s.rt, &c, peer_flush)?)
                } else if c.complete_without_menu() {
                    None
                } else {
                    return Err("original no-menu pressure Close receipt incomplete".into());
                };
                return Ok(FirstClose {
                    command: c,
                    title: title.expect("qualified title"),
                    menu: retired,
                    peer_flush,
                });
            }
        }
        if Instant::now() >= deadline {
            return Err("pressured first Browser did not close".into());
        }
        if turn(s.cx, s.rt, s.receiver, false).await? {
            s.out.wake_consumed += 1;
        }
        s.stats.check()?;
    }
}
