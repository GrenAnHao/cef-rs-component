#!/usr/bin/env python3
"""Owned-window presentation pixels for the 10.1 public GPUI CEF host.

This script observes the host's real on-screen client area. CEF OSR raw
frames are explicitly NOT treated as presentation, and PrintWindow/WM_PRINT is
not used: the pixels come from PIL.ImageGrab.grab(bbox=<owned client bbox>)
cropped to that client. Only the owned window's client crop is saved or
inspected; no other window's contents are interpreted.

Phases:

* ``first``: reads ``case-dir/view-metrics.json`` written by the host, requires
  a 320x240 DIP canvas at the declared surface bounds, samples a 3x3 grid
  inside those bounds (expected solid RGB(26,76,126)) and the header point
  (20 DIP from the client's right edge, y=24 DIP; the label text is
  left-aligned so the right edge is bare red) expected RGB(159,34,34), and
  verifies the real frame physical size against DIP * scale_factor.
* ``detached``: requires the passing ``window-first.json`` from the same case
  directory, reuses the same metrics (same window/position/scale, all
  re-checked), samples the same region (expected opaque green RGB(0,170,68))
  and the header (red). This proves the host window is still alive and the old
  page region is no longer painted; it does not prove paint counters stopped.

Machine contract for scripts/test-component-host.ps1: the top-level JSON
fields ``success: bool``, ``pid: int`` and ``hwnd: int`` are always present.
``hwnd`` is 0 when no owned window was identified, and ``success`` is then
false with ``error`` set. Nothing is ever reported as passing on a partial run.

Safety and proof boundaries (also written into the JSON):

* the only Win32 writes are SetThreadDpiAwarenessContext; no input injection,
  no cursor movement, no foreground activation, no window movement/resize;
* occlusion is sampled with WindowFromPoint + GetAncestor(GA_ROOT) at a 3x3
  grid over the whole client and at every color point, immediately before and
  immediately after the grab; any foreign root fails closed;
* probes and the grab are separate point-in-time operations, so this is not an
  atomic protection guarantee; an occluder can appear between them;
* color is compared as exact integer RGB with no tolerance; screenshot
  alpha/RGBA is not validated; no OS multi-DPI-window or IME coverage is
  claimed.

Failure leaves ``window-<phase>.json`` with a non-null ``error`` and exits 1.
There is no retry that could hide a mismatch.
"""

from __future__ import annotations

import argparse
import ctypes
import ctypes.wintypes as wt
import hashlib
import json
import math
import os
import struct
import sys
import traceback
from datetime import datetime, timezone
from pathlib import Path

from PIL import ImageGrab

TITLE = "GPUI-CEF public host"
PAGE_RGB = (26, 76, 126)
DETACHED_RGB = (0, 170, 68)
HEADER_RGB = (159, 34, 34)
HEADER_SAMPLE_DIP = (660.0, 24.0)  # 20 DIP from the client's right edge, bare red (label text is left-aligned)
HEADER_HEIGHT_DIP = 48.0
CANVAS_DIP = (320.0, 240.0)
HOST_DIP = (680.0, 440.0)
FRAME_FRACTIONS = (0.25, 0.5, 0.75)
CLIENT_GRID_FRACTIONS = (1.0 / 6.0, 0.5, 5.0 / 6.0)

GA_ROOT = 2
SM_CXSCREEN = 0
SM_CYSCREEN = 1
SM_XVIRTUALSCREEN = 76
SM_YVIRTUALSCREEN = 77
SM_CXVIRTUALSCREEN = 78
SM_CYVIRTUALSCREEN = 79
DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4

WNDENUMPROC = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)


class CaptureError(RuntimeError):
    pass


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def pointer_value(value) -> int:
    """Normalize a pointer/handle-shaped value without truncating 64-bit values."""
    if value is None:
        return 0
    if isinstance(value, int):
        return value
    try:
        return int(value.value or 0)
    except AttributeError:
        return int(value)


def signed_hex(value: int) -> str:
    return "0x%016x" % (value & ((1 << 64) - 1))


def add_check(checks, name: str, passed: bool, detail) -> bool:
    checks.append({"name": name, "passed": bool(passed), "detail": detail})
    return bool(passed)


def load_win32():
    if sys.platform != "win32":
        raise CaptureError("this evidence script supports Windows only")
    user32 = ctypes.WinDLL("user32", use_last_error=True)

    required = (
        "EnumWindows",
        "IsWindow",
        "IsWindowVisible",
        "IsIconic",
        "GetWindowTextLengthW",
        "GetWindowTextW",
        "GetWindowThreadProcessId",
        "GetClientRect",
        "ClientToScreen",
        "GetDpiForWindow",
        "GetForegroundWindow",
        "WindowFromPoint",
        "GetAncestor",
        "GetSystemMetrics",
        "SetThreadDpiAwarenessContext",
        "AreDpiAwarenessContextsEqual",
    )
    for name in required:
        if not hasattr(user32, name):
            raise CaptureError(f"user32!{name} is unavailable")

    user32.EnumWindows.argtypes = [WNDENUMPROC, wt.LPARAM]
    user32.EnumWindows.restype = wt.BOOL
    user32.IsWindow.argtypes = [wt.HWND]
    user32.IsWindow.restype = wt.BOOL
    user32.IsWindowVisible.argtypes = [wt.HWND]
    user32.IsWindowVisible.restype = wt.BOOL
    user32.IsIconic.argtypes = [wt.HWND]
    user32.IsIconic.restype = wt.BOOL
    user32.GetWindowTextLengthW.argtypes = [wt.HWND]
    user32.GetWindowTextLengthW.restype = ctypes.c_int
    user32.GetWindowTextW.argtypes = [wt.HWND, wt.LPWSTR, ctypes.c_int]
    user32.GetWindowTextW.restype = ctypes.c_int
    user32.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
    user32.GetWindowThreadProcessId.restype = wt.DWORD
    user32.GetClientRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
    user32.GetClientRect.restype = wt.BOOL
    user32.ClientToScreen.argtypes = [wt.HWND, ctypes.POINTER(wt.POINT)]
    user32.ClientToScreen.restype = wt.BOOL
    user32.GetDpiForWindow.argtypes = [wt.HWND]
    user32.GetDpiForWindow.restype = wt.UINT
    user32.GetForegroundWindow.argtypes = []
    user32.GetForegroundWindow.restype = wt.HWND
    user32.WindowFromPoint.argtypes = [wt.POINT]
    user32.WindowFromPoint.restype = wt.HWND
    user32.GetAncestor.argtypes = [wt.HWND, wt.UINT]
    user32.GetAncestor.restype = wt.HWND
    user32.GetSystemMetrics.argtypes = [ctypes.c_int]
    user32.GetSystemMetrics.restype = ctypes.c_int
    user32.SetThreadDpiAwarenessContext.argtypes = [ctypes.c_void_p]
    user32.SetThreadDpiAwarenessContext.restype = ctypes.c_void_p
    user32.AreDpiAwarenessContextsEqual.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    user32.AreDpiAwarenessContextsEqual.restype = wt.BOOL
    return user32


