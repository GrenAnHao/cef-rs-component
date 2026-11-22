"""Drags the page's own draggable element with real OS input.

The in-page HTML5 drag only completes if the embedder runs the OLE drag loop
inside `RenderHandler::StartDragging`, so this is the only honest check for that
path: synthetic CEF input never reaches the OS, and `DoDragDrop` waits for a real
button release.
"""

import ctypes
import ctypes.wintypes as wintypes
import os
import subprocess
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
import poc_paths
import time

user32 = ctypes.WinDLL("user32", use_last_error=True)

MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG),
        ("dy", wintypes.LONG),
        ("mouseData", wintypes.DWORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong)),
    ]


class INPUT(ctypes.Structure):
    class _U(ctypes.Union):
        _fields_ = [("mi", MOUSEINPUT)]

    _anonymous_ = ("u",)
    _fields_ = [("type", wintypes.DWORD), ("u", _U)]


def send_mouse(flags):
    item = INPUT(type=0, u=INPUT._U(mi=MOUSEINPUT(0, 0, 0, flags, 0, None)))
    array = (INPUT * 1)(item)
    user32.SendInput(1, array, ctypes.sizeof(INPUT))


def find_window(title_hint):
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def callback(hwnd, _):
        length = user32.GetWindowTextLengthW(hwnd)
        if length:
            buffer = ctypes.create_unicode_buffer(length + 1)
            user32.GetWindowTextW(hwnd, buffer, length + 1)
            if title_hint in buffer.value:
                found.append(hwnd)
        return True

    user32.EnumWindows(callback, 0)
    return found


def main():
    exe = str(poc_paths.bundle_dir('debug') / 'gpui-cef-poc.exe')
    result_file = str(poc_paths.bundle_dir('debug') / 'self-test-result.txt')
    if os.path.exists(result_file):
        os.remove(result_file)

    env = dict(os.environ, POC_SELF_TEST="2", POC_EXIT_AFTER_MS="25000")
    log = open("html5-drag.log", "w", encoding="utf-8")
    process = subprocess.Popen(
        [exe], env=env, stdout=log, stderr=subprocess.STDOUT
    )
    time.sleep(4.0)

    windows = find_window("GPUI + CEF PoC")
    if not windows:
        process.kill()
        print("FAIL: window not found")
        return 1
    hwnd = windows[0]
    user32.ShowWindow(hwnd, 9)
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.8)

    def to_screen(x, y):
        point = wintypes.POINT(x, y)
        user32.ClientToScreen(hwnd, ctypes.byref(point))
        return point.x, point.y

    # The page puts #dragme at view x=820..1000, y=20..100, and #dropzone at
    # y=140..240. The surface starts below the 48px header plus a 12px margin.
    surface_x, surface_y = 12, 60
    sx, sy = to_screen(surface_x + 900, surface_y + 60)
    dx, dy = to_screen(surface_x + 900, surface_y + 190)
    print(f"drag {sx},{sy} -> {dx},{dy}")

    user32.SetCursorPos(sx, sy)
    time.sleep(0.4)
    send_mouse(MOUSEEVENTF_LEFTDOWN)
    time.sleep(0.35)
    for step in range(1, 13):
        user32.SetCursorPos(
            sx + (dx - sx) * step // 12,
            sy + (dy - sy) * step // 12,
        )
        time.sleep(0.09)
    time.sleep(0.35)
    send_mouse(MOUSEEVENTF_LEFTUP)
    time.sleep(1.5)

    try:
        process.wait(timeout=60)
    except subprocess.TimeoutExpired:
        process.kill()
        print("FAIL: app did not exit (drag loop still blocking?)")
        return 1

    log.close()
    if not os.path.exists(result_file):
        print("FAIL: no result file")
        return 1
    with open(result_file, encoding="utf-8") as handle:
        text = handle.read()
    print(text.strip()[:600])

    if "dragstart" not in text:
        print("FAIL: drag never started")
        return 1
    if "htmldrop:" not in text:
        print("NOTE: StartDragging is declined by default; see render_handler.rs")
        return 2
    print("HTML5_DRAG_RESULT=ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
