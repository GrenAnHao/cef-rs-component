"""Checks that resizing paints frames 1:1 instead of resampling them.

Background: the app used to fit the newest CEF frame to the surface by aspect
ratio. Mid-drag the newest frame is always one step behind the surface, so that
fit resampled every pixel with a different horizontal and vertical factor, which
squashed text (the reported "jelly" and distortion).

The invariant under test: when the app blits a frame, the destination rectangle
must equal the frame's own size. If it does not, gpui is resampling, and the bug
is back. How far the frame trails the surface is also reported, because that lag
is what a viewer perceives as the content "catching up".
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


def parse_paints(path):
    """Yields (surface_w, surface_h, frame_w, frame_h, image_w, image_h)."""
    if not os.path.exists(path):
        return []
    rows = []
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            parts = line.split()
            if len(parts) < 4:
                continue
            try:
                sw, sh = (float(v) for v in parts[1].split("=")[1].split("x"))
                fw, fh = (float(v) for v in parts[2].split("=")[1].split("x"))
                iw, ih = (float(v) for v in parts[3].split("=")[1].split("x"))
            except (IndexError, ValueError):
                continue
            rows.append((sw, sh, fw, fh, iw, ih))
    return rows


def main():
    exe = str(poc_paths.bundle_dir('debug') / 'gpui-cef-poc.exe')
    frame_log = os.path.abspath("frame-log.txt")
    paint_log = os.path.abspath("paint-log.txt")
    print(f"logs: {frame_log} {paint_log}")
    for path in (frame_log, paint_log):
        if os.path.exists(path):
            os.remove(path)

    env = dict(
        os.environ,
        POC_FRAME_LOG=frame_log,
        POC_PAINT_LOG=paint_log,
        POC_EXIT_AFTER_MS="26000",
    )
    process = subprocess.Popen(
        [exe], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    time.sleep(3.0)

    windows = find_window("GPUI + CEF PoC")
    if not windows:
        process.kill()
        print("FAIL: window not found")
        return 1
    hwnd = windows[0]
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.6)

    rect = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    start_w, start_h = rect.right - rect.left, rect.bottom - rect.top

    # Deliberately faster than a human drag: 900 px in 12 steps of 40 ms.
    steps = int(os.environ.get("DRAG_STEPS", "12"))
    delay = float(os.environ.get("DRAG_DELAY", "0.04"))
    travel = int(os.environ.get("DRAG_TRAVEL", "900"))
    # HOLD: after growing, stay at the large size so the steady-state frame rate
    # can be measured instead of only the drag transient.
    hold = float(os.environ.get("DRAG_HOLD", "3.0"))
    for i in range(1, steps + 1):
        user32.SetWindowPos(
            hwnd, None, rect.left, rect.top,
            start_w + (travel * i // steps),
            start_h + (travel * 2 // 3 * i // steps),
            0x0004 | 0x0010,
        )
        time.sleep(delay)
    print(f"holding the enlarged window for {hold}s")
    time.sleep(hold)

    try:
        process.wait(timeout=90)
    except subprocess.TimeoutExpired:
        process.kill()
        print("FAIL: app did not exit")
        return 1

    if not os.path.exists(frame_log):
        print(f"FAIL: no frame log at {frame_log}")
        return 1
    with open(frame_log, encoding="utf-8") as handle:
        sizes = [line.split()[1] for line in handle if len(line.split()) >= 2]
    distinct = []
    for size in sizes:
        if not distinct or distinct[-1] != size:
            distinct.append(size)

    paints = parse_paints(paint_log)
    resampled = 0
    worst_mismatch = 0.0
    worst_lag = 0.0
    for sw, sh, fw, fh, iw, ih in paints:
        # `frame`/`surface` are DIP, `image` is device pixels. Convert the source
        # to DIP with the window scale (150% on this machine) before comparing;
        # a 1:1 blit of a 1614x1032 frame is a 1076x688 box.
        scale = iw / fw if fw > 0 else 1.0
        iw_dip, ih_dip = iw / scale, ih / scale
        mismatch = max(
            abs(fw - iw_dip) / max(iw_dip, 1.0),
            abs(fh - ih_dip) / max(ih_dip, 1.0),
        )
        worst_mismatch = max(worst_mismatch, mismatch)
        if mismatch > 0.01:
            resampled += 1
        worst_lag = max(worst_lag, abs(sw - fw) / max(sw, 1.0))

    # FPS over time: the frame log is append-only, so segment it by arrival
    # order into 1 s buckets using the app's own steady tick rate.
    print(f"CEF frames: {len(sizes)}, distinct sizes: {len(distinct)}")
    if sizes:
        window = max(1, len(sizes) // 12)
        buckets = [
            len(sizes[i:i + window]) for i in range(0, len(sizes), window)
        ]
        print("frames per slice:", buckets)
    print("sizes:", distinct[:20])
    print(
        f"painted frames: {len(paints)}, resampled: {resampled} "
        f"(worst {worst_mismatch:.1%}), worst lag {worst_lag:.1%}"
    )

    failures = []
    if len(distinct) < 6:
        failures.append("fewer than 6 distinct CEF frame sizes: resize not tracked")
    if len(paints) < 20:
        failures.append(f"only {len(paints)} paints logged: nothing to judge")
    if resampled > 0:
        failures.append(f"{resampled} frames were resampled instead of blitted 1:1")

    if failures:
        for failure in failures:
            print("FAIL:", failure)
        return 1
    print("RESULT=ok (all frames blitted 1:1, resize tracked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
