//! Real CEF Alloy native child `HWND` (`BrowserSurfaceMode::NativeChildHwnd`) probe scenario.
//!
//! Covers pre-creation validation, `SetAsChild` parenting, `WS_EX_NOACTIVATE` strip,
//! zero OSR frame capture/budget on native child browsers, OSR command rejections, physical
//! pixel bounds tracking, `SetVisible(false)` (`0x0` + `SW_HIDE`) & restore,
//! `NotifyParentMoved`, `SetFocus`, `TrustedContentPolicy`, `EvaluateScript`, `PrintToPdf`,
//! real DevTools (`has_dev_tools()` association + explicit `close_dev_tools`), `CefURLRequest`,
//! graceful close (`IsWindow == false`), parent-destroyed-first safety, and live `Offscreen`
//! coexistence inside the same `Runtime`.
use std::{
    collections::HashSet,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    num::NonZeroUsize,
    path::Path,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use rust_cef_core::{
    contracts::{
        BrowserConfig, BrowserId, BrowserOptions, BrowserSurfaceMode, CreateError,
        NativeChildHwndConfig, PixelRect, RenderPreference, SessionState, ThreadMode, Viewport,
    },
    host_capabilities::PdfExportOptions,
    input::{BrowserTouchEvent, InputEvent, InputKind, TouchEventType, TouchPointerType},
    js_bridge::{EvalOutcome, JsBridgeConfig, JsFrameScope},
    ports::{
        BrowserCommand, BrowserEvent, BrowserHandle, CloseMode, EngineError, MouseModifiers,
        PolicyAction, SurfaceKind,
    },
    url_request::{UrlRequestSpec, UrlRequestTarget},
};
use rust_cef_runtime::{
    PdfExportStatus, UrlRequestState,
    runtime::{BrowserCreateError, Runtime},
    test_support::{FrameProbeControl, NativeChildHwndReport},
};

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

const GWL_STYLE: i32 = -16;
const GWL_EXSTYLE: i32 = -20;
const WS_VISIBLE: isize = 0x1000_0000;
const WS_EX_NOACTIVATE: isize = 0x0800_0000;
const WS_POPUP: u32 = 0x8000_0000;
const WS_CLIPCHILDREN: u32 = 0x0200_0000;
const WS_CLIPSIBLINGS: u32 = 0x0400_0000;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const PM_REMOVE: u32 = 0x0001;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct WinPoint {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct WinRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(module_name: *const u16) -> isize;
}

#[allow(clashing_extern_declarations)]
#[link(name = "user32")]
unsafe extern "system" {
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: isize,
        menu: isize,
        instance: isize,
        param: *mut core::ffi::c_void,
    ) -> isize;
    fn DestroyWindow(hwnd: isize) -> i32;
    fn IsWindow(hwnd: isize) -> i32;
    fn GetParent(hwnd: isize) -> isize;
    fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
    fn SetWindowPos(
        hwnd: isize,
        insert_after: isize,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    fn GetWindowRect(hwnd: isize, rect: *mut WinRect) -> i32;
    fn ScreenToClient(hwnd: isize, point: *mut WinPoint) -> i32;
    fn PeekMessageW(msg: *mut u64, hwnd: isize, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(msg: *const u64) -> i32;
    fn DispatchMessageW(msg: *const u64) -> isize;
}

/// Drains pending Win32 messages on the calling thread without blocking so cross-thread
/// `SetAsChild`/`DestroyWindow` send-messages never deadlock in `ThreadMode::Threaded`.
fn pump_win32_messages() {
    let mut msg = [0u64; 8];
    for _ in 0..256 {
        let has_msg = unsafe { PeekMessageW(msg.as_mut_ptr(), 0, 0, 0, PM_REMOVE) };
        if has_msg == 0 {
            break;
        }
        unsafe {
            TranslateMessage(msg.as_ptr());
            DispatchMessageW(msg.as_ptr());
        }
    }
}

struct TestParentWindow {
    hwnd: isize,
    pump_win32: bool,
}

impl TestParentWindow {
    fn create(mode: ThreadMode, x: i32, y: i32, width: i32, height: i32) -> Result<Self, String> {
        // Built-in "STATIC" control avoids custom WNDCLASS registration/cleanup.
        const CLASS: [u16; 7] = [
            b'S' as u16,
            b'T' as u16,
            b'A' as u16,
            b'T' as u16,
            b'I' as u16,
            b'C' as u16,
            0,
        ];
        const TITLE: [u16; 1] = [0];
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                CLASS.as_ptr(),
                TITLE.as_ptr(),
                WS_POPUP | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                x,
                y,
                width,
                height,
                0,
                0,
                instance,
                ptr::null_mut(),
            )
        };
        if hwnd == 0 || unsafe { IsWindow(hwnd) == 0 } {
            return Err("CreateWindowExW failed for hidden Win32 parent".into());
        }
        let pump_win32 = mode == ThreadMode::Threaded;
        if pump_win32 {
            pump_win32_messages();
        }
        Ok(Self { hwnd, pump_win32 })
    }

    fn raw(&self) -> isize {
        self.hwnd
    }

    fn nonzero(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.hwnd as usize).expect("live HWND non-zero")
    }

    fn destroy(&mut self) {
        if self.hwnd != 0 && unsafe { IsWindow(self.hwnd) != 0 } {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            if self.pump_win32 {
                pump_win32_messages();
            }
        }
        self.hwnd = 0;
    }
}

