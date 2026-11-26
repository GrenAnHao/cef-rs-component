//! Standalone example opening Bing China (`https://cn.bing.com/`, plus general
//! HTTP/HTTPS web navigation and resources) with `rust-cef-gpui`.
//!
//! Run via:
//!   .\scripts\run-bing.ps1
//!   .\scripts\run-bing.ps1 -NativeHwnd
//!   .\scripts\run-bing.ps1 -SelfTestOsr
//!   .\scripts\run-bing.ps1 -Url https://bing.cn
#[cfg(not(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc")))]
fn main() {
    eprintln!("bing_cn example requires Windows x64 MSVC");
    std::process::exit(2);
}

#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
fn main() {
    std::process::exit(win::entry());
}

#[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
mod win {
    use gpui::{
        App, AppContext as _, AsyncApp, Bounds, Context, Entity, Focusable as _, IntoElement,
        KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent,
        MouseMoveEvent, MouseUpEvent, PlatformInput, Render, Window, WindowBounds, WindowHandle,
        WindowOptions, div, prelude::*, px, rgb, size,
    };
    use rust_cef_gpui::{
        CefBrowser, ContextMenuConfig, ContextMenuRegistration, ContextMenuServices,
        DefaultMenuPresenter, FindServices, HostShortcutPolicy, NativeCefBrowser,
        NativeDropTargetServices, NativeKeyboardServices, NativeTouchServices, Runtime,
        RuntimeServices, SourceViewOptions, SourceViewServices,
        contracts::{
            BrowserConfig, BrowserOptions, BrowserSurfaceMode, NativeChildHwndConfig, PixelRect,
            ProfileStorage, RenderPreference, RuntimeConfig, RuntimeFiles, RuntimeSettings,
            RuntimeState, ThreadMode, TrustedContentPolicy, Viewport, step_canonical_zoom_level,
            zoom_level_to_milli,
        },
        dispatcher::{self, UiWakeReceiver, UiWakeSender},
        find::FindLimits,
        host_capabilities::{
            DownloadProgress, DownloadRequest, DownloadService, DownloadState, FileDialogDecision,
            FileDialogKind, FileDialogRequest, FileDialogService, HostCapabilities, HostRequestId,
            JsDialogDecision, JsDialogKind, JsDialogRequest, JsDialogService, PopupRequest,
            PopupService,
        },
        inbound_drag::{InboundDragCommand, InboundDragPayload},
        menu::{MenuContextLimits, MenuLimits},
        outbound_drag_contracts::{DragOperationMask, OutboundDragFinish, OutboundDragPolicy},
        ports::{
            BrowserCommand, BrowserEvent, BrowserEventEnvelope, BrowserEventSink, BrowserHandle,
            CloseMode, FrameSink, MouseModifiers, OwnedFrame, PublishResult, UiDispatcher,
        },
        runtime::ShutdownStatus,
        source::SourceViewLimits,
        window_hwnd,
    };
    use std::{
        cell::{Cell, RefCell},
        future::Future,
        path::PathBuf,
        rc::Rc,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        task::Poll,
        time::{Duration, Instant},
    };

    const DEFAULT_BING_URL: &str = "https://cn.bing.com/";
    const TOP_BAR_HEIGHT_DIP: f32 = 38.0;
    const WINDOW_WIDTH_DIP: f32 = 1200.0;
    const WINDOW_HEIGHT_DIP: f32 = 800.0;

