//! Passive Win32 observations on the GPUI owner thread, separate from paint timing.
use super::*;

pub(super) const PEER_TITLE: &str = "GPUI-CEF profile timing peer";

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumThreadWindows(
        thread: u32,
        callback: unsafe extern "system" fn(isize, isize) -> i32,
        state: isize,
    ) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn GetWindowTextW(hwnd: isize, text: *mut u16, capacity: i32) -> i32;
    fn GetDpiForWindow(hwnd: isize) -> u32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn IsWindow(hwnd: isize) -> i32;
    fn IsIconic(hwnd: isize) -> i32;
    fn IsZoomed(hwnd: isize) -> i32;
    fn GetForegroundWindow() -> isize;
    fn GetWindowRect(hwnd: isize, rect: *mut ResizeRect) -> i32;
    fn GetClientRect(hwnd: isize, rect: *mut ResizeRect) -> i32;
    fn SetWindowPos(
        hwnd: isize,
        insert_after: isize,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        flags: u32,
    ) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ResizeRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
impl ResizeRect {
    fn size(self) -> Result<(i32, i32), String> {
        let w = self.right.checked_sub(self.left);
        let h = self.bottom.checked_sub(self.top);
        match (w, h) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Ok((w, h)),
            _ => Err("passive Resize invalid native rect".into()),
        }
    }
}
const PASSIVE_RESIZE_FLAGS: u32 = 0x0002 | 0x0004 | 0x0010; // NOMOVE | NOZORDER | NOACTIVATE

/// Validation-only original primary identity, without input hook or subclass.
pub(in crate::windows) struct PassiveResizeWindow {
    window: NativeWindow,
    pid: u32,
    thread: u32,
}
impl PassiveResizeWindow {
    pub(in crate::windows) fn observe(scale: f32) -> Result<Self, String> {
        let owned = owned_titles()?;
        let window = owned.a.ok_or("passive Resize original HWND not visible")?;
        if owned.b.is_some() || window.dpi as f32 / 96.0 != scale {
            return Err("passive Resize original window/DPI not unique".into());
        }
        let result = Self {
            window,
            pid: owned.pid,
            thread: owned.thread,
        };
        result.sample()?;
        Ok(result)
    }

    fn sample(&self) -> Result<(ResizeRect, (i32, i32)), String> {
        let owned = owned_titles()?;
        let h = self.window.hwnd;
        if owned.a != Some(self.window)
            || owned.b.is_some()
            || owned.pid != self.pid
            || owned.thread != self.thread
            || unsafe { GetForegroundWindow() } == h
            || unsafe { IsIconic(h) } != 0
            || unsafe { IsZoomed(h) } != 0
        {
            return Err(format!(
                "passive Resize original HWND/DPI changed or not an unfocused normal window                  (owner_a={} extra={} pid_ok={} thread_ok={} fg={} iconic={} zoomed={})",
                owned.a == Some(self.window),
                owned.b.is_some(),
                owned.pid == self.pid,
                owned.thread == self.thread,
                unsafe { GetForegroundWindow() } == h,
                unsafe { IsIconic(h) } != 0,
                unsafe { IsZoomed(h) } != 0,
            ));
        }
        let (mut window, mut client) = (ResizeRect::default(), ResizeRect::default());
        // SAFETY: original owned normal HWND on its creation thread, writable stack RECTs.
        if unsafe { GetWindowRect(h, &mut window) } == 0
            || unsafe { GetClientRect(h, &mut client) } == 0
        {
            return Err("passive Resize native rectangle query failed".into());
        }
        Ok((window, client.size()?))
    }

    fn client_target(&self, dip: (u32, u32)) -> (i32, i32) {
        let scale = self.window.dpi as f32 / 96.0;
        (
            (dip.0 as f32 * scale).round() as i32,
            (dip.1 as f32 * scale).round() as i32,
        )
    }

