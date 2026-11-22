"""Real Win32 input regression: reopen a CEF menu without pressing Escape.

Uses only the window belonging to the process this script starts. The old bug
leaves CEF's first callback pending, so the second right click never opens a
new menu. Screenshots preserve what was actually drawn, not just event logs.
"""
import ctypes
import ctypes.wintypes as wt
import importlib.util
import os
import re
from pathlib import Path
import subprocess
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
import poc_paths
import time

from PIL import ImageGrab

spec = importlib.util.spec_from_file_location("os_input", Path(__file__).with_name("os-input-test.py"))
os_input = importlib.util.module_from_spec(spec)
spec.loader.exec_module(os_input)
u = os_input.user32
u.GetForegroundWindow.restype = wt.HWND


def wait_for(predicate, description, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = predicate()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError(f"timeout: {description}")


def process_window(pid):
    found = []

    @ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    def visit(hwnd, _):
        owner = wt.DWORD()
        u.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid and u.IsWindowVisible(hwnd):
            found.append(hwnd)
        return True

    u.EnumWindows(visit, 0)
    return found[0] if found else None


def main():
    # Match screenshot / cursor / client coordinates on a scaled desktop.
    u.SetProcessDPIAware()
    artifacts = Path("target/context-menu-test")
    artifacts.mkdir(parents=True, exist_ok=True)
    log_path = artifacts / "app.log"
    env = dict(os.environ, POC_SELF_TEST="2", POC_LOG_MENU="1", POC_EXIT_AFTER_MS="45000")
    with log_path.open("w", encoding="utf-8") as log:
        process = subprocess.Popen([str(poc_paths.bundle_dir('debug') / 'gpui-cef-poc.exe')], env=env,
                                   stdout=log, stderr=subprocess.STDOUT)
        try:
            hwnd = wait_for(lambda: process_window(process.pid), "test process window")
            u.ShowWindow(hwnd, 9)
            if not u.SetForegroundWindow(hwnd):
                # Windows can deny foreground activation to a background test
                # process. An Alt event releases that lock; verify before any
                # pointer injection instead of clicking whichever app is on top.
                os_input.send(os_input.key_vk(0x12), os_input.key_vk(0x12, up=True))
                u.SetForegroundWindow(hwnd)
            wait_for(lambda: u.GetForegroundWindow() == hwnd, "test window foreground")
            wait_for(lambda: "page title: probe" in log_path.read_text(encoding="utf-8", errors="replace"),
                     "interaction page ready")
            time.sleep(0.8)
            rect = wt.RECT()
            u.GetClientRect(hwnd, ctypes.byref(rect))
            origin = wt.POINT(0, 0)
            u.ClientToScreen(hwnd, ctypes.byref(origin))
            width, height = rect.right, rect.bottom

            def screenshot(name):
                image = ImageGrab.grab(bbox=(origin.x, origin.y, origin.x + width, origin.y + height)).convert("RGB")
                image.save(artifacts / f"{name}.png")
                return image

            def click(x, y, right=True):
                assert u.GetForegroundWindow() == hwnd, "another app took foreground; refusing to click it"
                u.SetCursorPos(origin.x + x, origin.y + y)
                time.sleep(0.12)
                down, up = (0x0008, 0x0010) if right else (0x0002, 0x0004)
                os_input.send(os_input.mouse(down))
                time.sleep(0.08)
                os_input.send(os_input.mouse(up))

            def opened():
                return sum(line.startswith("CONTEXT_MENU ") and
                           not line.startswith(("CONTEXT_MENU dismissed", "CONTEXT_MENU pick"))
                           for line in log_path.read_text(encoding="utf-8", errors="replace").splitlines())

            points = [(int(width * x), int(height * y)) for x, y in
                      ((0.30, 0.40), (0.73, 0.45), (0.46, 0.72))]
            baseline = screenshot("baseline")
            for index, (x, y) in enumerate(points, 1):
                # No Escape or left-click between these right-clicks.
                click(x, y)
                time.sleep(0.4)
                image = screenshot(f"right-{index}")
                wait_for(lambda: opened() == index, f"fresh CEF menu for right-click {index}", timeout=3)
                # These points leave room below/right: the menu should start at
                # the pointer. Check rendered pixels as well as the CEF callback.
                box = (x + 8, y + 3, x + 150, y + 38)
                pixels = list(image.crop(box).getdata())
                original = list(baseline.crop(box).getdata())
                changed = sum(a != b for a, b in zip(pixels, original)) / len(pixels)
                white = sum(min(pixel) >= 250 for pixel in pixels) / len(pixels)
                assert changed > 0.02 and white > 0.6, (
                    f"menu not drawn at click {index}: changed={changed:.3f}, white={white:.3f}")
                print(f"right-click {index}: new menu drawn at client ({x}, {y})", flush=True)

            # An outside left-click must release the callback, even after many
            # frames. This catches subscriptions left attached to dead entities.
            def dismissed():
                return log_path.read_text(encoding="utf-8", errors="replace").count("CONTEXT_MENU dismissed")

            dismiss_count = dismissed()
            click(int(width * 0.85), int(height * 0.8), right=False)
            wait_for(lambda: dismissed() == dismiss_count + 1, "outside-click dismiss")
            click(*points[0])
            wait_for(lambda: opened() == 4, "reopen after outside-click")
            dismiss_count = dismissed()
            os_input.send(os_input.key_vk(0x1B), os_input.key_vk(0x1B, up=True))
            wait_for(lambda: dismissed() == dismiss_count + 1, "Escape dismiss")
            click(*points[1])
            wait_for(lambda: opened() == 5, "reopen after Escape")
            screenshot("after-escape")

            # A retained entity must keep keyboard selection across frames and
            # return an actual command to CEF, not merely dismiss a drawing.
            dismiss_count = dismissed()
            os_input.send(os_input.key_vk(0x1B), os_input.key_vk(0x1B, up=True))
            wait_for(lambda: dismissed() == dismiss_count + 1, "dismiss before editing")
            scale = u.GetDpiForWindow(hwnd) / 96
            field = (round(132 * scale), round(160 * scale))
            click(*field, right=False)
            for char in "menu":
                os_input.send(os_input.key_unicode(char), os_input.key_unicode(char, up=True))
            wait_for(lambda: "input:menu" in log_path.read_text(encoding="utf-8", errors="replace"),
                     "typing before menu selection")
            click(*field)
            wait_for(lambda: opened() == 6, "editable menu")
            time.sleep(0.4)
            os_input.send(os_input.key_vk(0x26), os_input.key_vk(0x26, up=True))  # Up -> Select all
            time.sleep(0.3)
            screenshot("keyboard-selection")
            os_input.send(os_input.key_vk(0x0D), os_input.key_vk(0x0D, up=True))
            wait_for(lambda: "CONTEXT_MENU pick 117" in log_path.read_text(encoding="utf-8", errors="replace"),
                     "Select all command returned to CEF")
            time.sleep(0.3)
            os_input.send(os_input.key_unicode("z"), os_input.key_unicode("z", up=True))
            wait_for(lambda: re.search(r"input:z(?:[|\r\n]|$)", log_path.read_text(encoding="utf-8", errors="replace")),
                     "menu command selected all text: replacement must be z, not menuz")
            click(*points[1])
            wait_for(lambda: opened() == 7, "reopen after command")
            dismiss_count = dismissed()
            os_input.send(os_input.key_vk(0x1B), os_input.key_vk(0x1B, up=True))
            wait_for(lambda: dismissed() == dismiss_count + 1, "final dismiss")
            # Also exercise a physical menu-item click. The editable menu's
            # last row is Select all (the same row verified by keyboard above).
            click(*field, right=False)
            click(*field)
            wait_for(lambda: opened() == 8, "editable menu for pointer selection")
            time.sleep(0.4)
            click(field[0] + round(40 * scale), field[1] + round(202 * scale), right=False)
            wait_for(lambda: log_path.read_text(encoding="utf-8", errors="replace").count("CONTEXT_MENU pick 117") == 2,
                     "pointer Select all command returned to CEF")
            time.sleep(0.3)
            os_input.send(os_input.key_unicode("q"), os_input.key_unicode("q", up=True))
            wait_for(lambda: re.search(r"input:q(?:[|\r\n]|$)", log_path.read_text(encoding="utf-8", errors="replace")),
                     "pointer command selected all text")

            # Deactivation must release an open callback as well.
            click(*points[1])
            wait_for(lambda: opened() == 9, "menu before deactivation")
            dismiss_count = dismissed()
            u.ShowWindow(hwnd, 6)  # SW_MINIMIZE
            wait_for(lambda: dismissed() == dismiss_count + 1, "deactivation dismiss")
            u.ShowWindow(hwnd, 9)
            u.SetForegroundWindow(hwnd)
            wait_for(lambda: u.GetForegroundWindow() == hwnd, "restored test window foreground")
            time.sleep(0.4)
            click(*points[2])
            wait_for(lambda: opened() == 10, "reopen after deactivation")
            dismiss_count = dismissed()
            os_input.send(os_input.key_vk(0x1B), os_input.key_vk(0x1B, up=True))
            wait_for(lambda: dismissed() == dismiss_count + 1, "dismiss restored menu")

            text = log_path.read_text(encoding="utf-8", errors="replace")
            opens = re.findall(r"MENU_OPEN id=(\d+)", text)
            resolves = re.findall(r"MENU_RESOLVE id=(\d+)", text)
            assert len(opens) == 10 and sorted(opens) == sorted(resolves) and len(set(resolves)) == 10, (
                f"callbacks must complete exactly once: opened={opens}, resolved={resolves}")
            print("outside-click, Escape, keyboard/pointer commands, text replacement, deactivation: ok", flush=True)
            print("10 CEF callbacks completed exactly once", flush=True)
            print("CONTEXT_MENU_RESULT=ok", flush=True)
            return 0
        except AssertionError as error:
            print(f"CONTEXT_MENU_RESULT=FAIL: {error}", flush=True)
            print(f"Evidence: {artifacts.resolve()}", flush=True)
            return 1
        finally:
            # Close only this run's window; never terminate the user's instance.
            hwnd = process_window(process.pid)
            if hwnd:
                u.PostMessageW(hwnd, 0x0010, 0, 0)  # WM_CLOSE
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    sys.exit(main())