def set_thread_dpi_awareness(user32):
    # DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 is the pseudo-handle -4; pass it
    # as a signed pointer so the 64-bit value is a full 0xFFFFFFFFFFFFFFFC, not
    # a truncated 32-bit constant.
    previous = user32.SetThreadDpiAwarenessContext(ctypes.c_void_p(-4))
    previous_value = pointer_value(previous)
    if previous_value == 0:
        error = ctypes.get_last_error()
        raise CaptureError(
            "SetThreadDpiAwarenessContext(PER_MONITOR_AWARE_V2) failed "
            f"(GetLastError={error})"
        )
    current = user32.SetThreadDpiAwarenessContext(ctypes.c_void_p(-4))
    current_value = pointer_value(current)
    # Context handles are opaque; Win32 requires semantic equality, not bits.
    if not user32.AreDpiAwarenessContextsEqual(current, ctypes.c_void_p(-4)):
        raise CaptureError(
            "SetThreadDpiAwarenessContext did not report the requested context as "
            f"the active previous context (got {signed_hex(current_value)})"
        )
    return {
        "requested": "DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2",
        "requested_value": signed_hex(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2),
        "previous_hex": signed_hex(previous_value),
        "confirmed_hex": signed_hex(current_value),
    }


def enum_visible_top_level_windows(user32, pid: int):
    found = []
    failures = []

    @WNDENUMPROC
    def visit(hwnd, _lparam):
        handle = pointer_value(hwnd)
        if not handle:
            failures.append("EnumWindows yielded a null HWND")
            return True
        if not user32.IsWindow(handle):
            # The window was destroyed between enumeration and inspection. It
            # is not ours to reason about; skipping cannot hide an owned
            # window that still exists.
            return True
        if not user32.IsWindowVisible(handle):
            return True
        owner = wt.DWORD()
        if not user32.GetWindowThreadProcessId(handle, ctypes.byref(owner)):
            failures.append(
                f"GetWindowThreadProcessId failed for HWND {hex(handle)} "
                f"(GetLastError={ctypes.get_last_error()})"
            )
            return True
        if int(owner.value) != pid:
            return True
        # EnumWindows already yields top-level windows; require the root
        # identity too so an intermediate wrapper cannot be accepted.
        if pointer_value(user32.GetAncestor(handle, GA_ROOT)) != handle:
            failures.append(f"HWND {hex(handle)} is not its own GA_ROOT")
            return True
        found.append(handle)
        return True

    if not user32.EnumWindows(visit, 0):
        raise CaptureError(f"EnumWindows failed (GetLastError={ctypes.get_last_error()})")
    if failures:
        raise CaptureError("; ".join(failures))
    return found


def read_window_title(user32, hwnd: int) -> str:
    length = int(user32.GetWindowTextLengthW(hwnd))
    if length < 0:
        raise CaptureError(
            f"GetWindowTextLengthW failed for HWND {hex(hwnd)} "
            f"(GetLastError={ctypes.get_last_error()})"
        )
    capacity = length + 1
    buffer = ctypes.create_unicode_buffer(capacity)
    copied = int(user32.GetWindowTextW(hwnd, buffer, capacity))
    if copied < 0:
        raise CaptureError(
            f"GetWindowTextW failed for HWND {hex(hwnd)} "
            f"(GetLastError={ctypes.get_last_error()})"
        )
    if copied >= capacity:
        raise CaptureError(f"window title changed while reading HWND {hex(hwnd)}")
    return buffer.value


def inspect_window(user32, hwnd: int):
    handle = int(hwnd)
    if not user32.IsWindow(handle):
        raise CaptureError(f"HWND {hex(handle)} is no longer a window")

    owner = wt.DWORD()
    if not user32.GetWindowThreadProcessId(handle, ctypes.byref(owner)):
        raise CaptureError(
            f"GetWindowThreadProcessId failed for HWND {hex(handle)} "
            f"(GetLastError={ctypes.get_last_error()})"
        )

    title = read_window_title(user32, handle)
    visible = bool(user32.IsWindowVisible(handle))
    iconic = bool(user32.IsIconic(handle))

    rect = wt.RECT()
    if not user32.GetClientRect(handle, ctypes.byref(rect)):
        raise CaptureError(
            f"GetClientRect failed for HWND {hex(handle)} "
            f"(GetLastError={ctypes.get_last_error()})"
        )
    width = int(rect.right - rect.left)
    height = int(rect.bottom - rect.top)
    if width <= 0 or height <= 0:
        raise CaptureError(f"HWND {hex(handle)} has an empty client area ({width}x{height})")

    origin = wt.POINT(0, 0)
    if not user32.ClientToScreen(handle, ctypes.byref(origin)):
        raise CaptureError(
            f"ClientToScreen failed for HWND {hex(handle)} "
            f"(GetLastError={ctypes.get_last_error()})"
        )
    left = int(origin.x)
    top = int(origin.y)

    dpi = int(user32.GetDpiForWindow(handle))
    if dpi <= 0:
        raise CaptureError(f"GetDpiForWindow returned 0 for HWND {hex(handle)}")

    return {
        "hwnd": handle,
        "hwnd_hex": hex(handle),
        "owner_pid": int(owner.value),
        "title": title,
        "visible": visible,
        "iconic": iconic,
        "dpi": dpi,
        "dpi_scale": dpi / 96.0,
        "client_left": left,
        "client_top": top,
        "client_width": width,
        "client_height": height,
        "client_right": left + width,
        "client_bottom": top + height,
        "client_physical": {
            "left": left,
            "top": top,
            "right": left + width,
            "bottom": top + height,
            "width": width,
            "height": height,
        },
    }


def virtual_screen(user32):
    left = int(user32.GetSystemMetrics(SM_XVIRTUALSCREEN))
    top = int(user32.GetSystemMetrics(SM_YVIRTUALSCREEN))
    width = int(user32.GetSystemMetrics(SM_CXVIRTUALSCREEN))
    height = int(user32.GetSystemMetrics(SM_CYVIRTUALSCREEN))
    if width <= 0 or height <= 0:
        raise CaptureError(f"invalid virtual screen metrics {left},{top},{width},{height}")
    return {"left": left, "top": top, "width": width, "height": height}