impl Drop for TestParentWindow {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn window_rect_in_parent(parent: isize, child: isize) -> Option<PixelRect> {
    let mut wr = WinRect::default();
    if unsafe { GetWindowRect(child, &mut wr) } == 0 {
        return None;
    }
    let mut top_left = WinPoint {
        x: wr.left,
        y: wr.top,
    };
    if unsafe { ScreenToClient(parent, &mut top_left) } == 0 {
        return None;
    }
    let width = u32::try_from((wr.right - wr.left).max(0)).ok()?;
    let height = u32::try_from((wr.bottom - wr.top).max(0)).ok()?;
    Some(PixelRect {
        x: top_left.x,
        y: top_left.y,
        width,
        height,
    })
}

fn style_has_ws_visible(hwnd: isize) -> bool {
    (unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } & WS_VISIBLE) != 0
}

fn ex_style_has_noactivate(hwnd: isize) -> bool {
    (unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } & WS_EX_NOACTIVATE) != 0
}

fn wait_until(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    deadline: Instant,
    label: &str,
    mut ready: impl FnMut(&mut Runtime) -> Result<bool, String>,
) -> Result<(), String> {
    loop {
        if mode == ThreadMode::Threaded {
            pump_win32_messages();
        }
        if ready(rt)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("native child hwnd timeout: {label}"));
        }
        tick(rt, mode)?;
        if mode == ThreadMode::Threaded {
            pump_win32_messages();
        }
    }
}

pub struct NativeHttpFixture {
    pub origin: String,
    stop: Arc<AtomicBool>,
    workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    acceptor: Option<thread::JoinHandle<()>>,
}

