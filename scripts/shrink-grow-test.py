"""Real-window regression: tiny -> large must recover CEF frames and displayed pixels.

The default static page exposes stalled capture; SHRINK_ANIMATED=1 also checks
animation. SHRINK_DRAG=1 uses the native border drag instead of SetWindowPos.
Requires an unobscured interactive desktop. Logs: E:/cargo-target/gpui-cef-poc/shrink-grow-test/.
"""
import ctypes
from ctypes import wintypes
import os
import re
from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
import poc_paths
import subprocess
import time
from urllib.parse import quote

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))
user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.GetClientRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.ClientToScreen.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.POINT)]
user32.GetDC.argtypes = [wintypes.HWND]
user32.GetDC.restype = wintypes.HDC
user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]
gdi32.GetPixel.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int]
gdi32.GetPixel.restype = wintypes.DWORD
user32.SetWindowPos.argtypes = [wintypes.HWND, wintypes.HWND, ctypes.c_int,
                              ctypes.c_int, ctypes.c_int, ctypes.c_int, wintypes.UINT]
user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]


def find_window(pid):
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def visit(hwnd, _):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        title = ctypes.create_unicode_buffer(256)
        user32.GetWindowTextW(hwnd, title, len(title))
        if owner.value == pid and title.value == "GPUI + CEF PoC":
            found.append(hwnd)
        return True

    user32.EnumWindows(visit, 0)
    return found[0] if found else None


def frames(path):
    if not path.exists():
        return []
    return path.read_text(encoding="utf-8", errors="replace").splitlines()


def sample(hwnd, fraction=(0.5, 0.5)):
    rect = wintypes.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(rect))
    point = wintypes.POINT(int(rect.right * fraction[0]), int(rect.bottom * fraction[1]))
    user32.ClientToScreen(hwnd, ctypes.byref(point))
    dc = user32.GetDC(None)
    try:
        return gdi32.GetPixel(dc, point.x, point.y)
    finally:
        user32.ReleaseDC(None, dc)


