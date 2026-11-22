"""Verifies the two reported bugs against the real window.

1. Focus loss must not stop CEF rendering: blur the window and check that
   frames keep arriving.
2. Resizing must track the drag: resize in small steps and check that the
   viewport follows each step instead of jumping once at the end.

Counting is done by the app itself (`POC_FRAME_LOG=<path>` writes one line per
frame), so nothing depends on scraping a pipe that a crashed reader can block.
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
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)


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


def responsive(hwnd, timeout_ms=1500):
    result = ctypes.c_size_t(0)
    ok = user32.SendMessageTimeoutW(
        hwnd, 0, 0, 0, 0x0002, timeout_ms, ctypes.byref(result)
    )
    return bool(ok)


def read_frames(path):
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8", errors="replace") as handle:
        return [line.strip() for line in handle if line.strip()]


def main():
    exe = str(poc_paths.bundle_dir('debug') / 'gpui-cef-poc.exe')
    log_path = os.path.abspath("frame-log.txt")
    if os.path.exists(log_path):
        os.remove(log_path)

    env = dict(os.environ, POC_FRAME_LOG=log_path, POC_EXIT_AFTER_MS="22000")
    process = subprocess.Popen(
        [exe], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )

    time.sleep(4.5)
    windows = find_window("GPUI + CEF PoC")
    if not windows:
        process.kill()
        print("FAIL: window not found")
        return 1
    hwnd = windows[0]
    failures = []

    # --- 1. focus loss must keep rendering -------------------------------
    before = len(read_frames(log_path))
    # Hand activation to the desktop: the PoC window is no longer active.
    user32.SetForegroundWindow(user32.GetDesktopWindow())
    time.sleep(2.5)
    after = len(read_frames(log_path))
    blurred_frames = after - before
    print(f"frames while unfocused: {blurred_frames}")
    if blurred_frames < 10:
        failures.append("frames stopped when the window lost focus")

    # --- 2. resize must track the drag -----------------------------------
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.6)
    rect = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    start_w, start_h = rect.right - rect.left, rect.bottom - rect.top

    steps = 20
    resize_start = len(read_frames(log_path))
    sizes = []
    for i in range(1, steps + 1):
        user32.SetWindowPos(
            hwnd, None, rect.left, rect.top,
            start_w + (300 * i // steps), start_h + (200 * i // steps),
            0x0004 | 0x0010,
        )
        time.sleep(0.05)
        if not responsive(hwnd):
            failures.append(f"window stopped responding at resize step {i}")
            break
    time.sleep(1.2)
    resize_frames = read_frames(log_path)[resize_start:]
    distinct = []
    for line in resize_frames:
        parts = line.split()
        if len(parts) >= 2 and (not distinct or distinct[-1] != parts[1]):
            distinct.append(parts[1])
    print(f"frame sizes during/after resize: {distinct}")
    if len([d for d in distinct if d != distinct[0]]) < 3:
        failures.append("CEF did not repaint at intermediate sizes while resizing")

    try:
        process.wait(timeout=45)
    except subprocess.TimeoutExpired:
        process.kill()
        failures.append("app did not exit")

    if failures:
        for failure in failures:
            print("FAIL:", failure)
        return 1
    print("RESULT=ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
