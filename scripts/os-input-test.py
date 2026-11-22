"""Injects real OS-level input into the PoC window with Win32 SendInput.

This is an independent check of the GPUI -> CEF input path: the events enter
through the same WM_MOUSEMOVE / WM_KEYDOWN / WM_CHAR messages a human would
produce, bypassing the app's own scripting helpers.
"""

import ctypes
import ctypes.wintypes as wintypes
import os
import re
import subprocess
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
import poc_paths
import time

user32 = ctypes.WinDLL("user32", use_last_error=True)

INPUT_MOUSE = 0
INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_UNICODE = 0x0004
MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
MOUSEEVENTF_WHEEL = 0x0800


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG),
        ("dy", wintypes.LONG),
        ("mouseData", wintypes.DWORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong)),
    ]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", wintypes.WORD),
        ("wScan", wintypes.WORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong)),
    ]


class INPUT(ctypes.Structure):
    class _U(ctypes.Union):
        _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT)]

    _anonymous_ = ("u",)
    _fields_ = [("type", wintypes.DWORD), ("u", _U)]


def send(*inputs):
    array = (INPUT * len(inputs))(*inputs)
    sent = user32.SendInput(len(inputs), array, ctypes.sizeof(INPUT))
    if sent != len(inputs):
        raise ctypes.WinError(ctypes.get_last_error())


def mouse(flags, dx=0, dy=0, data=0):
    return INPUT(
        type=INPUT_MOUSE,
        u=INPUT._U(
            mi=MOUSEINPUT(dx, dy, data & 0xFFFFFFFF, flags, 0, None)
        ),
    )


def key_unicode(char, up=False):
    flags = KEYEVENTF_UNICODE | (KEYEVENTF_KEYUP if up else 0)
    return INPUT(
        type=INPUT_KEYBOARD,
        u=INPUT._U(ki=KEYBDINPUT(0, ord(char), flags, 0, None)),
    )


def caps_lock_on():
    return bool(user32.GetKeyState(0x14) & 1)


def key_vk(vk, up=False):
    flags = KEYEVENTF_KEYUP if up else 0
    return INPUT(type=INPUT_KEYBOARD, u=INPUT._U(ki=KEYBDINPUT(vk, 0, flags, 0, None)))


def find_window(title_hint):
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def callback(hwnd, _):
        length = user32.GetWindowTextLengthW(hwnd)
        if length:
            buffer = ctypes.create_unicode_buffer(length + 1)
            user32.GetWindowTextW(hwnd, buffer, length + 1)
            if title_hint in buffer.value:
                found.append((hwnd, buffer.value))
        return True

    user32.EnumWindows(callback, 0)
    return found


def main():
    exe = str(poc_paths.bundle_dir('debug') / 'gpui-cef-poc.exe')
    env = dict(os.environ, POC_SELF_TEST="2", POC_EXIT_AFTER_MS="16000")
    process = subprocess.Popen(
        [exe], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        encoding="utf-8", errors="replace",
    )

    time.sleep(3.5)
    windows = find_window("GPUI + CEF PoC")
    if not windows:
        process.kill()
        print("FAIL: window not found")
        return 1
    hwnd, title = windows[0]
    user32.ShowWindow(hwnd, 9)  # SW_RESTORE
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.6)

    # The window is centred on open, and the geometry settles a moment after it
    # becomes visible. Converting client coordinates before that yields a stale
    # origin and the click lands on the wrong page element, so wait for the
    # top-left to stop moving.
    def client_origin():
        point = wintypes.POINT(0, 0)
        user32.ClientToScreen(hwnd, ctypes.byref(point))
        return (point.x, point.y)

    previous = None
    for _ in range(40):
        current = client_origin()
        if current == previous:
            break
        previous = current
        time.sleep(0.1)

    rect = wintypes.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(rect))
    # Client coordinates of the browser surface: below the 48px header and the
    # 12px margin. Convert to screen space, which is what the OS cursor uses.
    # Inside #field: 20..640 x 20..240 in view DIP, and the surface starts at
    # (12, 60) in client space (12px margin, 48px header).
    client_x, client_y = 12 + 120, 60 + 100
    point = wintypes.POINT(client_x, client_y)
    user32.ClientToScreen(hwnd, ctypes.byref(point))
    print(f"client {rect.right}x{rect.bottom}, clicking at screen {point.x},{point.y}")
    print("caps lock on:", caps_lock_on())
    user32.SetCursorPos(point.x, point.y)
    time.sleep(0.4)
    send(mouse(MOUSEEVENTF_MOVE, 1, 1))
    time.sleep(0.3)
    send(mouse(MOUSEEVENTF_LEFTDOWN))
    time.sleep(0.05)
    send(mouse(MOUSEEVENTF_LEFTUP))
    time.sleep(0.4)

    # Wheel and real keystrokes through the OS keyboard path.
    send(mouse(MOUSEEVENTF_WHEEL, data=(0xFFFFFFFF - 119)))  # -120
    time.sleep(0.3)
    # Use real virtual keys rather than KEYEVENTF_UNICODE: that is the path a
    # physical keyboard takes (WM_KEYDOWN -> WM_CHAR), and it is what the app
    # must not double-handle.
    for vk in (0x58, 0x59):  # X, Y
        send(key_vk(vk))
        time.sleep(0.05)
        send(key_vk(vk, up=True))
        time.sleep(0.25)
    time.sleep(0.3)
    send(key_vk(0x08), key_vk(0x08, up=True))  # BACKSPACE
    time.sleep(0.4)

    try:
        # Drain while waiting: a full stdout pipe can block CEF's UI thread and
        # prevent its exit timer from running. wait() followed by read() cannot.
        output, _ = process.communicate(timeout=60)
    except subprocess.TimeoutExpired:
        process.kill()
        output, _ = process.communicate()
        print(output)
        print("FAIL: app did not exit")
        return 1

    # Prefer the file the app writes: console/pipe encoding can mangle CJK
    # and a killed process can lose buffered output.
    result_file = str(poc_paths.bundle_dir('debug') / 'self-test-result.txt')
    title = ""
    verdict = ""
    if os.path.exists(result_file):
        with open(result_file, encoding="utf-8") as handle:
            for line in handle:
                if line.startswith("title="):
                    title = line[len("title="):].strip()
                elif line.startswith("result="):
                    verdict = line[len("result="):].strip()
    else:
        title_match = re.search(r"SELF_TEST_TITLE=(.*)", output)
        result_match = re.search(r"SELF_TEST_RESULT=(.*)", output)
        title = title_match.group(1) if title_match else ""
        verdict = result_match.group(1) if result_match else ""

    print("page title:", title[:400])
    print("result:", verdict)
    if verdict != "ok":
        print("FAIL: app self-check did not pass")
        return 1
    # CapsLock may be on, which makes a plain X produce "X". Compare
    # case-insensitively so the check is about delivery, not keyboard state.
    if "input:x" not in title.lower():
        print("MISSING input:x (typing did not reach the page)")
        return 1
    # A single keypress must insert one character. Adjacent duplicates would
    # mean both KEYEVENT_CHAR and the IME path inserted text.
    lowered = title.lower()
    if "xx" in lowered or "yy" in lowered:
        print("DUPLICATE characters detected (double dispatch)")
        return 1
    print("OS_INPUT_RESULT=ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