    fn post_win32_key(hwnd: isize, vk: usize, up: bool) {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
            fn MapVirtualKeyW(uCode: u32, uMapType: u32) -> u32;
        }
        const WM_KEYDOWN: u32 = 0x0100;
        const WM_KEYUP: u32 = 0x0101;
        let scan = unsafe { MapVirtualKeyW(vk as u32, 0) } & 0xff;
        let base = 1u32 | (scan << 16);
        let (msg, lparam) = if up {
            (WM_KEYUP, (base | 0xC000_0000) as isize)
        } else {
            (WM_KEYDOWN, base as isize)
        };
        unsafe {
            PostMessageW(hwnd, msg, vk, lparam);
        }
    }

    fn post_win32_char(hwnd: isize, ch: usize) {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
        }
        const WM_CHAR: u32 = 0x0102;
        unsafe {
            PostMessageW(hwnd, WM_CHAR, ch, 1);
        }
    }

    fn end_win32_menu() {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn EndMenu() -> i32;
        }
        unsafe {
            EndMenu();
        }
    }

    fn apply_simulated_resize_steps(hwnd: isize, base_w: i32, base_h: i32, steps: &[(i32, i32)]) {
        #[allow(clashing_extern_declarations)]
        #[link(name = "user32")]
        unsafe extern "system" {
            fn SetWindowPos(
                hwnd: isize,
                insert_after: isize,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> i32;
        }
        const SWP_NOMOVE: u32 = 0x0002;
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;
        unsafe {
            for &(dw, dh) in steps {
                SetWindowPos(
                    hwnd,
                    0,
                    0,
                    0,
                    base_w + dw,
                    base_h + dh,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }

    fn begin_simulated_win32_drag_resize(hwnd: isize) -> Option<(i32, i32)> {
        #[repr(C)]
        struct Win32Rect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        #[allow(clashing_extern_declarations)]
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetWindowRect(hwnd: isize, rect: *mut Win32Rect) -> i32;
            fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
        }
        const WM_ENTERSIZEMOVE: u32 = 0x0231;

        let mut rc = Win32Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if unsafe { GetWindowRect(hwnd, &mut rc) } == 0 {
            return None;
        }
        let base_w = (rc.right - rc.left).max(400);
        let base_h = (rc.bottom - rc.top).max(300);
        unsafe {
            SendMessageW(hwnd, WM_ENTERSIZEMOVE, 0, 0);
        }
        apply_simulated_resize_steps(
            hwnd,
            base_w,
            base_h,
            &[
                (12, 8),
                (24, 16),
                (36, 24),
                (48, 28),
                (60, 32),
                (72, 36),
                (84, 40),
                (96, 40),
            ],
        );
        Some((base_w, base_h))
    }

    fn finish_simulated_win32_drag_resize(hwnd: isize, base_w: i32, base_h: i32) {
        #[allow(clashing_extern_declarations)]
        #[link(name = "user32")]
        unsafe extern "system" {
            fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
        }
        const WM_EXITSIZEMOVE: u32 = 0x0232;
        apply_simulated_resize_steps(
            hwnd,
            base_w,
            base_h,
            &[
                (84, 36),
                (72, 32),
                (60, 28),
                (48, 24),
                (36, 18),
                (24, 12),
                (12, 6),
                (0, 0),
            ],
        );
        unsafe {
            SendMessageW(hwnd, WM_EXITSIZEMOVE, 0, 0);
        }
    }

    fn ensure_win32_focus(hwnd: isize) -> bool {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetForegroundWindow() -> isize;
            fn SetForegroundWindow(hwnd: isize) -> i32;
            fn GetFocus() -> isize;
            fn SetFocus(hwnd: isize) -> isize;
        }
        unsafe {
            if GetForegroundWindow() != hwnd {
                let _ = SetForegroundWindow(hwnd);
            }
            if GetForegroundWindow() == hwnd && GetFocus() != hwnd {
                let _ = SetFocus(hwnd);
            }
            GetForegroundWindow() == hwnd && GetFocus() == hwnd
        }
    }

    fn exercise_ole_drop_target_and_data_object(
        hwnd: isize,
        scale: f32,
        handle: &BrowserHandle,
    ) -> Result<(), String> {
        use std::ffi::c_void;
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct ComPoint {
            x: i32,
            y: i32,
        }
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct FormatEtc {
            cf_format: u16,
            ptd: *mut c_void,
            dw_aspect: u32,
            lindex: i32,
            tymed: u32,
        }
        #[repr(C)]
        struct StgMedium {
            tymed: u32,
            union_ptr: *mut c_void,
            p_unk_for_release: *mut c_void,
        }
        #[repr(C)]
        struct IDataObjectVtbl {
            _qi: usize,
            _add_ref: usize,
            _release: usize,
            get_data:
                unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut StgMedium) -> i32,
            _get_data_here: usize,
            query_get_data: unsafe extern "system" fn(*mut c_void, *const FormatEtc) -> i32,
        }
        #[repr(C)]
        struct IDropTargetVtbl {
            _qi: usize,
            _add_ref: usize,
            _release: usize,
            drag_enter:
                unsafe extern "system" fn(*mut c_void, *mut c_void, u32, ComPoint, *mut u32) -> i32,
            drag_over: unsafe extern "system" fn(*mut c_void, u32, ComPoint, *mut u32) -> i32,
            drag_leave: unsafe extern "system" fn(*mut c_void) -> i32,
            drop:
                unsafe extern "system" fn(*mut c_void, *mut c_void, u32, ComPoint, *mut u32) -> i32,
        }
        #[link(name = "ole32")]
        unsafe extern "system" {
            fn OleInitialize(pv_reserved: *mut c_void) -> i32;
            fn OleUninitialize();
            fn ReleaseStgMedium(medium: *mut StgMedium);
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetPropW(hwnd: *mut c_void, name: *const u16) -> *mut c_void;
            fn ClientToScreen(hwnd: *mut c_void, pt: *mut ComPoint) -> i32;
            fn RegisterClipboardFormatW(format: *const u16) -> u32;
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GlobalLock(mem: *mut c_void) -> *mut c_void;
            fn GlobalUnlock(mem: *mut c_void) -> i32;
            fn GlobalSize(mem: *mut c_void) -> usize;
        }

        const OLE_DROP_TARGET_PROP: &[u16] = &[
            0x4f, 0x6c, 0x65, 0x44, 0x72, 0x6f, 0x70, 0x54, 0x61, 0x72, 0x67, 0x65, 0x74, 0x49,
            0x6e, 0x74, 0x65, 0x72, 0x66, 0x61, 0x63, 0x65, 0,
        ];
        const CFSTR_URL_W: &[u16] = &[
            0x55, 0x6e, 0x69, 0x66, 0x6f, 0x72, 0x6d, 0x52, 0x65, 0x73, 0x6f, 0x75, 0x72, 0x63,
            0x65, 0x4c, 0x6f, 0x63, 0x61, 0x74, 0x6f, 0x72, 0x57, 0,
        ];
        const CFSTR_DRAG_IMAGE_BITS: &[u16] = &[
            0x44, 0x72, 0x61, 0x67, 0x49, 0x6d, 0x61, 0x67, 0x65, 0x42, 0x69, 0x74, 0x73, 0,
        ];

        let ole_hr = unsafe { OleInitialize(std::ptr::null_mut()) };
        if ole_hr < 0 {
            return Err(format!("OleInitialize failed: 0x{:08x}", ole_hr as u32));
        }
        let ole_init = ole_hr == 0 || ole_hr == 1;

        let res = (|| -> Result<(), String> {
            let pixels = vec![0x80u8; 4 * 4 * 4];
            let drag_img =
                rust_cef_gpui::outbound_drag_contracts::DragImage::bounded(4, 4, 2, 2, pixels)
                    .ok_or("DragImage::bounded")?;
            let payload = rust_cef_gpui::outbound_drag_contracts::DraggedPayload::bounded(
                Some("https://gpui.rs/"),
                None,
                Some("https://gpui.rs/"),
                Vec::new(),
            )
            .with_image(Some(drag_img));
            let data_obj = rust_cef_gpui::ole_drag::new_ole_data_object(&payload);
            if data_obj.is_null() {
                return Err("new_ole_data_object returned null".into());
            }
            let plain_payload = rust_cef_gpui::outbound_drag_contracts::DraggedPayload::bounded(
                Some("PlainExternalText"),
                None,
                None,
                Vec::new(),
            );
            let plain_obj = rust_cef_gpui::ole_drag::new_ole_data_object(&plain_payload);
            if plain_obj.is_null() {
                unsafe { rust_cef_gpui::ole_drag::release_ole_data_object(data_obj) };
                return Err("new_ole_data_object(plain) returned null".into());
            }

            let obj_check = (|| -> Result<(), String> {
                let data_vtbl = unsafe { *(data_obj as *const *const IDataObjectVtbl) };
                let cf_url_w = unsafe { RegisterClipboardFormatW(CFSTR_URL_W.as_ptr()) } as u16;
                let cf_drag_bits =
                    unsafe { RegisterClipboardFormatW(CFSTR_DRAG_IMAGE_BITS.as_ptr()) } as u16;
                for (name, cf) in [
                    ("CF_UNICODETEXT", 13u16),
                    ("URLW", cf_url_w),
                    ("DragImageBits", cf_drag_bits),
                ] {
                    let fmt = FormatEtc {
                        cf_format: cf,
                        ptd: std::ptr::null_mut(),
                        dw_aspect: 1,
                        lindex: -1,
                        tymed: 1,
                    };
                    let hr = unsafe { ((*data_vtbl).query_get_data)(data_obj, &fmt) };
                    if hr != 0 {
                        return Err(format!(
                            "IDataObject::QueryGetData({name}) hr=0x{:08x}",
                            hr as u32
                        ));
                    }
                }
                let fmt_bits = FormatEtc {
                    cf_format: cf_drag_bits,
                    ptd: std::ptr::null_mut(),
                    dw_aspect: 1,
                    lindex: -1,
                    tymed: 1,
                };
                let mut med = StgMedium {
                    tymed: 0,
                    union_ptr: std::ptr::null_mut(),
                    p_unk_for_release: std::ptr::null_mut(),
                };
                let hr = unsafe { ((*data_vtbl).get_data)(data_obj, &fmt_bits, &mut med) };
                if hr != 0 || med.union_ptr.is_null() {
                    return Err(format!(
                        "IDataObject::GetData(DragImageBits) hr=0x{:08x}",
                        hr as u32
                    ));
                }
                let ok_dims = unsafe {
                    let sz = GlobalSize(med.union_ptr);
                    let ptr = GlobalLock(med.union_ptr);
                    let dims = if !ptr.is_null() && sz >= 8 {
                        let b = std::slice::from_raw_parts(ptr.cast::<u8>(), 8);
                        Some((
                            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                            u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                        ))
                    } else {
                        None
                    };
                    if !ptr.is_null() {
                        GlobalUnlock(med.union_ptr);
                    }
                    dims == Some((4, 4))
                };
                unsafe { ReleaseStgMedium(&mut med) };
                if !ok_dims {
                    return Err("SHDRAGIMAGE DragImageBits dimensions != (4, 4)".into());
                }

                let hwnd_ptr = hwnd as *mut c_void;
                let dt = unsafe { GetPropW(hwnd_ptr, OLE_DROP_TARGET_PROP.as_ptr()) };
                if dt.is_null() {
                    return Err("window missing ComDropTarget property".into());
                }
                let dt_vtbl = unsafe { *(dt as *const *const IDropTargetVtbl) };

                let to_screen = |dip_x: f32, dip_y: f32| -> ComPoint {
                    let mut pt = ComPoint {
                        x: (dip_x * scale).round() as i32,
                        y: (dip_y * scale).round() as i32,
                    };
                    unsafe { ClientToScreen(hwnd_ptr, &mut pt) };
                    pt
                };
                let pt_para = to_screen(120.0, TOP_BAR_HEIGHT_DIP + 138.0);
                let pt_input = to_screen(160.0, TOP_BAR_HEIGHT_DIP + 34.0);

                // 1. Over non-droppable paragraph `#para1` with plain external text:
                //    (Chromium accepts external URL drops on page body to navigate, whereas
                //     external plain text over `#para1` must yield DROPEFFECT_NONE / `🚫`.)
                let mut eff = 1u32 | 2 | 4;
                let _ = unsafe { ((*dt_vtbl).drag_enter)(dt, plain_obj, 1, pt_para, &mut eff) };
                std::thread::sleep(Duration::from_millis(30));
                eff = 1 | 2 | 4;
                let _ = unsafe { ((*dt_vtbl).drag_over)(dt, 1, pt_para, &mut eff) };
                if eff != 0 {
                    let _ = unsafe { ((*dt_vtbl).drag_leave)(dt) };
                    return Err(format!("expected DROPEFFECT_NONE over #para1, got {eff}"));
                }

                // 2. Simulate dragging out across window border (DragLeave): resets drag_target_operation.
                let _ = unsafe { ((*dt_vtbl).drag_leave)(dt) };
                std::thread::sleep(Duration::from_millis(20));
                if handle.drag_target_operation() != DragOperationMask::NONE {
                    return Err("drag_target_operation not cleared on DragLeave".into());
                }

                // 3. Plain text over `<input id='sb_form_q'>` negotiates DROPEFFECT_COPY (`1`), then leaves.
                let mut eff_plain = 1u32 | 2 | 4;
                let _ =
                    unsafe { ((*dt_vtbl).drag_enter)(dt, plain_obj, 1, pt_input, &mut eff_plain) };
                for _ in 0..40 {
                    if eff_plain != 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    eff_plain = 1 | 2 | 4;
                    let _ = unsafe { ((*dt_vtbl).drag_over)(dt, 1, pt_input, &mut eff_plain) };
                }
                let _ = unsafe { ((*dt_vtbl).drag_leave)(dt) };
                if eff_plain != 1 {
                    return Err(format!(
                        "expected DROPEFFECT_COPY (1) for plain text over #sb_form_q, got {eff_plain}"
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));

                // 4. Re-enter with URL + SHDRAGIMAGE `data_obj` over `<input id='sb_form_q'>` and drop URL.
                let mut eff_in = 1u32 | 2 | 4;
                let _ = unsafe { ((*dt_vtbl).drag_enter)(dt, data_obj, 1, pt_input, &mut eff_in) };
                for _ in 0..40 {
                    if eff_in != 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    eff_in = 1 | 2 | 4;
                    let _ = unsafe { ((*dt_vtbl).drag_over)(dt, 1, pt_input, &mut eff_in) };
                }
                if eff_in != 1 && eff_in != 4 {
                    let _ = unsafe { ((*dt_vtbl).drag_leave)(dt) };
                    return Err(format!(
                        "expected Copy(1) or Link(4) over #sb_form_q, got {eff_in}"
                    ));
                }
                let mut eff_drop = 1u32 | 2 | 4;
                let _ = unsafe { ((*dt_vtbl).drop)(dt, data_obj, 0, pt_input, &mut eff_drop) };
                if eff_drop != 1 && eff_drop != 4 {
                    return Err(format!(
                        "expected non-zero drop effect on #sb_form_q, got {eff_drop}"
                    ));
                }
                Ok(())
            })();

            unsafe {
                rust_cef_gpui::ole_drag::release_ole_data_object(plain_obj);
                rust_cef_gpui::ole_drag::release_ole_data_object(data_obj);
            }
            obj_check
        })();

        if ole_init {
            unsafe { OleUninitialize() };
        }
        res
    }

    struct Args {
        bundle: PathBuf,
        helper: PathBuf,
        cache_root: PathBuf,
        url: String,
        mode: ThreadMode,
        backend: RenderPreference,
        native_hwnd: bool,
        custom_menu: bool,
        self_test_osr: bool,
        self_test_osr_dnd: bool,
        smoke: Option<Duration>,
    }

    fn parse_args() -> Result<Args, String> {
        let exe_dir = std::env::current_exe()
            .map_err(|e| format!("current_exe: {e}"))?
            .parent()
            .ok_or("missing exe directory")?
            .to_path_buf();
        let mut bundle = exe_dir.clone();
        let mut helper = exe_dir.join("rust-cef-helper.exe");
        let mut cache_root = exe_dir.join("cache");
        let mut url = DEFAULT_BING_URL.to_owned();
        let mut mode = ThreadMode::Threaded;
        let mut backend = RenderPreference::D3d11Required;
        let mut native_hwnd = false;
        let mut custom_menu = false;
        let mut self_test_osr = false;
        let mut self_test_osr_dnd = false;
        let mut smoke = None;

        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--native-hwnd" => native_hwnd = true,
                "--custom-menu" => custom_menu = true,
                "--self-test-osr" => self_test_osr = true,
                "--self-test-osr-dnd" => self_test_osr_dnd = true,
                "--do-not-de-elevate" => {
                    // Consumed directly by libcef from GetCommandLineW() before
                    // BasicStartupComplete() when launched from an elevated Windows shell.
                }
                arg if arg.starts_with("--do-not-de-elevate=") => {}
                "--bundle" => {
                    bundle = PathBuf::from(it.next().ok_or("missing value for --bundle")?);
                }
                "--helper" => {
                    helper = PathBuf::from(it.next().ok_or("missing value for --helper")?);
                }
                "--cache-root" => {
                    cache_root = PathBuf::from(it.next().ok_or("missing value for --cache-root")?);
                }
                "--url" => {
                    url = it.next().ok_or("missing value for --url")?;
                }
                "--mode" => match it.next().as_deref() {
                    Some("Integrated") => mode = ThreadMode::Integrated,
                    Some("Threaded") => mode = ThreadMode::Threaded,
                    _ => return Err("--mode must be Integrated or Threaded".into()),
                },
                "--backend" => match it.next().as_deref() {
                    Some("Software") => backend = RenderPreference::SoftwareRequired,
                    Some("D3d11") => backend = RenderPreference::D3d11Required,
                    _ => return Err("--backend must be Software or D3d11".into()),
                },
                "--smoke-ms" => {
                    let ms: u64 = it
                        .next()
                        .ok_or("missing value for --smoke-ms")?
                        .parse()
                        .map_err(|_| "invalid --smoke-ms")?;
                    smoke = Some(Duration::from_millis(ms.clamp(200, 60_000)));
                }
                "--help" | "-h" => {
                    println!(
                        "bing_cn [--url https://cn.bing.com/] [--mode Threaded|Integrated] [--backend D3d11|Software] [--native-hwnd] [--custom-menu] [--self-test-osr|--self-test-osr-dnd] [--do-not-de-elevate] [--smoke-ms 500..60000] [--bundle <dir>] [--helper <exe>] [--cache-root <dir>]"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        if self_test_osr && self_test_osr_dnd {
            return Err("--self-test-osr and --self-test-osr-dnd cannot be combined".into());
        }
        if self_test_osr_dnd && native_hwnd {
            return Err("--self-test-osr-dnd requires OSR mode (omit --native-hwnd)".into());
        }
        Ok(Args {
            bundle,
            helper,
            cache_root,
            url,
            mode,
            backend,
            native_hwnd,
            custom_menu,
            self_test_osr,
            self_test_osr_dnd,
            smoke,
        })
    }

    struct Observer {
        created: AtomicU64,
        closed: AtomicU64,
        denied_resources: AtomicU64,
        denied_navs: AtomicU64,
        incomplete_frames: AtomicU64,
        resize_exhausted: AtomicU64,
        fail_on_renderer_terminated: bool,
        renderer_crashed: Mutex<Option<String>>,
        renderer_recovered: AtomicBool,
        fault: Mutex<Option<String>>,
    }

    impl Observer {
        fn new(fail_on_renderer_terminated: bool) -> Self {
            Self {
                created: AtomicU64::new(0),
                closed: AtomicU64::new(0),
                denied_resources: AtomicU64::new(0),
                denied_navs: AtomicU64::new(0),
                incomplete_frames: AtomicU64::new(0),
                resize_exhausted: AtomicU64::new(0),
                fail_on_renderer_terminated,
                renderer_crashed: Mutex::new(None),
                renderer_recovered: AtomicBool::new(false),
                fault: Mutex::new(None),
            }
        }
    }

    impl BrowserEventSink for Observer {
        fn publish(&self, envelope: BrowserEventEnvelope) -> PublishResult {
            match envelope.event {
                BrowserEvent::Created => {
                    self.created.fetch_add(1, Ordering::Release);
                }
                BrowserEvent::Closed => {
                    self.closed.fetch_add(1, Ordering::Release);
                }
                BrowserEvent::PolicyDenied { action } => {
                    use rust_cef_gpui::ports::PolicyAction;
                    match action {
                        PolicyAction::Resource => {
                            self.denied_resources.fetch_add(1, Ordering::Relaxed);
                        }
                        PolicyAction::Navigation | PolicyAction::Redirect => {
                            self.denied_navs.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                    println!("BING_POLICY_DENIED action={action:?}");
                }
                BrowserEvent::LoadFailed {
                    is_main_frame,
                    code,
                } if is_main_frame => {
                    println!("BING_MAIN_LOAD_FAILED code={code}");
                }
                // Transient letterboxed intermediate frames (`IncompleteFrame`) and
                // idle/hidden resize exhaustion (`ResizeExhausted`) are recoverable
                // during interactive resize/maximize/move and must not close the browser.
                BrowserEvent::CaptureFailed(rust_cef_gpui::ports::EngineError::Capture {
                    kind: rust_cef_gpui::ports::CaptureFailureKind::IncompleteFrame,
                    code: None,
                }) => {
                    self.incomplete_frames.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::CaptureFailed(rust_cef_gpui::ports::EngineError::Capture {
                    kind: rust_cef_gpui::ports::CaptureFailureKind::ResizeExhausted,
                    code: None,
                }) => {
                    self.resize_exhausted.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::CaptureFailed(rust_cef_gpui::ports::EngineError::Capture {
                    kind: rust_cef_gpui::ports::CaptureFailureKind::Busy,
                    ..
                }) => {}
                BrowserEvent::RenderViewReady => {
                    *self
                        .renderer_crashed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
                    self.renderer_recovered.store(true, Ordering::Release);
                }
                fault @ BrowserEvent::RendererTerminated {
                    native_status,
                    code,
                } => {
                    if self.fail_on_renderer_terminated {
                        let msg = format!("{fault:?}");
                        self.fault
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get_or_insert(msg);
                    } else {
                        eprintln!(
                            "Renderer crashed (status={native_status}, code={code:?}); click Reload or press F5/Ctrl+R to recover"
                        );
                        let badge = format!(
                            "Renderer crashed (code={}) — Reload",
                            code.map_or_else(|| "?".into(), |c| c.to_string())
                        );
                        *self
                            .renderer_crashed
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = Some(badge);
                        self.renderer_recovered.store(false, Ordering::Release);
                    }
                }
                fault @ (BrowserEvent::CreateFailed(_) | BrowserEvent::CaptureFailed(_)) => {
                    let msg = format!("{fault:?}");
                    self.fault
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_or_insert(msg);
                }
                _ => {}
            }
            PublishResult::Published
        }
    }

    struct IgnoreFrame;
    impl FrameSink for IgnoreFrame {
        fn publish(&self, _: OwnedFrame) -> PublishResult {
            PublishResult::Published
        }
    }

    fn sanitize_download_filename(raw: &str) -> String {
        let leaf = std::path::Path::new(raw)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("download.bin");
        let cleaned: String = leaf
            .chars()
            .map(|ch| {
                if ch.is_control()
                    || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                {
                    '_'
                } else {
                    ch
                }
            })
            .collect();
        let trimmed = cleaned.trim_matches(['.', ' ']);
        if trimmed.is_empty() {
            "download.bin".to_owned()
        } else {
            trimmed.to_owned()
        }
    }

    fn unique_download_path(dir: &std::path::Path, suggested: &str) -> PathBuf {
        let name = sanitize_download_filename(suggested);
        let first = dir.join(&name);
        if !first.exists() {
            return first;
        }
        let stem = std::path::Path::new(&name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("download");
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(|s| s.to_str());
        for i in 1..=999u32 {
            let next_name = match ext {
                Some(ext) if !ext.is_empty() => format!("{stem} ({i}).{ext}"),
                _ => format!("{stem} ({i})"),
            };
            let cand = dir.join(next_name);
            if !cand.exists() {
                return cand;
            }
        }
        dir.join(format!("{stem}-{}", std::process::id()))
    }

    fn run_win32_file_dialog(req: &FileDialogRequest) -> FileDialogDecision {
        if matches!(req.mode, FileDialogKind::OpenFolder) {
            return FileDialogDecision::Cancel;
        }
        #[repr(C)]
        struct OpenFileNameW {
            struct_size: u32,
            hwnd_owner: isize,
            h_instance: isize,
            filter: *const u16,
            custom_filter: *mut u16,
            max_cust_filter: u32,
            filter_index: u32,
            file: *mut u16,
            max_file: u32,
            file_title: *mut u16,
            max_file_title: u32,
            initial_dir: *const u16,
            title: *const u16,
            flags: u32,
            file_offset: u16,
            file_extension: u16,
            def_ext: *const u16,
            cust_data: usize,
            hook: usize,
            template_name: *const u16,
            reserved: *mut std::ffi::c_void,
            reserved_u32: u32,
            flags_ex: u32,
        }
        #[link(name = "comdlg32")]
        unsafe extern "system" {
            fn GetOpenFileNameW(ofn: *mut OpenFileNameW) -> i32;
            fn GetSaveFileNameW(ofn: *mut OpenFileNameW) -> i32;
        }
        const OFN_EXPLORER: u32 = 0x0008_0000;
        const OFN_ALLOWMULTISELECT: u32 = 0x0000_0200;
        const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
        const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
        const OFN_OVERWRITEPROMPT: u32 = 0x0000_0002;
        const OFN_NOCHANGEDIR: u32 = 0x0000_0008;

        let mut buf = vec![0u16; 8192];
        let initial_name: Vec<u16> = sanitize_download_filename(&req.default_file_path)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        if req.mode == FileDialogKind::Save && initial_name.len() <= buf.len() {
            buf[..initial_name.len()].copy_from_slice(&initial_name);
        }
        let title_w: Vec<u16> = if req.title.is_empty() {
            "Select file\0".encode_utf16().collect()
        } else {
            req.title.encode_utf16().chain(std::iter::once(0)).collect()
        };
        let filter_w: Vec<u16> = "All Files\0*.*\0\0".encode_utf16().collect();
        let mut flags = OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_PATHMUSTEXIST;
        match req.mode {
            FileDialogKind::Open => flags |= OFN_FILEMUSTEXIST,
            FileDialogKind::OpenMultiple => flags |= OFN_FILEMUSTEXIST | OFN_ALLOWMULTISELECT,
            FileDialogKind::Save => flags |= OFN_OVERWRITEPROMPT,
            FileDialogKind::OpenFolder => {}
        }
        let mut ofn = OpenFileNameW {
            struct_size: std::mem::size_of::<OpenFileNameW>() as u32,
            hwnd_owner: 0,
            h_instance: 0,
            filter: filter_w.as_ptr(),
            custom_filter: std::ptr::null_mut(),
            max_cust_filter: 0,
            filter_index: 1,
            file: buf.as_mut_ptr(),
            max_file: buf.len() as u32,
            file_title: std::ptr::null_mut(),
            max_file_title: 0,
            initial_dir: std::ptr::null(),
            title: title_w.as_ptr(),
            flags,
            file_offset: 0,
            file_extension: 0,
            def_ext: std::ptr::null(),
            cust_data: 0,
            hook: 0,
            template_name: std::ptr::null(),
            reserved: std::ptr::null_mut(),
            reserved_u32: 0,
            flags_ex: 0,
        };
        let ok = unsafe {
            if req.mode == FileDialogKind::Save {
                GetSaveFileNameW(&mut ofn)
            } else {
                GetOpenFileNameW(&mut ofn)
            }
        };
        if ok == 0 {
            return FileDialogDecision::Cancel;
        }
        let parts: Vec<String> = buf
            .split(|&c| c == 0)
            .take_while(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect();
        if parts.is_empty() {
            FileDialogDecision::Cancel
        } else if parts.len() == 1 {
            FileDialogDecision::Accept(vec![PathBuf::from(&parts[0])])
        } else {
            let dir = PathBuf::from(&parts[0]);
            FileDialogDecision::Accept(parts[1..].iter().map(|p| dir.join(p)).collect())
        }
    }

    struct BingDownloadHandler {
        dir: PathBuf,
        wake: Arc<UiWakeSender>,
        pending: Mutex<Vec<(rust_cef_gpui::host_capabilities::DownloadId, PathBuf)>>,
        updates: Mutex<Vec<DownloadProgress>>,
    }

    impl DownloadService for BingDownloadHandler {
        fn on_before_download(&self, request: DownloadRequest) {
            let _ = std::fs::create_dir_all(&self.dir);
            let path = unique_download_path(&self.dir, &request.suggested_name);
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((request.id, path));
            let _ = self.wake.wake();
        }

        fn on_download_updated(&self, progress: DownloadProgress) {
            self.updates
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(progress);
            let _ = self.wake.wake();
        }
    }

    struct BingFileDialogHandler {
        auto_cancel: bool,
        wake: Arc<UiWakeSender>,
        decisions: Arc<Mutex<Vec<(HostRequestId, FileDialogDecision)>>>,
    }

    impl FileDialogService for BingFileDialogHandler {
        fn on_file_dialog(&self, request: FileDialogRequest) {
            if self.auto_cancel {
                self.decisions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((request.id, FileDialogDecision::Cancel));
                let _ = self.wake.wake();
                return;
            }
            let out = self.decisions.clone();
            let wake = self.wake.clone();
            std::thread::spawn(move || {
                let id = request.id;
                let decision = run_win32_file_dialog(&request);
                out.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((id, decision));
                let _ = wake.wake();
            });
        }
    }

    struct BingJsDialogHandler {
        auto_accept: bool,
        wake: Arc<UiWakeSender>,
        decisions: Arc<Mutex<Vec<(HostRequestId, JsDialogDecision)>>>,
    }

    impl JsDialogService for BingJsDialogHandler {
        fn on_js_dialog(&self, request: JsDialogRequest) {
            if self.auto_accept {
                let decision = JsDialogDecision::Accept {
                    prompt_input: (request.kind == JsDialogKind::Prompt)
                        .then_some(request.default_prompt_text),
                };
                self.decisions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((request.id, decision));
                let _ = self.wake.wake();
                return;
            }
            let out = self.decisions.clone();
            let wake = self.wake.clone();
            std::thread::spawn(move || {
                #[link(name = "user32")]
                unsafe extern "system" {
                    fn MessageBoxW(
                        hwnd: isize,
                        text: *const u16,
                        caption: *const u16,
                        flags: u32,
                    ) -> i32;
                }
                const MB_OK: u32 = 0x0000_0000;
                const MB_OKCANCEL: u32 = 0x0000_0001;
                const MB_ICONINFORMATION: u32 = 0x0000_0040;
                const MB_ICONQUESTION: u32 = 0x0000_0020;
                const IDOK: i32 = 1;

                let title_w: Vec<u16> = format!("Page Dialog ({})\0", request.origin_url)
                    .encode_utf16()
                    .collect();
                let msg_w: Vec<u16> = request
                    .message_text
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let flags = match request.kind {
                    JsDialogKind::Alert => MB_OK | MB_ICONINFORMATION,
                    JsDialogKind::Confirm | JsDialogKind::Prompt => MB_OKCANCEL | MB_ICONQUESTION,
                };
                let clicked = unsafe { MessageBoxW(0, msg_w.as_ptr(), title_w.as_ptr(), flags) };
                let decision = if clicked == IDOK {
                    JsDialogDecision::Accept {
                        prompt_input: (request.kind == JsDialogKind::Prompt)
                            .then_some(request.default_prompt_text),
                    }
                } else {
                    JsDialogDecision::Cancel
                };
                out.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((request.id, decision));
                let _ = wake.wake();
            });
        }
    }

    #[derive(Clone, Copy)]
    enum ShortcutAction {
        ToggleDevTools,
        Reload,
        Back,
        Forward,
        ZoomStep(i32),
        ZoomReset,
        CancelFind,
    }

    #[derive(Default)]
    struct PopupForwarder {
        urls: Mutex<Vec<String>>,
    }

    impl PopupService for PopupForwarder {
        fn on_popup_request(&self, request: PopupRequest) {
            if request.policy_allowed && !request.target_url.is_empty() {
                self.urls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(request.target_url);
            }
        }
    }

    #[derive(Clone)]
    enum BrowserView {
        Osr(Entity<CefBrowser>),
        Native(Entity<NativeCefBrowser>),
    }

    impl BrowserView {
        fn consume(&self, window: &mut Window, cx: &mut App) -> Result<(), String> {
            match self {
                Self::Osr(v) => v
                    .update(cx, |b, cx| b.consume(window, cx))
                    .map(|_| ())
                    .map_err(|e| format!("consume osr: {e:?}")),
                Self::Native(v) => v
                    .update(cx, |b, cx| b.consume(window, cx))
                    .map_err(|e| format!("consume native: {e:?}")),
            }
        }

        fn into_any(self) -> gpui::AnyElement {
            match self {
                Self::Osr(v) => v.into_any_element(),
                Self::Native(v) => v.into_any_element(),
            }
        }
    }

    struct BingWindow {
        view: Option<BrowserView>,
        handle: Option<BrowserHandle>,
        open_devtools: Option<Rc<dyn Fn()>>,
        cancel_find: Option<Rc<dyn Fn()>>,
        menu: Option<Rc<RefCell<DefaultMenuPresenter>>>,
        native_hwnd: bool,
        home_url: String,
        address: String,
        title: String,
        loading: bool,
        can_back: bool,
        can_forward: bool,
        zoom_milli: i32,
        find_badge: Option<String>,
        download_badge: Option<String>,
        crash_badge: Option<String>,
        status: &'static str,
    }

    impl Render for BingWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let menu = self
                .menu
                .as_ref()
                .and_then(|m| m.borrow().element(window, cx));
            let back_handle = self.handle.clone();
            let fwd_handle = self.handle.clone();
            let reload_handle = self.handle.clone();
            let home_handle = self.handle.clone();
            let zoom_handle = self.handle.clone();
            let zoom_view = self.view.clone();
            let crash_reload_handle = self.handle.clone();
            let open_devtools = self.open_devtools.clone();
            let cancel_find = self.cancel_find.clone();
            let home_url = self.home_url.clone();
            let effective_zoom_milli = match &self.view {
                Some(BrowserView::Osr(v)) => {
                    zoom_level_to_milli(v.read(cx).zoom_level()).unwrap_or(self.zoom_milli)
                }
                _ => self.zoom_milli,
            };
            let zoom_pct =
                (1.2_f64.powf(f64::from(effective_zoom_milli) / 1000.0) * 100.0).round() as i32;

            let btn = |id: &'static str, label: &'static str, enabled: bool| {
                div()
                    .id(id)
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .bg(if enabled {
                        rgb(0x334155)
                    } else {
                        rgb(0x1e293b)
                    })
                    .text_color(if enabled {
                        rgb(0xf8fafc)
                    } else {
                        rgb(0x64748b)
                    })
                    .child(label)
            };

            let toolbar = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .px(px(10.0))
                .h(px(TOP_BAR_HEIGHT_DIP))
                .flex_shrink_0()
                .bg(rgb(0x0f172a))
                .border_b_1()
                .border_color(rgb(0x1e293b))
                .text_size(px(12.0))
                .child(btn("bing-back", "Back", self.can_back).on_mouse_down(
                    MouseButton::Left,
                    move |_, _, _| {
                        if let Some(h) = &back_handle {
                            let _ = h.submit(BrowserCommand::Back);
                        }
                    },
                ))
                .child(btn("bing-fwd", "Forward", self.can_forward).on_mouse_down(
                    MouseButton::Left,
                    move |_, _, _| {
                        if let Some(h) = &fwd_handle {
                            let _ = h.submit(BrowserCommand::Forward);
                        }
                    },
                ))
                .child(btn("bing-reload", "Reload", true).on_mouse_down(
                    MouseButton::Left,
                    move |_, _, _| {
                        if let Some(h) = &reload_handle {
                            let _ = h.submit(BrowserCommand::Reload);
                        }
                    },
                ))
                .child(btn("bing-home", "Bing", true).on_mouse_down(
                    MouseButton::Left,
                    move |_, _, _| {
                        if let Some(h) = &home_handle {
                            let _ = h.submit(BrowserCommand::Navigate(home_url.clone()));
                        }
                    },
                ))
                .child(
                    btn("bing-devtools", "DevTools", open_devtools.is_some()).on_mouse_down(
                        MouseButton::Left,
                        move |_, _, _| {
                            if let Some(cb) = &open_devtools {
                                cb();
                            }
                        },
                    ),
                )
                .child(
                    div()
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(4.0))
                        .bg(rgb(0x1d4ed8))
                        .text_color(rgb(0xffffff))
                        .child(if self.native_hwnd {
                            "NativeHWND"
                        } else {
                            "OSR"
                        }),
                )
                .when(effective_zoom_milli != 0, |el| {
                    el.child(
                        div()
                            .id("bing-zoom")
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(rgb(0x334155))
                            .text_color(rgb(0xe2e8f0))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    match &zoom_view {
                                        Some(BrowserView::Osr(v)) => {
                                            v.update(cx, |b, cx| b.reset_zoom(window, cx));
                                            this.zoom_milli = 0;
                                        }
                                        _ => {
                                            if let Some(h) = &zoom_handle {
                                                let _ = h.submit_zoom_level_fast(0.0);
                                                this.zoom_milli = 0;
                                            }
                                        }
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(format!("{zoom_pct}%")),
                    )
                })
                .when_some(self.find_badge.clone(), |el, badge| {
                    let cancel_cb = cancel_find.clone();
                    el.child(
                        div()
                            .id("bing-find-badge")
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(rgb(0x0f766e))
                            .text_color(rgb(0xf0fdfa))
                            .on_mouse_down(MouseButton::Left, move |_, _, _| {
                                if let Some(cb) = &cancel_cb {
                                    cb();
                                }
                            })
                            .child(format!("{badge} ×")),
                    )
                })
                .when_some(self.download_badge.clone(), |el, badge| {
                    el.child(
                        div()
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(rgb(0x1e3a8a))
                            .text_color(rgb(0xdbeafe))
                            .child(badge),
                    )
                })
                .when_some(self.crash_badge.clone(), |el, badge| {
                    let h = crash_reload_handle.clone();
                    el.child(
                        div()
                            .id("bing-crash-badge")
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(rgb(0x991b1b))
                            .text_color(rgb(0xfef2f2))
                            .on_mouse_down(MouseButton::Left, move |_, _, _| {
                                if let Some(h) = &h {
                                    let _ = h.submit(BrowserCommand::Reload);
                                }
                            })
                            .child(badge),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .px(px(8.0))
                        .py(px(3.0))
                        .rounded(px(4.0))
                        .bg(rgb(0x1e293b))
                        .text_color(rgb(0xe2e8f0))
                        .overflow_hidden()
                        .child(format!(
                            "{}{}  {}",
                            if self.loading { "[Loading] " } else { "" },
                            self.address,
                            if self.title.is_empty() {
                                String::new()
                            } else {
                                format!("| {}", self.title)
                            }
                        )),
                );

            let content = if let Some(view) = self.view.clone() {
                div()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(view.into_any())
            } else {
                div()
                    .flex_1()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(0x0b1220))
                    .text_color(rgb(0xcbd5e1))
                    .child(self.status)
            };

            let root = div()
                .flex()
                .flex_col()
                .size_full()
                .bg(rgb(0x0f172a))
                .overflow_hidden()
                .child(toolbar)
                .child(content);

            match menu {
                Some(m) => root.child(m),
                None => root,
            }
        }
    }

    async fn wait_wake(
        cx: &mut AsyncApp,
        receiver: &UiWakeReceiver,
        timeout: Option<Duration>,
    ) -> Result<bool, String> {
        let Some(timeout) = timeout else {
            return if receiver.recv().await {
                Ok(true)
            } else {
                Err("wake channel closed".into())
            };
        };
        let mut wake = std::pin::pin!(receiver.recv());
        let mut timer = std::pin::pin!(cx.background_executor().timer(timeout));
        std::future::poll_fn(|cx| {
            if let Poll::Ready(ok) = wake.as_mut().poll(cx) {
                return Poll::Ready(if ok {
                    Ok(true)
                } else {
                    Err("wake channel closed".into())
                });
            }
            if timer.as_mut().poll(cx).is_ready() {
                Poll::Ready(Ok(false))
            } else {
                Poll::Pending
            }
        })
        .await
    }

    async fn service_loop(
        cx: &mut AsyncApp,
        rt: &Rc<RefCell<Runtime>>,
        receiver: &UiWakeReceiver,
        deadline: Option<Instant>,
    ) -> Result<(), String> {
        let mode = rt.borrow().mode();
        let mut timeout = deadline.map(|t| t.saturating_duration_since(Instant::now()));
        if mode == ThreadMode::Integrated {
            let pump = Duration::from_millis(4);
            timeout = Some(timeout.map_or(pump, |v| v.min(pump)));
        }
        let max_turn = Duration::from_millis(50);
        timeout = Some(timeout.map_or(max_turn, |v| v.min(max_turn)));
        let _ = wait_wake(cx, receiver, timeout).await?;
        if mode == ThreadMode::Integrated {
            rt.borrow_mut().pump().map_err(|e| format!("pump: {e:?}"))?;
        }
        Ok(())
    }

    async fn shutdown_runtime(
        cx: &mut AsyncApp,
        rt: &Rc<RefCell<Runtime>>,
        receiver: &UiWakeReceiver,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        let ticket = rt
            .borrow_mut()
            .request_shutdown(CloseMode::Graceful, deadline);
        loop {
            let status = rt.borrow_mut().poll_shutdown(&ticket);
            match status {
                ShutdownStatus::Ready => {
                    return rt.borrow_mut().finish_shutdown().is_ok();
                }
                ShutdownStatus::ProcessExitRequired { .. }
                | ShutdownStatus::Blocked { .. }
                | ShutdownStatus::Stopped
                | ShutdownStatus::NotRequested => {
                    eprintln!("shutdown exited with status: {status:?}");
                    return false;
                }
                _ => {
                    if service_loop(cx, rt, receiver, Some(deadline))
                        .await
                        .is_err()
                    {
                        return false;
                    }
                }
            }
        }
    }

    async fn drive(
        cx: &mut AsyncApp,
        win: WindowHandle<BingWindow>,
        args: Args,
        close: Rc<Cell<bool>>,
        sender: Arc<UiWakeSender>,
        receiver: UiWakeReceiver,
    ) -> i32 {
        let bundle_sibling_dl = args
            .bundle
            .parent()
            .unwrap_or(&args.bundle)
            .join("bing-cn-downloads");
        let any_self_test = args.self_test_osr || args.self_test_osr_dnd;
        let dl_dir = if any_self_test {
            bundle_sibling_dl
        } else {
            std::env::var_os("USERPROFILE")
                .map(|p| PathBuf::from(p).join("Downloads"))
                .unwrap_or(bundle_sibling_dl)
        };
        let obs = Arc::new(Observer::new(any_self_test || args.smoke.is_some()));
        let rt = match Runtime::start(
            RuntimeConfig {
                files: RuntimeFiles {
                    directory: args.bundle,
                    helper_executable: args.helper,
                },
                root_cache_path: args.cache_root,
                thread_mode: args.mode,
                default_profile: ProfileStorage::Memory,
                content_policy: TrustedContentPolicy::allow_all_http_https(true),
                settings: RuntimeSettings {
                    windowless_rendering_enabled: !args.native_hwnd,
                    ..Default::default()
                },
            },
            RuntimeServices {
                events: obs.clone(),
                frames: Arc::new(IgnoreFrame),
                ui: sender.clone(),
            },
        ) {
            Ok(rt) => Rc::new(RefCell::new(rt)),
            Err(rust_cef_gpui::runtime::StartError::ProcessExitRequired {
                exit_code: Some(38),
            }) => {
                eprintln!(
                    "Runtime::start exited with code 38 (CEF_RESULT_CODE_NORMAL_EXIT_AUTO_DE_ELEVATED): \
                     CEF 156 detected an elevated (Administrator) process and triggered Chromium auto-de-elevation. \
                     Run from a non-elevated terminal, or pass --do-not-de-elevate (or -DoNotDeElevate in scripts/run-bing.ps1)."
                );
                return 38;
            }
            Err(e) => {
                eprintln!("Runtime::start failed: {e:?}");
                return 1;
            }
        };

        let startup_deadline = Instant::now() + Duration::from_secs(30);
        while rt.borrow().state() != RuntimeState::Ready {
            if close.get() {
                let _ = shutdown_runtime(cx, &rt, &receiver).await;
                return 0;
            }
            if Instant::now() >= startup_deadline {
                eprintln!("Runtime readiness timed out");
                let _ = shutdown_runtime(cx, &rt, &receiver).await;
                return 1;
            }
            if let Err(e) = service_loop(cx, &rt, &receiver, Some(startup_deadline)).await {
                eprintln!("{e}");
                let _ = shutdown_runtime(cx, &rt, &receiver).await;
                return 1;
            }
        }

        let popups = Arc::new(PopupForwarder::default());
        let downloads = Arc::new(BingDownloadHandler {
            dir: dl_dir,
            wake: sender.clone(),
            pending: Mutex::new(Vec::new()),
            updates: Mutex::new(Vec::new()),
        });
        let file_dialog_decisions = Arc::new(Mutex::new(Vec::new()));
        let file_dialogs = Arc::new(BingFileDialogHandler {
            auto_cancel: any_self_test,
            wake: sender.clone(),
            decisions: file_dialog_decisions.clone(),
        });
        let js_dialog_decisions = Arc::new(Mutex::new(Vec::new()));
        let js_dialogs = Arc::new(BingJsDialogHandler {
            auto_accept: any_self_test,
            wake: sender.clone(),
            decisions: js_dialog_decisions.clone(),
        });
        {
            let rt_ref = rt.borrow();
            let _ = rt_ref.set_host_capabilities(HostCapabilities {
                download: Some(downloads.clone()),
                file_dialog: Some(file_dialogs.clone()),
                js_dialog: Some(js_dialogs.clone()),
                popup: Some(popups.clone()),
                ..Default::default()
            });
        }

        let nz = |n: usize| std::num::NonZeroUsize::new(n).expect("non-zero");
        let limits = MenuLimits {
            max_entries: nz(64),
            max_label_bytes: nz(512),
            max_total_label_bytes: nz(8192),
        };
        let context_limits = MenuContextLimits {
            max_frame_id_bytes: nz(512),
            max_text_bytes: nz(4096),
            max_suggestions: nz(5),
            max_total_bytes: nz(8192),
        };
        let use_custom_menu = !args.native_hwnd && (args.custom_menu || args.self_test_osr);
        if !args.native_hwnd {
            let rt_ref = rt.borrow();
            let _ = rt_ref.configure_devtools(nz(1));
            if use_custom_menu {
                let _ = rt_ref.configure_context_menus(ContextMenuConfig {
                    capacity: nz(1),
                    model_limits: limits,
                    context_limits,
                });
                let _ = rt_ref.configure_menu_navigation(true);
                let _ = rt_ref.configure_finds(FindLimits {
                    capacity: nz(1),
                    max_query_bytes: nz(256),
                });
                let _ = rt_ref.configure_source_views(SourceViewLimits {
                    capacity: nz(2),
                    max_source_bytes: nz(2 * 1024 * 1024),
                });
            }
        }

        let (width, height, scale) = win
            .update(cx, |_, window, _| {
                let s = window.bounds().size;
                (
                    f32::from(s.width).max(1.0),
                    (f32::from(s.height) - TOP_BAR_HEIGHT_DIP).max(1.0),
                    window.scale_factor().max(1.0),
                )
            })
            .unwrap_or((
                WINDOW_WIDTH_DIP,
                WINDOW_HEIGHT_DIP - TOP_BAR_HEIGHT_DIP,
                1.0,
            ));

        let parent_hwnd = win
            .update(cx, |_, window, _| window_hwnd(window))
            .ok()
            .flatten()
            .expect("Win32 HWND required for browser host window");
        let mut options = BrowserOptions::default()
            .with_tooltip_events(!args.native_hwnd)
            .with_outbound_drag_policy(if args.native_hwnd {
                OutboundDragPolicy::Deny
            } else {
                OutboundDragPolicy::AllowAsync
            });
        if !args.native_hwnd {
            options = options
                .with_frame_rate(rust_cef_gpui::recommended_windowless_frame_rate())
                .with_parent_hwnd(parent_hwnd);
        }
        if args.native_hwnd {
            options = options.with_surface_mode(BrowserSurfaceMode::NativeChildHwnd(
                NativeChildHwndConfig {
                    parent_hwnd,
                    initial_bounds: PixelRect {
                        x: 0,
                        y: (TOP_BAR_HEIGHT_DIP * scale).round() as i32,
                        width: (width * scale).round().max(1.0) as u32,
                        height: (height * scale).round().max(1.0) as u32,
                    },
                    activate_on_creation: false,
                },
            ));
        }

        let default_profile = rt.borrow().default_profile_handle();
        let handle = match rt.borrow_mut().create_browser(BrowserConfig {
            initial_url: args.url.clone(),
            profile: Some(default_profile),
            render_preference: args.backend,
            viewport: Viewport {
                width: width.round() as i32,
                height: height.round() as i32,
                scale,
            },
            options,
        }) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("create_browser failed: {e:?}");
                let _ = shutdown_runtime(cx, &rt, &receiver).await;
                return 1;
            }
        };

        let browser_id = handle.snapshot().browser;
        let attachment = rt.borrow().attach_browser(&handle).expect("attach browser");
        let rt_for_drag = rt.clone();
        let rt_for_devtools = rt.clone();
        #[derive(Clone, Debug)]
        struct ObservedDrag {
            url: Option<String>,
            html: Option<String>,
            file_contents: Option<(String, usize, bool)>,
            image: Option<(u32, u32, i32, i32, usize)>,
        }
        let observed_drag: Rc<RefCell<Option<ObservedDrag>>> = Rc::new(RefCell::new(None));
        let observed_drag_cb = observed_drag.clone();

        let attached_view = win
            .update(cx, |root, window, cx| {
                let view = if args.native_hwnd {
                    BrowserView::Native(
                        NativeCefBrowser::new(attachment, window, cx)
                            .expect("NativeCefBrowser::new"),
                    )
                } else {
                    let v = CefBrowser::new(attachment, window, cx).expect("CefBrowser::new");
                    v.update(cx, |b, _| {
                        b.set_pinch_zoom_enabled(true);
                        b.set_ctrl_wheel_zoom_enabled(true);
                        if args.mode == ThreadMode::Integrated {
                            let rt_pump = rt_for_drag.clone();
                            b.set_outbound_drag_pump(Some(Rc::new(move || {
                                if let Ok(mut guard) = rt_pump.try_borrow_mut() {
                                    let _ = guard.pump();
                                }
                            })));
                        }
                        if args.self_test_osr {
                            let recorded = observed_drag_cb.clone();
                            b.set_on_outbound_drag(Some(Box::new(move |start, _, _, _| {
                                let img = start.payload.image.as_ref().map(|i| {
                                    (i.width, i.height, i.hotspot_x, i.hotspot_y, i.bgra.len())
                                });
                                let fc = start.payload.file_contents.as_ref().map(|f| {
                                    (
                                        f.file_name.clone(),
                                        f.bytes.len(),
                                        f.bytes.starts_with(b"\x89PNG\r\n\x1A\n"),
                                    )
                                });
                                *recorded.borrow_mut() = Some(ObservedDrag {
                                    url: start.payload.url.clone(),
                                    html: start.payload.html.clone(),
                                    file_contents: fc,
                                    image: img,
                                });
                                Some(OutboundDragFinish {
                                    drag_id: start.drag_id,
                                    x: start.x,
                                    y: start.y,
                                    operation: DragOperationMask::NONE,
                                })
                            })));
                        }
                    });
                    let focus = v.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                    BrowserView::Osr(v)
                };
                root.view = Some(view.clone());
                root.handle = Some(handle.clone());
                if !args.native_hwnd {
                    let rt_dt = rt_for_devtools.clone();
                    root.open_devtools = Some(Rc::new(move || {
                        if let Ok(guard) = rt_dt.try_borrow() {
                            let _ = guard.open_devtools(browser_id);
                        }
                    }));
                }
                cx.notify();
                view
            })
            .expect("attach view");

        let shortcut_queue: Rc<RefCell<Vec<ShortcutAction>>> = Rc::new(RefCell::new(Vec::new()));
        let find_active = Rc::new(Cell::new(false));
        let mut keyboard: Option<NativeKeyboardServices> =
            if let BrowserView::Osr(entity) = &attached_view {
                win.update(cx, |_, window, cx| {
                    let mut kb = NativeKeyboardServices::new(nz(1), cx).ok()?;
                    let q = shortcut_queue.clone();
                    let find_flag = find_active.clone();
                    let wake_sc = sender.clone();
                    let policy: HostShortcutPolicy = Rc::new(move |vk, mods, up, _window, _cx| {
                        if up {
                            return false;
                        }
                        let action = match (vk, mods.control, mods.alt, mods.shift) {
                            (0x7B, false, false, false) => Some(ShortcutAction::ToggleDevTools), // F12
                            (0x49, true, false, true) => Some(ShortcutAction::ToggleDevTools), // Ctrl+Shift+I
                            (0x74, false, false, false) | (0x52, true, false, false) => {
                                Some(ShortcutAction::Reload) // F5 / Ctrl+R
                            }
                            (0x25, false, true, false) => Some(ShortcutAction::Back), // Alt+Left
                            (0x27, false, true, false) => Some(ShortcutAction::Forward), // Alt+Right
                            (0xBB | 0x6B, true, false, false) => {
                                Some(ShortcutAction::ZoomStep(1)) // Ctrl+= / Ctrl++
                            }
                            (0xBD | 0x6D, true, false, false) => {
                                Some(ShortcutAction::ZoomStep(-1)) // Ctrl+-
                            }
                            (0x30 | 0x60, true, false, false) => {
                                Some(ShortcutAction::ZoomReset) // Ctrl+0
                            }
                            (0x1B, false, false, false) if find_flag.get() => {
                                Some(ShortcutAction::CancelFind) // Escape
                            }
                            _ => None,
                        };
                        if let Some(act) = action {
                            q.borrow_mut().push(act);
                            let _ = wake_sc.wake();
                            true
                        } else {
                            false
                        }
                    });
                    kb.set_host_shortcut_policy(Some(policy));
                    kb.attach(entity, window, cx).ok()?;
                    Some(kb)
                })
                .ok()
                .flatten()
            } else {
                None
            };

        let mut touch_services: Option<NativeTouchServices> =
            if let BrowserView::Osr(entity) = &attached_view {
                win.update(cx, |_, window, cx| {
                    let mut ts = NativeTouchServices::new(nz(1)).ok()?;
                    ts.attach(entity, window, cx).ok()?;
                    Some(ts)
                })
                .ok()
                .flatten()
            } else {
                None
            };

        let mut drop_services: Option<NativeDropTargetServices> =
            if let BrowserView::Osr(entity) = &attached_view {
                win.update(cx, |_, window, cx| {
                    let mut ds = NativeDropTargetServices::new(nz(1)).ok()?;
                    ds.attach(entity, window, cx).ok()?;
                    Some(ds)
                })
                .ok()
                .flatten()
            } else {
                None
            };

        let observed_menus: Rc<RefCell<Vec<Vec<i32>>>> = Rc::new(RefCell::new(Vec::new()));
        let mut menu_pair: Option<(ContextMenuServices, Arc<ContextMenuRegistration>)> = None;
        let mut sources: Option<SourceViewServices> = None;
        let mut finds: Option<FindServices> = None;
        let presenter = if use_custom_menu && let BrowserView::Osr(entity) = &attached_view {
            let mut menus = ContextMenuServices::new(nz(1), nz(1), limits, context_limits);
            if let Ok(target) = win
                .update(cx, |_, window, cx| menus.register(entity, window, cx))
                .expect("menu register")
                && let Ok(registration) = rt.borrow().bind_context_menu(target)
                && let Ok(p) = win
                    .update(cx, |_, window, cx| {
                        DefaultMenuPresenter::new(entity, window, limits, context_limits, cx)
                            .map(|p| Rc::new(RefCell::new(p)))
                    })
                    .expect("menu presenter")
            {
                let registration = Arc::new(registration);
                if args.self_test_osr {
                    let recorded = observed_menus.clone();
                    p.borrow()
                        .set_layout_observer(Some(Box::new(move |layout, _| {
                            let ids: Vec<i32> =
                                layout.commands.iter().map(|c| c.command_id).collect();
                            recorded.borrow_mut().push(ids);
                            end_win32_menu();
                        })));
                }
                let src_ok = win
                    .update(cx, |_, window, cx| {
                        let source_opts = SourceViewOptions {
                            max_operations: nz(2),
                            max_source_bytes: nz(2 * 1024 * 1024),
                            bounds: Bounds::centered(None, size(px(900.0), px(640.0)), cx),
                            focus: true,
                        };
                        let mut src = SourceViewServices::new(nz(2), source_opts);
                        let ok = src
                            .register(entity, window, &menus, registration.clone(), cx)
                            .is_ok();
                        if ok {
                            sources = Some(src);
                        }
                        ok
                    })
                    .unwrap_or(false);
                let _ = src_ok;
                let find_ok = win
                    .update(cx, |_, window, cx| {
                        let mut fs = FindServices::new(nz(1), nz(256));
                        let ok = fs
                            .register(entity, window, &menus, registration.clone(), cx)
                            .is_ok();
                        if ok {
                            finds = Some(fs);
                        }
                        ok
                    })
                    .unwrap_or(false);
                let _ = find_ok;
                let cancel_queue = shortcut_queue.clone();
                let cancel_wake = sender.clone();
                let _ = win.update(cx, |root, _, cx| {
                    root.menu = Some(p.clone());
                    root.cancel_find = Some(Rc::new(move || {
                        cancel_queue.borrow_mut().push(ShortcutAction::CancelFind);
                        let _ = cancel_wake.wake();
                    }));
                    cx.notify();
                });
                menu_pair = Some((menus, registration));
                Some(p)
            } else {
                None
            }
        } else {
            None
        };

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum SelfTestPhase {
            WaitBingReady,
            WaitBlankMenu,
            WaitDataPage(Option<Instant>),
            WaitInputFocus {
                since: Instant,
                activated_once: bool,
            },
            WaitEnterKey(Instant),
            WaitEnterSubmit(Instant),
            WaitSelectionSettle(Instant),
            WaitCopyMenu,
            WaitXButtons(Instant),
            WaitInboundDragCursor(Instant),
            WaitInboundDragDrop(Instant),
            WaitLinkMenu,
            WaitImgMenu,
            WaitTooltipAndScreenPoint(Instant),
            WaitDragStart(Instant),
            WaitImgDragCooldown(Instant),
            WaitImgDragStart(Instant),
            WaitNav2Page(Option<Instant>),
            WaitNav2Back(Instant),
            WaitCursorChanges {
                since: Instant,
                stage: u8,
                nudged: bool,
            },
            WaitDevTools {
                since: Instant,
                stage: u8,
            },
            WaitLiveResize {
                since: Instant,
                stage: u8,
                hwnd: isize,
                base_win_size: (i32, i32),
                base_frame_width: u32,
                paints_before: u64,
                rejected_before: u64,
            },
            Done(Instant),
        }
        let mut self_test_phase = SelfTestPhase::WaitBingReady;
        let mut blank_menu_ok = false;
        let mut resize_no_flash_ok = false;
        let mut resize_live_reflow_ok = false;
        let mut enter_submit_ok = false;
        let mut drag_select_copy_ok = false;
        let mut link_img_menu_ok = false;
        let mut link_menu_ok = false;
        let mut tooltip_ok = false;
        let mut screen_point_ok = false;
        let mut inbound_drag_cursor_ok = false;
        let mut inbound_drag_drop_ok = false;
        let mut drag_image_ok = false;
        let mut drag_image_info: Option<(u32, u32, i32, i32, usize)> = None;
        let mut img_drag_file_ok = false;
        let mut img_drag_info: Option<(String, usize, bool)> = None;
        let mut xbutton_dom_ok = false;
        let mut xbutton_unhandled_nav_ok = false;
        let mut cursor_changes_ok = false;
        let mut devtools_ok = false;
        let mut devtools_inactive_hover_ok = false;
        let mut kb_mode = "native_win32";
        let mut bing_denied_resources = 0u64;
        let mut bing_denied_navs = 0u64;
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum DndSelfTestPhase {
            WaitInitialReady,
            WaitFixturePage(Option<Instant>),
            S1WaitActive(Instant),
            S1WaitMidProbe(u64, Instant),
            S1WaitEndProbe(u64, Instant),
            S2WaitSelSettle(Instant),
            S2WaitSelProbe(u64, Instant),
            S2WaitActive(Instant),
            S2WaitEndProbe(u64, Instant),
            S3TextWaitActive { since: Instant, nudged: bool },
            S3TextWaitDropSettle(Instant),
            S3TextWaitProbe(u64, Instant),
            S3LinkWaitClear(Instant),
            S3LinkWaitActive { since: Instant, nudged: bool },
            S3LinkWaitDropSettle(Instant),
            S3LinkWaitProbe(u64, Instant),
            S4EscWaitClear(Instant),
            S4EscWaitActive { since: Instant, nudged: bool },
            S4EscWaitCleared(Instant),
            S4EscWaitProbe(u64, Instant),
            S4IntWaitSelSettle(Instant),
            S4IntWaitLnk1Active(Instant),
            S4IntWaitLnk1Release(Instant),
            S4IntWaitBlankSettle(Instant),
            S4IntWaitLnk2Active(Instant),
            S4IntWaitProbe(u64, Instant),
            S5OleRun,
            S5OleWaitSettle(Instant),
            S5OleWaitProbe(u64, Instant),
            Done(Instant),
        }
        let mut dnd_phase = DndSelfTestPhase::WaitInitialReady;
        let mut dnd_probe_seq = 0u64;
        let mut dnd_s2_sel0 = String::new();
        let mut dnd_s1_link_drag_no_sel_ok = false;
        let mut dnd_s2_sel_drag_stable_ok = false;
        let mut dnd_s3_drop_text_ok = false;
        let mut dnd_s3_drop_link_ok = false;
        let mut dnd_s4_escape_cancel_ok = false;
        let mut dnd_s4_interleaved_ok = false;
        let mut dnd_s5_ole_outbound_and_drop_ok = false;

        let self_test_deadline = any_self_test.then(|| Instant::now() + Duration::from_secs(45));
        let mut self_test_passed = !any_self_test;

        let smoke_deadline = args.smoke.map(|d| Instant::now() + d);
        let loop_deadline = match (smoke_deadline, self_test_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let mut last_addr = String::new();
        let mut last_title = String::new();
        let mut cur_loading = true;
        let mut ready_printed = false;

        loop {
            if close.get() || loop_deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            if let Some(err) = obs.fault.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                eprintln!("Browser fault: {err}");
                break;
            }
            for target_url in
                std::mem::take(&mut *popups.urls.lock().unwrap_or_else(|e| e.into_inner()))
            {
                let _ = handle.submit(BrowserCommand::Navigate(target_url));
            }
            for (dl_id, dl_path) in
                std::mem::take(&mut *downloads.pending.lock().unwrap_or_else(|e| e.into_inner()))
            {
                let _ = rt.borrow().confirm_download(dl_id, dl_path);
            }
            for prog in
                std::mem::take(&mut *downloads.updates.lock().unwrap_or_else(|e| e.into_inner()))
            {
                let badge = match prog.state {
                    DownloadState::InProgress => {
                        Some(format!("DL {}%", prog.percent_complete.max(0)))
                    }
                    DownloadState::Complete => prog
                        .full_path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .map(|n| format!("Saved: {n}")),
                    DownloadState::Canceled => Some("DL canceled".into()),
                    DownloadState::Interrupted => Some("DL interrupted".into()),
                };
                if let Some(b) = badge {
                    let _ = win.update(cx, |root, _, cx| {
                        root.download_badge = Some(b);
                        cx.notify();
                    });
                }
            }
            for (req_id, decision) in std::mem::take(
                &mut *file_dialog_decisions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            ) {
                let _ = rt.borrow().resolve_file_dialog(req_id, decision);
            }
            for (req_id, decision) in std::mem::take(
                &mut *js_dialog_decisions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            ) {
                let _ = rt.borrow().resolve_js_dialog(req_id, decision);
            }
            for act in std::mem::take(&mut *shortcut_queue.borrow_mut()) {
                match act {
                    ShortcutAction::ToggleDevTools => {
                        let assoc = rt.borrow().devtools_associated(browser_id);
                        if assoc == Some(true) {
                            let _ = rt.borrow().close_dev_tools(browser_id);
                        } else {
                            let _ = rt.borrow().open_devtools(browser_id);
                        }
                    }
                    ShortcutAction::Reload => {
                        let _ = handle.submit(BrowserCommand::Reload);
                    }
                    ShortcutAction::Back => {
                        let _ = handle.submit(BrowserCommand::Back);
                    }
                    ShortcutAction::Forward => {
                        let _ = handle.submit(BrowserCommand::Forward);
                    }
                    ShortcutAction::ZoomStep(steps) => {
                        let _ = win.update(cx, |root, window, cx| {
                            if let BrowserView::Osr(v) = &attached_view {
                                v.update(cx, |b, cx| b.step_zoom(steps, window, cx));
                                if let Ok(milli) = zoom_level_to_milli(v.read(cx).zoom_level()) {
                                    root.zoom_milli = milli;
                                    cx.notify();
                                }
                            } else {
                                let cur = f64::from(root.zoom_milli) / 1000.0;
                                let next = step_canonical_zoom_level(cur, steps);
                                if handle.submit_zoom_level_fast(next).is_ok()
                                    && let Ok(milli) = zoom_level_to_milli(next)
                                {
                                    root.zoom_milli = milli;
                                    cx.notify();
                                }
                            }
                        });
                    }
                    ShortcutAction::ZoomReset => {
                        let _ = win.update(cx, |root, window, cx| {
                            if let BrowserView::Osr(v) = &attached_view {
                                v.update(cx, |b, cx| b.reset_zoom(window, cx));
                            } else {
                                let _ = handle.submit_zoom_level_fast(0.0);
                            }
                            root.zoom_milli = 0;
                            cx.notify();
                        });
                    }
                    ShortcutAction::CancelFind => {
                        if let Some(fs) = finds.as_ref() {
                            let _ = fs.cancel(browser_id);
                        }
                        find_active.set(false);
                        let _ = win.update(cx, |root, _, cx| {
                            root.find_badge = None;
                            cx.notify();
                        });
                    }
                }
            }

            let _ = win.update(cx, |root, window, cx| {
                let _ = attached_view.consume(window, cx);
                if let BrowserView::Osr(v) = &attached_view {
                    let _ = v.update(cx, |b, cx| {
                        if b.diagnostics().input.pending {
                            let _ = b.retry_input(window, cx);
                        }
                    });
                }
                let snap = handle.snapshot();
                let display_zoom_milli = if let BrowserView::Osr(v) = &attached_view {
                    zoom_level_to_milli(v.read(cx).zoom_level())
                        .unwrap_or(snap.navigation.zoom_level_milli)
                } else {
                    snap.navigation.zoom_level_milli
                };
                if display_zoom_milli != root.zoom_milli {
                    root.zoom_milli = display_zoom_milli;
                    cx.notify();
                }
                let recovered = obs.renderer_recovered.swap(false, Ordering::AcqRel);
                let next_crash_badge = if recovered {
                    None
                } else {
                    obs.renderer_crashed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                };
                if next_crash_badge != root.crash_badge {
                    root.crash_badge = next_crash_badge;
                    cx.notify();
                }
                if snap.navigation.address != last_addr
                    || snap.navigation.title != last_title
                    || snap.navigation.is_loading != root.loading
                    || snap.navigation.can_go_back != root.can_back
                    || snap.navigation.can_go_forward != root.can_forward
                {
                    last_addr = snap.navigation.address.clone();
                    last_title = snap.navigation.title.clone();
                    cur_loading = snap.navigation.is_loading;
                    root.address = snap.navigation.address;
                    root.title = snap.navigation.title;
                    root.loading = snap.navigation.is_loading;
                    root.can_back = snap.navigation.can_go_back;
                    root.can_forward = snap.navigation.can_go_forward;
                    println!(
                        "BING_NAV state={:?} loading={} url={} title={:?}",
                        snap.state, root.loading, root.address, root.title
                    );
                    cx.notify();
                }
                if !ready_printed {
                    if args.native_hwnd {
                        if let Some(hwnd) = rt.borrow().native_browser_hwnd(browser_id) {
                            ready_printed = true;
                            println!("BING_READY mode=native-hwnd child_hwnd=0x{:x}", hwnd.get());
                        }
                    } else if let BrowserView::Osr(v) = &attached_view {
                        let d = v.read(cx).diagnostics();
                        if d.paint_calls > 0 && d.frame_width > 0 {
                            ready_printed = true;
                            println!(
                                "BING_READY mode=osr width={} height={} seq={}",
                                d.frame_width, d.frame_height, d.painted_sequence
                            );
                        }
                    }
                }
            });

            if let (Some((menus, _)), Some(p)) = (menu_pair.as_mut(), presenter.as_ref()) {
                let _ = win.update(cx, |_, window, cx| {
                    let _ = menus.dispatch(browser_id, window, cx, |update, window, cx| {
                        p.borrow_mut().update(&update, window, cx)
                    });
                });
            }
            if let Some(src) = sources.as_mut() {
                let _ = cx.update(|cx| src.dispatch(cx));
            }
            if let Some(fs) = finds.as_mut() {
                let updates = cx.update(|cx| fs.dispatch(cx));
                if let Some(last_upd) = updates.last() {
                    let st = &last_upd.status;
                    let active = matches!(
                        st.phase,
                        rust_cef_gpui::find::FindPhase::Admitted
                            | rust_cef_gpui::find::FindPhase::Searching
                    );
                    find_active.set(active);
                    let badge = match (st.outcome, &st.last_result) {
                        (Some(rust_cef_gpui::find::FindOutcome::Cancelled), _) => None,
                        (Some(rust_cef_gpui::find::FindOutcome::EmptySelection), _) => None,
                        (Some(rust_cef_gpui::find::FindOutcome::Result), Some(r)) => {
                            Some(format!("Found {}/{}", r.active_ordinal, r.matches))
                        }
                        (None, Some(r)) => Some(format!("Find {}/{}", r.active_ordinal, r.matches)),
                        (Some(rust_cef_gpui::find::FindOutcome::Failed), _) => {
                            Some("Find failed".into())
                        }
                        _ if active => Some("Finding...".into()),
                        _ => None,
                    };
                    let _ = win.update(cx, |root, _, cx| {
                        root.find_badge = badge;
                        cx.notify();
                    });
                }
            }

            if args.self_test_osr && !args.native_hwnd {
                let mut exit_selftest = false;
                let mut retire_kb_for_fallback = false;
                let mut run_live_resize_hwnd: Option<(isize, u32, u64, u64)> = None;
                let mut finish_live_resize_hwnd: Option<(isize, i32, i32)> = None;
                let any_win: gpui::AnyWindowHandle = win.into();
                let _ = cx.update_window(any_win, |_, window, cx| {
                    let point = |x: f32, y: f32| gpui::point(px(x), px(y));
                    let dispatch_click = |btn: MouseButton, x: f32, y: f32, window: &mut Window, cx: &mut App| {
                        window.dispatch_event(
                            PlatformInput::MouseDown(MouseDownEvent {
                                button: btn,
                                position: point(x, y),
                                modifiers: Modifiers::default(),
                                click_count: 1,
                                first_mouse: false,
                            }),
                            cx,
                        );
                        window.dispatch_event(
                            PlatformInput::MouseUp(MouseUpEvent {
                                button: btn,
                                position: point(x, y),
                                modifiers: Modifiers::default(),
                                click_count: 1,
                            }),
                            cx,
                        );
                    };

                    match self_test_phase {
                        SelfTestPhase::WaitBingReady => {
                            if ready_printed && !cur_loading {
                                bing_denied_resources = obs.denied_resources.load(Ordering::Acquire);
                                bing_denied_navs = obs.denied_navs.load(Ordering::Acquire);
                                if !window.is_window_active() {
                                    window.activate_window();
                                }
                                if let BrowserView::Osr(v) = &attached_view {
                                    let focus = v.read(cx).focus_handle(cx);
                                    window.focus(&focus, cx);
                                }
                                if window.is_window_active() {
                                    dispatch_click(
                                        MouseButton::Right,
                                        24.0,
                                        TOP_BAR_HEIGHT_DIP + 120.0,
                                        window,
                                        cx,
                                    );
                                    self_test_phase = SelfTestPhase::WaitBlankMenu;
                                }
                            }
                        }
                        SelfTestPhase::WaitBlankMenu => {
                            if let Some(ids) = observed_menus.borrow().first() {
                                blank_menu_ok = [100, 101, 102, 117, 132, 28440]
                                    .iter()
                                    .all(|req| ids.contains(req));
                                let probe_html = "data:text/html;charset=utf-8,%3C!doctype%20html%3E%3Cbody%20style%3D'margin%3A0%3Bpadding%3A16px%3Bfont-family%3Asans-serif'%3E%3Cdiv%20id%3D'sel'%20style%3D'font-size%3A28px%3Bline-height%3A40px%3Buser-select%3Atext'%3ESelectableOsrText1234567890%3C%2Fdiv%3E%3Cform%20onsubmit%3D%22document.title%3D'SUBMIT%3A'%2Bdocument.getElementById('q').value%3Breturn%20false%3B%22%3E%3Cinput%20id%3D'q'%20autofocus%20oninput%3D%22if(this.value.includes('DroppedOsrText'))document.title%3D'DROP%3AOK'%22%20style%3D'width%3A240px%3Bheight%3A32px%3Bmargin-top%3A12px'%3E%3C%2Fform%3E%3Ca%20id%3D'lnk'%20title%3D'OsrLinkTip'%20href%3D'https%3A%2F%2Fcn.bing.com%2F'%20onmousemove%3D%22if(window._watch_dt_hov)%7Bwindow._dt_hov%3D1%3Bdocument.title%3D'DTHOV%3AOK'%3B%7D%22%20style%3D'position%3Afixed%3Bleft%3A16px%3Btop%3A124px%3Bdisplay%3Ainline-block%3Bfont-size%3A24px%3Bline-height%3A36px%3Bpadding%3A4px%2012px'%3EDragOsrLink%3C%2Fa%3E%3Cimg%20id%3D'pic'%20src%3D'data%3Aimage%2Fpng%3Bbase64%2CiVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg%3D%3D'%20style%3D'position%3Afixed%3Bleft%3A240px%3Btop%3A124px%3Bwidth%3A64px%3Bheight%3A64px'%3E%3Cscript%3Ewindow._xb%3D''%3Bwindow.addEventListener('mousedown'%2Ce%3D%3E%7Bif(e.button%3D%3D%3D3%7C%7Ce.button%3D%3D%3D4)%7Be.preventDefault()%3Bwindow._xb%2B%3D'D'%2Be.button%3Bdocument.title%3D'XB%3A'%2Bwindow._xb%3B%7D%7D)%3Bwindow.addEventListener('mouseup'%2Ce%3D%3E%7Bif(e.button%3D%3D%3D3%7C%7Ce.button%3D%3D%3D4)%7Be.preventDefault()%3Bwindow._xb%2B%3D'U'%2Be.button%3Bdocument.title%3D'XB%3A'%2Bwindow._xb%3B%7D%7D)%3B%3C%2Fscript%3E%3C%2Fbody%3E";
                                let _ = handle.submit(BrowserCommand::Navigate(probe_html.into()));
                                self_test_phase = SelfTestPhase::WaitDataPage(None);
                            }
                        }
                        SelfTestPhase::WaitDataPage(settled) => {
                            if last_addr.starts_with("data:text/html") && !cur_loading {
                                match settled {
                                    None => {
                                        self_test_phase = SelfTestPhase::WaitDataPage(Some(Instant::now()));
                                    }
                                    Some(since) if since.elapsed() >= Duration::from_millis(200) => {
                                        if !window.is_window_active() {
                                            window.activate_window();
                                        }
                                        if window.is_window_active() {
                                            dispatch_click(
                                                MouseButton::Left,
                                                60.0,
                                                TOP_BAR_HEIGHT_DIP + 78.0,
                                                window,
                                                cx,
                                            );
                                            self_test_phase = SelfTestPhase::WaitInputFocus {
                                                since: Instant::now(),
                                                activated_once: false,
                                            };
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        SelfTestPhase::WaitInputFocus {
                            since,
                            activated_once,
                        } => {
                            let wait = if activated_once {
                                Duration::from_millis(250)
                            } else {
                                Duration::from_millis(150)
                            };
                            if since.elapsed() >= wait {
                                if let BrowserView::Osr(v) = &attached_view {
                                    let focus = v.read(cx).focus_handle(cx);
                                    window.focus(&focus, cx);
                                }
                                if let Some(hwnd) = window_hwnd(window) {
                                    let hwnd = hwnd.get() as isize;
                                    if ensure_win32_focus(hwnd) && window.is_window_active() {
                                        kb_mode = "native_win32";
                                        post_win32_key(hwnd, 0x41, false);
                                        post_win32_key(hwnd, 0x41, true);
                                        post_win32_key(hwnd, 0x42, false);
                                        post_win32_key(hwnd, 0x42, true);
                                        post_win32_key(hwnd, 0x0D, false);
                                        post_win32_key(hwnd, 0x0D, true);
                                        self_test_phase =
                                            SelfTestPhase::WaitEnterSubmit(Instant::now());
                                    } else if !activated_once {
                                        window.activate_window();
                                        self_test_phase = SelfTestPhase::WaitInputFocus {
                                            since: Instant::now(),
                                            activated_once: true,
                                        };
                                    } else {
                                        // Unattended background run cannot steal OS GetForegroundWindow;
                                        // retire the WH_GETMESSAGE hook outside this borrow and test
                                        // GPUI WM_CHAR + KeyDown(enter) OSR routing directly.
                                        retire_kb_for_fallback = true;
                                    }
                                }
                            }
                        }
                        SelfTestPhase::WaitEnterKey(since) => {
                            if since.elapsed() >= Duration::from_millis(80) {
                                if let BrowserView::Osr(v) = &attached_view {
                                    let focus = v.read(cx).focus_handle(cx);
                                    window.focus(&focus, cx);
                                }
                                let enter = Keystroke {
                                    key: "enter".into(),
                                    key_char: None,
                                    modifiers: Modifiers::default(),
                                };
                                window.dispatch_event(
                                    PlatformInput::KeyDown(KeyDownEvent {
                                        keystroke: enter.clone(),
                                        is_held: false,
                                        prefer_character_input: false,
                                    }),
                                    cx,
                                );
                                window.dispatch_event(
                                    PlatformInput::KeyUp(KeyUpEvent { keystroke: enter }),
                                    cx,
                                );
                                self_test_phase = SelfTestPhase::WaitEnterSubmit(Instant::now());
                            }
                        }
                        SelfTestPhase::WaitEnterSubmit(since) => {
                            if last_title.eq_ignore_ascii_case("SUBMIT:ab") || since.elapsed() >= Duration::from_secs(3) {
                                enter_submit_ok = last_title.eq_ignore_ascii_case("SUBMIT:ab");
                                if !window.is_window_active() {
                                    window.activate_window();
                                }
                                let y = TOP_BAR_HEIGHT_DIP + 36.0;
                                window.dispatch_event(
                                    PlatformInput::MouseDown(MouseDownEvent {
                                        button: MouseButton::Left,
                                        position: point(12.0, y),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                        first_mouse: false,
                                    }),
                                    cx,
                                );
                                for x in [80.0_f32, 160.0, 260.0, 340.0] {
                                    window.dispatch_event(
                                        PlatformInput::MouseMove(MouseMoveEvent {
                                            position: point(x, y),
                                            pressed_button: Some(MouseButton::Left),
                                            modifiers: Modifiers::default(),
                                        }),
                                        cx,
                                    );
                                }
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(340.0, y),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                self_test_phase = SelfTestPhase::WaitSelectionSettle(Instant::now());
                            }
                        }
                        SelfTestPhase::WaitSelectionSettle(since) => {
                            if since.elapsed() >= Duration::from_millis(180) {
                                if !window.is_window_active() {
                                    window.activate_window();
                                }
                                dispatch_click(
                                    MouseButton::Right,
                                    120.0,
                                    TOP_BAR_HEIGHT_DIP + 36.0,
                                    window,
                                    cx,
                                );
                                self_test_phase = SelfTestPhase::WaitCopyMenu;
                            }
                        }
                        SelfTestPhase::WaitCopyMenu => {
                            if observed_menus.borrow().len() >= 2 {
                                let ids = &observed_menus.borrow()[1];
                                drag_select_copy_ok = ids.contains(&113)
                                    && [100, 101, 102, 117].iter().all(|req| ids.contains(req));
                                if !window.is_window_active() {
                                    window.activate_window();
                                }
                                dispatch_click(
                                    MouseButton::Navigate(gpui::NavigationDirection::Back),
                                    360.0,
                                    TOP_BAR_HEIGHT_DIP + 180.0,
                                    window,
                                    cx,
                                );
                                dispatch_click(
                                    MouseButton::Navigate(gpui::NavigationDirection::Forward),
                                    360.0,
                                    TOP_BAR_HEIGHT_DIP + 180.0,
                                    window,
                                    cx,
                                );
                                self_test_phase = SelfTestPhase::WaitXButtons(Instant::now());
                            }
                        }
                        SelfTestPhase::WaitXButtons(since) => {
                            if last_title == "XB:D3U3D4U4" || since.elapsed() >= Duration::from_secs(2) {
                                xbutton_dom_ok =
                                    last_title == "XB:D3U3D4U4" && last_addr.starts_with("data:text/html");
                                let _ = handle.submit(BrowserCommand::DragTarget(
                                    InboundDragCommand::Enter {
                                        payload: InboundDragPayload {
                                            text: Some("DroppedOsrText".into()),
                                            ..Default::default()
                                        },
                                        x: 60,
                                        y: 78,
                                        modifiers: MouseModifiers::default(),
                                        allowed_ops: DragOperationMask::EVERY,
                                    },
                                ));
                                let _ = handle.submit(BrowserCommand::DragTarget(
                                    InboundDragCommand::Over {
                                        x: 60,
                                        y: 78,
                                        modifiers: MouseModifiers::default(),
                                        allowed_ops: DragOperationMask::EVERY,
                                    },
                                ));
                                self_test_phase =
                                    SelfTestPhase::WaitInboundDragCursor(Instant::now());
                            }
                        }
                        SelfTestPhase::WaitInboundDragCursor(since) => {
                            if handle.drag_target_operation() != DragOperationMask::NONE
                                || since.elapsed() >= Duration::from_millis(600)
                            {
                                inbound_drag_cursor_ok =
                                    handle.drag_target_operation() != DragOperationMask::NONE;
                                let _ = handle.submit(BrowserCommand::DragTarget(
                                    InboundDragCommand::Drop {
                                        x: 60,
                                        y: 78,
                                        modifiers: MouseModifiers::default(),
                                    },
                                ));
                                self_test_phase =
                                    SelfTestPhase::WaitInboundDragDrop(Instant::now());
                            }
                        }
                        SelfTestPhase::WaitInboundDragDrop(since) => {
                            if last_title == "DROP:OK"
                                || since.elapsed() >= Duration::from_secs(2)
                            {
                                inbound_drag_drop_ok =
                                    inbound_drag_cursor_ok && last_title == "DROP:OK";
                                if !window.is_window_active() {
                                    window.activate_window();
                                }
                                dispatch_click(
                                    MouseButton::Right,
                                    64.0,
                                    TOP_BAR_HEIGHT_DIP + 144.0,
                                    window,
                                    cx,
                                );
                                self_test_phase = SelfTestPhase::WaitLinkMenu;
                            }
                        }
                        SelfTestPhase::WaitLinkMenu => {
                            if observed_menus.borrow().len() >= 3 {
                                let ids = &observed_menus.borrow()[2];
                                link_menu_ok = ids.contains(&50100) && ids.contains(&50104);
                                if !window.is_window_active() {
                                    window.activate_window();
                                }
                                dispatch_click(
                                    MouseButton::Right,
                                    272.0,
                                    TOP_BAR_HEIGHT_DIP + 156.0,
                                    window,
                                    cx,
                                );
                                self_test_phase = SelfTestPhase::WaitImgMenu;
                            }
                        }
                        SelfTestPhase::WaitImgMenu => {
                            if observed_menus.borrow().len() >= 4 {
                                let ids = &observed_menus.borrow()[3];
                                let img_menu_ok = ids.contains(&50120)
                                    && ids.contains(&50121)
                                    && ids.contains(&50122);
                                link_img_menu_ok = link_menu_ok && img_menu_ok;
                                screen_point_ok = rt
                                    .borrow()
                                    .browser_screen_point(browser_id, 40, 50)
                                    .is_some();
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(64.0, TOP_BAR_HEIGHT_DIP + 144.0),
                                        pressed_button: None,
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(68.0, TOP_BAR_HEIGHT_DIP + 144.0),
                                        pressed_button: None,
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                self_test_phase =
                                    SelfTestPhase::WaitTooltipAndScreenPoint(Instant::now());
                            }
                        }
                        SelfTestPhase::WaitTooltipAndScreenPoint(since) => {
                            let tip = if let BrowserView::Osr(v) = &attached_view {
                                v.read(cx).tooltip().to_owned()
                            } else {
                                String::new()
                            };
                            if tip == "OsrLinkTip"
                                || since.elapsed() >= Duration::from_millis(1200)
                            {
                                tooltip_ok = tip == "OsrLinkTip";
                                if !window.is_window_active() {
                                    window.activate_window();
                                }
                                let start_x = 64.0_f32;
                                let start_y = TOP_BAR_HEIGHT_DIP + 144.0;
                                window.dispatch_event(
                                    PlatformInput::MouseDown(MouseDownEvent {
                                        button: MouseButton::Left,
                                        position: point(start_x, start_y),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                        first_mouse: false,
                                    }),
                                    cx,
                                );
                                for step in 1..=6 {
                                    let d = step as f32 * 18.0;
                                    window.dispatch_event(
                                        PlatformInput::MouseMove(MouseMoveEvent {
                                            position: point(start_x + d, start_y + d),
                                            pressed_button: Some(MouseButton::Left),
                                            modifiers: Modifiers::default(),
                                        }),
                                        cx,
                                    );
                                }
                                self_test_phase = SelfTestPhase::WaitDragStart(Instant::now());
                            }
                        }
                        SelfTestPhase::WaitDragStart(since) => {
                            let drag = observed_drag.borrow_mut().take();
                            if drag.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(172.0, TOP_BAR_HEIGHT_DIP + 252.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                if let Some(d) = drag {
                                    drag_image_info = d.image;
                                    drag_image_ok = d.url.as_deref() == Some("https://cn.bing.com/")
                                        && d.image.is_some_and(|(w, h, _, _, len)| {
                                            w > 0 && h > 0 && len == (w as usize) * (h as usize) * 4
                                        });
                                }
                                self_test_phase = SelfTestPhase::WaitImgDragCooldown(Instant::now());
                            }
                        }
                        SelfTestPhase::WaitImgDragCooldown(since) => {
                            if since.elapsed() >= Duration::from_millis(120) {
                                if !window.is_window_active() {
                                    window.activate_window();
                                }
                                let start_x = 272.0_f32;
                                let start_y = TOP_BAR_HEIGHT_DIP + 156.0;
                                window.dispatch_event(
                                    PlatformInput::MouseDown(MouseDownEvent {
                                        button: MouseButton::Left,
                                        position: point(start_x, start_y),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                        first_mouse: false,
                                    }),
                                    cx,
                                );
                                for step in 1..=6 {
                                    let d = step as f32 * 18.0;
                                    window.dispatch_event(
                                        PlatformInput::MouseMove(MouseMoveEvent {
                                            position: point(start_x + d, start_y + d),
                                            pressed_button: Some(MouseButton::Left),
                                            modifiers: Modifiers::default(),
                                        }),
                                        cx,
                                    );
                                }
                                self_test_phase = SelfTestPhase::WaitImgDragStart(Instant::now());
                            }
                        }
                        SelfTestPhase::WaitImgDragStart(since) => {
                            let drag = observed_drag.borrow_mut().take();
                            if drag.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(380.0, TOP_BAR_HEIGHT_DIP + 264.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                if let Some(d) = drag {
                                    let html_has_img =
                                        d.html.as_deref().is_some_and(|h| h.contains("<img"));
                                    img_drag_info = d.file_contents.clone();
                                    img_drag_file_ok = html_has_img
                                        && d.file_contents.as_ref().is_some_and(
                                            |(name, len, is_png)| {
                                                name.ends_with(".png") && *len > 8 && *is_png
                                            },
                                        );
                                }
                                let nav2 = "data:text/html;charset=utf-8,%3C!doctype%20html%3E%3Ctitle%3ENAV2%3C%2Ftitle%3E%3Cbody%3EPage2%3C%2Fbody%3E";
                                let _ = handle.submit(BrowserCommand::Navigate(nav2.into()));
                                self_test_phase = SelfTestPhase::WaitNav2Page(None);
                            }
                        }
                        SelfTestPhase::WaitNav2Page(settled) => {
                            if last_title == "NAV2" && !cur_loading {
                                match settled {
                                    None => {
                                        self_test_phase = SelfTestPhase::WaitNav2Page(Some(Instant::now()));
                                    }
                                    Some(since) if since.elapsed() >= Duration::from_millis(150) => {
                                        if !window.is_window_active() {
                                            window.activate_window();
                                        }
                                        dispatch_click(
                                            MouseButton::Navigate(gpui::NavigationDirection::Back),
                                            200.0,
                                            TOP_BAR_HEIGHT_DIP + 120.0,
                                            window,
                                            cx,
                                        );
                                        self_test_phase = SelfTestPhase::WaitNav2Back(Instant::now());
                                    }
                                    _ => {}
                                }
                            }
                        }
                        SelfTestPhase::WaitNav2Back(since) => {
                            if (last_title != "NAV2" && last_addr.starts_with("data:text/html") && !cur_loading)
                                || since.elapsed() >= Duration::from_millis(1200)
                            {
                                xbutton_unhandled_nav_ok = last_title != "NAV2";
                                let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                    script: "document.documentElement.style.minHeight='100vh';document.body.style.minHeight='100vh';document.title='CUR_READY';"
                                        .into(),
                                    frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                });
                                self_test_phase = SelfTestPhase::WaitCursorChanges {
                                    since: Instant::now(),
                                    stage: 0,
                                    nudged: false,
                                };
                            }
                        }
                        SelfTestPhase::WaitCursorChanges {
                            since,
                            stage,
                            nudged,
                        } => {
                            use rust_cef_gpui::cursor::{NativeCursorData, StandardCursor};
                            let (cur, ux) = if let BrowserView::Osr(v) = &attached_view {
                                let b = v.read(cx);
                                (b.native_cursor().clone(), b.cursor())
                            } else {
                                (
                                    NativeCursorData::Standard(StandardCursor::Arrow),
                                    rust_cef_gpui::contracts::BrowserCursor::Arrow,
                                )
                            };
                            match stage {
                                0 if last_title == "CUR_READY" => {
                                    if !window.is_window_active() {
                                        window.activate_window();
                                    }
                                    window.dispatch_event(
                                        PlatformInput::MouseMove(MouseMoveEvent {
                                            position: point(64.0, TOP_BAR_HEIGHT_DIP + 144.0),
                                            pressed_button: None,
                                            modifiers: Modifiers::default(),
                                        }),
                                        cx,
                                    );
                                    window.dispatch_event(
                                        PlatformInput::MouseMove(MouseMoveEvent {
                                            position: point(74.0, TOP_BAR_HEIGHT_DIP + 144.0),
                                            pressed_button: None,
                                            modifiers: Modifiers::default(),
                                        }),
                                        cx,
                                    );
                                    self_test_phase = SelfTestPhase::WaitCursorChanges {
                                        since: Instant::now(),
                                        stage: 1,
                                        nudged: false,
                                    };
                                }
                                1 if matches!(cur, NativeCursorData::Standard(StandardCursor::Hand)) => {
                                    window.dispatch_event(
                                        PlatformInput::MouseMove(MouseMoveEvent {
                                            position: point(500.0, TOP_BAR_HEIGHT_DIP + 220.0),
                                            pressed_button: None,
                                            modifiers: Modifiers::default(),
                                        }),
                                        cx,
                                    );
                                    let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                        script: "document.documentElement.style.cursor='wait';document.body.style.cursor='wait';"
                                            .into(),
                                        frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                    });
                                    self_test_phase = SelfTestPhase::WaitCursorChanges {
                                        since: Instant::now(),
                                        stage: 2,
                                        nudged: false,
                                    };
                                }
                                1 if !nudged && since.elapsed() >= Duration::from_millis(250) => {
                                    window.dispatch_event(
                                        PlatformInput::MouseMove(MouseMoveEvent {
                                            position: point(80.0, TOP_BAR_HEIGHT_DIP + 144.0),
                                            pressed_button: None,
                                            modifiers: Modifiers::default(),
                                        }),
                                        cx,
                                    );
                                    self_test_phase = SelfTestPhase::WaitCursorChanges {
                                        since,
                                        stage: 1,
                                        nudged: true,
                                    };
                                }
                                2 if matches!(cur, NativeCursorData::Standard(StandardCursor::Wait)) => {
                                    let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                        script: "document.documentElement.style.cursor='none';document.body.style.cursor='none';"
                                            .into(),
                                        frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                    });
                                    self_test_phase = SelfTestPhase::WaitCursorChanges {
                                        since: Instant::now(),
                                        stage: 3,
                                        nudged: false,
                                    };
                                }
                                3 if matches!(cur, NativeCursorData::Hidden) => {
                                    let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                        script: "document.documentElement.style.cursor='grab';document.body.style.cursor='grab';"
                                            .into(),
                                        frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                    });
                                    self_test_phase = SelfTestPhase::WaitCursorChanges {
                                        since: Instant::now(),
                                        stage: 4,
                                        nudged: false,
                                    };
                                }
                                4 if matches!(
                                    cur,
                                    NativeCursorData::BgraBitmap { .. }
                                        | NativeCursorData::MonochromeMask { .. }
                                        | NativeCursorData::Standard(StandardCursor::Hand)
                                ) =>
                                {
                                    let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                        script: "document.documentElement.style.cursor=\"url('data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAYAAACp8Z5+AAAAEklEQVR4nGP4z8DwHxkzkC4AADxAH+HggXe0AAAAAElFTkSuQmCC') 2 2, auto\";document.body.style.cursor=document.documentElement.style.cursor;"
                                            .into(),
                                        frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                    });
                                    self_test_phase = SelfTestPhase::WaitCursorChanges {
                                        since: Instant::now(),
                                        stage: 5,
                                        nudged: false,
                                    };
                                }
                                5 if matches!(
                                    &cur,
                                    NativeCursorData::BgraBitmap { width, height, pixels, .. }
                                        if *width > 0 && *height > 0 && !pixels.is_empty()
                                ) =>
                                {
                                    let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                        script: "document.documentElement.style.cursor='';document.body.style.cursor='';"
                                            .into(),
                                        frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                    });
                                    self_test_phase = SelfTestPhase::WaitCursorChanges {
                                        since: Instant::now(),
                                        stage: 6,
                                        nudged: false,
                                    };
                                }
                                6 if matches!(cur, NativeCursorData::Standard(StandardCursor::Arrow)) => {
                                    cursor_changes_ok = true;
                                    let _ = rt.borrow().open_devtools_at(browser_id, 40, 40);
                                    self_test_phase = SelfTestPhase::WaitDevTools {
                                        since: Instant::now(),
                                        stage: 0,
                                    };
                                }
                                _ if since.elapsed() >= Duration::from_secs(2) => {
                                    eprintln!("BING_OSR_CURSOR_FAIL stage={stage} nudged={nudged} cur={cur:?} ux={ux:?} title={last_title:?}");
                                    let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                        script: "document.documentElement.style.cursor='';document.body.style.cursor='';"
                                            .into(),
                                        frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                    });
                                    let _ = rt.borrow().open_devtools_at(browser_id, 40, 40);
                                    self_test_phase = SelfTestPhase::WaitDevTools {
                                        since: Instant::now(),
                                        stage: 0,
                                    };
                                }
                                _ => {}
                            }
                        }
                        SelfTestPhase::WaitDevTools { since, stage } => {
                            let assoc = rt.borrow().devtools_associated(browser_id);
                            if assoc == Some(true)
                                && stage == 0
                                && since.elapsed() >= Duration::from_millis(600)
                            {
                                let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                    script: "window._watch_dt_hov=1;document.title='WATCH_HOV';"
                                        .into(),
                                    frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                });
                                self_test_phase = SelfTestPhase::WaitDevTools {
                                    since: Instant::now(),
                                    stage: 1,
                                };
                            } else if assoc == Some(true) && stage == 1 && last_title == "WATCH_HOV"
                            {
                                // Intentionally dispatch MouseMove regardless of window.is_window_active()
                                // (DevTools floating window may hold OS focus right now).
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(64.0, TOP_BAR_HEIGHT_DIP + 144.0),
                                        pressed_button: None,
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(74.0, TOP_BAR_HEIGHT_DIP + 144.0),
                                        pressed_button: None,
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                self_test_phase = SelfTestPhase::WaitDevTools {
                                    since: Instant::now(),
                                    stage: 2,
                                };
                            } else if assoc == Some(true)
                                && stage == 2
                                && (last_title == "DTHOV:OK"
                                    || since.elapsed() >= Duration::from_millis(1500))
                            {
                                devtools_ok = true;
                                devtools_inactive_hover_ok = last_title == "DTHOV:OK";
                                let (paints_before, rejected_before, base_frame_width) =
                                    if let BrowserView::Osr(v) = &attached_view {
                                        let d = v.read(cx).diagnostics();
                                        (d.paint_calls, d.rejected_paints, d.frame_width)
                                    } else {
                                        (0, 0, 0)
                                    };
                                if let Some(hwnd) = window_hwnd(window) {
                                    run_live_resize_hwnd = Some((
                                        hwnd.get() as isize,
                                        base_frame_width,
                                        paints_before,
                                        rejected_before,
                                    ));
                                } else {
                                    self_test_phase = SelfTestPhase::Done(Instant::now());
                                }
                            } else if since.elapsed() >= Duration::from_secs(4) {
                                println!(
                                    "BING_OSR_SELFTEST blank_menu_ok={blank_menu_ok} link_img_menu_ok={link_img_menu_ok} tooltip_ok={tooltip_ok} screen_point_ok={screen_point_ok} inbound_drag_drop_ok={inbound_drag_drop_ok} enter_submit_ok={enter_submit_ok} drag_select_copy_ok={drag_select_copy_ok} drag_image_ok={drag_image_ok} drag_img={drag_image_info:?} img_drag_file_ok={img_drag_file_ok} img_file={img_drag_info:?} xbutton_dom_ok={xbutton_dom_ok} xbutton_unhandled_nav_ok={xbutton_unhandled_nav_ok} cursor_changes_ok={cursor_changes_ok} devtools_ok=false devtools_inactive_hover_ok={devtools_inactive_hover_ok} resize_live_reflow_ok={resize_live_reflow_ok} resize_no_flash_ok={resize_no_flash_ok} assoc={assoc:?} stage={stage} title={last_title:?} kb_mode={kb_mode} bing_denied_resources={bing_denied_resources} bing_denied_navs={bing_denied_navs}"
                                );
                                self_test_phase = SelfTestPhase::Done(Instant::now());
                            }
                        }
                        SelfTestPhase::WaitLiveResize {
                            since,
                            stage,
                            hwnd,
                            base_win_size,
                            base_frame_width,
                            paints_before,
                            rejected_before,
                        } => {
                            let (paints_after, rejected_after, frame_width) =
                                if let BrowserView::Osr(v) = &attached_view {
                                    let d = v.read(cx).diagnostics();
                                    (d.paint_calls, d.rejected_paints, d.frame_width)
                                } else {
                                    (0, 0, 0)
                                };
                            if stage == 0 {
                                if frame_width >= base_frame_width.saturating_add(80) {
                                    resize_live_reflow_ok = true;
                                    finish_live_resize_hwnd =
                                        Some((hwnd, base_win_size.0, base_win_size.1));
                                    self_test_phase = SelfTestPhase::WaitLiveResize {
                                        since: Instant::now(),
                                        stage: 1,
                                        hwnd,
                                        base_win_size,
                                        base_frame_width,
                                        paints_before,
                                        rejected_before,
                                    };
                                } else if since.elapsed() >= Duration::from_secs(2) {
                                    finish_live_resize_hwnd =
                                        Some((hwnd, base_win_size.0, base_win_size.1));
                                    self_test_phase = SelfTestPhase::WaitLiveResize {
                                        since: Instant::now(),
                                        stage: 1,
                                        hwnd,
                                        base_win_size,
                                        base_frame_width,
                                        paints_before,
                                        rejected_before,
                                    };
                                }
                            } else if frame_width == base_frame_width
                                || since.elapsed() >= Duration::from_secs(2)
                            {
                                resize_no_flash_ok = paints_after >= paints_before.saturating_add(4)
                                    && rejected_after == rejected_before
                                    && frame_width == base_frame_width;
                                self_test_passed = blank_menu_ok
                                    && link_img_menu_ok
                                    && tooltip_ok
                                    && screen_point_ok
                                    && inbound_drag_drop_ok
                                    && enter_submit_ok
                                    && drag_select_copy_ok
                                    && drag_image_ok
                                    && img_drag_file_ok
                                    && xbutton_dom_ok
                                    && xbutton_unhandled_nav_ok
                                    && cursor_changes_ok
                                    && devtools_ok
                                    && devtools_inactive_hover_ok
                                    && resize_live_reflow_ok
                                    && resize_no_flash_ok
                                    && bing_denied_resources == 0
                                    && bing_denied_navs == 0;
                                println!(
                                    "BING_OSR_SELFTEST blank_menu_ok={blank_menu_ok} link_img_menu_ok={link_img_menu_ok} tooltip_ok={tooltip_ok} screen_point_ok={screen_point_ok} inbound_drag_drop_ok={inbound_drag_drop_ok} enter_submit_ok={enter_submit_ok} drag_select_copy_ok={drag_select_copy_ok} drag_image_ok={drag_image_ok} drag_img={drag_image_info:?} img_drag_file_ok={img_drag_file_ok} img_file={img_drag_info:?} xbutton_dom_ok={xbutton_dom_ok} xbutton_unhandled_nav_ok={xbutton_unhandled_nav_ok} cursor_changes_ok={cursor_changes_ok} devtools_ok={devtools_ok} devtools_inactive_hover_ok={devtools_inactive_hover_ok} resize_live_reflow_ok={resize_live_reflow_ok} resize_no_flash_ok={resize_no_flash_ok} resize_paints={}->{paints_after} rejected_paints={}->{rejected_after} kb_mode={kb_mode} bing_denied_resources={bing_denied_resources} bing_denied_navs={bing_denied_navs}",
                                    paints_before,
                                    rejected_before
                                );
                                self_test_phase = SelfTestPhase::Done(Instant::now());
                            }
                        }
                        SelfTestPhase::Done(since) => {
                            if since.elapsed() >= Duration::from_millis(100) {
                                exit_selftest = true;
                            }
                        }
                    }
                });
                if let Some((hwnd, base_frame_width, paints_before, rejected_before)) =
                    run_live_resize_hwnd
                {
                    if let Some(base_win_size) = begin_simulated_win32_drag_resize(hwnd) {
                        self_test_phase = SelfTestPhase::WaitLiveResize {
                            since: Instant::now(),
                            stage: 0,
                            hwnd,
                            base_win_size,
                            base_frame_width,
                            paints_before,
                            rejected_before,
                        };
                    } else {
                        self_test_phase = SelfTestPhase::Done(Instant::now());
                    }
                }
                if let Some((hwnd, base_w, base_h)) = finish_live_resize_hwnd {
                    finish_simulated_win32_drag_resize(hwnd, base_w, base_h);
                }
                if retire_kb_for_fallback {
                    if let Some(mut kb) = keyboard.take() {
                        let _ = cx.update(|_| kb.retire());
                    }
                    let _ = cx.update_window(any_win, |_, window, cx| {
                        if let BrowserView::Osr(v) = &attached_view {
                            let focus = v.read(cx).focus_handle(cx);
                            window.focus(&focus, cx);
                        }
                        if let Some(hwnd) = window_hwnd(window) {
                            let hwnd = hwnd.get() as isize;
                            post_win32_char(hwnd, b'a' as usize);
                            post_win32_char(hwnd, b'b' as usize);
                        }
                    });
                    kb_mode = "gpui_osr";
                    self_test_phase = SelfTestPhase::WaitEnterKey(Instant::now());
                }
                if exit_selftest {
                    break;
                }
            }

            if args.self_test_osr_dnd && !args.native_hwnd {
                let mut exit_dnd_selftest = false;
                let mut esc_hwnd: Option<isize> = None;
                let mut ole_test_params: Option<(isize, f32)> = None;
                let any_win: gpui::AnyWindowHandle = win.into();
                let kb_active = keyboard.as_ref().is_some_and(|k| !k.diagnostics().retired);
                let parse_dnd_probe =
                    |title: &str, expected_seq: u64| -> Option<(String, String, u64)> {
                        let prefix = format!("DND:{expected_seq}|");
                        let rest = title.strip_prefix(&prefix)?;
                        let mut sel = String::new();
                        let mut q = String::new();
                        let mut nav = 0u64;
                        for part in rest.split('|') {
                            if let Some(v) = part.strip_prefix("sel=") {
                                sel = v.to_owned();
                            } else if let Some(v) = part.strip_prefix("q=") {
                                q = v.to_owned();
                            } else if let Some(v) = part.strip_prefix("nav=") {
                                nav = v.parse::<u64>().unwrap_or(99);
                            }
                        }
                        Some((sel, q, nav))
                    };
                let issue_dnd_probe = |seq: &mut u64| -> u64 {
                    *seq += 1;
                    let id = *seq;
                    let script = format!(
                        "document.title='DND:{id}|sel='+window.getSelection().toString()+'|q='+document.getElementById('sb_form_q').value+'|nav='+(window._nav||0);"
                    );
                    let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                        script,
                        frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                    });
                    id
                };
                let _ = cx.update_window(any_win, |_, window, cx| {
                    let point = |x: f32, y: f32| gpui::point(px(x), px(y));
                    let dispatch_click = |btn: MouseButton, x: f32, y: f32, window: &mut Window, cx: &mut App| {
                        window.dispatch_event(
                            PlatformInput::MouseDown(MouseDownEvent {
                                button: btn,
                                position: point(x, y),
                                modifiers: Modifiers::default(),
                                click_count: 1,
                                first_mouse: false,
                            }),
                            cx,
                        );
                        window.dispatch_event(
                            PlatformInput::MouseUp(MouseUpEvent {
                                button: btn,
                                position: point(x, y),
                                modifiers: Modifiers::default(),
                                click_count: 1,
                            }),
                            cx,
                        );
                    };
                    let dispatch_drag_path = |x0: f32, y0: f32, x1: f32, y1: f32, window: &mut Window, cx: &mut App| {
                        if !window.is_window_active() {
                            window.activate_window();
                        }
                        if let Some(hwnd) = window_hwnd(window) {
                            let _ = ensure_win32_focus(hwnd.get() as isize);
                        }
                        window.dispatch_event(
                            PlatformInput::MouseDown(MouseDownEvent {
                                button: MouseButton::Left,
                                position: point(x0, y0),
                                modifiers: Modifiers::default(),
                                click_count: 1,
                                first_mouse: false,
                            }),
                            cx,
                        );
                        let steps = 8u8;
                        for step in 1..=steps {
                            let t = f32::from(step) / f32::from(steps);
                            window.dispatch_event(
                                PlatformInput::MouseMove(MouseMoveEvent {
                                    position: point(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t),
                                    pressed_button: Some(MouseButton::Left),
                                    modifiers: Modifiers::default(),
                                }),
                                cx,
                            );
                        }
                    };
                    let view_state = |cx: &App| -> (bool, bool, Option<DragOperationMask>, bool) {
                        if let BrowserView::Osr(v) = &attached_view {
                            let b = v.read(cx);
                            (
                                b.has_active_drag(),
                                b.has_active_drag_preview(),
                                b.active_drag_cursor_op(),
                                b.is_left_mouse_down(),
                            )
                        } else {
                            (false, false, None, false)
                        }
                    };
                    let (active_drag, drag_preview, drag_op, left_down) = view_state(cx);
                    let y_off = TOP_BAR_HEIGHT_DIP;

                    match dnd_phase {
                        DndSelfTestPhase::WaitInitialReady => {
                            if ready_printed && !cur_loading {
                                if !window.is_window_active() {
                                    window.activate_window();
                                }
                                if let BrowserView::Osr(v) = &attached_view {
                                    let focus = v.read(cx).focus_handle(cx);
                                    window.focus(&focus, cx);
                                }
                                let dnd_html = "data:text/html;charset=utf-8,%3C!doctype%20html%3E%3Cbody%20style%3D'margin%3A0%3Bpadding%3A0%3Bfont-family%3Asans-serif'%3E%3Cinput%20id%3D'sb_form_q'%20style%3D'position%3Afixed%3Bleft%3A20px%3Btop%3A16px%3Bwidth%3A360px%3Bheight%3A36px%3Bfont-size%3A16px'%3E%3Ca%20id%3D'lnk1'%20href%3D'https%3A%2F%2Fexample.com%2Fresult-1'%20onclick%3D%22window._nav%3D(window._nav%7C%7C0)%2B1%3Breturn%20false%3B%22%20style%3D'position%3Afixed%3Bleft%3A20px%3Btop%3A80px%3Bfont-size%3A22px%3Bline-height%3A32px'%3ESearch%20Result%20One%3C%2Fa%3E%3Cdiv%20id%3D'para1'%20style%3D'position%3Afixed%3Bleft%3A20px%3Btop%3A124px%3Bwidth%3A520px%3Bfont-size%3A18px%3Bline-height%3A28px%3Buser-select%3Atext'%3ESnippet%20paragraph%20text%20below%20result%20one%20that%20must%20never%20be%20painted%20blue%20during%20link%20drag.%3C%2Fdiv%3E%3Cdiv%20id%3D'sel_box'%20style%3D'position%3Afixed%3Bleft%3A20px%3Btop%3A190px%3Bwidth%3A480px%3Bfont-size%3A24px%3Bline-height%3A36px%3Buser-select%3Atext'%3ESelectedPhraseToken%3C%2Fdiv%3E%3Ca%20id%3D'lnk2'%20href%3D'https%3A%2F%2Fexample.com%2Fresult-2'%20onclick%3D%22window._nav%3D(window._nav%7C%7C0)%2B1%3Breturn%20false%3B%22%20style%3D'position%3Afixed%3Bleft%3A20px%3Btop%3A250px%3Bfont-size%3A22px%3Bline-height%3A32px'%3ESecond%20Result%20Link%3C%2Fa%3E%3C%2Fbody%3E";
                                let _ = handle.submit(BrowserCommand::Navigate(dnd_html.into()));
                                dnd_phase = DndSelfTestPhase::WaitFixturePage(None);
                            }
                        }
                        DndSelfTestPhase::WaitFixturePage(settled) => {
                            if last_addr.starts_with("data:text/html") && !cur_loading {
                                match settled {
                                    None => {
                                        dnd_phase = DndSelfTestPhase::WaitFixturePage(Some(Instant::now()));
                                    }
                                    Some(since) if since.elapsed() >= Duration::from_millis(200) => {
                                        dispatch_drag_path(60.0, y_off + 96.0, 420.0, y_off + 340.0, window, cx);
                                        dnd_phase = DndSelfTestPhase::S1WaitActive(Instant::now());
                                    }
                                    _ => {}
                                }
                            }
                        }
                        DndSelfTestPhase::S1WaitActive(since) => {
                            if active_drag || since.elapsed() >= Duration::from_secs(2) {
                                let mid_ok = active_drag && drag_preview && drag_op == Some(DragOperationMask::NONE);
                                for &(mx, my) in &[(140.0_f32, 138.0_f32), (280.0, 138.0), (420.0, 340.0)] {
                                    window.dispatch_event(
                                        PlatformInput::MouseMove(MouseMoveEvent {
                                            position: point(mx, y_off + my),
                                            pressed_button: Some(MouseButton::Left),
                                            modifiers: Modifiers::default(),
                                        }),
                                        cx,
                                    );
                                }
                                dnd_s1_link_drag_no_sel_ok = mid_ok;
                                let seq = issue_dnd_probe(&mut dnd_probe_seq);
                                dnd_phase = DndSelfTestPhase::S1WaitMidProbe(seq, Instant::now());
                            }
                        }
                        DndSelfTestPhase::S1WaitMidProbe(seq, since) => {
                            let parsed = parse_dnd_probe(&last_title, seq);
                            if parsed.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                let mid_clean = parsed.is_some_and(|(sel, _, nav)| sel.is_empty() && nav == 0);
                                dnd_s1_link_drag_no_sel_ok &= mid_clean;
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(420.0, y_off + 340.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                let (a2, _, _, l2) = view_state(cx);
                                dnd_s1_link_drag_no_sel_ok &= !a2 && !l2;
                                let next_seq = issue_dnd_probe(&mut dnd_probe_seq);
                                dnd_phase = DndSelfTestPhase::S1WaitEndProbe(next_seq, Instant::now());
                            }
                        }
                        DndSelfTestPhase::S1WaitEndProbe(seq, since) => {
                            let parsed = parse_dnd_probe(&last_title, seq);
                            if parsed.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                let end_clean = parsed.is_some_and(|(sel, _, nav)| sel.is_empty() && nav == 0);
                                dnd_s1_link_drag_no_sel_ok &= end_clean;
                                dispatch_drag_path(21.0, y_off + 208.0, 320.0, y_off + 208.0, window, cx);
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(320.0, y_off + 208.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                dnd_phase = DndSelfTestPhase::S2WaitSelSettle(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S2WaitSelSettle(since) => {
                            if since.elapsed() >= Duration::from_millis(120) {
                                let seq = issue_dnd_probe(&mut dnd_probe_seq);
                                dnd_phase = DndSelfTestPhase::S2WaitSelProbe(seq, Instant::now());
                            }
                        }
                        DndSelfTestPhase::S2WaitSelProbe(seq, since) => {
                            let parsed = parse_dnd_probe(&last_title, seq);
                            if parsed.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                if let Some((sel, _, _)) = parsed
                                    && sel.contains("SelectedPhraseToken")
                                {
                                    dnd_s2_sel0 = sel;
                                    dnd_s2_sel_drag_stable_ok = true;
                                }
                                dispatch_drag_path(80.0, y_off + 208.0, 420.0, y_off + 340.0, window, cx);
                                dnd_phase = DndSelfTestPhase::S2WaitActive(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S2WaitActive(since) => {
                            if active_drag || since.elapsed() >= Duration::from_secs(2) {
                                dnd_s2_sel_drag_stable_ok &= active_drag && drag_preview;
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(420.0, y_off + 340.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                let (a2, _, _, l2) = view_state(cx);
                                dnd_s2_sel_drag_stable_ok &= !a2 && !l2;
                                let next_seq = issue_dnd_probe(&mut dnd_probe_seq);
                                dnd_phase = DndSelfTestPhase::S2WaitEndProbe(next_seq, Instant::now());
                            }
                        }
                        DndSelfTestPhase::S2WaitEndProbe(seq, since) => {
                            let parsed = parse_dnd_probe(&last_title, seq);
                            if parsed.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                dnd_s2_sel_drag_stable_ok &= !dnd_s2_sel0.is_empty()
                                    && parsed.is_some_and(|(sel, _, nav)| sel == dnd_s2_sel0 && nav == 0);
                                dispatch_drag_path(80.0, y_off + 208.0, 160.0, y_off + 34.0, window, cx);
                                dnd_phase = DndSelfTestPhase::S3TextWaitActive {
                                    since: Instant::now(),
                                    nudged: false,
                                };
                            }
                        }
                        DndSelfTestPhase::S3TextWaitActive { since, nudged } => {
                            if active_drag && drag_op.is_some_and(|op| op != DragOperationMask::NONE) {
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(160.0, y_off + 34.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                let (a2, _, _, l2) = view_state(cx);
                                dnd_s3_drop_text_ok = !a2 && !l2;
                                dnd_phase = DndSelfTestPhase::S3TextWaitDropSettle(Instant::now());
                            } else if active_drag && !nudged {
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(164.0, y_off + 34.0),
                                        pressed_button: Some(MouseButton::Left),
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(160.0, y_off + 34.0),
                                        pressed_button: Some(MouseButton::Left),
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                dnd_phase = DndSelfTestPhase::S3TextWaitActive { since, nudged: true };
                            } else if since.elapsed() >= Duration::from_secs(2) {
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(160.0, y_off + 34.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                dnd_s3_drop_text_ok = false;
                                dnd_phase = DndSelfTestPhase::S3TextWaitDropSettle(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S3TextWaitDropSettle(since) => {
                            if since.elapsed() >= Duration::from_millis(100) {
                                let seq = issue_dnd_probe(&mut dnd_probe_seq);
                                dnd_phase = DndSelfTestPhase::S3TextWaitProbe(seq, Instant::now());
                            }
                        }
                        DndSelfTestPhase::S3TextWaitProbe(seq, since) => {
                            let parsed = parse_dnd_probe(&last_title, seq);
                            if parsed.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                dnd_s3_drop_text_ok &= parsed.is_some_and(|(_, q, nav)| {
                                    q.contains("SelectedPhraseToken") && nav == 0
                                });
                                let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                    script: "document.getElementById('sb_form_q').value='';".into(),
                                    frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                });
                                dispatch_click(MouseButton::Left, 420.0, y_off + 340.0, window, cx);
                                dnd_phase = DndSelfTestPhase::S3LinkWaitClear(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S3LinkWaitClear(since) => {
                            if since.elapsed() >= Duration::from_millis(80) {
                                dispatch_drag_path(60.0, y_off + 96.0, 160.0, y_off + 34.0, window, cx);
                                dnd_phase = DndSelfTestPhase::S3LinkWaitActive {
                                    since: Instant::now(),
                                    nudged: false,
                                };
                            }
                        }
                        DndSelfTestPhase::S3LinkWaitActive { since, nudged } => {
                            if active_drag && drag_op.is_some_and(|op| op != DragOperationMask::NONE) {
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(160.0, y_off + 34.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                let (a2, _, _, l2) = view_state(cx);
                                dnd_s3_drop_link_ok = !a2 && !l2;
                                dnd_phase = DndSelfTestPhase::S3LinkWaitDropSettle(Instant::now());
                            } else if active_drag && !nudged {
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(164.0, y_off + 34.0),
                                        pressed_button: Some(MouseButton::Left),
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(160.0, y_off + 34.0),
                                        pressed_button: Some(MouseButton::Left),
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                dnd_phase = DndSelfTestPhase::S3LinkWaitActive { since, nudged: true };
                            } else if since.elapsed() >= Duration::from_secs(2) {
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(160.0, y_off + 34.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                dnd_s3_drop_link_ok = false;
                                dnd_phase = DndSelfTestPhase::S3LinkWaitDropSettle(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S3LinkWaitDropSettle(since) => {
                            if since.elapsed() >= Duration::from_millis(100) {
                                let seq = issue_dnd_probe(&mut dnd_probe_seq);
                                dnd_phase = DndSelfTestPhase::S3LinkWaitProbe(seq, Instant::now());
                            }
                        }
                        DndSelfTestPhase::S3LinkWaitProbe(seq, since) => {
                            let parsed = parse_dnd_probe(&last_title, seq);
                            if parsed.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                dnd_s3_drop_link_ok &= parsed.is_some_and(|(_, q, nav)| {
                                    q.contains("https://example.com/result-1") && nav == 0
                                });
                                let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                    script: "document.getElementById('sb_form_q').value='';".into(),
                                    frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                });
                                dispatch_click(MouseButton::Left, 420.0, y_off + 340.0, window, cx);
                                dnd_phase = DndSelfTestPhase::S4EscWaitClear(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S4EscWaitClear(since) => {
                            if since.elapsed() >= Duration::from_millis(80) {
                                dispatch_drag_path(60.0, y_off + 266.0, 160.0, y_off + 34.0, window, cx);
                                dnd_phase = DndSelfTestPhase::S4EscWaitActive {
                                    since: Instant::now(),
                                    nudged: false,
                                };
                            }
                        }
                        DndSelfTestPhase::S4EscWaitActive { since, nudged } => {
                            if active_drag && drag_op.is_some_and(|op| op != DragOperationMask::NONE) {
                                dnd_s4_escape_cancel_ok = true;
                                if kb_active {
                                    esc_hwnd = window_hwnd(window).map(|h| h.get() as isize);
                                } else {
                                    window.dispatch_event(
                                        PlatformInput::KeyDown(KeyDownEvent {
                                            keystroke: Keystroke::parse("escape").unwrap(),
                                            is_held: false,
                                            prefer_character_input: false,
                                        }),
                                        cx,
                                    );
                                    window.dispatch_event(
                                        PlatformInput::KeyUp(KeyUpEvent {
                                            keystroke: Keystroke::parse("escape").unwrap(),
                                        }),
                                        cx,
                                    );
                                }
                                dnd_phase = DndSelfTestPhase::S4EscWaitCleared(Instant::now());
                            } else if active_drag && !nudged {
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(164.0, y_off + 34.0),
                                        pressed_button: Some(MouseButton::Left),
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(160.0, y_off + 34.0),
                                        pressed_button: Some(MouseButton::Left),
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                dnd_phase = DndSelfTestPhase::S4EscWaitActive { since, nudged: true };
                            } else if since.elapsed() >= Duration::from_secs(2) {
                                dnd_s4_escape_cancel_ok = false;
                                dnd_phase = DndSelfTestPhase::S4EscWaitCleared(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S4EscWaitCleared(since) => {
                            if !active_drag || since.elapsed() >= Duration::from_secs(1) {
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(160.0, y_off + 34.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                let (a2, _, _, l2) = view_state(cx);
                                dnd_s4_escape_cancel_ok &= !a2 && !l2;
                                let seq = issue_dnd_probe(&mut dnd_probe_seq);
                                dnd_phase = DndSelfTestPhase::S4EscWaitProbe(seq, Instant::now());
                            }
                        }
                        DndSelfTestPhase::S4EscWaitProbe(seq, since) => {
                            let parsed = parse_dnd_probe(&last_title, seq);
                            if parsed.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                dnd_s4_escape_cancel_ok &= parsed.is_some_and(|(_, q, nav)| {
                                    q.is_empty() && nav == 0
                                });
                                dispatch_drag_path(21.0, y_off + 208.0, 320.0, y_off + 208.0, window, cx);
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(320.0, y_off + 208.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                dnd_phase = DndSelfTestPhase::S4IntWaitSelSettle(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S4IntWaitSelSettle(since) => {
                            if since.elapsed() >= Duration::from_millis(80) {
                                dispatch_drag_path(60.0, y_off + 96.0, 420.0, y_off + 340.0, window, cx);
                                dnd_phase = DndSelfTestPhase::S4IntWaitLnk1Active(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S4IntWaitLnk1Active(since) => {
                            if active_drag || since.elapsed() >= Duration::from_secs(2) {
                                dnd_s4_interleaved_ok = active_drag;
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(420.0, y_off + 340.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                dnd_phase = DndSelfTestPhase::S4IntWaitLnk1Release(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S4IntWaitLnk1Release(since) => {
                            if since.elapsed() >= Duration::from_millis(80) {
                                window.dispatch_event(
                                    PlatformInput::MouseMove(MouseMoveEvent {
                                        position: point(420.0, y_off + 340.0),
                                        pressed_button: None,
                                        modifiers: Modifiers::default(),
                                    }),
                                    cx,
                                );
                                dispatch_click(MouseButton::Left, 420.0, y_off + 340.0, window, cx);
                                dnd_phase = DndSelfTestPhase::S4IntWaitBlankSettle(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S4IntWaitBlankSettle(since) => {
                            if since.elapsed() >= Duration::from_millis(80) {
                                dispatch_drag_path(60.0, y_off + 266.0, 420.0, y_off + 340.0, window, cx);
                                dnd_phase = DndSelfTestPhase::S4IntWaitLnk2Active(Instant::now());
                            }
                        }
                        DndSelfTestPhase::S4IntWaitLnk2Active(since) => {
                            if active_drag || since.elapsed() >= Duration::from_secs(2) {
                                dnd_s4_interleaved_ok &= active_drag;
                                window.dispatch_event(
                                    PlatformInput::MouseUp(MouseUpEvent {
                                        button: MouseButton::Left,
                                        position: point(420.0, y_off + 340.0),
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                    }),
                                    cx,
                                );
                                for &(mx, my) in &[
                                    (40.0_f32, 138.0_f32),
                                    (340.0, 138.0),
                                    (40.0, 208.0),
                                    (340.0, 208.0),
                                ] {
                                    window.dispatch_event(
                                        PlatformInput::MouseMove(MouseMoveEvent {
                                            position: point(mx, y_off + my),
                                            pressed_button: None,
                                            modifiers: Modifiers::default(),
                                        }),
                                        cx,
                                    );
                                }
                                let (a2, _, _, l2) = view_state(cx);
                                dnd_s4_interleaved_ok &= !a2 && !l2;
                                let seq = issue_dnd_probe(&mut dnd_probe_seq);
                                dnd_phase = DndSelfTestPhase::S4IntWaitProbe(seq, Instant::now());
                            }
                        }
                        DndSelfTestPhase::S4IntWaitProbe(seq, since) => {
                            let parsed = parse_dnd_probe(&last_title, seq);
                            if parsed.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                dnd_s4_interleaved_ok &= !active_drag
                                    && !left_down
                                    && parsed.is_some_and(|(sel, _, nav)| sel.is_empty() && nav == 0);
                                let _ = handle.submit(BrowserCommand::ExecuteJavaScript {
                                    script: "document.getElementById('sb_form_q').value='';".into(),
                                    frame_scope: rust_cef_gpui::js_bridge::JsFrameScope::MainFrame,
                                });
                                dnd_phase = DndSelfTestPhase::S5OleRun;
                            }
                        }
                        DndSelfTestPhase::S5OleRun => {
                            if let Some(hwnd) = window_hwnd(window) {
                                ole_test_params = Some((hwnd.get() as isize, window.scale_factor()));
                            }
                            dnd_phase = DndSelfTestPhase::S5OleWaitSettle(Instant::now());
                        }
                        DndSelfTestPhase::S5OleWaitSettle(since) => {
                            if since.elapsed() >= Duration::from_millis(100) {
                                let seq = issue_dnd_probe(&mut dnd_probe_seq);
                                dnd_phase = DndSelfTestPhase::S5OleWaitProbe(seq, Instant::now());
                            }
                        }
                        DndSelfTestPhase::S5OleWaitProbe(seq, since) => {
                            let parsed = parse_dnd_probe(&last_title, seq);
                            if parsed.is_some() || since.elapsed() >= Duration::from_secs(2) {
                                dnd_s5_ole_outbound_and_drop_ok &= parsed.is_some_and(|(_, q, nav)| {
                                    q.contains("https://gpui.rs/") && nav == 0
                                });
                                self_test_passed = dnd_s1_link_drag_no_sel_ok
                                    && dnd_s2_sel_drag_stable_ok
                                    && dnd_s3_drop_text_ok
                                    && dnd_s3_drop_link_ok
                                    && dnd_s4_escape_cancel_ok
                                    && dnd_s4_interleaved_ok
                                    && dnd_s5_ole_outbound_and_drop_ok;
                                println!(
                                    "BING_OSR_DND_SELFTEST s1_link_drag_no_sel_ok={dnd_s1_link_drag_no_sel_ok} s2_sel_drag_stable_ok={dnd_s2_sel_drag_stable_ok} s3_drop_text_ok={dnd_s3_drop_text_ok} s3_drop_link_ok={dnd_s3_drop_link_ok} s4_escape_cancel_ok={dnd_s4_escape_cancel_ok} s4_interleaved_ok={dnd_s4_interleaved_ok} s5_ole_outbound_and_drop_ok={dnd_s5_ole_outbound_and_drop_ok} sel0={dnd_s2_sel0:?}"
                                );
                                dnd_phase = DndSelfTestPhase::Done(Instant::now());
                            }
                        }
                        DndSelfTestPhase::Done(since) => {
                            if since.elapsed() >= Duration::from_millis(100) {
                                exit_dnd_selftest = true;
                            }
                        }
                    }
                });
                if let Some((hwnd, scale)) = ole_test_params {
                    match exercise_ole_drop_target_and_data_object(hwnd, scale, &handle) {
                        Ok(()) => dnd_s5_ole_outbound_and_drop_ok = true,
                        Err(e) => {
                            eprintln!("BING_OSR_DND_OLE_ERR: {e}");
                            dnd_s5_ole_outbound_and_drop_ok = false;
                        }
                    }
                }
                if let Some(hwnd) = esc_hwnd {
                    post_win32_key(hwnd, 0x1B, false);
                    post_win32_key(hwnd, 0x1B, true);
                }
                if exit_dnd_selftest {
                    break;
                }
            }

            if let Err(e) = service_loop(cx, &rt, &receiver, loop_deadline).await {
                eprintln!("service loop error: {e}");
                break;
            }
        }

        if any_self_test && !self_test_passed {
            let active = win
                .update(cx, |_, window, _| window.is_window_active())
                .unwrap_or(false);
            let menu_diag = menu_pair
                .as_mut()
                .and_then(|(m, reg)| Some((m.status(browser_id).ok(), reg.status())));
            let pres_diag = presenter.as_ref().map(|p| p.borrow().status());
            let kb_diag = keyboard.as_ref().map(|k| k.diagnostics());
            let view_diag = if let BrowserView::Osr(v) = &attached_view {
                win.update(cx, |_, _, cx| v.read(cx).diagnostics()).ok()
            } else {
                None
            };
            eprintln!(
                "BING_SELFTEST_TIMEOUT phase={:?} dnd_phase={:?} active={} handler_counts={:?} observed={:?} kb_diag={:?} view_diag={:?} menu_diag={:?} pres_diag={:?}",
                self_test_phase,
                dnd_phase,
                active,
                rt.borrow().context_menu_handler_counts(),
                *observed_menus.borrow(),
                kb_diag,
                view_diag,
                menu_diag,
                pres_diag
            );
        }

        if let Some(mut ts) = touch_services.take() {
            let _ = ts.retire();
        }
        if let Some(mut ds) = drop_services.take() {
            let _ = ds.retire();
        }
        if let Some(mut fs) = finds.take() {
            let _ = cx.update(|cx| fs.shutdown(cx));
            drop(fs);
        }
        if let Some(mut src) = sources.take() {
            let _ = cx.update(|cx| src.shutdown(cx));
            drop(src);
        }
        let _ = win.update(cx, |root, _, _| {
            root.menu = None;
        });
        if let Some(p) = presenter {
            let _ = cx.update(|cx| p.borrow_mut().shutdown(cx));
        }
        drop(menu_pair);

        let final_images = win
            .update(cx, |root, window, cx| {
                let mut imgs = 0u64;
                if let Some(view) = root.view.take() {
                    match view {
                        BrowserView::Osr(v) => {
                            v.update(cx, |b, cx| {
                                let _ = b.release_image(window, cx);
                                imgs = b.diagnostics().created_images;
                            });
                        }
                        BrowserView::Native(v) => v.update(cx, |b, _| b.retire()),
                    }
                }
                root.status = "Shutting down CEF...";
                cx.notify();
                imgs
            })
            .unwrap_or(0);

        if let Some(mut kb) = keyboard.take() {
            let _ = cx.update(|_| kb.retire());
        }

        let _ = shutdown_runtime(cx, &rt, &receiver).await;

        let d = rt.borrow().diagnostics();
        let created = obs.created.load(Ordering::Acquire);
        let closed = obs.closed.load(Ordering::Acquire);
        println!(
            "BING_STOP created={created} closed={closed} denied_resources={} denied_navs={} images={final_images} incomplete_frames={} resize_exhausted={} shutdown={} native_after={}",
            obs.denied_resources.load(Ordering::Acquire),
            obs.denied_navs.load(Ordering::Acquire),
            obs.incomplete_frames.load(Ordering::Relaxed),
            obs.resize_exhausted.load(Ordering::Relaxed),
            d.cef_shutdown_calls,
            d.native_calls_after_shutdown
        );
        if d.cef_shutdown_calls == 1
            && d.native_calls_after_shutdown == 0
            && created == closed
            && self_test_passed
        {
            0
        } else {
            1
        }
    }

    pub fn entry() -> i32 {
        let args = match parse_args() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
        let exit_code = Rc::new(Cell::new(0));
        let out_code = exit_code.clone();
        let initial_url = args.url.clone();
        let native_hwnd = args.native_hwnd;

        gpui_platform::application().run(move |cx| {
            if !native_hwnd {
                rust_cef_gpui::init_default_menu_ui(cx);
            }
            let (sender, receiver) = dispatcher::channel();
            let sender = Arc::new(sender);
            let close = Rc::new(Cell::new(false));
            let allow_close = Rc::new(Cell::new(false));
            let (req_close, wake, can_close) = (close.clone(), sender.clone(), allow_close.clone());

            let win = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                            None,
                            size(px(WINDOW_WIDTH_DIP), px(WINDOW_HEIGHT_DIP)),
                            cx,
                        ))),
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some("Bing - cef-rs-component".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    move |window, cx| {
                        window.on_window_should_close(cx, move |_, _| {
                            if can_close.get() {
                                return true;
                            }
                            req_close.set(true);
                            let _ = wake.wake();
                            false
                        });
                        cx.new(|_| BingWindow {
                            view: None,
                            handle: None,
                            open_devtools: None,
                            cancel_find: None,
                            menu: None,
                            native_hwnd,
                            home_url: initial_url.clone(),
                            address: initial_url,
                            title: String::new(),
                            loading: true,
                            can_back: false,
                            can_forward: false,
                            zoom_milli: 0,
                            find_badge: None,
                            download_badge: None,
                            crash_badge: None,
                            status: "Starting CEF...",
                        })
                    },
                )
                .expect("open BingWindow");

            cx.spawn(async move |cx: &mut AsyncApp| {
                let code = drive(cx, win, args, close, sender, receiver).await;
                out_code.set(code);
                allow_close.set(true);
                cx.update(|app: &mut App| app.quit());
            })
            .detach();
        });

        exit_code.get()
    }
}