def primary_screen(user32):
    width = int(user32.GetSystemMetrics(SM_CXSCREEN))
    height = int(user32.GetSystemMetrics(SM_CYSCREEN))
    if width <= 0 or height <= 0:
        raise CaptureError(f"invalid primary screen metrics {width}x{height}")
    return {"left": 0, "top": 0, "width": width, "height": height}


def foreground_evidence(user32, hwnd: int, pid: int):
    foreground = pointer_value(user32.GetForegroundWindow())
    foreground_pid = None
    if foreground:
        owner = wt.DWORD()
        if user32.GetWindowThreadProcessId(foreground, ctypes.byref(owner)):
            foreground_pid = int(owner.value)
    return {
        "hwnd": foreground if foreground else None,
        "hwnd_hex": hex(foreground) if foreground else None,
        "pid": foreground_pid,
        "is_owned_window": bool(foreground) and foreground == hwnd,
        "is_owned_process": foreground_pid == pid,
    }


def run_probes(user32, hwnd: int, state, specs):
    results = []
    for spec in specs:
        client_x, client_y = spec["client_px"]
        screen_x = state["client_left"] + client_x
        screen_y = state["client_top"] + client_y
        found = pointer_value(user32.WindowFromPoint(wt.POINT(screen_x, screen_y)))
        root = pointer_value(user32.GetAncestor(found, GA_ROOT)) if found else 0
        results.append(
            {
                "name": spec["name"],
                "client_px": [client_x, client_y],
                "screen_px": [screen_x, screen_y],
                "window_from_point": found,
                "window_from_point_hex": hex(found) if found else None,
                "root": root,
                "root_hex": hex(root) if root else None,
                "owned": bool(found) and root == hwnd,
            }
        )
    return results


def occlusion_detail(probes):
    occluded = [probe for probe in probes if not probe["owned"]]
    return {
        "probe_count": len(probes),
        "occluded_count": len(occluded),
        "occluded": occluded[:12],
    }


def round_half_up(value: float) -> int:
    return int(math.floor(value + 0.5))


def dip_to_client_px(dip_x: float, dip_y: float, scale: float):
    return (round_half_up(dip_x * scale), round_half_up(dip_y * scale))


def device_candidates(dip: float, scale: float):
    """CEF target identity: f32 multiply, then ceil, as audited in task 9.3."""
    def f32(value):
        return struct.unpack("<f", struct.pack("<f", value))[0]
    return [math.ceil(f32(f32(dip) * f32(scale)))]


def load_metrics(path: Path):
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise CaptureError(f"cannot read {path}: {exc}")
    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        raise CaptureError(f"invalid JSON in {path}: {exc}")
    if not isinstance(data, dict):
        raise CaptureError(f"{path} is not a JSON object")

    def number_field(key: str) -> float:
        if key not in data:
            raise CaptureError(f"{path} is missing {key}")
        value = data[key]
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise CaptureError(f"{path} field {key} is not a number")
        value = float(value)
        if not math.isfinite(value):
            raise CaptureError(f"{path} field {key} is not finite")
        return value

    def integer_field(key: str, minimum: int) -> int:
        if key not in data:
            raise CaptureError(f"{path} is missing {key}")
        value = data[key]
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise CaptureError(f"{path} field {key} is not an integer")
        if isinstance(value, float):
            if not math.isfinite(value) or not value.is_integer():
                raise CaptureError(f"{path} field {key} is not an integer")
            value = int(value)
        if value < minimum:
            raise CaptureError(f"{path} field {key} is below {minimum}")
        return int(value)

    bounds = data.get("surface_bounds")
    if not isinstance(bounds, list) or len(bounds) != 4:
        raise CaptureError(f"{path} field surface_bounds is not [x,y,w,h]")
    for index, item in enumerate(bounds):
        if isinstance(item, bool) or not isinstance(item, (int, float)):
            raise CaptureError(f"{path} field surface_bounds[{index}] is not a number")
        if not math.isfinite(float(item)):
            raise CaptureError(f"{path} field surface_bounds[{index}] is not finite")
    sx, sy, sw, sh = (float(item) for item in bounds)

    return {
        "path": str(path),
        "surface_bounds_dip": [sx, sy, sw, sh],
        "scale_factor": number_field("scale_factor"),
        "frame_width": integer_field("frame_width", 1),
        "frame_height": integer_field("frame_height", 1),
        "paint_calls": integer_field("paint_calls", 0),
        "painted_sequence": integer_field("painted_sequence", 0),
    }


def save_png_atomic(image, path: Path):
    temporary = path.with_name(path.name + ".tmp")
    try:
        image.save(temporary, format="PNG")
        os.replace(temporary, path)
    except Exception as exc:
        try:
            temporary.unlink()
        except OSError:
            pass
        raise CaptureError(f"cannot write {path}: {exc}") from exc


def write_json_atomic(path: Path, record):
    temporary = path.with_name(path.name + ".tmp")
    text = json.dumps(record, indent=2, ensure_ascii=False) + "\n"
    try:
        temporary.write_text(text, encoding="utf-8")
        os.replace(temporary, path)
    except (OSError, TypeError, ValueError):
        try:
            temporary.unlink()
        except OSError:
            pass
        raise


def sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class StrictParser(argparse.ArgumentParser):
    def error(self, message):
        self.print_usage(sys.stderr)
        print(f"capture-component-host: {message}", file=sys.stderr)
        raise SystemExit(1)


def best_effort_error_json(argv, message: str):
    """Argument errors still leave evidence when a phase/case-dir is readable.

    The runner treats a missing JSON as an infrastructure error; a JSON with a
    clear error is easier to diagnose. This never invents a passing result.
    """
    fields = {}
    for index, token in enumerate(argv):
        if token in ("--case-dir", "--phase", "--pid") and index + 1 < len(argv):
            fields.setdefault(token, argv[index + 1])
        elif token.startswith("--case-dir="):
            fields.setdefault("--case-dir", token.split("=", 1)[1])
        elif token.startswith("--phase="):
            fields.setdefault("--phase", token.split("=", 1)[1])
        elif token.startswith("--pid="):
            fields.setdefault("--pid", token.split("=", 1)[1])
    phase = fields.get("--phase")
    if phase not in ("first", "detached") or "--case-dir" not in fields:
        return
    try:
        case_dir = Path(fields["--case-dir"])
        case_dir.mkdir(parents=True, exist_ok=True)
        pid_value = fields.get("--pid", "")
        try:
            pid = int(pid_value)
        except (TypeError, ValueError):
            pid = 0
        write_json_atomic(
            case_dir / f"window-{phase}.json",
            {
                "schema": "1",
                "kind": "component-host-owned-window-pixels",
                "phase": phase,
                "pid": pid,
                "hwnd": 0,
                "error": message,
                "all_passed": False,
                "success": False,
                "finished_utc": utc_now(),
            },
        )
    except (OSError, TypeError, ValueError):
        pass


