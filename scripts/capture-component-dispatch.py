#!/usr/bin/env python3
"""Owned-window pixels for one stage of the 10.2 public-host dispatch chain.

The independent host writes ``case-dir/dispatch-stage-N.json``. Its schema is the JSON
integer 1, its stage/pid must match, its colors/active flags must equal the fixed stage
table below, and its diagnostics must satisfy the active/inactive rules. Metadata is
layout/diagnostics input only, never pixel evidence. This script proves the PID's single
visible top-level window stayed identical (static WindowSet plus PID/title/visible/iconic/
DPI/client bbox before and after), grabs the real desktop client area once, samples 19
predeclared points, checks every pixel of both full viewports, and writes
``window-stage-N.json`` + ``window-stage-N.png``. Only the owned client crop is persisted.
No PrintWindow, OSR raw frames, input, cursor motion, foreground activation or window
move/resize. Capture is not atomic; limitations are recorded in the JSON.
"""

from __future__ import annotations

import importlib.util, json, math, struct, sys, traceback
from datetime import datetime, timezone
from pathlib import Path


def _host():
    path = Path(__file__).resolve().with_name("capture-component-host.py")
    spec = importlib.util.spec_from_file_location("capture_component_host", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


H, CHECKS = _host(), []
E = H.CaptureError
TITLE, HEADER_RGB, GREEN = H.TITLE, (159, 34, 34), (0, 170, 68)
HOST_DIP, VIEW_DIP, HEADER = (680.0, 440.0), (320.0, 240.0), (660.0, 24.0)
REGIONS = {"a": (0.0, 48.0, 320.0, 240.0), "b": (340.0, 48.0, 320.0, 240.0)}
FRACTIONS, GRID = (0.25, 0.5, 0.75), (1.0 / 6.0, 0.5, 5.0 / 6.0)
# Fixed stage table, slot -> (RGB, active); green is the host's detached fallback.
EXPECTED = {
    0: {"a": ((26, 76, 126), True), "b": ((110, 40, 180), True)},
    1: {"a": ((190, 90, 20), True), "b": ((110, 40, 180), True)},
    2: {"a": ((190, 90, 20), True), "b": ((20, 150, 170), True)},
    3: {"a": ((26, 76, 126), True), "b": ((20, 150, 170), True)},
    4: {"a": (GREEN, False), "b": ((20, 150, 170), True)},
    5: {"a": (GREEN, False), "b": (GREEN, False)},
}
STABLE = ("hwnd", "owner_pid", "title", "visible", "iconic", "dpi", "client_left",
          "client_top", "client_width", "client_height")
LIMITS = {
    "atomicity": "probes/snapshots and the grab are separate point-in-time operations; not an "
                 "atomic isolation guarantee.",
    "occlusion_coverage": "3x3 client grid plus every sample (28 probes); unprobed pixels are "
                          "unchecked and WindowFromPoint ignores hidden/disabled/transparent "
                          "windows (no CAPTUREBLT), so a click-through layered overlay may be "
                          "neither detected nor captured.",
    "alpha": "RGB only; screenshot alpha/RGBA is not validated.",
    "dpi": "one owned window at its current monitor DPI; no multi-DPI extrapolation.",
    "ime": "IME composition and candidate placement are not exercised.",
    "multi_screen": "only current virtual-desktop placement; no multi-monitor claim.",
    "presentation": "cropped actual desktop pixels, not PrintWindow or OSR raw.",
    "metadata": "colors/active must equal the fixed table; pixels compare only to it.",
}


def now():
    return datetime.now(timezone.utc).isoformat()


def f32(value):
    return struct.pack("<f", float(value))


def num(value, what):
    """JSON number only: no string coercion, no bool, finite."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise E(f"{what}: not a JSON number: {value!r}")
    if not math.isfinite(float(value)):
        raise E(f"{what}: not finite: {value!r}")
    return float(value)


def uint(value, what):
    """Strict JSON integer: type must be int (bool/float rejected), non-negative."""
    if type(value) is not int or value < 0:
        raise E(f"{what}: not a non-negative JSON integer: {value!r}")
    return value


def need(ok, name, detail, message=None):
    H.add_check(CHECKS, name, ok, detail)
    if not ok:
        raise E(message or name)


def diag(slot, active, d, scale):
    """Active views must have painted at the CEF f32 size; inactive ones released."""
    label, bounds = f"views.{slot}.diagnostics", d.get("surface_bounds")
    paint, sequence = uint(d.get("paint_calls"), label), uint(d.get("painted_sequence"), label)
    width = uint(d.get("frame_width"), label)
    height = uint(d.get("frame_height"), label)
    created = uint(d.get("created_images"), label)
    released = uint(d.get("released_images"), label)
    if not isinstance(bounds, list) or len(bounds) != 4:
        raise E(f"{label}.surface_bounds is not a JSON [x,y,w,h]: {bounds!r}")
    actual = [num(item, label) for item in bounds]
    need(all(f32(a) == f32(b) for a, b in zip(actual, REGIONS[slot])),
         f"diag_surface_bounds_{slot}_exact_region",
         {"slot": slot, "expected": list(REGIONS[slot]), "actual": actual, "rule": "f32 exact"},
         f"{label}.surface_bounds does not exactly match region {slot}")
    need(f32(num(d.get("scale_factor"), label)) == f32(scale), f"diag_scale_factor_{slot}_f32",
         {"slot": slot, "diagnostics": d.get("scale_factor"), "metadata": scale},
         f"{label}.scale_factor does not match metadata scale {scale}")
    if not active:
        need(created == released and created > 0, f"diag_inactive_images_{slot}_balanced",
             {"created": created, "released": released},
             f"{label} inactive view requires created_images == released_images > 0")
        return
    need(paint > 0, f"diag_paint_calls_{slot}_positive", {"paint_calls": paint})
    need(sequence > 0, f"diag_painted_sequence_{slot}_positive", {"sequence": sequence})
    expected = [H.device_candidates(VIEW_DIP[0], scale)[0],
                H.device_candidates(VIEW_DIP[1], scale)[0]]
    need([width, height] == expected, f"diag_frame_size_{slot}_ceil_f32",
         {"frame": [width, height], "expected": expected, "rule": "ceil(f32(DIP*scale))"},
         f"{label} frame size is not 320x240 DIP * scale {scale}")


def validate(path, pid, stage):
    """Parse metadata; enforce schema/stage/pid, the fixed table and the diag rules."""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise E(f"cannot read {path}: {exc}") from exc
    except json.JSONDecodeError as exc:
        raise E(f"invalid JSON in {path}: {exc}") from exc
    if not isinstance(data, dict) or type(data.get("schema")) is not int \
            or data.get("schema") != 1:
        raise E(f"{path}: schema must be the JSON integer 1")
    if uint(data.get("stage"), "stage") != stage or uint(data.get("pid"), "pid") != pid:
        raise E(f"{path}: stage/pid mismatch")
    scale, fixed, seen, browsers = num(data.get("scale_factor"), "scale_factor"), \
        EXPECTED[stage], set(), {}
    need(scale > 0.0, "metadata_scale_factor_positive", {"scale_factor": scale})
    views = data.get("views")
    if not isinstance(views, list) or len(views) != 2:
        raise E(f"{path}: views must be exactly two")
    for index, view in enumerate(views):
        if not isinstance(view, dict) or view.get("slot") not in ("a", "b"):
            raise E(f"{path}: views[{index}] invalid")
        slot, label, (color, active) = view["slot"], f"{path} views[{index}]", fixed[view["slot"]]
        ids, rgb_in = view.get("browser"), view.get("color")
        if slot in seen:
            raise E(f"{label}: duplicate slot")
        if not isinstance(ids, list) or len(ids) != 2:
            raise E(f"{label}.browser is not [runtime,local]")
        if any(type(item) is not int or item <= 0 for item in ids):
            raise E(f"{label}.browser ids must be positive JSON integers: {ids!r}")
        if not isinstance(rgb_in, list) or len(rgb_in) != 3:
            raise E(f"{label}.color is not a JSON [r,g,b]")
        rgb = tuple(uint(channel, f"{label}.color") for channel in rgb_in)
        if max(rgb) > 255:
            raise E(f"{label}.color above 255")
        if not isinstance(view.get("active"), bool) or not isinstance(view.get("diagnostics"), dict):
            raise E(f"{label} active/diagnostics invalid")
        need(rgb == color, f"metadata_color_{slot}",
             {"slot": slot, "expected": list(color), "metadata": list(rgb)},
             f"metadata color for slot {slot} differs from the fixed stage {stage} table")
        need(view["active"] is active, f"metadata_active_{slot}",
             {"slot": slot, "expected": active, "metadata": view["active"]},
             f"metadata active for slot {slot} differs from the fixed stage {stage} table")
        seen.add(slot)
        browsers[slot] = list(ids)
        diag(slot, view["active"], view["diagnostics"], scale)
    if seen != {"a", "b"}:
        raise E(f"{path}: missing slot a or b")
    need(browsers["a"][0] == browsers["b"][0], "metadata_browser_runtime_shared",
         browsers, "slots a and b must report the same runtime id")
    need(browsers["a"][1] != browsers["b"][1], "metadata_browser_local_distinct",
         browsers, "slots a and b must report different local browser ids")
    return data, scale, browsers


def new_record(stage, pid, case_dir):
    return {
        "schema": "1", "kind": "component-dispatch-owned-window-pixels",
        "stage": int(stage), "pid": int(pid), "hwnd": 0, "success": False, "error": None,
        "case_dir": str(case_dir), "metadata_path": None, "metadata_sha256": None,
        "browser_ids": None, "started_utc": now(), "finished_utc": None, "metrics": None,
        "scale_factor": None, "thread_dpi_awareness": None, "virtual_screen": None,
        "primary_screen": None, "client_bbox_physical": None,
        "window": {"pre": None, "post": None},
        "foreground": {"before": None, "after": None, "forced": False},
        "owned_windows_for_pid": None, "owned_windows_after_capture": None,
        "source": {"presentation": "actual desktop pixels of the owned HWND client area",
                   "osr_raw_used": False, "print_window_used": False,
                   "input_injection": False, "cursor_moved": False,
                   "foreground_hijack": False, "window_moved": False,
                   "window_resized": False, "image_resized_or_filtered": False},
        "occlusion": {"client_grid_fractions": list(GRID), "probe_count": None,
                      "probes_before": [], "probes_after": [],
                      "all_owned_before": None, "all_owned_after": None},
        "capture": {"method": "PIL.ImageGrab.grab(bbox=owned client physical bbox, "
                              "all_screens=<only when not fully on the primary monitor>)",
                    "bbox_physical": None, "bbox_on_primary_monitor": None,
                    "all_screens_internal": None, "image_mode": None, "image_size": None,
                    "note": "Pillow may internally capture the monitor/virtual desktop; only "
                            "the owned client crop is saved or inspected."},
        "points": [], "viewport_rects_physical": {},
        "viewport_rects_physical_space": "client-relative physical pixels inside "
                                         "window-stage-N.png",
        "viewports": {}, "png_path": None, "png_sha256": None,
        "same_window": {"required": int(stage) > 0, "previous_stage": None},
        "checks": CHECKS, "failed_checks": [], "limitations": LIMITS,
    }


def prior_browsers(data):
    """Full {slot: [runtime, local]} identity from a prior stage artifact, if readable."""
    ids = data.get("browser_ids")
    if isinstance(ids, dict) and set(ids) == {"a", "b"}:
        return {slot: list(ids[slot]) for slot in ("a", "b")}
    views = (data.get("metrics") or {}).get("views")
    if not isinstance(views, list):
        return None
    found = {v.get("slot"): v.get("browser") for v in views
             if isinstance(v, dict) and v.get("slot") in ("a", "b")}
    return found if set(found) == {"a", "b"} else None


def chain(record, args, hwnd, bbox, scale, browsers):
    """Stage > 0 must continue the passing, same-window, same-browser stage N-1."""
    previous = args.stage - 1
    path = args.case_dir / f"window-stage-{previous}.json"
    entry = record["same_window"]
    entry.update({"previous_stage": previous, "previous_path": str(path),
                  "current_client_bbox_physical": bbox, "current_scale_factor": scale,
                  "current_browser_ids": browsers, "previous_browser_ids": None,
                  "same_browser_ids": False, "same_window": False})
    need(path.is_file(), "previous_stage_artifact_present", {"path": str(path)},
         f"stage {args.stage} requires {path} from stage {previous}")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise E(f"cannot read prior stage artifact {path}: {exc}") from exc
    if not isinstance(data, dict):
        raise E(f"prior stage artifact {path} is not an object")
    need(data.get("success") is True, "previous_stage_success_true",
         {"path": str(path), "previous_success": data.get("success")},
         f"prior stage {previous} did not succeed; stages cannot be skipped")
    prev_scale = data.get("scale_factor")
    same = (data.get("stage") == previous and data.get("pid") == args.pid
            and data.get("hwnd") == hwnd and data.get("client_bbox_physical") == bbox
            and isinstance(prev_scale, (int, float)) and not isinstance(prev_scale, bool)
            and f32(prev_scale) == f32(scale))
    previous_ids = prior_browsers(data)
    same_bids = previous_ids == browsers
    entry.update({"previous_success": data.get("success"),
                  "previous_stage_value": data.get("stage"), "previous_pid": data.get("pid"),
                  "previous_hwnd": data.get("hwnd"),
                  "previous_client_bbox_physical": data.get("client_bbox_physical"),
                  "previous_scale_factor": prev_scale, "previous_browser_ids": previous_ids,
                  "same_browser_ids": same_bids, "same_window": same})
    need(same, "previous_stage_same_pid_hwnd_clientbbox_scale", entry,
         f"stage {args.stage} is not on the same window as passing stage {previous}")
    need(same_bids, "previous_stage_same_browser_ids", entry,
         f"stage {args.stage} browser ids differ from passing stage {previous}: "
         f"{previous_ids} != {browsers}")


def sample_specs(pre, scale, fixed):
    """19 predeclared points: 3x3 per region plus the bare red header point."""
    out = []
    for slot in ("a", "b"):
        rx, ry, rw, rh = REGIONS[slot]
        for row, fy in enumerate(FRACTIONS):
            for column, fx in enumerate(FRACTIONS):
                dip = [rx + rw * fx, ry + rh * fy]
                client_px = H.dip_to_client_px(dip[0], dip[1], scale)
                if not (0 <= client_px[0] < pre["client_width"]
                        and 0 <= client_px[1] < pre["client_height"]):
                    raise E(f"sample {slot}-{column}-{row} maps outside the client")
                out.append({"name": f"{slot}-{column}-{row}", "kind": slot, "dip": dip,
                            "client_px": list(client_px),
                            "expected_rgb": list(fixed[slot][0])})
    header = list(HEADER)
    inside = any(rx <= header[0] < rx + rw and ry <= header[1] < ry + rh
                 for rx, ry, rw, rh in REGIONS.values())
    need(not inside and header[0] == HOST_DIP[0] - 20.0,
         "header_sample_is_bare_red_outside_view_regions",
         {"header_sample_dip": header, "inside_view_region": inside},
         "header sample must be the bare red point 20 DIP from the right edge at y=24 DIP")
    header_px = H.dip_to_client_px(header[0], header[1], scale)
    if not (0 <= header_px[0] < pre["client_width"]
            and 0 <= header_px[1] < pre["client_height"]):
        raise E("header sample maps outside the client")
    out.append({"name": "header-right-20-24", "kind": "header", "dip": header,
                "client_px": list(header_px), "expected_rgb": list(HEADER_RGB)})
    return out


def probe_specs(pre, sample_list):
    """28 occlusion probes: a 3x3 whole-client grid plus every sample point."""
    out = [{"name": f"client-grid-{column}-{row}", "client_px": [
        min(max(int(math.floor(pre["client_width"] * fx)), 0), pre["client_width"] - 1),
        min(max(int(math.floor(pre["client_height"] * fy)), 0), pre["client_height"] - 1)]}
        for row, fy in enumerate(GRID) for column, fx in enumerate(GRID)]
    return out + [{"name": f"sample-{s['name']}", "client_px": list(s["client_px"])}
                  for s in sample_list]


def check_pixels(record, image, pre, sample_list, scale, fixed):
    """Exact per-point RGB plus every physical pixel of both full viewport rects."""
    before = {(p["client_px"][0], p["client_px"][1]): p
              for p in record["occlusion"]["probes_before"]}
    after = {(p["client_px"][0], p["client_px"][1]): p
             for p in record["occlusion"]["probes_after"]}
    points = []
    for sample in sample_list:
        key = (sample["client_px"][0], sample["client_px"][1])
        if key not in before or key not in after:
            raise E(f"internal probe coverage missing for {sample['name']}")
        actual = [int(channel) for channel in image.getpixel(key)]
        points.append({
            "name": sample["name"], "kind": sample["kind"], "dip": sample["dip"],
            "client_px": sample["client_px"],
            "screen_px": [pre["client_left"] + key[0], pre["client_top"] + key[1]],
            "expected_rgb": sample["expected_rgb"], "actual_rgb": actual,
            "passed": tuple(actual) == tuple(sample["expected_rgb"]),
            "occlusion_before": {k: before[key][k] for k in ("root", "root_hex", "owned")},
            "occlusion_after": {k: after[key][k] for k in ("root", "root_hex", "owned")}})
    record["points"] = points
    for slot in ("a", "b"):
        rx, ry, rw, rh = REGIONS[slot]
        x0, y0 = H.dip_to_client_px(rx, ry, scale)
        x1, y1 = H.dip_to_client_px(rx + rw, ry + rh, scale)
        if not (0 <= x0 < x1 <= image.width and 0 <= y0 < y1 <= image.height):
            raise E(f"viewport {slot} physical rect is outside the client image")
        crop, expected = image.crop((x0, y0, x1, y1)), tuple(fixed[slot][0])
        colors = crop.getcolors(2)
        distinct = [] if colors is None else [[n, [int(c) for c in rgb]] for n, rgb in colors]
        solid = (colors is not None and len(colors) == 1 and tuple(colors[0][1]) == expected
                 and colors[0][0] == crop.width * crop.height)
        record["viewports"][slot] = {
            "client_physical_rect": [x0, y0, x1, y1],
            "screen_physical_rect": [pre["client_left"] + x0, pre["client_top"] + y0,
                                     pre["client_left"] + x1, pre["client_top"] + y1],
            "size": [crop.width, crop.height], "expected_rgb": list(expected),
            "method": "direct PIL crop of the owned client RGB grab; every pixel compared "
                      "with getcolors(2); no resize or filter",
            "distinct_color_count": None if colors is None else len(colors),
            "distinct_colors": distinct, "solid_expected": solid}
        record["viewport_rects_physical"][slot] = [x0, y0, x1, y1]
        slot_points = [p for p in points if p["kind"] == slot]
        failed = [p["name"] for p in slot_points if not p["passed"]]
        H.add_check(CHECKS, f"viewport_{slot}_fully_expected_rgb", solid,
                    record["viewports"][slot])
        H.add_check(CHECKS, f"viewport_{slot}_sample_grid_exact_rgb",
                    not failed and len(slot_points) == 9,
                    {"slot": slot, "expected_rgb": list(expected), "failed": failed})
    header = [p for p in points if p["kind"] == "header"]
    failed = [p["name"] for p in header if not p["passed"]]
    H.add_check(CHECKS, "header_bar_rgb_159_34_34", not failed and len(header) == 1,
                {"expected_rgb": list(HEADER_RGB), "failed": failed,
                 "white_text_not_judged": True})
    H.add_check(CHECKS, "all_19_points_exact",
                len(points) == 19 and all(p["passed"] for p in points),
                {"sample_count": len(points), "failed":
                 [p["name"] for p in points if not p["passed"]]})
    bad = [c["name"] for c in CHECKS if not c["passed"]]
    if bad:
        raise E("pixel checks failed: " + ", ".join(bad))


def grab(args, record, user32, hwnd, pre, bbox, scale, fixed):
    """One grab plus post-state re-inspection and all pixel checks."""
    sample_list = sample_specs(pre, scale, fixed)
    probe_list = probe_specs(pre, sample_list)
    record["occlusion"]["probe_count"] = len(probe_list)
    record["foreground"]["before"] = H.foreground_evidence(user32, hwnd, args.pid)
    before_probes = H.run_probes(user32, hwnd, pre, probe_list)
    record["occlusion"]["probes_before"] = before_probes
    record["occlusion"]["all_owned_before"] = all(p["owned"] for p in before_probes)
    need(record["occlusion"]["all_owned_before"], "occlusion_probes_before_capture_all_owned",
         H.occlusion_detail(before_probes),
         "another window covers the owned client before capture")

    primary = record["primary_screen"]
    on_primary = (pre["client_left"] >= primary["left"] and pre["client_top"] >= primary["top"]
                  and pre["client_right"] <= primary["left"] + primary["width"]
                  and pre["client_bottom"] <= primary["top"] + primary["height"])
    png_path = args.case_dir / f"window-stage-{args.stage}.png"
    record["capture"].update({"bbox_physical": bbox, "bbox_on_primary_monitor": on_primary,
                              "all_screens_internal": not on_primary})
    try:
        image = H.ImageGrab.grab(bbox=tuple(bbox), all_screens=not on_primary)
    except Exception as exc:
        raise E(f"ImageGrab.grab failed: {type(exc).__name__}: {exc}") from exc
    record["capture"]["image_mode"] = image.mode
    record["capture"]["image_size"] = [int(image.width), int(image.height)]
    # Save before judging pixels so a color/size failure still leaves the actual PNG.
    H.save_png_atomic(image, png_path)
    record["png_path"], record["png_sha256"] = str(png_path), H.sha256_file(png_path)
    need(image.mode == "RGB" and image.size == (pre["client_width"], pre["client_height"]),
         "screenshot_is_rgb_client_crop",
         {"mode": image.mode, "size": [int(image.width), int(image.height)],
          "expected": ["RGB", [pre["client_width"], pre["client_height"]]],
          "alpha_checked": False},
         f"screenshot is not an RGB client-sized crop: {image.mode} {image.size}")

    try:
        windows_after = H.enum_visible_top_level_windows(user32, args.pid)
        post = H.inspect_window(user32, hwnd)
    except E as exc:
        record["capture"]["post_state_error"] = str(exc)
        raise
    record["window"]["post"] = post
    record["owned_windows_after_capture"] = {"count": len(windows_after),
                                             "handles_hex": [hex(h) for h in windows_after]}
    need(windows_after == [hwnd], "static_window_set_unchanged_after_capture",
         record["owned_windows_after_capture"],
         "the owned visible top-level window set changed across the screenshot")
    changed = {key: [pre[key], post[key]] for key in STABLE if pre[key] != post[key]}
    need(not changed, "window_state_stable_across_capture",
         {"hwnd": pre["hwnd_hex"], "changed": changed, "checked_fields": list(STABLE)},
         f"owned window identity/client/DPI changed across capture: {changed}")

    record["foreground"]["after"] = H.foreground_evidence(user32, hwnd, args.pid)
    after_probes = H.run_probes(user32, hwnd, pre, probe_list)
    record["occlusion"]["probes_after"] = after_probes
    record["occlusion"]["all_owned_after"] = all(p["owned"] for p in after_probes)
    need(record["occlusion"]["all_owned_after"], "occlusion_probes_after_capture_all_owned",
         H.occlusion_detail(after_probes),
         "another window covered the owned client after capture")
    check_pixels(record, image, pre, sample_list, scale, fixed)


def run(args):
    case_dir = args.case_dir
    try:
        case_dir.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        print(f"capture-component-dispatch: cannot create {case_dir}: {exc}", file=sys.stderr)
        return 1
    json_path = case_dir / f"window-stage-{args.stage}.json"
    record = new_record(args.stage, args.pid, case_dir)
    try:
        for stale in (case_dir / f"window-stage-{args.stage}.png", json_path):
            try:
                stale.unlink()
            except FileNotFoundError:
                pass
            except OSError as exc:
                raise E(f"cannot remove stale {stale}: {exc}") from exc

        user32 = H.load_win32()
        record["thread_dpi_awareness"] = H.set_thread_dpi_awareness(user32)
        H.add_check(CHECKS, "thread_dpi_awareness_per_monitor_v2", True,
                    record["thread_dpi_awareness"])
        metadata_path = case_dir / f"dispatch-stage-{args.stage}.json"
        record["metrics"], scale, browsers = validate(metadata_path, args.pid, args.stage)
        record["browser_ids"] = browsers
        record["metadata_path"] = str(metadata_path)
        record["metadata_sha256"] = H.sha256_file(metadata_path)
        record["scale_factor"] = scale
        H.add_check(CHECKS, "metadata_file_valid", True, str(metadata_path))
        fixed = EXPECTED[args.stage]

        windows = H.enum_visible_top_level_windows(user32, args.pid)
        record["owned_windows_for_pid"] = {"count": len(windows),
                                           "handles_hex": [hex(h) for h in windows]}
        need(len(windows) == 1, "exactly_one_visible_top_level_window_for_pid",
             record["owned_windows_for_pid"],
             f"pid {args.pid} must own exactly one visible top-level window, found {len(windows)}")
        hwnd = windows[0]
        record["hwnd"] = int(hwnd)
        pre = H.inspect_window(user32, hwnd)
        record["window"]["pre"] = pre
        bbox = [pre["client_left"], pre["client_top"], pre["client_right"], pre["client_bottom"]]
        record["client_bbox_physical"] = bbox
        need(pre["owner_pid"] == args.pid and pre["title"] == TITLE and pre["visible"]
             and not pre["iconic"], "window_identity_visible_not_minimized",
             {"expected_pid": args.pid, "expected_title": TITLE, "hwnd": pre["hwnd_hex"],
              "pid": pre["owner_pid"], "title": pre["title"], "visible": pre["visible"],
              "iconic": pre["iconic"]},
             f"HWND {pre['hwnd_hex']} is not the visible, non-minimized owned title {TITLE!r}")
        nominal = [H.round_half_up(HOST_DIP[0] * scale), H.round_half_up(HOST_DIP[1] * scale)]
        deviation = [pre["client_width"] - nominal[0], pre["client_height"] - nominal[1]]
        need(abs(deviation[0]) <= 1 and abs(deviation[1]) <= 1,
             "client_physical_size_from_host_680x440_dip",
             {"actual": [pre["client_width"], pre["client_height"]], "nominal": nominal,
              "deviation": deviation, "allowed_deviation": 1},
             f"owned client {pre['client_width']}x{pre['client_height']} is not 680x440 DIP "
             f"at scale {scale} within the allowed one Win32 pixel")
        need(f32(pre["dpi"] / 96.0) == f32(scale), "window_dpi_matches_metadata_scale_factor",
             {"window_dpi": pre["dpi"], "dpi_scale": pre["dpi"] / 96.0, "metadata": scale,
              "rule": "f32 exact"},
             f"window DPI scale {pre['dpi'] / 96.0} does not match metadata scale {scale}")
        if args.stage > 0:
            chain(record, args, hwnd, bbox, scale, browsers)

        screen, primary = H.virtual_screen(user32), H.primary_screen(user32)
        record["virtual_screen"], record["primary_screen"] = screen, primary
        need(pre["client_left"] >= screen["left"] and pre["client_top"] >= screen["top"]
             and pre["client_right"] <= screen["left"] + screen["width"]
             and pre["client_bottom"] <= screen["top"] + screen["height"],
             "client_bbox_fully_on_virtual_screen",
             {"bbox": bbox, "virtual_screen": screen},
             "owned client bbox is not fully inside the desktop")
        grab(args, record, user32, hwnd, pre, bbox, scale, fixed)

    except E as exc:
        record["error"] = str(exc)
    except KeyboardInterrupt:
        record["error"] = "interrupted before evidence completed"
    except Exception as exc:
        record["error"] = f"unexpected {type(exc).__name__}: {exc}"
        record["traceback"] = traceback.format_exc()

    record["finished_utc"] = now()
    record["failed_checks"] = [c["name"] for c in CHECKS if not c["passed"]]
    if record["error"] is None and record["failed_checks"]:
        record["error"] = "failed checks: " + ", ".join(record["failed_checks"])
    record["success"] = (record["error"] is None and bool(CHECKS)
                         and all(c["passed"] for c in CHECKS))
    record["pid"], record["hwnd"] = int(args.pid), int(record["hwnd"] or 0)
    record["stage"] = int(args.stage)
    try:
        H.write_json_atomic(json_path, record)
    except (OSError, TypeError, ValueError) as exc:
        print(f"capture-component-dispatch: cannot write {json_path}: {exc}", file=sys.stderr)
        return 1
    print(f"window-stage-{args.stage}: {'PASS' if record['success'] else 'FAIL'} "
          f"hwnd={hex(record['hwnd'])} points={len(record['points'])} -> {json_path}",
          flush=True)
    if record["error"] is not None:
        print(f"window-stage-{args.stage}: error: {record['error']}", file=sys.stderr,
              flush=True)
    return 0 if record["success"] else 1


def main():
    parser = H.StrictParser(
        description="Capture one declared dispatch stage of the owned GPUI-CEF host window",
        allow_abbrev=False)
    parser.add_argument("--pid", type=int, required=True, help="owning host process id")
    parser.add_argument("--case-dir", type=Path, required=True, help="artifact directory")
    parser.add_argument("--stage", type=int, choices=range(0, 6), required=True)
    args = parser.parse_args(sys.argv[1:])
    if args.pid <= 0:
        parser.error("--pid must be a positive process id")
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
