//! Explicit real-GPUI / real-CEF fixture with injected platform events, NOT Win32/OLE proof.
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn main() {
    std::process::exit(2);
}
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn main() {
    std::process::exit(windows::run());
}
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "file-drag-events/ole_target.rs"]
mod ole_target;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows {
    use super::ole_target::OwnedOleTarget;
    const OLE_TITLE: &str = "GPUI-CEF owned OLE target R58";
    const MANUAL_OLE_TITLE: &str = "OLE TARGET - do not click";
    use gpui::{
        App, AppContext, AsyncApp, Bounds, Context, Entity, ExternalPaths, FileDropEvent,
        IntoElement, MouseButton, MouseUpEvent, PlatformInput, Render, WeakEntity, Window,
        WindowBounds, WindowHandle, WindowOptions, canvas, div, point, prelude::*, px, rgb, size,
    };
    use rust_cef_gpui::{
        CefBrowser, Runtime, RuntimeServices, UiComposition,
        contracts::{
            BrowserConfig, ProfileStorage, RenderPreference, RuntimeConfig, RuntimeFiles,
            SessionState, ThreadMode, TrustedContentPolicy, Viewport,
        },
        dispatcher::{self, UiWakeReceiver, UiWakeSender},
        file_drop::FileDragPhase,
        ports::{
            BrowserEvent, BrowserEventEnvelope, BrowserEventSink, BrowserHandle, CloseMode,
            CloseResult, EngineError, FrameInfo, FrameSink, OwnedFrame, PublishResult, SurfaceKind,
            UiDispatcher,
        },
        runtime::ShutdownStatus,
    };
    use std::{
        cell::Cell,
        future::Future,
        io::Write,
        num::NonZeroUsize,
        path::{Path, PathBuf},
        rc::Rc,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        task::Poll,
        time::{Duration, Instant},
    };