def parse_args(argv):
    parser = StrictParser(
        description="Sample the owned GPUI-CEF host window's actual client pixels",
        allow_abbrev=False,
    )
    parser.add_argument("--pid", type=int, required=True, help="owning host process id")
    parser.add_argument("--case-dir", type=Path, required=True, help="artifact directory")
    parser.add_argument("--phase", choices=("first", "detached"), required=True)
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        best_effort_error_json(argv, f"invalid command line (exit {exc.code})")
        raise
    if args.pid <= 0:
        message = "--pid must be a positive process id"
        best_effort_error_json(argv, message)
        parser.error(message)
    return args


def run(args) -> int:
    case_dir: Path = args.case_dir
    phase: str = args.phase
    try:
        case_dir.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        print(f"capture-component-host: cannot create {case_dir}: {exc}", file=sys.stderr)
        return 1

    json_path = case_dir / f"window-{phase}.json"
    png_path = case_dir / f"window-{phase}.png"
    unlink_errors = []
    for stale in (png_path, json_path):
        try:
            stale.unlink()
        except FileNotFoundError:
            pass
        except OSError as exc:
            unlink_errors.append(f"cannot remove stale {stale}: {exc}")
    if unlink_errors:
        for message in unlink_errors:
            print(f"capture-component-host: {message}", file=sys.stderr)
        try:
            write_json_atomic(
                json_path,
                {
                    "schema": "1",
                    "kind": "component-host-owned-window-pixels",
                    "phase": phase,
                    "pid": args.pid,
                    "hwnd": 0,
                    "error": "; ".join(unlink_errors),
                    "all_passed": False,
                    "success": False,
                    "finished_utc": utc_now(),
                },
            )
        except (OSError, TypeError, ValueError) as exc:
            print(f"capture-component-host: cannot write {json_path}: {exc}", file=sys.stderr)
        return 1

    checks = []
    record = {
        "schema": "1",
        "kind": "component-host-owned-window-pixels",
        "phase": phase,
        "pid": args.pid,
        "hwnd": 0,
        "hwnd_meaning": "0 means no owned window was identified in this run; see error",
        "expected_title": TITLE,
        "case_dir": str(case_dir),
        "started_utc": utc_now(),
        "finished_utc": None,
        "error": None,
        "source": {
            "presentation": "actual desktop pixels of the owned HWND client area",
            "osr_raw_used": False,
            "print_window_used": False,
            "input_injection": False,
            "cursor_moved": False,
            "foreground_hijack": False,
            "window_moved": False,
            "window_resized": False,
        },
        "declared": {
            "host_client_dip": list(HOST_DIP),
            "canvas_dip": list(CANVAS_DIP),
            "header_height_dip": HEADER_HEIGHT_DIP,
            "header_sample_dip": list(HEADER_SAMPLE_DIP),
            "header_sample_rule": "20 DIP in from the client's right edge, y=24 DIP, bare red beside the left-aligned label",
            "frame_grid_fractions": list(FRAME_FRACTIONS),
            "first_frame_rgb": list(PAGE_RGB),
            "detached_region_rgb": list(DETACHED_RGB),
            "header_rgb": list(HEADER_RGB),
            "color_tolerance": "none; exact integer RGB equality per point",
        },
        "phase_semantics": {
            "first": (
                "the visible client really presents the embedded CEF page in the "
                "declared 320x240 DIP frame, next to the unchanged host header"
            ),
            "detached": (
                "after the BrowserView entity is removed the host window is still "
                "alive and repaints the same region opaque green under the same "
                "red header; the removed page is no longer presented"
            ),
            "not_claimed": (
                "paint counters after detach, unsampled client pixels, screenshot "
                "alpha, OS multi-DPI-window behavior, IME"
            ),
        },
        "thread_dpi_awareness": None,
        "virtual_screen": None,
        "primary_screen": None,
        "metrics": None,
        "scale_factor": None,
        "owned_windows_for_pid": None,
        "owned_windows_after_capture": None,
        "owned_window": None,
        "owned_window_handle": None,
        "owned_window_pid": None,
        "owned_window_after_capture": None,
        "client_physical_geometry": None,
        "detached_comparison": None,
        "capture": {
            "method": (
                "PIL.ImageGrab.grab(bbox=owned client physical bbox, "
                "all_screens=<only when the owned client is off the primary "
                "monitor>); Pillow internally reads the monitor/virtual-desktop "
                "surface and returns just the bbox crop"
            ),
            "bbox_physical": None,
            "include_layered_windows": False,
            "image_path": str(png_path),
            "image_size": None,
            "image_mode": None,
            "png_sha256": None,
            "alpha_checked": False,
            "print_window_used": False,
            "osr_raw_used": False,
            "note": (
                "PIL.ImageGrab.grab(bbox=...) reads the desktop surface and crops "
                "to the owned client bbox; only that client crop is saved or "
                "inspected. No full-screen image is written, and no other "
                "window's pixels are interpreted."
            ),
        },
        "foreground": {
            "before": None,
            "after": None,
            "forced": False,
            "note": "foreground was recorded, never requested; overlap is rejected by probes",
        },
        "occlusion": {
            "client_grid_fractions": list(CLIENT_GRID_FRACTIONS),
            "probes_before": [],
            "probes_after": [],
            "all_owned_before": None,
            "all_owned_after": None,
        },
        "points": [],
        "checks": checks,
        "failed_checks": [],
        "all_passed": False,
        "success": False,
        "limitations": {
            "atomicity": (
                "WindowFromPoint probes, state snapshots and the pixel grab are "
                "separate point-in-time operations; an occluder can appear after "
                "a probe and before or during the grab. This is not an atomic "
                "isolation guarantee."
            ),
            "occlusion_coverage": (
                "Coverage is the 3x3 client grid plus every color sample point; "
                "unprobed client pixels are not occlusion-checked."
            ),
            "window_from_point": (
                "Occluder detection uses WindowFromPoint, which ignores hidden, "
                "disabled and WS_EX_TRANSPARENT windows; a click-through layered "
                "overlay would not be detected. ImageGrab is called without "
                "include_layered_windows (no CAPTUREBLT), so such an overlay may "
                "also be absent from the grab."
            ),
            "color": "RGB only, exact per point; screenshot alpha/RGBA is not validated.",
            "presentation": (
                "The PNG is a cropped desktop screen grab, not a PrintWindow "
                "re-render and not CEF OSR raw output."
            ),
            "dpi": (
                "One owned window at its current monitor DPI only. No OS "
                "multi-DPI-window, mixed-monitor scaling, or cross-monitor claim."
            ),
            "ime": "IME composition and candidate placement are not exercised.",
            "screen_grab_internals": (
                "PIL.ImageGrab internally captures the monitor (or virtual "
                "desktop when the owned client is not on the primary monitor). "
                "The full surface is never persisted, returned to the caller "
                "by this script, or pixel-inspected; only the owned client "
                "crop is saved and sampled."
            ),
            "detached": (
                "detached proves the host client still renders green in the old "
                "view region plus the red header; it does not prove paint "
                "counters stopped or that no unsampled pixels changed."
            ),
        },
    }

    try:
        user32 = load_win32()
        awareness = set_thread_dpi_awareness(user32)
        record["thread_dpi_awareness"] = awareness
        add_check(
            checks,
            "thread_dpi_awareness_per_monitor_v2",
            True,
            awareness,
        )

        metrics_path = case_dir / "view-metrics.json"
        try:
            metrics = load_metrics(metrics_path)
        except CaptureError as exc:
            add_check(checks, "metrics_file_valid", False, str(exc))
            raise
        record["metrics"] = metrics
        add_check(checks, "metrics_file_valid", True, str(metrics_path))

        sx, sy, sw, sh = metrics["surface_bounds_dip"]
        scale = metrics["scale_factor"]
        record["scale_factor"] = scale

        if not add_check(
            checks,
            "metrics_scale_factor_positive",
            scale > 0.0,
            {"scale_factor": scale},
        ):
            raise CaptureError(f"view-metrics.json scale_factor must be positive, got {scale}")

        if not add_check(
            checks,
            "metrics_surface_bounds_valid",
            sw > 0.0 and sh > 0.0 and sx >= 0.0 and sy >= 0.0,
            {"surface_bounds_dip": [sx, sy, sw, sh]},
        ):
            raise CaptureError(
                f"view-metrics.json surface_bounds is not a positive client-DIP rectangle: "
                f"{[sx, sy, sw, sh]}"
            )

        if not add_check(
            checks,
            "metrics_canvas_size_320x240_dip",
            abs(sw - CANVAS_DIP[0]) <= 1e-4 and abs(sh - CANVAS_DIP[1]) <= 1e-4,
            {"expected_dip": list(CANVAS_DIP), "actual_dip": [sw, sh], "tolerance_dip": 1e-4},
        ):
            raise CaptureError(f"view-metrics.json canvas is not 320x240 DIP: {sw}x{sh}")

        if not add_check(
            checks,
            "metrics_surface_bounds_inside_host_680x440_dip",
            sx + sw <= HOST_DIP[0] + 1e-4 and sy + sh <= HOST_DIP[1] + 1e-4,
            {
                "host_client_dip": list(HOST_DIP),
                "surface_bounds_dip": [sx, sy, sw, sh],
            },
        ):
            raise CaptureError("view-metrics.json surface_bounds lies outside the 680x440 host client")

        if phase == "first":
            if not add_check(
                checks,
                "metrics_paint_calls_positive",
                metrics["paint_calls"] > 0,
                {"paint_calls": metrics["paint_calls"]},
            ):
                raise CaptureError("view-metrics.json paint_calls is not positive")
            if not add_check(
                checks,
                "metrics_painted_sequence_positive",
                metrics["painted_sequence"] > 0,
                {"painted_sequence": metrics["painted_sequence"]},
            ):
                raise CaptureError("view-metrics.json painted_sequence is not positive")

        windows = enum_visible_top_level_windows(user32, args.pid)
        record["owned_windows_for_pid"] = {
            "count": len(windows),
            "handles_hex": [hex(handle) for handle in windows],
        }
        if not add_check(
            checks,
            "exactly_one_visible_top_level_window_for_pid",
            len(windows) == 1,
            {
                "pid": args.pid,
                "count": len(windows),
                "handles_hex": [hex(handle) for handle in windows],
            },
        ):
            raise CaptureError(
                f"pid {args.pid} must own exactly one visible top-level window, "
                f"found {len(windows)}"
            )

        hwnd = windows[0]
        state = inspect_window(user32, hwnd)
        record["hwnd"] = hwnd
        record["owned_window"] = state
        record["owned_window_handle"] = state["hwnd"]
        record["owned_window_pid"] = state["owner_pid"]
        dpi_scale = state["dpi"] / 96.0
        record["client_physical_geometry"] = {
            "hwnd": state["hwnd_hex"],
            "pid": state["owner_pid"],
            "bbox_physical": [
                state["client_left"],
                state["client_top"],
                state["client_right"],
                state["client_bottom"],
            ],
            "width": state["client_width"],
            "height": state["client_height"],
            "client_dip": [state["client_width"] / scale, state["client_height"] / scale],
            "scale_factor": scale,
            "dpi": state["dpi"],
            "dpi_scale": dpi_scale,
        }

        if not add_check(
            checks,
            "window_belongs_to_pid",
            state["owner_pid"] == args.pid,
            {
                "expected_pid": args.pid,
                "hwnd": state["hwnd_hex"],
                "actual_pid": state["owner_pid"],
            },
        ):
            raise CaptureError(f"HWND {state['hwnd_hex']} does not belong to pid {args.pid}")

        if not add_check(
            checks,
            "window_title_exact",
            state["title"] == TITLE,
            {
                "expected_title": TITLE,
                "actual_title": state["title"],
                "hwnd": state["hwnd_hex"],
            },
        ):
            raise CaptureError(
                f"owned window title mismatch: {state['title']!r} != {TITLE!r}"
            )

        if not add_check(
            checks,
            "window_visible_not_minimized",
            state["visible"] and not state["iconic"],
            {
                "visible": state["visible"],
                "iconic": state["iconic"],
                "hwnd": state["hwnd_hex"],
            },
        ):
            raise CaptureError("owned window must be visible and not minimized")

        accepted_client_w = [round_half_up(HOST_DIP[0] * scale)]
        accepted_client_h = [round_half_up(HOST_DIP[1] * scale)]
        # Win32 non-client frame rounding can move the realized client by one
        # physical pixel from round(DIP*scale) at fractional scaling. Samples
        # are placed from the realized client rect, so that pixel can never
        # shift a sample; a larger difference is a real failure.
        width_deviation = state["client_width"] - accepted_client_w[0]
        height_deviation = state["client_height"] - accepted_client_h[0]
        if not add_check(
            checks,
            "client_physical_size_from_host_680x440_dip",
            abs(width_deviation) <= 1 and abs(height_deviation) <= 1,
            {
                "actual_physical": [state["client_width"], state["client_height"]],
                "scale_factor": scale,
                "nominal_physical_width": accepted_client_w[0],
                "nominal_physical_height": accepted_client_h[0],
                "deviation_physical": [width_deviation, height_deviation],
                "allowed_deviation_physical": 1,
                "rule": (
                    "nominal round(680*scale) x round(440*scale) DevicePixels; at "
                    "most one physical pixel of non-client frame rounding is "
                    "recorded, and it cannot shift a sample because sample "
                    "positions derive from the realized client rect"
                ),
            },
        ):
            raise CaptureError(
                "owned client physical size does not match 680x440 DIP at the "
                f"declared scale {scale}: {state['client_width']}x{state['client_height']}"
            )

        dpi_scale = state["dpi"] / 96.0
        if not add_check(
            checks,
            "window_dpi_matches_metrics_scale_factor",
            struct.pack("<f", dpi_scale) == struct.pack("<f", scale),
            {
                "window_dpi": state["dpi"],
                "dpi_scale": dpi_scale,
                "metrics_scale_factor": scale,
                "rule": "exact equality after conversion to platform f32",
            },
        ):
            raise CaptureError(
                f"window DPI scale {dpi_scale} does not match metrics scale_factor {scale}"
            )

        client_dip_w = state["client_width"] / scale
        client_dip_h = state["client_height"] / scale
        if not add_check(
            checks,
            "surface_bounds_inside_actual_client_dip",
            sx + sw <= client_dip_w + 1e-3 and sy + sh <= client_dip_h + 1e-3,
            {
                "surface_bounds_dip": [sx, sy, sw, sh],
                "actual_client_dip": [client_dip_w, client_dip_h],
            },
        ):
            raise CaptureError("declared surface_bounds does not fit the actual client in DIP")

        if phase == "first":
            frame_width = metrics["frame_width"]
            frame_height = metrics["frame_height"]
            accepted_frame_w = device_candidates(sw, scale)
            accepted_frame_h = device_candidates(sh, scale)
            if not add_check(
                checks,
                "first_frame_physical_size_matches_dip_times_scale",
                frame_width in accepted_frame_w and frame_height in accepted_frame_h,
                {
                    "actual_frame_physical": [frame_width, frame_height],
                    "surface_bounds_dip": [sw, sh],
                    "scale_factor": scale,
                    "accepted_physical_width": accepted_frame_w,
                    "accepted_physical_height": accepted_frame_h,
                    "rule": (
                        "real CEF frame size equals ceil(f32(DIP*scale)); "
                        "parent metrics remain authoritative, no fractional slack"
                    ),
                },
            ):
                raise CaptureError(
                    "frame physical size does not match 320x240 DIP * scale_factor: "
                    f"{frame_width}x{frame_height} vs {sw}x{sh} * {scale}"
                )

        screen = virtual_screen(user32)
        record["virtual_screen"] = screen
        primary = primary_screen(user32)
        record["primary_screen"] = primary
        bbox_inside = (
            state["client_left"] >= screen["left"]
            and state["client_top"] >= screen["top"]
            and state["client_right"] <= screen["left"] + screen["width"]
            and state["client_bottom"] <= screen["top"] + screen["height"]
        )
        if not add_check(
            checks,
            "client_bbox_fully_on_virtual_screen",
            bbox_inside,
            {
                "client_bbox_physical": [
                    state["client_left"],
                    state["client_top"],
                    state["client_right"],
                    state["client_bottom"],
                ],
                "virtual_screen": screen,
            },
        ):
            raise CaptureError("owned client bbox is not fully inside the desktop; cannot grab actual pixels")

        # Ask Pillow for the smallest surface that contains the owned bbox. With
        # all_screens=False Pillow reads only the primary monitor; a client on
        # another monitor requires the whole virtual desktop internally so the
        # requested bbox exists. Either way only the client crop is returned,
        # saved, and inspected here.
        bbox_on_primary = (
            state["client_left"] >= primary["left"]
            and state["client_top"] >= primary["top"]
            and state["client_right"] <= primary["left"] + primary["width"]
            and state["client_bottom"] <= primary["top"] + primary["height"]
        )
        all_screens = not bbox_on_primary
        record["capture"]["all_screens_internal"] = all_screens
        record["capture"]["bbox_on_primary_monitor"] = bbox_on_primary

        expected_frame_rgb = list(PAGE_RGB if phase == "first" else DETACHED_RGB)
        sample_specs = []
        for row, fraction_y in enumerate(FRAME_FRACTIONS):
            for column, fraction_x in enumerate(FRAME_FRACTIONS):
                dip_x = sx + sw * fraction_x
                dip_y = sy + sh * fraction_y
                if not (sx < dip_x < sx + sw and sy < dip_y < sy + sh):
                    raise CaptureError(f"frame sample {column},{row} is not strictly inside the frame")
                client_x, client_y = dip_to_client_px(dip_x, dip_y, scale)
                if not (
                    0 <= client_x < state["client_width"]
                    and 0 <= client_y < state["client_height"]
                ):
                    raise CaptureError(f"frame sample {column},{row} maps outside the client")
                sample_specs.append(
                    {
                        "name": f"frame-{column}-{row}",
                        "kind": "frame",
                        "dip": [dip_x, dip_y],
                        "client_px": [client_x, client_y],
                        "expected_rgb": expected_frame_rgb,
                    }
                )

        header_x, header_y = HEADER_SAMPLE_DIP
        if not add_check(
            checks,
            "header_sample_is_bare_red_area_of_48_dip_bar",
            HOST_DIP[0] - HEADER_SAMPLE_DIP[0] == 20.0
            and 0.0 <= header_x < HOST_DIP[0]
            and 0.0 <= header_y < HEADER_HEIGHT_DIP,
            {
                "header_sample_dip": [header_x, header_y],
                "header_height_dip": HEADER_HEIGHT_DIP,
                "right_edge_inset_dip": HOST_DIP[0] - header_x,
                "note": (
                    "right-edge sample avoids the left-aligned title glyphs; the "
                    "color equality itself is not relaxed"
                ),
            },
        ):
            raise CaptureError(
                "header sample is not the bare red area 20 DIP from the right edge "
                "at y=24 DIP"
            )

        header_client_x, header_client_y = dip_to_client_px(header_x, header_y, scale)
        if not (
            0 <= header_client_x < state["client_width"]
            and 0 <= header_client_y < state["client_height"]
        ):
            raise CaptureError("header sample maps outside the client")
        header_inside_frame = sx <= header_x < sx + sw and sy <= header_y < sy + sh
        if not add_check(
            checks,
            "header_sample_outside_presented_frame_region",
            not header_inside_frame,
            {
                "header_sample_dip": [header_x, header_y],
                "surface_bounds_dip": [sx, sy, sw, sh],
                "header_sample_inside_surface_bounds": header_inside_frame,
                "surface_bounds_top_dip": sy,
                "rule": (
                    "the header sample must fall outside the canvas rect; an "
                    "overlapping point would demand two exact colors at one pixel"
                ),
            },
        ):
            raise CaptureError(
                "declared surface_bounds overlaps the host header bar; the header "
                "and frame samples would contradict each other"
            )
        sample_specs.append(
            {
                "name": "header-right-20-24",
                "kind": "header",
                "dip": [header_x, header_y],
                "client_px": [header_client_x, header_client_y],
                "expected_rgb": list(HEADER_RGB),
            }
        )

        probe_specs = []
        for row, fraction_y in enumerate(CLIENT_GRID_FRACTIONS):
            for column, fraction_x in enumerate(CLIENT_GRID_FRACTIONS):
                client_x = min(
                    max(int(math.floor(state["client_width"] * fraction_x)), 0),
                    state["client_width"] - 1,
                )
                client_y = min(
                    max(int(math.floor(state["client_height"] * fraction_y)), 0),
                    state["client_height"] - 1,
                )
                probe_specs.append(
                    {
                        "name": f"client-grid-{column}-{row}",
                        "client_px": [client_x, client_y],
                    }
                )
        for spec in sample_specs:
            probe_specs.append(
                {
                    "name": f"sample-{spec['name']}",
                    "client_px": list(spec["client_px"]),
                }
            )

        record["foreground"]["before"] = foreground_evidence(user32, hwnd, args.pid)

        if phase == "detached":
            first_path = case_dir / "window-first.json"
            record["detached_comparison"] = {"first_phase_json": str(first_path), "present": False}
            if first_path.is_file():
                try:
                    first_record = json.loads(first_path.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError) as exc:
                    add_check(
                        checks,
                        "detached_first_phase_artifact_readable",
                        False,
                        f"{first_path}: {exc}",
                    )
                    raise CaptureError(f"cannot read prior first-phase artifact: {exc}")
                first_geometry = first_record.get("client_physical_geometry") or {}
                first_hwnd = first_record.get("hwnd")
                if not first_hwnd:
                    first_hwnd = first_record.get("owned_window_handle")
                first_scale = first_record.get("scale_factor")
                first_bbox = first_geometry.get("bbox_physical")
                current_bbox = [
                    state["client_left"],
                    state["client_top"],
                    state["client_right"],
                    state["client_bottom"],
                ]
                first_metrics = first_record.get("metrics") or {}
                same = (
                    first_hwnd == hwnd
                    and first_scale == scale
                    and first_bbox == current_bbox
                    and first_metrics.get("surface_bounds_dip") == [sx, sy, sw, sh]
                )
                record["detached_comparison"] = {
                    "first_phase_json": str(first_path),
                    "present": True,
                    "first_hwnd": hex(first_hwnd) if isinstance(first_hwnd, int) else first_hwnd,
                    "current_hwnd": state["hwnd_hex"],
                    "first_scale_factor": first_scale,
                    "current_scale_factor": scale,
                    "first_bbox_physical": first_bbox,
                    "current_bbox_physical": current_bbox,
                    "first_surface_bounds_dip": first_metrics.get("surface_bounds_dip"),
                    "current_surface_bounds_dip": [sx, sy, sw, sh],
                    "same_window_and_geometry": same,
                    "first_phase_all_passed": first_record.get("all_passed"),
                }
                add_check(
                    checks,
                    "detached_same_window_and_geometry_as_first",
                    same,
                    record["detached_comparison"],
                )
                if not same:
                    raise CaptureError(
                        "detached window/geometry differs from the first phase: "
                        f"hwnd {first_hwnd}->{hwnd}, bbox {first_bbox}->{current_bbox}, "
                        f"scale {first_scale}->{scale}, "
                        f"surface_bounds {first_metrics.get('surface_bounds_dip')}->"
                        f"{[sx, sy, sw, sh]}"
                    )
                first_passed = first_record.get("all_passed") is True
                add_check(
                    checks,
                    "detached_first_phase_passed",
                    first_passed,
                    {
                        "first_phase_all_passed": first_record.get("all_passed"),
                        "first_phase_failed_checks": first_record.get("failed_checks"),
                        "rule": (
                            "detached is same-window evidence for an already proven "
                            "first phase; a failing first phase cannot be turned into "
                            "a passing detached result"
                        ),
                    },
                )
                if not first_passed:
                    raise CaptureError(
                        "the first phase did not pass; detached cannot be accepted as "
                        "same-window presentation evidence"
                    )
            else:
                add_check(
                    checks,
                    "detached_first_phase_artifact_present",
                    False,
                    f"{first_path} is required for the same-window/same-geometry comparison",
                )
                raise CaptureError(
                    "detached requires the prior first-phase window-first.json; "
                    "same-position evidence cannot be assumed"
                )

        probes_before = run_probes(user32, hwnd, state, probe_specs)
        record["occlusion"]["probes_before"] = probes_before
        record["occlusion"]["all_owned_before"] = all(
            probe["owned"] for probe in probes_before
        )
        if not add_check(
            checks,
            "occlusion_probes_before_capture_all_owned",
            record["occlusion"]["all_owned_before"],
            occlusion_detail(probes_before),
        ):
            raise CaptureError(
                "another window covers the owned client before capture; "
                "refusing to treat mixed pixels as presentation"
            )

        bbox = (
            state["client_left"],
            state["client_top"],
            state["client_right"],
            state["client_bottom"],
        )
        record["capture"]["bbox_physical"] = list(bbox)
        try:
            image = ImageGrab.grab(bbox=bbox, all_screens=all_screens)
        except Exception as exc:
            raise CaptureError(f"ImageGrab.grab failed: {type(exc).__name__}: {exc}") from exc

        record["capture"]["image_mode"] = image.mode
        record["capture"]["image_size"] = [int(image.width), int(image.height)]
        # Save before judging pixels so a color/size failure still leaves the
        # actual screenshot next to the failing JSON.
        save_png_atomic(image, png_path)
        record["capture"]["png_sha256"] = sha256_file(png_path)
        if not add_check(
            checks,
            "screenshot_is_rgb_client_crop",
            image.mode == "RGB"
            and image.size == (state["client_width"], state["client_height"]),
            {
                "mode": image.mode,
                "actual_size": [int(image.width), int(image.height)],
                "expected_rgb_mode": "RGB",
                "expected_client_size": [state["client_width"], state["client_height"]],
                "alpha_checked": False,
            },
        ):
            raise CaptureError(
                "screenshot is not an RGB client-sized crop: "
                f"{image.mode} {image.size} vs RGB "
                f"{(state['client_width'], state['client_height'])}"
            )

        try:
            windows_after = enum_visible_top_level_windows(user32, args.pid)
            state_after = inspect_window(user32, hwnd)
        except CaptureError as exc:
            record["capture"]["post_state_error"] = str(exc)
            raise
        record["owned_windows_after_capture"] = {
            "count": len(windows_after),
            "handles_hex": [hex(handle) for handle in windows_after],
        }
        record["owned_window_after_capture"] = state_after

        if not add_check(
            checks,
            "exactly_one_visible_top_level_window_after_capture",
            windows_after == [hwnd],
            {
                "pid": args.pid,
                "expected_hwnd": hex(hwnd),
                "count": len(windows_after),
                "handles_hex": [hex(handle) for handle in windows_after],
            },
        ):
            raise CaptureError("the owned window set changed across the screenshot")

        stable_keys = (
            "hwnd",
            "owner_pid",
            "title",
            "visible",
            "iconic",
            "dpi",
            "client_left",
            "client_top",
            "client_width",
            "client_height",
        )
        changed = {
            key: [state[key], state_after[key]]
            for key in stable_keys
            if state[key] != state_after[key]
        }
        if not add_check(
            checks,
            "window_state_stable_across_capture",
            not changed,
            {
                "hwnd": hex(hwnd),
                "changed_fields_before_after": changed,
                "checked_fields": list(stable_keys),
            },
        ):
            raise CaptureError(
                "owned window identity/client bounds/DPI changed across capture: "
                f"{changed}"
            )

        record["foreground"]["after"] = foreground_evidence(user32, hwnd, args.pid)

        probes_after = run_probes(user32, hwnd, state, probe_specs)
        record["occlusion"]["probes_after"] = probes_after
        record["occlusion"]["all_owned_after"] = all(
            probe["owned"] for probe in probes_after
        )
        if not add_check(
            checks,
            "occlusion_probes_after_capture_all_owned",
            record["occlusion"]["all_owned_after"],
            occlusion_detail(probes_after),
        ):
            raise CaptureError(
                "another window covered the owned client after capture; "
                "the grabbed pixels are not accepted as presentation"
            )

        probes_before_by_coord = {
            (probe["client_px"][0], probe["client_px"][1]): probe
            for probe in probes_before
        }
        probes_after_by_coord = {
            (probe["client_px"][0], probe["client_px"][1]): probe
            for probe in probes_after
        }

        points = []
        for spec in sample_specs:
            key = (spec["client_px"][0], spec["client_px"][1])
            before = probes_before_by_coord.get(key)
            after = probes_after_by_coord.get(key)
            if before is None or after is None:
                raise CaptureError(f"internal probe coverage missing for {spec['name']}")
            actual = image.getpixel(key)
            actual_rgb = [int(channel) for channel in actual]
            points.append(
                {
                    "name": spec["name"],
                    "kind": spec["kind"],
                    "dip": spec["dip"],
                    "client_px": spec["client_px"],
                    "screen_px": [
                        state["client_left"] + key[0],
                        state["client_top"] + key[1],
                    ],
                    "expected_rgb": spec["expected_rgb"],
                    "actual_rgb": actual_rgb,
                    "passed": tuple(actual_rgb) == tuple(spec["expected_rgb"]),
                    "occlusion_before": {
                        "root": before["root"],
                        "root_hex": before["root_hex"],
                        "owned": before["owned"],
                    },
                    "occlusion_after": {
                        "root": after["root"],
                        "root_hex": after["root_hex"],
                        "owned": after["owned"],
                    },
                }
            )
        record["points"] = points

        frame_points = [point for point in points if point["kind"] == "frame"]
        header_points = [point for point in points if point["kind"] == "header"]
        add_check(
            checks,
            "frame_grid_has_nine_points",
            len(frame_points) == 9,
            {"count": len(frame_points)},
        )

        frame_failed = [point["name"] for point in frame_points if not point["passed"]]
        frame_check_name = (
            "first_frame_grid_rgb_26_76_126"
            if phase == "first"
            else "detached_region_rgb_0_170_68"
        )
        add_check(
            checks,
            frame_check_name,
            not frame_failed and len(frame_points) == 9,
            {
                "expected_rgb": expected_frame_rgb,
                "failed_points": frame_failed,
                "tolerance": "none",
            },
        )

        header_failed = [point["name"] for point in header_points if not point["passed"]]
        add_check(
            checks,
            "header_bar_rgb_159_34_34",
            not header_failed and len(header_points) == 1,
            {
                "expected_rgb": list(HEADER_RGB),
                "failed_points": header_failed,
                "tolerance": "none",
                "hint": (
                    "a non-red pixel 20 DIP from the right edge at y=24 DIP means "
                    "the host header bar moved or is not painted there; fix the "
                    "host, not this check"
                ),
            },
        )

        add_check(
            checks,
            "all_declared_pixels_exact",
            not frame_failed and not header_failed,
            {
                "frame_rgb": expected_frame_rgb,
                "header_rgb": list(HEADER_RGB),
                "failed_points": frame_failed + header_failed,
                "tolerance": "none",
            },
        )

    except CaptureError as exc:
        record["error"] = str(exc)
    except KeyboardInterrupt:
        record["error"] = "interrupted before evidence completed"
    except Exception as exc:
        record["error"] = f"unexpected {type(exc).__name__}: {exc}"
        record["traceback"] = traceback.format_exc()
    finally:
        record["finished_utc"] = utc_now()
        failed_checks = [check["name"] for check in checks if not check["passed"]]
        record["failed_checks"] = failed_checks
        if record["error"] is None and failed_checks:
            record["error"] = "failed checks: " + ", ".join(failed_checks)
        all_passed = (
            record["error"] is None
            and bool(checks)
            and all(check["passed"] for check in checks)
        )
        record["all_passed"] = all_passed
        record["success"] = all_passed
        # These three keys are the machine contract for the PowerShell runner:
        # success:bool, pid:int, hwnd:int. hwnd is 0 when no owned window was
        # identified (success is then false and error is set), never a string
        # or null, so the runner can type it strictly.
        record["pid"] = int(args.pid)
        record["hwnd"] = int(record["hwnd"]) if record["hwnd"] else 0
        try:
            write_json_atomic(json_path, record)
        except (OSError, TypeError, ValueError) as exc:
            print(f"capture-component-host: cannot write {json_path}: {exc}", file=sys.stderr)
            return 1

    state_text = "no-window"
    if record["owned_window"] is not None:
        window = record["owned_window"]
        state_text = (
            f"hwnd={window['hwnd_hex']} pid={window['owner_pid']} "
            f"client={window['client_width']}x{window['client_height']}"
        )
    print(
        f"window-{phase}: {'PASS' if record['all_passed'] else 'FAIL'} "
        f"{state_text} points={len(record['points'])} -> {json_path}",
        flush=True,
    )
    if record["error"] is not None:
        print(f"window-{phase}: error: {record['error']}", file=sys.stderr, flush=True)
    return 0 if record["all_passed"] else 1


def main() -> int:
    return run(parse_args(sys.argv[1:]))


if __name__ == "__main__":
    sys.exit(main())