    pub(in crate::windows) fn resize(&self, dip: (u32, u32)) -> Result<(), String> {
        let (window, client) = self.sample()?;
        let target = self.client_target(dip);
        if client == target {
            return Ok(());
        }
        let (w, h) = window.size()?;
        let outer = |current: i32, client: i32, next: i32| {
            current
                .checked_sub(client)
                .filter(|padding| (0..=2048).contains(padding))
                .and_then(|padding| padding.checked_add(next))
                .filter(|extent| (1..=8192).contains(extent))
                .ok_or("passive Resize invalid nonclient/target size")
        };
        // SAFETY: only the original scoped HWND; no movement, z-order or activation.
        if unsafe {
            SetWindowPos(
                self.window.hwnd,
                0,
                0,
                0,
                outer(w, client.0, target.0)?,
                outer(h, client.1, target.1)?,
                PASSIVE_RESIZE_FLAGS,
            )
        } == 0
        {
            return Err("passive Resize SetWindowPos failed".into());
        }
        let (after, actual) = self.sample()?;
        if (after.left, after.top) != (window.left, window.top) || actual != target {
            return Err(
                "passive Resize moved the original window or missed exact client size".into(),
            );
        }
        Ok(())
    }

    pub(in crate::windows) fn evidence(&self, dip: (u32, u32)) -> Result<String, String> {
        let (_, client) = self.sample()?;
        if client != self.client_target(dip) {
            return Err("passive Resize client changed before capture gate".into());
        }
        Ok(object(vec![
            ("hwnd", self.window.hwnd.to_string()),
            ("pid", self.pid.to_string()),
            ("thread", self.thread.to_string()),
            ("dpi", self.window.dpi.to_string()),
            ("client", format!("[{},{}]", client.0, client.1)),
            ("foreground", bool_json(false)),
            ("resize_flags", PASSIVE_RESIZE_FLAGS.to_string()),
        ]))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct NativeWindow {
    hwnd: isize,
    dpi: u32,
}

struct OwnedTitles {
    thread: u32,
    pid: u32,
    primary: Vec<u16>,
    peer: Vec<u16>,
    a: Option<NativeWindow>,
    b: Option<NativeWindow>,
    ambiguous: bool,
}

unsafe extern "system" fn observe(hwnd: isize, state: isize) -> i32 {
    // EnumThreadWindows is synchronous; it only borrows the stack search state.
    let state = unsafe { &mut *(state as *mut OwnedTitles) };
    let mut pid = 0;
    let tid = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if tid != state.thread || pid != state.pid || unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    let mut text = [0u16; 128];
    let len = unsafe { GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32) };
    if len <= 0 || len as usize >= text.len() - 1 {
        return 1;
    }
    let text = &text[..len as usize];
    let slot = if text == state.primary {
        &mut state.a
    } else if text == state.peer {
        &mut state.b
    } else {
        return 1;
    };
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 || slot.replace(NativeWindow { hwnd, dpi }).is_some() {
        state.ambiguous = true;
    }
    1
}

fn owned_titles() -> Result<OwnedTitles, String> {
    let mut state = OwnedTitles {
        thread: unsafe { GetCurrentThreadId() },
        pid: std::process::id(),
        primary: TITLE.encode_utf16().collect(),
        peer: PEER_TITLE.encode_utf16().collect(),
        a: None,
        b: None,
        ambiguous: false,
    };
    let found = unsafe {
        EnumThreadWindows(
            state.thread,
            observe,
            &mut state as *mut OwnedTitles as isize,
        )
    };
    if found == 0 || state.ambiguous {
        return Err("could not uniquely observe the original owned GPUI windows".into());
    }
    Ok(state)
}

pub(super) struct Pair {
    a: NativeWindow,
    b: NativeWindow,
    pub(super) gpui_a: u64,
    pub(super) gpui_b: u64,
    thread: u32,
}

impl Pair {
    pub(super) fn observe(
        cx: &mut AsyncApp,
        a_root: WindowHandle<HostRoot>,
        b_root: WindowHandle<HostRoot>,
    ) -> Result<Self, String> {
        let facts = |cx: &mut AsyncApp, root| {
            update(cx, root, |_, w, _| {
                (w.window_handle().window_id().as_u64(), w.scale_factor())
            })
        };
        let (gpui_a, scale_a) = facts(cx, a_root)?;
        let (gpui_b, scale_b) = facts(cx, b_root)?;
        let owned = owned_titles()?;
        let a = owned.a.ok_or("original A observed HWND missing")?;
        let b = owned.b.ok_or("original B observed HWND missing")?;
        if gpui_a == gpui_b
            || a.hwnd == b.hwnd
            || a.hwnd == 0
            || b.hwnd == 0
            || (a.dpi as f32 / 96.0 - scale_a).abs() > 0.001
            || (b.dpi as f32 / 96.0 - scale_b).abs() > 0.001
        {
            return Err("GPUI/native timing window identity or scale mismatch".into());
        }
        Ok(Self {
            a,
            b,
            gpui_a,
            gpui_b,
            thread: owned.thread,
        })
    }