impl NativeHttpFixture {
    pub fn start() -> Result<Self, String> {
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind native fixture: {e}"))?;
        let addr: SocketAddr = listener
            .local_addr()
            .map_err(|e| format!("native fixture local_addr: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("native fixture nonblocking: {e}"))?;
        let origin = format!("http://{addr}");
        let stop = Arc::new(AtomicBool::new(false));
        let workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let stop_accept = stop.clone();
        let workers_accept = workers.clone();
        let acceptor = thread::spawn(move || {
            while !stop_accept.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let s = stop_accept.clone();
                        let h = thread::spawn(move || handle_client(stream, s));
                        workers_accept.lock().unwrap().push(h);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            origin,
            stop,
            workers,
            acceptor: Some(acceptor),
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin, path)
    }
}

impl Drop for NativeHttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
        let workers: Vec<_> = self.workers.lock().unwrap().drain(..).collect();
        for w in workers {
            let _ = w.join();
        }
    }
}

fn handle_client(mut stream: TcpStream, stop: Arc<AtomicBool>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let mut line = String::new();
    while !stop.load(Ordering::Acquire) {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    let (status, ctype, body): (&str, &str, &[u8]) = match path {
        "/echo" => ("200 OK", "text/plain; charset=utf-8", b"native-url-ok"),
        _ => (
            "200 OK",
            "text/html; charset=utf-8",
            b"<!doctype html><html><head><title>Native Child Probe</title></head><body><h1>Native</h1></body></html>",
        ),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

pub fn run(
    rt: &mut Runtime,
    mode: ThreadMode,
    backend: RenderPreference,
    osr_handle: &BrowserHandle,
    tick: Tick,
    fixture: &NativeHttpFixture,
    report_dir: &Path,
    frames: &AtomicU64,
    valid_browsers: &Mutex<HashSet<BrowserId>>,
    policy_denials: &Arc<Mutex<Vec<PolicyAction>>>,
) -> Result<NativeChildHwndReport, String> {
    let mut report = NativeChildHwndReport::default();
    let deadline = Instant::now() + Duration::from_secs(60);
    let osr_id = osr_handle.snapshot().browser;
    let osr_source = rt
        .frame_source(osr_id)
        .map_err(|e| format!("b0 frame_source: {e:?}"))?;

    // 1. Pre-creation invalid parent_hwnd and invalid bounds rejections.
    let mut temp_parent = TestParentWindow::create(mode, 50, 50, 200, 150)?;
    let destroyed_parent_nz = temp_parent.nonzero();
    temp_parent.destroy();
    let bad_parent_cfg = BrowserConfig {
        initial_url: fixture.url("/native"),
        profile: None,
        render_preference: backend,
        viewport: Viewport {
            width: 300,
            height: 200,
            scale: 1.0,
        },
        options: BrowserOptions::default().with_surface_mode(BrowserSurfaceMode::NativeChildHwnd(
            NativeChildHwndConfig {
                parent_hwnd: destroyed_parent_nz,
                initial_bounds: PixelRect {
                    x: 0,
                    y: 0,
                    width: 300,
                    height: 200,
                },
                activate_on_creation: false,
            },
        )),
    };
    report.invalid_parent_rejected = matches!(
        rt.create_browser(bad_parent_cfg),
        Err(BrowserCreateError::Core(CreateError::InvalidOptions))
    );

    let mut parent1 = TestParentWindow::create(mode, 100, 100, 800, 600)?;
    let too_wide_cfg = BrowserConfig {
        initial_url: fixture.url("/native"),
        profile: None,
        render_preference: backend,
        viewport: Viewport {
            width: 300,
            height: 200,
            scale: 1.0,
        },
        options: BrowserOptions::default().with_surface_mode(BrowserSurfaceMode::NativeChildHwnd(
            NativeChildHwndConfig {
                parent_hwnd: parent1.nonzero(),
                initial_bounds: PixelRect {
                    x: 0,
                    y: 0,
                    width: 16_385,
                    height: 100,
                },
                activate_on_creation: false,
            },
        )),
    };
    let oob_bounds_cfg = BrowserConfig {
        initial_url: fixture.url("/native"),
        profile: None,
        render_preference: backend,
        viewport: Viewport {
            width: 300,
            height: 200,
            scale: 1.0,
        },
        options: BrowserOptions::default().with_surface_mode(BrowserSurfaceMode::NativeChildHwnd(
            NativeChildHwndConfig {
                parent_hwnd: parent1.nonzero(),
                initial_bounds: PixelRect {
                    x: 1_000_001,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                activate_on_creation: false,
            },
        )),
    };
    report.invalid_bounds_rejected = matches!(
        rt.create_browser(too_wide_cfg),
        Err(BrowserCreateError::Core(CreateError::InvalidOptions))
    ) && matches!(
        rt.create_browser(oob_bounds_cfg),
        Err(BrowserCreateError::Core(CreateError::InvalidOptions))
    );

    // 2. Create Browser 1 in NativeChildHwnd mode inside the same Runtime.
    let initial_rect = PixelRect {
        x: 10,
        y: 20,
        width: 300,
        height: 200,
    };
    let expected_surface = BrowserSurfaceMode::NativeChildHwnd(NativeChildHwndConfig {
        parent_hwnd: parent1.nonzero(),
        initial_bounds: initial_rect,
        activate_on_creation: false,
    });
    let handle1 = rt
        .create_browser(BrowserConfig {
            initial_url: fixture.url("/native"),
            profile: None,
            render_preference: backend,
            viewport: Viewport {
                width: 300,
                height: 200,
                scale: 1.0,
            },
            options: BrowserOptions::default()
                .with_surface_mode(expected_surface)
                .with_js_bridge(JsBridgeConfig {
                    enabled: true,
                    allowed_origins: vec![fixture.origin.clone()],
                    ..Default::default()
                }),
        })
        .map_err(|e| format!("create native child b1: {e:?}"))?;
    let id1 = handle1.snapshot().browser;

    wait_until(rt, mode, tick, deadline, "b1 alive & child HWND", |rt| {
        let snap = handle1.snapshot();
        Ok(snap.state == SessionState::Alive
            && !snap.navigation.is_loading
            && snap.navigation.address == fixture.url("/native")
            && rt.native_browser_hwnd(id1).is_some())
    })?;
    let child1_raw = rt
        .native_browser_hwnd(id1)
        .ok_or("b1 missing child HWND")?
        .get() as isize;

    // 3. Verify Win32 parent/window/style invariants.
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "b1 WS_EX_NOACTIVATE stripped",
        |_| Ok(!ex_style_has_noactivate(child1_raw)),
    )?;
    report.child_is_window = unsafe { IsWindow(child1_raw) != 0 };
    report.child_parent_matches = unsafe { GetParent(child1_raw) == parent1.raw() };
    report.noactivate_stripped = !ex_style_has_noactivate(child1_raw);
    report.surface_mode_matches = rt.browser_surface_mode(id1) == Some(expected_surface);

    // 4. Zero OSR frames & OSR command rejections on Browser 1.
    let frame_snap = rt
        .frame_source(id1)
        .map_err(|e| format!("b1 frame_source: {e:?}"))?
        .snapshot();
    report.zero_osr_frames_on_native = frame_snap.view.frame.is_none()
        && frame_snap.popup.frame.is_none()
        && !valid_browsers.lock().unwrap().contains(&id1);

    let input_epoch = handle1.input_snapshot().map(|s| s.epoch).unwrap_or(1);
    let rej_viewport = handle1.submit(BrowserCommand::SetViewport(Viewport {
        width: 400,
        height: 300,
        scale: 1.0,
    }));
    // Submit `TouchEvent` before `Input` so a rejected `Input` does not arm
    // an input-reset cutpoint before `TouchEvent` reaches `Session::admit_command`.
    let rej_touch = handle1.submit(BrowserCommand::TouchEvent(BrowserTouchEvent {
        id: 1,
        x: 10.0,
        y: 10.0,
        radius_x: 1.0,
        radius_y: 1.0,
        rotation_angle: 0.0,
        pressure: 1.0,
        event_type: TouchEventType::Pressed,
        modifiers: MouseModifiers::default(),
        pointer_type: TouchPointerType::Touch,
    }));
    let rej_input = handle1.submit(BrowserCommand::Input(InputEvent {
        epoch: input_epoch,
        kind: InputKind::MouseMove {
            x: 5,
            y: 5,
            leaving: false,
            modifiers: MouseModifiers::default(),
        },
    }));
    report.osr_commands_rejected = matches!(rej_viewport, Err(EngineError::InvalidRequest))
        && matches!(rej_touch, Err(EngineError::InvalidRequest))
        && matches!(rej_input, Err(EngineError::InvalidRequest));
    if !report.osr_commands_rejected {
        eprintln!(
            "OSR_REJECT_MISMATCH vp={rej_viewport:?} touch={rej_touch:?} input={rej_input:?} epoch={input_epoch}"
        );
    }

    // 5. Bounds resize, SetVisible(false) hide 0x0, SetVisible(true) restore.
    let resized_rect = PixelRect {
        x: 12,
        y: 24,
        width: 420,
        height: 260,
    };
    handle1
        .submit(BrowserCommand::SetNativeBounds(resized_rect))
        .map_err(|e| format!("b1 SetNativeBounds: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 resized bounds", |_| {
        Ok(style_has_ws_visible(child1_raw)
            && window_rect_in_parent(parent1.raw(), child1_raw) == Some(resized_rect))
    })?;
    report.bounds_tracked_ok = true;

    handle1
        .submit(BrowserCommand::SetVisible(false))
        .map_err(|e| format!("b1 SetVisible(false): {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 hidden 0x0", |_| {
        Ok(!style_has_ws_visible(child1_raw)
            && window_rect_in_parent(parent1.raw(), child1_raw)
                .is_some_and(|r| r.width == 0 && r.height == 0))
    })?;
    report.hide_zero_size_ok = true;

    handle1
        .submit(BrowserCommand::SetVisible(true))
        .map_err(|e| format!("b1 SetVisible(true): {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 restored bounds", |_| {
        Ok(style_has_ws_visible(child1_raw)
            && window_rect_in_parent(parent1.raw(), child1_raw) == Some(resized_rect))
    })?;
    report.restore_bounds_ok = true;

    // 6. NotifyParentMoved + SetFocus(true/false) execute cleanly on CEF UI thread.
    let moved_ok = unsafe {
        SetWindowPos(
            parent1.raw(),
            0,
            120,
            120,
            800,
            600,
            SWP_NOZORDER | SWP_NOACTIVATE,
        ) != 0
    };
    if mode == ThreadMode::Threaded {
        pump_win32_messages();
    }
    let notify_cmd = handle1.submit(BrowserCommand::NotifyParentMoved);
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "b1 NotifyParentMoved drained",
        |rt| Ok(rt.diagnostics().tasks.pending == 0),
    )?;
    report.notify_parent_moved_ok = moved_ok
        && notify_cmd.is_ok()
        && unsafe { IsWindow(child1_raw) != 0 }
        && window_rect_in_parent(parent1.raw(), child1_raw) == Some(resized_rect);

    let focus_on = handle1.submit(BrowserCommand::SetFocus(true));
    let focus_off = handle1.submit(BrowserCommand::SetFocus(false));
    wait_until(rt, mode, tick, deadline, "b1 SetFocus drained", |rt| {
        Ok(rt.diagnostics().tasks.pending == 0)
    })?;
    report.focus_commands_ok = focus_on.is_ok() && focus_off.is_ok();

    // 7. TrustedContentPolicy denial (both host submit guard and renderer navigation block).
    let denials_before = policy_denials.lock().unwrap().len();
    let sync_denied = matches!(
        handle1.submit(BrowserCommand::Navigate(
            "https://untrusted.example/blocked".into(),
        )),
        Err(EngineError::PolicyDenied)
    );
    handle1
        .submit(BrowserCommand::ExecuteJavaScript {
            script: "window.location.href='https://untrusted.example/blocked'".into(),
            frame_scope: JsFrameScope::MainFrame,
        })
        .map_err(|e| format!("b1 policy js navigate: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 policy denial", |_| {
        let guard = policy_denials.lock().unwrap();
        Ok(guard.len() > denials_before
            && guard[denials_before..].contains(&PolicyAction::Navigation))
    })?;
    report.policy_denied_ok = sync_denied;

    // 8. EvaluateScript parity.
    let eval_id = handle1
        .evaluate_script("1 + 41".into(), JsFrameScope::MainFrame, 2000)
        .map_err(|e| format!("b1 evaluate_script: {e:?}"))?;
    let mut eval_ok = false;
    wait_until(rt, mode, tick, deadline, "b1 eval result", |_| {
        for ev in handle1.take_js_events() {
            if let BrowserEvent::EvalCompleted { id, outcome } = ev
                && id.sequence == eval_id.sequence
                && matches!(&outcome, EvalOutcome::Value(json) if json == "42")
            {
                eval_ok = true;
            }
        }
        Ok(eval_ok)
    })?;
    report.eval_script_ok = eval_ok;

    // 9. PrintToPdf parity.
    let pdf_path = report_dir.join("native-child-probe.pdf");
    let pdf_ticket = rt
        .print_to_pdf(id1, pdf_path.clone(), PdfExportOptions::default())
        .map_err(|e| format!("b1 print_to_pdf: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 print_to_pdf", |_| {
        Ok(!matches!(pdf_ticket.poll(), PdfExportStatus::Pending))
    })?;
    let pdf_bytes = std::fs::read(&pdf_path).unwrap_or_default();
    report.print_to_pdf_ok = matches!(pdf_ticket.poll(), PdfExportStatus::Completed(out) if out.ok)
        && pdf_bytes.starts_with(b"%PDF-");

    // 10. Browser-frame CefURLRequest parity.
    let mut url_spec = UrlRequestSpec::get(
        UrlRequestTarget::BrowserMainFrame(id1),
        fixture.url("/echo"),
    );
    url_spec.timeout_ms = 2_000;
    let url_req = rt
        .start_url_request(url_spec)
        .map_err(|e| format!("b1 start_url_request: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 url_request", |_| {
        Ok(!matches!(url_req.poll(), UrlRequestState::Pending(_)))
    })?;
    report.url_request_ok = matches!(
        url_req.poll(),
        UrlRequestState::Completed(resp)
            if resp.http_status == Some(200) && resp.body == b"native-url-ok"
    );

    // 11. Parent-destroyed-first safety on Browser 2.
    let mut parent2 = TestParentWindow::create(mode, 140, 140, 640, 480)?;
    let handle2 = rt
        .create_browser(BrowserConfig {
            initial_url: fixture.url("/native"),
            profile: None,
            render_preference: backend,
            viewport: Viewport {
                width: 240,
                height: 180,
                scale: 1.0,
            },
            options: BrowserOptions::default().with_surface_mode(
                BrowserSurfaceMode::NativeChildHwnd(NativeChildHwndConfig {
                    parent_hwnd: parent2.nonzero(),
                    initial_bounds: PixelRect {
                        x: 0,
                        y: 0,
                        width: 240,
                        height: 180,
                    },
                    activate_on_creation: false,
                }),
            ),
        })
        .map_err(|e| format!("create native child b2: {e:?}"))?;
    let id2 = handle2.snapshot().browser;
    wait_until(rt, mode, tick, deadline, "b2 alive & child HWND", |rt| {
        Ok(
            handle2.snapshot().state == SessionState::Alive
                && rt.native_browser_hwnd(id2).is_some(),
        )
    })?;
    let child2_raw = rt
        .native_browser_hwnd(id2)
        .ok_or("b2 missing child HWND")?
        .get() as isize;
    if unsafe { IsWindow(child2_raw) == 0 } {
        return Err("b2 child HWND not a valid Win32 window before parent destroy".into());
    }
    let parent2_raw = parent2.raw();
    parent2.destroy();
    let parent_gone = unsafe { IsWindow(parent2_raw) == 0 };
    let _ = handle2.submit(BrowserCommand::SetNativeBounds(PixelRect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    }));
    let _ = handle2.submit(BrowserCommand::SetVisible(false));
    let _ = handle2.request_close(CloseMode::Graceful);
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "b2 close after parent destroy",
        |rt| {
            Ok(handle2.snapshot().state == SessionState::Closed
                && unsafe { IsWindow(parent2_raw) == 0 && IsWindow(child2_raw) == 0 }
                && rt.native_browser_hwnd(id2).is_none())
        },
    )?;
    report.parent_destroyed_first_safe = parent_gone;

    // 12. Real Offscreen + NativeChildHwnd coexistence while Browser 1 is live and
    // Browser 2 has already closed after parent destruction (prior to Chrome DevTools
    // popup teardown, which triggers a CEF 146 Integrated-mode OSR repaint quirk).
    let osr_frames_before = frames.load(Ordering::Acquire);
    let osr_seq_before = osr_source
        .latest(SurfaceKind::View)
        .ok_or("b0 missing initial VIEW")?
        .info
        .sequence;
    rt.probe_frame_control(osr_id, FrameProbeControl::InvalidateView)
        .map_err(|e| format!("b0 invalidate: {e:?}"))?;
    let step12_deadline = Instant::now() + Duration::from_secs(10);
    wait_until(
        rt,
        mode,
        tick,
        step12_deadline,
        "b0 coexistence frame",
        |_| {
            Ok(frames.load(Ordering::Acquire) > osr_frames_before
                && osr_source.latest(SurfaceKind::View).is_some_and(|f| {
                    f.info.sequence > osr_seq_before && f.info.width == 320 && f.info.height == 240
                }))
        },
    )?;
    report.coexistence_osr_frame_advanced = true;

    // 13. Real DevTools open + has_dev_tools() association + explicit close dissociation.
    rt.configure_devtools(NonZeroUsize::new(2).unwrap())
        .map_err(|e| format!("configure_devtools: {e:?}"))?;
    rt.open_devtools(id1)
        .map_err(|e| format!("b1 open_devtools: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 devtools associated", |rt| {
        Ok(rt.devtools_pending(id1) > 0 && rt.devtools_associated(id1) == Some(true))
    })?;
    report.devtools_associated_open_ok = true;
    rt.close_dev_tools(id1)
        .map_err(|e| format!("b1 close_dev_tools: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 devtools dissociated", |rt| {
        Ok(rt.devtools_pending(id1) == 0 && rt.devtools_associated_after_close(id1) == Some(false))
    })?;
    report.devtools_closed_dissociated_ok = true;

    // 14. Graceful close clears Win32 HWND (`IsWindow == false`) while keeping
    // the host parent HWND alive, and Threaded mode also verifies post-DevTools-close
    // OSR liveness on Browser 0.
    let _ = handle1.request_close(CloseMode::Graceful);
    wait_until(rt, mode, tick, deadline, "b1 graceful close", |rt| {
        Ok(handle1.snapshot().state == SessionState::Closed
            && unsafe { IsWindow(child1_raw) == 0 }
            && rt.native_browser_hwnd(id1).is_none())
    })?;
    if unsafe { IsWindow(parent1.raw()) == 0 } {
        return Err("b1 graceful close unexpectedly destroyed parent1 HWND".into());
    }
    report.graceful_close_hwnd_gone = true;
    if osr_handle.snapshot().state != SessionState::Alive {
        return Err("b0 offscreen survivor did not remain Alive".into());
    }
    if mode == ThreadMode::Threaded {
        let post_frames = frames.load(Ordering::Acquire);
        let post_seq = osr_source
            .latest(SurfaceKind::View)
            .ok_or("b0 missing post-devtools VIEW")?
            .info
            .sequence;
        rt.probe_frame_control(osr_id, FrameProbeControl::InvalidateView)
            .map_err(|e| format!("b0 post-devtools invalidate: {e:?}"))?;
        let post_deadline = Instant::now() + Duration::from_secs(10);
        wait_until(
            rt,
            mode,
            tick,
            post_deadline,
            "b0 post-devtools threaded frame",
            |_| {
                Ok(frames.load(Ordering::Acquire) > post_frames
                    && osr_source.latest(SurfaceKind::View).is_some_and(|f| {
                        f.info.sequence > post_seq && f.info.width == 320 && f.info.height == 240
                    }))
            },
        )?;
    }

    parent1.destroy();
    Ok(report)
}