def main():
    output = poc_paths.layout_root() / 'shrink-grow-test'
    output.mkdir(parents=True, exist_ok=True)
    frame_log, paint_log = output / "frames.log", output / "paints.log"
    for path in (frame_log, paint_log):
        path.unlink(missing_ok=True)
    animated = os.environ.get("SHRINK_ANIMATED") == "1"
    page = "<html><body style='margin:0;background:rgb(220,60,60)'>"
    if animated:
        page += """<script>setInterval(()=>{document.body.style.background=
        Math.floor(performance.now()/180)%2?'rgb(220,60,60)':'rgb(60,200,60)'},30);
        </script>"""
    page += "</body></html>"
    env = dict(os.environ, POC_FRAME_LOG=str(frame_log), POC_PAINT_LOG=str(paint_log),
               POC_EXIT_AFTER_MS="35000", START_URL="data:text/html," + quote(page),
               POC_ACCELERATED=os.environ.get("POC_ACCELERATED", "1"))
    real_drag = os.environ.get("SHRINK_DRAG") == "1"
    failures = []
    with (output / "app.log").open("w", encoding="utf-8") as log:
        process = subprocess.Popen([str(poc_paths.bundle_dir('debug') / 'gpui-cef-poc.exe')],
                                   env=env, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 10
            hwnd = None
            while time.monotonic() < deadline:
                hwnd = find_window(process.pid)
                if hwnd and len(frames(frame_log)) >= 2:
                    break
                time.sleep(0.05)
            if not hwnd:
                raise RuntimeError("app window not found")
            # Position on screen so GetPixel observes the actual browser surface.
            user32.SetForegroundWindow(hwnd)
            user32.SetWindowPos(hwnd, None, 0, 0, 1200, 1000, 0x0040)
            time.sleep(0.3)
            for cycle, small in enumerate([(420, 240), (220, 130), (160, 90),
                                            (420, 240), (160, 90), (220, 130)]):
                if real_drag:
                    user32.SetWindowPos(hwnd, None, 0, 0, 1200, 1000, 0x0040)
                    time.sleep(0.15)
                    rect = wintypes.RECT()
                    user32.GetWindowRect(hwnd, ctypes.byref(rect))
                    user32.SetCursorPos(rect.right - 3, rect.bottom - 3)
                    user32.mouse_event(0x0002, 0, 0, 0, 0)
                    time.sleep(0.08)
                for step in range(12):
                    width = 1200 + (small[0] - 1200) * (step + 1) // 12
                    height = 1000 + (small[1] - 1000) * (step + 1) // 12
                    if real_drag:
                        user32.SetCursorPos(width - 3, height - 3)
                    else:
                        user32.SetWindowPos(hwnd, None, 0, 0, width, height, 0x0040)
                    time.sleep(0.015)
                time.sleep(0.08)
                small_rect = wintypes.RECT()
                user32.GetWindowRect(hwnd, ctypes.byref(small_rect))
                shrunk = (small_rect.right - small_rect.left, small_rect.bottom - small_rect.top)
                if shrunk[0] >= 600 or shrunk[1] >= 400:
                    failures.append(f"cycle {cycle}: shrink input failed, actual={shrunk}")
                before = len(frames(frame_log))
                if real_drag:
                    user32.SetCursorPos(1197, 997)
                    time.sleep(0.03)
                    user32.mouse_event(0x0004, 0, 0, 0, 0)
                else:
                    user32.SetWindowPos(hwnd, None, 0, 0, 1200, 1000, 0x0040)
                colors = set()
                deadline = time.monotonic() + 1.8
                while time.monotonic() < deadline:
                    colors.add(sample(hwnd))
                    time.sleep(0.06)
                received = frames(frame_log)[before:]
                rect = wintypes.RECT()
                user32.GetWindowRect(hwnd, ctypes.byref(rect))
                actual = (rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top)
                probes = [sample(hwnd, point) for point in
                          [(0.1, 0.2), (0.5, 0.2), (0.9, 0.2),
                           (0.1, 0.5), (0.5, 0.5), (0.9, 0.5),
                           (0.1, 0.8), (0.5, 0.8), (0.9, 0.8)]]
                last_size = received[-1].split()[1] if received else "none"
                print(f"cycle={cycle} small={small} shrunk={shrunk} window={actual} frames={len(received)} "
                      f"last_size={last_size} colors={[hex(c) for c in colors]} "
                      f"probes={[hex(c) for c in probes]}", flush=True)
                if actual != (0, 0, 1200, 1000):
                    failures.append(f"cycle {cycle}: native resize did not reach target geometry")
                red, green = 0x3C3CDC, 0x3CC83C
                if not received:
                    failures.append(f"cycle {cycle}: CEF did not repaint after growing")
                if animated:
                    if not {red, green}.issubset(colors):
                        failures.append(f"cycle {cycle}: displayed animation froze")
                elif any(color != red for color in probes):
                    failures.append(f"cycle {cycle}: static content did not fill recovered surface")
        finally:
            if real_drag:
                user32.mouse_event(0x0004, 0, 0, 0, 0)
            if process.poll() is None:
                if hwnd:
                    user32.PostMessageW(hwnd, 0x0010, 0, 0)
                try:
                    # The application's CEF shutdown deadline is 10 seconds.
                    # Let it report that failure itself before forcing cleanup.
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    failures.append("app failed to close")
            if process.returncode != 0:
                failures.append(f"app exited with code {process.returncode}")
    # A live producer can still publish stale, letterboxed resize captures.
    # Check every published frame, not just the eventual settled screenshot.
    incomplete = 0
    inspected = 0
    for line in frames(frame_log):
        match = re.search(r"visible=Rect \{ x: (-?\d+), y: (-?\d+), "
                          r"width: (\d+), height: (\d+) \} content=Rect \{ "
                          r"x: (-?\d+), y: (-?\d+), width: (\d+), height: (\d+) \}", line)
        if match:
            inspected += 1
            geometry = tuple(map(int, match.groups()))
            if geometry[:4] != geometry[4:]:
                incomplete += 1
    if env["POC_ACCELERATED"] != "0" and inspected == 0:
        failures.append("no accelerated frame geometry logged; cannot check letterboxing")
    if incomplete:
        failures.append(f"published {incomplete} letterboxed intermediate frames")
    for failure in failures:
        print("FAIL:", failure)
    print("RESULT=" + ("failed" if failures else "ok"))
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())