    pub(super) fn peer_gpui_window(&self) -> u64 {
        self.gpui_b
    }

    pub(super) fn both_still_owned(&self) -> Result<(), String> {
        let owned = owned_titles()?;
        if owned.a == Some(self.a) && owned.b == Some(self.b) && owned.thread == self.thread {
            Ok(())
        } else {
            Err("original two HWNDs no longer match the timing pair".into())
        }
    }

    pub(super) fn renderer_snapshot(
        &self,
        cx: &mut AsyncApp,
        a_root: WindowHandle<HostRoot>,
        b_root: WindowHandle<HostRoot>,
    ) -> String {
        let mut capture = || -> Result<String, String> {
            self.both_still_owned()?;
            // Runtime test-support enumeration happens outside the GPUI update.
            let inventory = rust_cef_gpui::test_support::probe_dxgi_adapters()?;
            let mut windows = Vec::with_capacity(2);
            for (root, expected, hwnd) in [
                (a_root, self.gpui_a, self.a.hwnd),
                (b_root, self.gpui_b, self.b.hwnd),
            ] {
                let (id, gpu) = update(cx, root, |_, w, _| {
                    (w.window_handle().window_id().as_u64(), w.gpu_specs())
                })?;
                let gpu = gpu.ok_or("GPUI renderer adapter unavailable")?;
                if id != expected || gpu.device_name.is_empty() || gpu.device_name.len() > 512 {
                    return Err("GPUI renderer adapter identity or name invalid".into());
                }
                windows.push(object(vec![
                    ("gpui", id.to_string()),
                    ("hwnd", hwnd.to_string()),
                    ("name", quote(&gpu.device_name)),
                    ("software", bool_json(gpu.is_software_emulated)),
                ]));
            }
            self.both_still_owned()?;
            let adapters: Vec<_> = inventory
                .iter()
                .map(|a| {
                    object(vec![
                        ("name", quote(&a.name)),
                        ("software", bool_json(a.software)),
                        ("luid", format!("[{},{}]", a.luid.0, a.luid.1)),
                    ])
                })
                .collect();
            Ok(object(vec![
                ("pid", std::process::id().to_string()),
                ("thread", self.thread.to_string()),
                ("windows", format!("[{}]", windows.join(","))),
                ("dxgi", format!("[{}]", adapters.join(","))),
            ]))
        };
        // Older timing scopes do not claim renderer adapter correlation;
        // retain observation errors separately rather than pretending they match.
        match capture() {
            Ok(snapshot) => snapshot,
            Err(error) => object(vec![("error", quote(&error))]),
        }
    }

    pub(super) fn a_hwnd_gone(&self) -> bool {
        unsafe { IsWindow(self.a.hwnd) == 0 }
    }

    pub(super) fn report_after_a_removed(
        &self,
        native_destroy_yields: u32,
    ) -> Result<String, String> {
        let owned = owned_titles()?;
        if !self.a_hwnd_gone()
            || owned.a.is_some()
            || owned.b != Some(self.b)
            || owned.thread != self.thread
        {
            return Err("original A HWND still present or original B HWND changed".into());
        }
        Ok(object(vec![
            ("a_hwnd", self.a.hwnd.to_string()),
            ("b_hwnd", self.b.hwnd.to_string()),
            ("a_gpui", self.gpui_a.to_string()),
            ("b_gpui", self.gpui_b.to_string()),
            ("a_dpi", self.a.dpi.to_string()),
            ("b_dpi", self.b.dpi.to_string()),
            ("thread", self.thread.to_string()),
            ("pid", owned.pid.to_string()),
            ("a_hwnd_gone", "true".into()),
            ("native_destroy_yields", native_destroy_yields.to_string()),
            ("original_b_hwnd_still_owned", "true".into()),
        ]))
    }
}