    thread_local! { static APP_BORROW: Cell<bool> = const { Cell::new(false) }; }
    struct Borrow;
    impl Borrow {
        fn enter() -> Self {
            APP_BORROW.with(|x| assert!(!x.replace(true)));
            Self
        }
    }
    impl Drop for Borrow {
        fn drop(&mut self) {
            APP_BORROW.with(|x| x.set(false));
        }
    }
    fn app<R>(cx: &mut AsyncApp, f: impl FnOnce(&mut App) -> R) -> R {
        let _borrow = Borrow::enter();
        cx.update(f)
    }
    fn update<R>(
        cx: &mut AsyncApp,
        root: WindowHandle<Root>,
        f: impl FnOnce(&mut Root, &mut Window, &mut Context<Root>) -> R,
    ) -> Result<R, String> {
        let _borrow = Borrow::enter();
        root.update(cx, f).map_err(|e| e.to_string())
    }
    fn quote(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
    fn object(rows: Vec<(&str, String)>) -> String {
        format!(
            "{{{}}}",
            rows.into_iter()
                .map(|(k, v)| format!("{}:{v}", quote(k)))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
    fn array(rows: &[String]) -> String {
        format!("[{}]", rows.join(","))
    }
    fn write_new(p: &Path, data: &[u8]) -> Result<(), String> {
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(p)
            .map_err(|e| e.to_string())?;
        f.write_all(data).map_err(|e| e.to_string())
    }
    #[derive(Clone)]
    struct Options {
        helper: PathBuf,
        report: PathBuf,
        gate: PathBuf,
        mode: ThreadMode,
        backend: RenderPreference,
        real_ole: bool,
        ole_approved: bool,
        ole_close_while_dragging: bool,
        manual_source_focus: bool,
        control: Option<PathBuf>,
        nonce: i32,
    }
    impl Options {
        fn ole_title(&self) -> &'static str {
            if self.manual_source_focus {
                MANUAL_OLE_TITLE
            } else {
                OLE_TITLE
            }
        }
    }
    fn options() -> Result<Options, String> {
        let mut args = std::env::args().skip(1);
        let (mut helper, mut report, mut gate) = (None, None, None);
        let (mut mode, mut backend) = (None, None);
        let (mut real_ole, mut ole_approved, mut control, mut nonce) = (false, false, None, 0);
        let mut ole_close_while_dragging = false;
        let mut manual_source_focus = false;
        while let Some(k) = args.next() {
            let v = args.next().ok_or("option lacks value")?;
            match k.as_str() {
                "--input-source" => {
                    real_ole = match v.as_str() {
                        "ole" => true,
                        "injected" => false,
                        _ => return Err("known input source required".into()),
                    }
                }
                "--ole-input-approved" => {
                    if v != "true" {
                        return Err("explicit OLE approval required".into());
                    }
                    ole_approved = true;
                }
                "--ole-close-while-dragging" if v == "true" && !ole_close_while_dragging => {
                    ole_close_while_dragging = true;
                }
                "--ole-manual-source-focus" if v == "true" && !manual_source_focus => {
                    manual_source_focus = true;
                }
                "--ole-control" => control = Some(PathBuf::from(v)),
                "--ole-nonce" => nonce = v.parse::<i32>().map_err(|_| "invalid OLE nonce")?,
                "--helper" => helper = Some(PathBuf::from(v)),
                "--report" => report = Some(PathBuf::from(v)),
                "--observer-gate" => gate = Some(PathBuf::from(v)),
                "--mode" => {
                    mode = Some(match v.as_str() {
                        "Integrated" => ThreadMode::Integrated,
                        "Threaded" => ThreadMode::Threaded,
                        _ => return Err("explicit mode required".into()),
                    })
                }
                "--backend" => {
                    backend = Some(match v.as_str() {
                        "Software" => RenderPreference::SoftwareRequired,
                        "D3d11" => RenderPreference::D3d11Required,
                        _ => return Err("required backend required".into()),
                    })
                }
                _ => return Err("unknown option".into()),
            }
        }
        let o = Options {
            helper: helper.ok_or("helper required")?,
            report: report.ok_or("report required")?,
            gate: gate.ok_or("gate required")?,
            mode: mode.ok_or("mode required")?,
            backend: backend.ok_or("backend required")?,
            real_ole,
            ole_approved,
            ole_close_while_dragging,
            manual_source_focus,
            control,
            nonce,
        };
        if !o.helper.is_absolute() || !o.report.is_absolute() || !o.gate.is_absolute() {
            return Err("explicit absolute paths required".into());
        }
        if o.real_ole
            && (!o.ole_approved
                || !o
                    .control
                    .as_ref()
                    .is_some_and(|p| p.is_absolute() && p.is_dir())
                || o.nonce <= 0
                || o.mode != ThreadMode::Threaded
                || o.backend != RenderPreference::SoftwareRequired)
        {
            return Err(
                "real OLE currently requires explicit approval/control/nonce and Threaded Software"
                    .into(),
            );
        }
        if !o.real_ole
            && (o.ole_approved
                || o.ole_close_while_dragging
                || o.manual_source_focus
                || o.control.is_some()
                || o.nonce != 0)
        {
            return Err("OLE options require OLE source".into());
        }
        Ok(o)
    }
    #[derive(Default)]
    struct Stats {
        created: AtomicU64,
        closed: AtomicU64,
        blue: AtomicU64,
        last_blue: Mutex<Option<FrameInfo>>,
        violations: AtomicU64,
        error: Mutex<Option<String>>,
    }
    impl Stats {
        fn callback(&self) {
            if APP_BORROW.with(Cell::get) {
                self.violations.fetch_add(1, Ordering::Relaxed);
            }
        }
        fn check(&self) -> Result<(), String> {
            self.error.lock().unwrap().clone().map_or(Ok(()), Err)
        }
    }
    struct Events(Arc<Stats>);
    impl BrowserEventSink for Events {
        fn publish(&self, e: BrowserEventEnvelope) -> PublishResult {
            self.0.callback();
            match e.event {
                BrowserEvent::Created => {
                    self.0.created.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::Closed => {
                    self.0.closed.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::CreateFailed(e) | BrowserEvent::CaptureFailed(e) => {
                    self.0
                        .error
                        .lock()
                        .unwrap()
                        .get_or_insert(format!("native: {e:?}"));
                }
                BrowserEvent::RendererTerminated { .. } => {
                    self.0
                        .error
                        .lock()
                        .unwrap()
                        .get_or_insert("renderer terminated".into());
                }
                _ => {}
            }
            PublishResult::Published
        }
    }
    struct Frames(Arc<Stats>);
    impl FrameSink for Frames {
        fn publish(&self, f: OwnedFrame) -> PublishResult {
            self.0.callback();
            let i = ((f.info.height as usize / 2) * f.info.width as usize
                + f.info.width as usize / 2)
                * 4;
            if f.info.surface == SurfaceKind::View
                && f.info.width > 0
                && f.info.height > 0
                && f.info.surface_generation > 0
                && f.info.sequence > 0
                && f.pixels.get(i..i + 4) == Some(&[126, 76, 26, 255])
            {
                *self.0.last_blue.lock().unwrap() = Some(f.info);
                self.0.blue.fetch_add(1, Ordering::Relaxed);
            }
            PublishResult::Published
        }
    }
    struct Wake(UiWakeSender, Arc<Stats>);
    impl UiDispatcher for Wake {
        fn wake(&self) -> Result<(), EngineError> {
            self.1.callback();
            self.0.wake()
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Overlay {
        None,
        Block,
        Consume,
    }
    struct Root {
        browser: Option<Entity<CefBrowser>>,
        overlay: Overlay,
        generation: u64,
        green: bool,
        painted: Rc<Cell<u64>>,
        consumed: Rc<Cell<u64>>,
        renders: u64,
    }
    impl Render for Root {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.renders += 1;
            let mut content = div()
                .absolute()
                .left(px(40.))
                .top(px(60.))
                .w(px(320.))
                .h(px(240.))
                .relative()
                .bg(if self.green {
                    rgb(0x00aa44)
                } else {
                    rgb(0x1a4c7e)
                });
            if let Some(view) = self.browser.as_ref() {
                content = content.child(view.clone());
            }
            if self.overlay != Overlay::None {
                let overlay = div()
                    .id("file-overlay")
                    .absolute()
                    .inset_0()
                    .bg(rgb(0x664411));
                let overlay = if self.overlay == Overlay::Block {
                    overlay.occlude().into_any_element()
                } else {
                    let count = self.consumed.clone();
                    overlay
                        .on_drop::<ExternalPaths>(move |_, _, _| count.set(count.get() + 1))
                        .into_any_element()
                };
                content = content.child(overlay);
            }
            let paint = self.painted.clone();
            let generation = self.generation;
            div()
                .relative()
                .size_full()
                .bg(if self.green {
                    rgb(0x00aa44)
                } else {
                    rgb(0x151515)
                })
                .child(content)
                .child(
                    canvas(|_, _, _| (), move |_, _, _, _| paint.set(generation))
                        .absolute()
                        .inset_0(),
                )
        }
    }
    #[derive(Default)]
    struct Report {
        error: Option<String>,
        checks: Vec<String>,
        observations: Vec<String>,
        runtime: Option<String>,
        ready_frame: Option<String>,
        ole_verified: bool,
        ole_active_close_verified: bool,
        drag_retirement: Option<String>,
        finished: bool,
        green: bool,
        weak_none: bool,
        exit: i32,
    }
    fn publish_manual_plan(path: &Path, bytes: &[u8]) -> Result<(), String> {
        let pending = path.with_extension("pending");
        write_new(&pending, bytes)?;
        let published = std::fs::hard_link(&pending, path)
            .map_err(|e| format!("manual OLE plan publication: {e}"));
        if let Err(e) = std::fs::remove_file(&pending) {
            eprintln!("manual OLE temporary plan cleanup: {e}");
        }
        published
    }

    fn matching_marker(path: &Path, expected: &str) -> Result<bool, String> {
        match std::fs::read_to_string(path) {
            Ok(value) if value == expected => Ok(true),
            Ok(_) => Err("foreign or malformed OLE control marker".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("OLE control read: {error}")),
        }
    }

    fn manual_focus_failed(control: &Path, identity: &str) -> Result<(), String> {
        if matching_marker(
            &control.join("ole-driver-manual-failed.request"),
            &format!("ole-manual-failed:{identity}"),
        )? {
            return Err("manual OLE driver stopped before input".into());
        }
        Ok(())
    }

    fn has_dom_drop(title: &str) -> bool {
        title.split('|').any(|part| part.starts_with("drop:"))
    }
    fn browser_id_json(id: rust_cef_gpui::contracts::BrowserId) -> String {
        format!("[{},{}]", id.runtime().value(), id.local())
    }
    fn digest(b: &[u8]) -> u32 {
        b.iter().fold(2166136261u32, |h, b| {
            (h ^ u32::from(*b)).wrapping_mul(16777619)
        })
    }
    fn page(real_ole: bool) -> String {
        let html = "<body style='margin:0;background:rgb(26,76,126);height:100vh'><script>let n=0;document.title='ready';document.addEventListener('dragenter',e=>{e.preventDefault();document.title='enter:'+ ++n+':'+e.clientX+':'+e.clientY});document.addEventListener('dragover',e=>{e.preventDefault();document.title='over:'+n+':'+e.clientX+':'+e.clientY});document.addEventListener('dragleave',()=>{document.title='leave:'+n});document.addEventListener('drop',async e=>{e.preventDefault();let fs=[...e.dataTransfer.files],a=[];for(let f of fs){let b=new Uint8Array(await f.arrayBuffer()),h=2166136261;for(let x of b)h=Math.imul(h^x,16777619)>>>0;a.push(f.name+':'+b.length+':'+h)}document.title='drop:'+n+':'+fs.length+':'+a.join(',')})</script></body>";
        let ole_html = "<body style='margin:0;background:rgb(26,76,126);height:100vh'><script>let n=0,log=[],failed=false;function emit(s){if(failed)return;if(log.length>=64){failed=true;document.title='ole-log-overflow';return}log.push(s);document.title=log.join('|')}document.title='ready';document.addEventListener('dragenter',e=>{e.preventDefault();emit('enter:'+ ++n+':'+e.clientX+':'+e.clientY)});document.addEventListener('dragover',e=>{e.preventDefault();emit('over:'+n+':'+e.clientX+':'+e.clientY)});document.addEventListener('dragleave',()=>emit('leave:'+n));document.addEventListener('drop',async e=>{e.preventDefault();let fs=[...e.dataTransfer.files],a=[];for(let f of fs){let b=new Uint8Array(await f.arrayBuffer()),h=2166136261;for(let x of b)h=Math.imul(h^x,16777619)>>>0;a.push(f.name+':'+b.length+':'+h)}emit('drop:'+n+':'+fs.length+':'+a.join(','))})</script></body>";
        let html = if real_ole { ole_html } else { html };
        let mut url = String::from("data:text/html,");
        for b in html.bytes() {
            if b.is_ascii_alphanumeric() {
                url.push(b as char)
            } else {
                url.push_str(&format!("%{b:02X}"))
            }
        }
        url
    }
    async fn tick(cx: &mut AsyncApp, receiver: &UiWakeReceiver) -> Result<bool, String> {
        let mut timer = std::pin::pin!(cx.background_executor().timer(Duration::from_millis(5)));
        let mut wake = std::pin::pin!(receiver.recv());
        std::future::poll_fn(|ctx| {
            if let Poll::Ready(ok) = wake.as_mut().poll(ctx) {
                return Poll::Ready(if ok {
                    Ok(true)
                } else {
                    Err("wake closed".into())
                });
            }
            if timer.as_mut().poll(ctx).is_ready() {
                return Poll::Ready(Ok(false));
            }
            Poll::Pending
        })
        .await
    }
    async fn runtime_tick(
        cx: &mut AsyncApp,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
    ) -> Result<bool, String> {
        let woke = tick(cx, receiver).await?;
        if rt.mode() == ThreadMode::Integrated {
            if APP_BORROW.with(Cell::get) {
                return Err("pump inside GPUI borrow".into());
            }
            rt.pump().map_err(|e| format!("pump: {e:?}"))?;
        }
        Ok(woke)
    }
    struct Harness<'a> {
        cx: &'a mut AsyncApp,
        rt: &'a mut Runtime,
        receiver: &'a UiWakeReceiver,
        stats: &'a Stats,
        root: WindowHandle<Root>,
        weak: WeakEntity<CefBrowser>,
        composition: UiComposition,
        handle: BrowserHandle,
        report: &'a mut Report,
        paths: Vec<PathBuf>,
        url: String,
        real_ole: bool,
    }
    impl Harness<'_> {
        async fn turn(&mut self) -> Result<(), String> {
            if runtime_tick(self.cx, self.rt, self.receiver).await? {
                let composition = &mut self.composition;
                let r = app(self.cx, |cx| composition.dispatch(cx));
                if !r.failures.is_empty() {
                    return Err(format!("composition: {:?}", r.failures));
                }
            }
            self.stats.check()
        }
        async fn until(
            &mut self,
            label: &str,
            ready: impl FnMut(&mut Self) -> Result<bool, String>,
        ) -> Result<(), String> {
            self.until_checked(label, false, ready).await
        }
        async fn until_checked(
            &mut self,
            label: &str,
            strict_deadline: bool,
            mut ready: impl FnMut(&mut Self) -> Result<bool, String>,
        ) -> Result<(), String> {
            let until = Instant::now() + Duration::from_secs(20);
            loop {
                self.stats.check()?;
                if (!strict_deadline || Instant::now() < until)
                    && ready(self)?
                    && (!strict_deadline || Instant::now() < until)
                {
                    return Ok(());
                }
                if Instant::now() >= until {
                    let _ = self.observe(&format!("timeout-{label}"));
                    return Err(format!(
                        "{label} timeout; title={} snapshot={:?}",
                        self.handle.snapshot().navigation.title,
                        self.handle.input_snapshot()
                    ));
                }
                self.turn().await?;
            }
        }
        fn observe(&mut self, label: &str) -> Result<(), String> {
            let s = self
                .handle
                .input_snapshot()
                .map_err(|e| format!("snapshot: {e:?}"))?;
            let (active, paint, renders, consumed) = update(self.cx, self.root, |r, w, _| {
                (
                    w.is_window_active(),
                    r.painted.get(),
                    r.renders,
                    r.consumed.get(),
                )
            })?;
            self.report.observations.push(object(vec![
                ("label", quote(label)),
                ("title", quote(&self.handle.snapshot().navigation.title)),
                ("phase", quote(&format!("{:?}", s.file_drag_phase))),
                ("epoch", s.epoch.to_string()),
                ("inflight", s.inflight.to_string()),
                ("unconfirmed", s.native_execution_unconfirmed.to_string()),
                ("cleanup_pending", s.cleanup_pending.to_string()),
                ("active_window", active.to_string()),
                ("paint_generation", paint.to_string()),
                ("renders", renders.to_string()),
                ("overlay_consumed", consumed.to_string()),
            ]));
            if active && !self.real_ole {
                return Err("passive fixture target unexpectedly active at observation".into());
            }
            if self.handle.snapshot().navigation.address != self.url {
                return Err("file navigated".into());
            }
            Ok(())
        }
        fn inject(&mut self, event: PlatformInput) -> Result<(), String> {
            update(self.cx, self.root, |_, window, cx| {
                window.dispatch_event(event, cx);
            })
        }
        fn position(&mut self) -> Result<gpui::Point<gpui::Pixels>, String> {
            let weak = self.weak.clone();
            let d = update(self.cx, self.root, |_, _, cx| {
                weak.upgrade()
                    .ok_or("View gone")
                    .map(|v| v.read(cx).diagnostics())
            })??;
            Ok(point(
                px(d.surface_bounds.0 + 30.),
                px(d.surface_bounds.1 + 30.),
            ))
        }
        async fn enter(&mut self, n: u32, over: bool) -> Result<(), String> {
            let position = self.position()?;
            self.inject(PlatformInput::FileDrop(FileDropEvent::Entered {
                position,
                paths: ExternalPaths(self.paths.clone().into_iter().collect()),
            }))?;
            let title = format!("enter:{n}:30:30");
            self.until("UI Enter", |h| {
                Ok(h.handle.snapshot().navigation.title == title
                    && h.handle.input_snapshot().is_ok_and(|s| {
                        s.file_drag_phase == FileDragPhase::Entered
                            && s.inflight == 0
                            && !s.native_execution_unconfirmed
                    }))
            })
            .await?;
            self.observe(&format!("enter-{n}"))?;
            if over {
                self.inject(PlatformInput::FileDrop(FileDropEvent::Pending { position }))?;
                let title = format!("over:{n}:30:30");
                self.until("UI Over", |h| {
                    Ok(h.handle.snapshot().navigation.title == title
                        && h.handle.input_snapshot().is_ok_and(|s| {
                            s.file_drag_phase == FileDragPhase::Over
                                && s.inflight == 0
                                && !s.native_execution_unconfirmed
                        }))
                })
                .await?;
                self.observe(&format!("over-{n}"))?;
            }
            Ok(())
        }
        fn submit_translated(&mut self) -> Result<(), String> {
            // Raw FileDrop::Submit calls App::activate(true). Intentionally NOT tested/invoked.
            let position = self.position()?;
            self.inject(PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count: 1,
            }))
        }
        async fn idle(&mut self, title: &str) -> Result<(), String> {
            self.until("cancel/idle", |h| {
                Ok(h.handle.snapshot().navigation.title == title
                    && h.handle.input_snapshot().is_ok_and(|s| {
                        s.file_drag_phase == FileDragPhase::Idle
                            && !s.file_drag_active
                            && !s.cleanup_pending
                            && !s.native_execution_unconfirmed
                            && s.inflight == 0
                    }))
            })
            .await
        }
        async fn overlay(&mut self, kind: Overlay) -> Result<(), String> {
            let generation = update(self.cx, self.root, |r, _, cx| {
                r.overlay = kind;
                r.generation += 1;
                cx.notify();
                r.generation
            })?;
            self.until("actual root paint", |h| {
                update(h.cx, h.root, |r, _, _| r.painted.get() >= generation)
            })
            .await
        }
        async fn expect_ole_marker(&mut self, path: &Path, expected: &str) -> Result<(), String> {
            self.until("exact OLE control marker", |_| {
                matching_marker(path, expected)
            })
            .await
        }

        async fn retire_active_drag(
            &mut self,
            epoch: u64,
            expected_over: &str,
        ) -> Result<String, String> {
            let snapshot = self.handle.snapshot();
            let input = self
                .handle
                .input_snapshot()
                .map_err(|e| format!("pre-close input: {e:?}"))?;
            if snapshot.state != SessionState::Alive
                || snapshot.navigation.address != self.url
                || !snapshot
                    .navigation
                    .title
                    .split('|')
                    .any(|s| s == expected_over)
                || has_dom_drop(&snapshot.navigation.title)
                || input.epoch != epoch
                || input.file_drag_phase != FileDragPhase::Over
                || !input.file_drag_active
                || input.inflight != 0
                || input.cleanup_pending
                || input.cleanup_failed
                || input.native_execution_unconfirmed
            {
                return Err("original drag was not confirmed Over at Close".into());
            }
            let browser = snapshot.browser;
            let source = self
                .rt
                .frame_source(browser)
                .map_err(|e| format!("pre-close frame source: {e:?}"))?;
            let ticket = self.handle.request_close(CloseMode::Graceful);
            if ticket.browser != browser || ticket.sequence == 0 {
                return Err("wrong active drag CloseTicket".into());
            }
            let generation = update(self.cx, self.root, |r, _, cx| {
                r.browser = None;
                r.green = true;
                r.generation += 1;
                cx.notify();
                r.generation
            })?;
            self.until(
                "original active drag CloseTicket, Idle input and root paint",
                |h| {
                    let s = h.handle.snapshot();
                    if s.browser != browser
                        || s.navigation.address != h.url
                        || has_dom_drop(&s.navigation.title)
                    {
                        return Err("Drop or navigation observed during original drag Close".into());
                    }
                    Ok(ticket.result() == CloseResult::Closed
                        && s.state == SessionState::Closed
                        && source.snapshot().closed
                        && h.weak.upgrade().is_none()
                        && h.handle.input_snapshot().is_ok_and(|i| {
                            i.epoch > epoch
                                && i.file_drag_phase == FileDragPhase::Idle
                                && !i.file_drag_active
                                && !i.cleanup_pending
                                && !i.cleanup_failed
                                && !i.native_execution_unconfirmed
                                && i.inflight == 0
                        })
                        && update(h.cx, h.root, |r, _, _| {
                            r.green && r.painted.get() >= generation
                        })?)
                },
            )
            .await?;
            let after = self
                .handle
                .input_snapshot()
                .map_err(|e| format!("retired input: {e:?}"))?;
            self.report.weak_none = true;
            self.report.green = true;
            let receipt = object(vec![
                (
                    "ticket",
                    format!(
                        "[{},{},{}]",
                        browser.runtime().value(),
                        browser.local(),
                        ticket.sequence
                    ),
                ),
                ("before_epoch", epoch.to_string()),
                ("after_epoch", after.epoch.to_string()),
                ("closed", "true".into()),
                ("source_closed", "true".into()),
                ("view_released", "true".into()),
                ("idle_after_close", "true".into()),
                ("root_repainted", "true".into()),
                ("drop_observed", "false".into()),
            ]);
            self.report.drag_retirement = Some(receipt.clone());
            Ok(receipt)
        }

        async fn ole_body(&mut self, options: &Options, expected: &str) -> Result<(), String> {
            let control = options.control.as_ref().ok_or("OLE control absent")?;
            let owner = OwnedOleTarget::install(options.ole_title(), options.nonce)?;
            let facts = owner.facts()?;
            let weak = self.weak.clone();
            let d = update(self.cx, self.root, |_, _, cx| {
                weak.upgrade().map(|v| v.read(cx).diagnostics())
            })?
            .ok_or("OLE View absent")?;
            let start = self
                .handle
                .input_snapshot()
                .map_err(|e| format!("OLE initial input: {e:?}"))?;
            if start.file_drag_phase != FileDragPhase::Idle
                || start.inflight != 0
                || start.cleanup_pending
                || start.native_execution_unconfirmed
            {
                return Err("OLE initial native input not idle/confirmed".into());
            }
            let epoch = start.epoch;
            let scale = d.scale_factor;
            if !scale.is_finite() || scale <= 0. || (scale - facts.dpi as f32 / 96.).abs() > 0.0001
            {
                return Err("OLE target scale/DPI mismatch".into());
            }
            let point = [
                ((d.surface_bounds.0 + 30.) * scale).round() as i32,
                ((d.surface_bounds.1 + 30.) * scale).round() as i32,
            ];
            let over = [
                ((d.surface_bounds.0 + 32.) * scale).round() as i32,
                point[1],
            ];
            if point[0] < 0 || point[1] < 0 || over[0] >= facts.width || point[1] >= facts.height {
                return Err("OLE points outside owned client".into());
            }
            let files = self
                .paths
                .iter()
                .map(|p| {
                    object(vec![
                        ("path", quote(p.to_str().unwrap())),
                        ("name", quote(p.file_name().unwrap().to_str().unwrap())),
                    ])
                })
                .collect::<Vec<_>>();
            let plan = object(vec![
                ("kind", quote("owned-ole-target")),
                ("pid", std::process::id().to_string()),
                ("target", facts.json()),
                ("source_nonce", options.nonce.to_string()),
                (
                    "manual_source_focus",
                    options.manual_source_focus.to_string(),
                ),
                (
                    "close_while_dragging",
                    options.ole_close_while_dragging.to_string(),
                ),
                ("browser", browser_id_json(self.handle.snapshot().browser)),
                ("input_epoch", epoch.to_string()),
                (
                    "file_directory",
                    quote(options.report.parent().unwrap().to_str().unwrap()),
                ),
                ("files", array(&files)),
                ("point", format!("[{},{}]", point[0], point[1])),
                ("over_point", format!("[{},{}]", over[0], over[1])),
                ("expected_drop", quote(expected)),
            ]);
            let path = control.join("ole-target-ready.json");
            if options.manual_source_focus {
                publish_manual_plan(&path, plan.as_bytes())?;
            } else {
                write_new(&path, plan.as_bytes())?;
            }
            let identity = format!("{}:{}", facts.pid, options.nonce);
            if options.manual_source_focus {
                for (label, file, prefix) in [
                    (
                        "manual OLE source ready",
                        "ole-driver-manual-ready.request",
                        "ole-manual-ready",
                    ),
                    (
                        "manual OLE source foreground qualified",
                        "ole-driver-manual-focus.request",
                        "ole-manual-focus",
                    ),
                ] {
                    let path = control.join(file);
                    let expected = format!("{prefix}:{identity}");
                    self.until_checked(label, true, |_| {
                        manual_focus_failed(control, &identity)?;
                        matching_marker(&path, &expected)
                    })
                    .await?;
                }
                if owner.facts()? != facts
                    || self.handle.snapshot().state != SessionState::Alive
                    || self.handle.snapshot().navigation.title != "ready"
                    || !self.handle.input_snapshot().is_ok_and(|s| {
                        s.epoch == epoch
                            && s.file_drag_phase == FileDragPhase::Idle
                            && !s.file_drag_active
                            && s.inflight == 0
                            && !s.cleanup_pending
                            && !s.cleanup_failed
                            && !s.native_execution_unconfirmed
                    })
                {
                    return Err("manual OLE target changed before gesture authorization".into());
                }
                self.observe("manual-ole-target-gesture-ready")?;
                let ready = object(vec![
                    ("pid", facts.pid.to_string()),
                    ("nonce", options.nonce.to_string()),
                    ("hwnd", facts.hwnd.to_string()),
                    ("input_epoch", epoch.to_string()),
                ]);
                write_new(
                    &control.join("ole-target-gesture.ready.json"),
                    ready.as_bytes(),
                )?;
            }
            self.until_checked("real OLE DOM enter", options.manual_source_focus, |h| {
                if options.manual_source_focus {
                    manual_focus_failed(control, &identity)?;
                }
                Ok(h.handle
                    .snapshot()
                    .navigation
                    .title
                    .split('|')
                    .any(|s| s == "enter:1:30:30")
                    && h.handle.input_snapshot().is_ok_and(|s| {
                        s.epoch == epoch
                            && s.file_drag_active
                            && s.inflight == 0
                            && !s.cleanup_pending
                            && !s.native_execution_unconfirmed
                    }))
            })
            .await?;
            self.observe("real-ole-enter")?;
            write_new(
                &control.join("ole-target-enter.ready.json"),
                object(vec![
                    ("pid", std::process::id().to_string()),
                    ("nonce", options.nonce.to_string()),
                ])
                .as_bytes(),
            )?;
            self.until("real OLE DOM over", |h| {
                Ok(h.handle
                    .snapshot()
                    .navigation
                    .title
                    .split('|')
                    .any(|s| s == "over:1:32:30")
                    && h.handle.input_snapshot().is_ok_and(|s| {
                        s.epoch == epoch
                            && s.file_drag_phase == FileDragPhase::Over
                            && s.inflight == 0
                            && !s.cleanup_pending
                            && !s.native_execution_unconfirmed
                    }))
            })
            .await?;
            self.observe("real-ole-over")?;
            write_new(
                &control.join("ole-target-over.ready.json"),
                object(vec![
                    ("pid", std::process::id().to_string()),
                    ("nonce", options.nonce.to_string()),
                ])
                .as_bytes(),
            )?;
            if options.ole_close_while_dragging {
                let identity = format!("{}:{}", facts.pid, options.nonce);
                self.expect_ole_marker(
                    &control.join("ole-target-close.request"),
                    &format!("ole-close:{identity}"),
                )
                .await?;
                if owner.facts()? != facts {
                    return Err("original OLE owner changed before active close".into());
                }
                let receipt = self.retire_active_drag(epoch, "over:1:32:30").await?;
                if owner.facts()? != facts {
                    return Err("original OLE owner changed after active close".into());
                }
                self.observe("real-ole-closed-before-source-release")?;
                write_new(
                    &control.join("ole-target-close.ready.json"),
                    object(vec![
                        ("pid", facts.pid.to_string()),
                        ("nonce", options.nonce.to_string()),
                        ("hwnd", facts.hwnd.to_string()),
                        ("retirement", receipt),
                    ])
                    .as_bytes(),
                )?;
                self.expect_ole_marker(
                    &control.join("ole-cancel-verified.ack"),
                    &format!("ole-cancel:{identity}"),
                )
                .await?;
                if has_dom_drop(&self.handle.snapshot().navigation.title) || owner.facts()? != facts
                {
                    return Err("original OLE close/cancel receipt no longer current".into());
                }
                self.observe("real-ole-source-cancelled-after-close")?;
                self.report.ole_active_close_verified = true;
                self.report.checks.extend([
                    "real-ole-active-close-ticket".into(),
                    "real-ole-source-cancel-after-close".into(),
                ]);
                return Ok(());
            }
            self.until("real OLE DOM drop", |h| {
                Ok(h.handle
                    .snapshot()
                    .navigation
                    .title
                    .split('|')
                    .any(|s| s == expected)
                    && h.handle.input_snapshot().is_ok_and(|s| {
                        s.epoch == epoch
                            && s.file_drag_phase == FileDragPhase::Idle
                            && s.inflight == 0
                            && !s.file_drag_active
                            && !s.cleanup_pending
                            && !s.native_execution_unconfirmed
                    }))
            })
            .await?;
            self.observe("real-ole-two-files-drop")?;
            write_new(
                &control.join("ole-target-drop.ready.json"),
                object(vec![
                    ("pid", std::process::id().to_string()),
                    ("nonce", options.nonce.to_string()),
                ])
                .as_bytes(),
            )?;
            let ack = format!("ole-copy:{}:{}", std::process::id(), options.nonce);
            self.until("driver-verified original native COPY completion", |_| {
                match std::fs::read_to_string(control.join("ole-copy-verified.ack")) {
                    Ok(actual) => Ok(actual == ack),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(e) => Err(format!("OLE completion fence read: {e}")),
                }
            })
            .await?;
            self.report
                .checks
                .push("real-ole-enter-over-two-file-drop".into());
            let generation = update(self.cx, self.root, |r, _, cx| {
                r.browser = None;
                r.green = true;
                r.generation += 1;
                cx.notify();
                r.generation
            })?;
            self.until("OLE View release/Closed/root paint", |h| {
                Ok(h.weak.upgrade().is_none()
                    && h.handle.snapshot().state == SessionState::Closed
                    && update(h.cx, h.root, |r, _, _| {
                        r.green && r.painted.get() >= generation
                    })?)
            })
            .await?;
            self.report.weak_none = true;
            self.report.green = true;
            self.report.ole_verified = true;
            self.report.checks.push("real-ole-view-retirement".into());
            self.observe("real-ole-released")?;
            Ok(())
        }
        async fn body(&mut self, options: &Options, expected: &str) -> Result<(), String> {
            self.until("actual source and matching widget paint", |h| {
                let weak = h.weak.clone();
                let diag = update(h.cx, h.root, |_, _, cx| {
                    weak.upgrade().map(|v| v.read(cx).diagnostics())
                })?;
                let blue = *h.stats.last_blue.lock().unwrap();
                let (Some(d), Some(info)) = (diag, blue) else {
                    return Ok(false);
                };
                let ready = info.browser == h.handle.snapshot().browser
                    && info.surface == SurfaceKind::View
                    && d.paint_calls > 0
                    && d.painted_sequence == info.sequence
                    && d.painted_generation == info.surface_generation
                    && d.frame_width == info.width
                    && d.frame_height == info.height
                    && d.surface_bounds.2 == 320.
                    && d.surface_bounds.3 == 240.
                    && d.last_error.is_none()
                    && h.handle.snapshot().navigation.title == "ready";
                if ready {
                    h.report.ready_frame = Some(object(vec![
                        ("browser", quote(&format!("{:?}", info.browser))),
                        ("surface", quote("View")),
                        ("generation", info.surface_generation.to_string()),
                        ("sequence", info.sequence.to_string()),
                        ("width", info.width.to_string()),
                        ("height", info.height.to_string()),
                        ("paint_calls", d.paint_calls.to_string()),
                        ("scale", d.scale_factor.to_string()),
                        ("origin_x", d.surface_bounds.0.to_string()),
                        ("origin_y", d.surface_bounds.1.to_string()),
                        ("source_center_bgra", quote("126,76,26,255")),
                    ]));
                }
                Ok(ready)
            })
            .await?;
            self.observe("ready")?;
            write_new(
                &options.gate.with_extension("ready"),
                std::process::id().to_string().as_bytes(),
            )?;
            self.until("actual child image observer", |_| {
                Ok(options.gate.is_file())
            })
            .await?;
            if options.real_ole {
                return self.ole_body(options, expected).await;
            }
            self.enter(1, true).await?;
            self.submit_translated()?;
            self.until("actual two file drop", |h| {
                Ok(h.handle.snapshot().navigation.title == expected
                    && h.handle.input_snapshot().is_ok_and(|s| {
                        s.file_drag_phase == FileDragPhase::Idle
                            && s.inflight == 0
                            && !s.native_execution_unconfirmed
                    }))
            })
            .await?;
            self.observe("two-files-drop")?;
            self.report
                .checks
                .push("two-files-unicode-dip-and-bytes".into());
            self.enter(2, true).await?;
            self.overlay(Overlay::Block).await?;
            let position = self.position()?;
            self.inject(PlatformInput::FileDrop(FileDropEvent::Pending { position }))?;
            self.idle("leave:2").await?;
            self.submit_translated()?;
            self.observe("blocked-overlay")?;
            self.report.checks.push("blocking-hitbox-cancels".into());
            self.overlay(Overlay::None).await?;
            self.enter(3, true).await?;
            self.overlay(Overlay::Consume).await?;
            self.submit_translated()?;
            self.idle("leave:3").await?;
            let count = update(self.cx, self.root, |r, _, _| r.consumed.get())?;
            if count != 1 {
                return Err("foreground overlay did not consume exactly one Drop".into());
            }
            self.observe("consumed-drop-fence")?;
            self.report
                .checks
                .push("foreground-consumed-drop-post-dispatch-cancel".into());
            self.overlay(Overlay::None).await?;
            self.enter(4, false).await?;
            self.submit_translated()?;
            self.idle("leave:4").await?;
            self.observe("early-drop-refused")?;
            self.report.checks.push("drop-before-over-refused".into());
            self.enter(5, true).await?;
            self.inject(PlatformInput::FileDrop(FileDropEvent::Exited))?;
            self.idle("leave:5").await?;
            self.observe("window-exit")?;
            self.report.checks.push("window-exit-cancels".into());
            self.enter(6, true).await?;
            let epoch = self
                .handle
                .input_snapshot()
                .map_err(|e| format!("injected active input: {e:?}"))?
                .epoch;
            self.retire_active_drag(epoch, "over:6:30:30").await?;
            self.report
                .checks
                .push("active-drag-view-release-and-root-paint".into());
            self.observe("released")?;
            Ok(())
        }
    }
    async fn scenario(
        cx: &mut AsyncApp,
        root: WindowHandle<Root>,
        options: &Options,
        stats: Arc<Stats>,
        out: &mut Report,
    ) -> Result<(), String> {
        let (sender, receiver) = dispatcher::channel();
        let config = RuntimeConfig {
            thread_mode: options.mode,
            files: RuntimeFiles {
                directory: std::env::current_exe()
                    .map_err(|e| e.to_string())?
                    .parent()
                    .unwrap()
                    .into(),
                helper_executable: options.helper.clone(),
            },
            root_cache_path: options.report.parent().unwrap().join("cache"),
            default_profile: ProfileStorage::Memory,
            content_policy: TrustedContentPolicy {
                allowed_origins: vec![],
                allow_embedded_data: true,
            },
            settings: Default::default(),
        };
        let mut rt = Runtime::start(
            config,
            RuntimeServices {
                events: Arc::new(Events(stats.clone())),
                frames: Arc::new(Frames(stats.clone())),
                ui: Arc::new(Wake(sender, stats.clone())),
            },
        )
        .map_err(|e| format!("start: {e:?}"))?;
        let mut handle = None;
        let result = async {
            let until = Instant::now() + Duration::from_secs(20);
            while !rt.is_ready() {
                stats.check()?;
                if Instant::now() >= until {
                    return Err("context timeout".into());
                }
                runtime_tick(cx, &mut rt, &receiver).await?;
            }
            let dir = options.report.parent().unwrap();
            let paths = vec![dir.join("自有-alpha.txt"), dir.join("自有-😀.txt")];
            let payloads = [
                b"owned CEF file drop\n".as_slice(),
                "来自自有文件😀\n".as_bytes(),
            ];
            for (p, b) in paths.iter().zip(payloads) {
                write_new(p, b)?;
            }
            let expected = format!(
                "drop:1:2:自有-alpha.txt:{}:{},自有-😀.txt:{}:{}",
                payloads[0].len(),
                digest(payloads[0]),
                payloads[1].len(),
                digest(payloads[1])
            );
            let url = page(options.real_ole);
            let scale = update(cx, root, |_, w, _| w.scale_factor())?;
            let h = rt
                .create_browser(BrowserConfig {
                    initial_url: url.clone(),
                    profile: None,
                    render_preference: options.backend,
                    viewport: Viewport {
                        width: 320,
                        height: 240,
                        scale,
                    },
                    options: Default::default(),
                })
                .map_err(|e| format!("create: {e:?}"))?;
            handle = Some(h.clone());
            let attachment = rt
                .attach_browser(&h)
                .map_err(|e| format!("attachment: {e:?}"))?;
            let mut composition = UiComposition::new(NonZeroUsize::new(1).unwrap());
            let weak = update(cx, root, |r, w, cx| {
                let v = CefBrowser::new(attachment, w, cx).map_err(|e| e.to_string())?;
                composition.register(&v, w, cx).map_err(|e| e.to_string())?;
                let weak = v.downgrade();
                r.browser = Some(v);
                cx.notify();
                Ok::<_, String>(weak)
            })??;
            let mut run = Harness {
                cx,
                rt: &mut rt,
                receiver: &receiver,
                stats: &stats,
                root,
                weak,
                composition,
                handle: h,
                report: out,
                paths,
                url,
                real_ole: options.real_ole,
            };
            run.body(options, &expected).await
        }
        .await;
        if let Err(e) = result {
            out.error.get_or_insert(e);
        }
        let _ = update(cx, root, |r, _, cx| {
            r.browser = None;
            r.green = true;
            r.generation += 1;
            cx.notify();
        });
        let deadline = Instant::now() + Duration::from_secs(25);
        let ticket = rt.request_shutdown(CloseMode::Force, deadline);
        loop {
            match rt.poll_shutdown(&ticket) {
                ShutdownStatus::Ready => {
                    if APP_BORROW.with(Cell::get) {
                        out.error.get_or_insert("finish inside borrow".into());
                        break;
                    }
                    match rt.finish_shutdown() {
                        Ok(_) => out.finished = true,
                        Err(e) => {
                            out.error.get_or_insert(format!("finish: {e:?}"));
                        }
                    }
                    break;
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                out.error.get_or_insert("shutdown unfinished".into());
                break;
            }
            runtime_tick(cx, &mut rt, &receiver).await?;
        }
        if let Err(e) = stats.check() {
            out.error.get_or_insert(e);
        }
        let d = rt.diagnostics();
        let budget = handle
            .as_ref()
            .and_then(|h| rt.frame_budget_stats(h.snapshot().browser).ok())
            .map(|b| b.used);
        out.runtime = Some(object(vec![
            ("main_thread", d.main_thread_id.to_string()),
            (
                "cef_ui_thread",
                d.cef_ui_thread_id.map_or("null".into(), |v| v.to_string()),
            ),
            ("manual_pump", d.manual_pump_count.to_string()),
            (
                "actual_backend",
                quote(&format!("{:?}", d.observed_backend)),
            ),
            ("tasks_posted", d.tasks.posted.to_string()),
            ("tasks_completed", d.tasks.completed.to_string()),
            ("pending_tasks", d.tasks.pending.to_string()),
            ("pending_requests", d.pending_requests.to_string()),
            ("pending_profiles", d.pending_profiles.to_string()),
            ("shutdown_calls", d.cef_shutdown_calls.to_string()),
            ("native_after", d.native_calls_after_shutdown.to_string()),
            ("app_retained", d.app_reference_retained.to_string()),
            ("root_lease", d.root_lease_held.to_string()),
            (
                "budget_used",
                budget.map_or("null".into(), |v| v.to_string()),
            ),
        ]));
        if !out.finished
            || match options.mode {
                ThreadMode::Threaded => d.manual_pump_count != 0,
                ThreadMode::Integrated => d.manual_pump_count == 0,
            }
            || d.main_thread_id != d.init_thread_id
            || d.shutdown_thread_id != Some(d.main_thread_id)
            || !d.cef_ui_thread_id.is_some_and(|ui| match options.mode {
                ThreadMode::Threaded => ui != d.main_thread_id,
                ThreadMode::Integrated => ui == d.main_thread_id,
            })
            || d.observed_backend != Some(options.backend)
            || d.tasks.pending != 0
            || d.tasks.posted != d.tasks.completed
            || d.pending_requests != 0
            || d.pending_profiles != 0
            || d.cef_shutdown_calls != 1
            || d.native_calls_after_shutdown != 0
            || d.app_reference_retained
            || d.root_lease_held
            || budget != Some(0)
        {
            out.error
                .get_or_insert("final runtime/owner barrier mismatch".into());
        }
        Ok(())
    }
    pub fn run() -> i32 {
        let options = match options() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
        let stats = Arc::new(Stats::default());
        let outcome = Arc::new(Mutex::new(Report::default()));
        let state = outcome.clone();
        let counters = stats.clone();
        let opts = options.clone();
        gpui_platform::application().run(move |cx| {
            let root = match cx.open_window(
                WindowOptions {
                    focus: false,
                    titlebar: if opts.real_ole {
                        Some(gpui::TitlebarOptions {
                            title: Some(opts.ole_title().into()),
                            ..Default::default()
                        })
                    } else {
                        None
                    },
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(40.), px(40.)),
                        size(px(420.), px(380.)),
                    ))),
                    ..Default::default()
                },
                |w, cx| {
                    w.on_window_should_close(cx, |_, _| false);
                    cx.new(|_| Root {
                        browser: None,
                        overlay: Overlay::None,
                        generation: 1,
                        green: false,
                        painted: Rc::new(Cell::new(0)),
                        consumed: Rc::new(Cell::new(0)),
                        renders: 0,
                    })
                },
            ) {
                Ok(w) => w,
                Err(e) => {
                    state.lock().unwrap().error = Some(format!("window: {e}"));
                    cx.quit();
                    return;
                }
            };
            let state = state.clone();
            let counters = counters.clone();
            let opts = opts.clone();
            cx.spawn(async move |cx: &mut AsyncApp| {
                let mut report = Report::default();
                if let Err(e) = scenario(cx, root, &opts, counters, &mut report).await {
                    report.error.get_or_insert(e);
                }
                *state.lock().unwrap() = report;
                app(cx, |cx| cx.quit());
            })
            .detach();
            // No activate/window focus/native foreground request.
        });
        let out = outcome.lock().unwrap();
        let ok = out.error.is_none()
            && out.finished
            && out.green
            && out.weak_none
            && out.checks.len() == if options.real_ole { 2 } else { 6 }
            && (!options.ole_close_while_dragging
                || out.ole_active_close_verified && !out.ole_verified)
            && stats.created.load(Ordering::Relaxed) == 1
            && stats.closed.load(Ordering::Relaxed) == 1
            && stats.violations.load(Ordering::Relaxed) == 0;
        let text = object(vec![
            (
                "kind",
                quote(if options.ole_close_while_dragging {
                    "real-native-ole-active-close-slice"
                } else if options.real_ole {
                    "real-native-ole-file-slice"
                } else {
                    "real-gpui-injected-file-events"
                }),
            ),
            ("pid", std::process::id().to_string()),
            ("success", ok.to_string()),
            (
                "error",
                out.error.as_ref().map_or("null".into(), |e| quote(e)),
            ),
            ("mode", quote(&format!("{:?}", options.mode))),
            ("backend", quote(&format!("{:?}", options.backend))),
            ("foreground_requested", "false".into()),
            ("raw_platform_submit_verified", out.ole_verified.to_string()),
            (
                "submit_injection",
                quote(if options.real_ole {
                    "none; raw OS/OLE source"
                } else {
                    "translated MouseUp; raw Submit activation bypassed"
                }),
            ),
            (
                "real_win32_or_ole_verified",
                (out.ole_verified || out.ole_active_close_verified).to_string(),
            ),
            (
                "ole_active_close_verified",
                out.ole_active_close_verified.to_string(),
            ),
            (
                "drag_retirement",
                out.drag_retirement.clone().unwrap_or("null".into()),
            ),
            ("desktop_pixels_verified", "false".into()),
            ("platform_events_injected", (!options.real_ole).to_string()),
            ("task_12_3_accepted", "false".into()),
            (
                "checks",
                array(&out.checks.iter().map(|s| quote(s)).collect::<Vec<_>>()),
            ),
            ("observations", array(&out.observations)),
            ("weak_view_none", out.weak_none.to_string()),
            ("root_green_paint", out.green.to_string()),
            ("created", stats.created.load(Ordering::Relaxed).to_string()),
            ("closed", stats.closed.load(Ordering::Relaxed).to_string()),
            (
                "blue_frames",
                stats.blue.load(Ordering::Relaxed).to_string(),
            ),
            (
                "callback_borrow_violations",
                stats.violations.load(Ordering::Relaxed).to_string(),
            ),
            (
                "ready_frame",
                out.ready_frame.clone().unwrap_or("null".into()),
            ),
            ("runtime", out.runtime.clone().unwrap_or("null".into())),
        ]);
        if let Err(e) = write_new(&options.report, text.as_bytes()) {
            eprintln!("report: {e}");
            return 1;
        }
        if ok { 0 } else { out.exit.max(1) }
    }
}
