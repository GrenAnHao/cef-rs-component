#!/usr/bin/env python3
"""Actual owned GPUI client pixels and opt-in, attributed complete select clicks.

Raw OSR bytes only provide expected composition; they are never screenshot evidence.
A passing PNG is a separate real desktop client grab with pre/post owner probes.
"""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import importlib.util
import json
import math
import struct
import sys
import traceback
from ctypes import wintypes as wt
from pathlib import Path

from PIL import Image, ImageChops

spec = importlib.util.spec_from_file_location(
    'component_host_capture', Path(__file__).with_name('capture-component-host.py')
)
H = importlib.util.module_from_spec(spec)
spec.loader.exec_module(H)
MAGIC = 0x43464530
TITLES = {'a': 'GPUI-CEF public host', 'b': 'GPUI-CEF popup peer'}
TARGETS = ['a'] * 4 + ['b'] * 5
POPUPS = {1, 3, 5, 7}
CLICKS = {0, 1, 2, 4, 5, 6}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(path):
    return json.loads(path.read_text(encoding='utf-8-sig'))


def require(ok, message):
    if not ok:
        raise H.CaptureError(message)


def integer(value, label, minimum=None):
    require(type(value) is int, f'{label} is not an integer')
    require(minimum is None or value >= minimum, f'{label} below minimum')
    return value


def number(value, label):
    require(type(value) in (int, float) and math.isfinite(value), f'{label} is not finite numeric')
    return value


def f32(v):
    return struct.unpack('<f', struct.pack('<f', v))[0]


def px(v, s):
    return math.floor(v * s + 0.5)


def raw_frame(case, record, browser, surface):
    require(isinstance(record, dict), f'{surface} raw missing')
    file = record.get('file')
    require(type(file) is str and Path(file).name == file, 'raw path is not a case-local file name')
    path = (case / file).resolve()
    require(path.parent == case.resolve(), 'raw escaped case')
    require(record.get('browser') == browser and record.get('surface') == surface, 'raw identity mismatch')
    w = integer(record.get('width'), 'raw width', 1)
    h = integer(record.get('height'), 'raw height', 1)
    require(w * h * 4 == integer(record.get('bytes'), 'raw bytes', 1), 'raw dimensions/byte count mismatch')
    integer(record.get('generation'), 'raw generation', 1)
    integer(record.get('sequence'), 'raw sequence', 1)
    data = path.read_bytes()
    require(len(data) == w * h * 4, 'actual raw file length mismatch')
    return data, w, h, {'file': file, 'sha256': sha(path), 'bytes': len(data)}


def expected_view(case, meta, width, height):
    """Production verifier: 1:1 BGRA and integer premultiplied source-over, no filter."""
    browser = meta['browser']
    data, w, h, vr = raw_frame(case, meta['view_raw'], browser, 'View')
    diag = meta['diagnostics']
    scale = meta['scale_factor']
    view_bounds = meta['view_bounds']
    device_size = tuple(math.ceil(f32(f32(d) * f32(scale))) for d in view_bounds[2:])
    require((w, h) == device_size, 'view raw is not the current f32-ceil target')
    require(diag['surface_bounds'] == view_bounds, 'actual painted content geometry differs from declared content')
    require((w, h) == (diag['frame_width'], diag['frame_height']), 'view raw != displayed dimensions')
    require(meta['view_raw']['sequence'] == diag['painted_sequence'] and meta['view_raw']['generation'] == diag['painted_generation'], 'view raw != actual painted identity')
    require(width <= w and height <= h, 'view raw does not cover physical content clip')
    require(all(a == 255 for a in data[3::4]), 'opaque fixed page VIEW unexpectedly has alpha')
    out = bytearray(width * height * 3)
    for y in range(height):
        for x in range(width):
            si = (y * w + x) * 4
            di = (y * width + x) * 3
            out[di:di + 3] = bytes((data[si + 2], data[si + 1], data[si]))
    evidence = {'view_raw': vr, 'popup_raw': None, 'popup_pixels_visible': 0, 'popup_pixels_clipped': 0, 'popup_partial_alpha_pixels': 0}
    if meta['popup']['visible']:
        pop, pw, ph, pr = raw_frame(case, meta['popup_raw'], browser, 'Popup')
        pdiag = diag['popup']
        require((pw, ph) == (pdiag['frame_width'], pdiag['frame_height']), 'popup raw != displayed dimensions')
        require(meta['popup_raw']['generation'] == pdiag['painted_generation'] and meta['popup_raw']['sequence'] == pdiag['painted_sequence'], 'popup raw != actual painted identity')
        bounds = meta['popup']['bounds']
        ox, oy = px(bounds[0], scale), px(bounds[1], scale)
        expected_bounds = [view_bounds[0] + bounds[0], view_bounds[1] + bounds[1], f32(f32(pw) / f32(scale)), f32(f32(ph) / f32(scale))]
        require(all(f32(a) == f32(b) for a, b in zip(pdiag['painted_bounds'], expected_bounds)) and len(pdiag['painted_bounds']) == 4, 'popup was shifted or stretched instead of clipped')
        require(meta['popup_raw']['generation'] == meta['popup']['generation'], 'popup raw belongs to another visible generation')
        x0, y0 = max(0, ox), max(0, oy)
        x1, y1 = min(width, ox + pw), min(height, oy + ph)
        require(x1 > x0 and y1 > y0, 'real popup has no visible content intersection')
        partial = 0
        for y in range(y0, y1):
            for x in range(x0, x1):
                si = ((y - oy) * pw + x - ox) * 4
                di = (y * width + x) * 3
                a = pop[si + 3]
                rgb = (pop[si + 2], pop[si + 1], pop[si])
                require(all(c <= a for c in rgb), 'popup is not premultiplied BGRA')
                partial += 0 < a < 255
                for c in range(3):
                    out[di + c] = min(255, rgb[c] + (out[di + c] * (255 - a) + 127) // 255)
        evidence.update(popup_raw=pr, popup_pixels_visible=(x1 - x0) * (y1 - y0), popup_pixels_clipped=pw * ph - (x1 - x0) * (y1 - y0), popup_partial_alpha_pixels=partial, popup_origin_physical=[ox, oy], popup_clip_physical=[x0, y0, x1, y1])
    else:
        require(meta.get('popup_raw') is None, 'hidden popup still provides displayed raw')
    return Image.frombytes('RGB', (width, height), bytes(out)), evidence


def validate_meta(meta, args):
    require(type(meta) is dict, 'metadata is not object')
    require(integer(meta.get('schema'), 'schema') == 1 and integer(meta.get('stage'), 'stage') == args.stage and integer(meta.get('pid'), 'pid', 1) == args.pid, 'metadata schema/stage/pid mismatch')
    target = TARGETS[args.stage]
    require(meta.get('target') == target and meta.get('title') == TITLES[target], 'unexpected target/title')
    bid = meta.get('browser')
    require(type(bid) is list and len(bid) == 2 and all(type(v) is int and v > 0 for v in bid), 'invalid full BrowserId')
    s = number(meta.get('scale_factor'), 'scale')
    require(s > 0, 'nonpositive scale')
    expected = [0, 48, 320, 240] if target == 'a' else [0, 48, 360, 260]
    require(meta.get('view_bounds') == expected, 'unexpected independent window content geometry')
    pop = meta.get('popup')
    require(type(pop) is dict and type(pop.get('visible')) is bool, 'invalid popup visibility')
    require(pop['visible'] == (args.stage in POPUPS), 'wrong real popup visibility for fixed stage')
    b = pop.get('bounds')
    require(type(b) is list and len(b) == 4 and all(type(v) is int for v in b), 'invalid popup DIP bounds')
    integer(pop.get('generation'), 'popup generation')
    if pop['visible']:
        require(b[2] > 0 and b[3] > 0, 'visible popup bounds invalid')
    click = meta.get('next_click')
    require((click is not None) == (args.stage in CLICKS), 'unexpected click stage')
    if click is not None:
        require(type(click) is dict, 'click is not object')
        x = integer(click.get('x'), 'click x', 0)
        y = integer(click.get('y'), 'click y', 0)
        require(x < expected[2] and y < expected[3], 'click outside content clip')
        require(args.interactive_input_approved, 'real input requires explicit --interactive-input-approved')
    return s, expected


def probes(pre, view, scale, popup):
    result = []
    for row, fy in enumerate((1 / 6, .5, 5 / 6)):
        for col, fx in enumerate((1 / 6, .5, 5 / 6)):
            result.append({'name': f'client-{col}-{row}', 'client_px': [int(pre['client_width'] * fx), int(pre['client_height'] * fy)]})
    x, y, w, h = view
    for row, fy in enumerate((.25, .5, .75)):
        for col, fx in enumerate((.25, .5, .75)):
            result.append({'name': f'view-{col}-{row}', 'client_px': [px(x + w * fx, scale), px(y + h * fy, scale)]})
    result.append({'name': 'bare-header', 'client_px': [pre['client_width'] - 20, px(24, scale)]})
    if popup['visible']:
        bx, by, bw, bh = popup['bounds']
        x0 = max(0, bx)
        y0 = max(0, by)
        x1 = min(w, bx + bw)
        y1 = min(h, by + bh)
        require(x1 > x0 and y1 > y0, 'popup has empty visible DIP clip')
        for row, fy in enumerate((.25, .5, .75)):
            for col, fx in enumerate((.25, .5, .75)):
                result.append({'name': f'popup-{col}-{row}', 'client_px': [px(x + x0 + (x1 - x0) * fx, scale), px(y + y0 + (y1 - y0) * fy, scale)]})
    return result


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [('dx', wt.LONG), ('dy', wt.LONG), ('mouseData', wt.DWORD), ('dwFlags', wt.DWORD), ('time', wt.DWORD), ('dwExtraInfo', ctypes.c_size_t)]


class INPUTUNION(ctypes.Union):
    _fields_ = [('mi', MOUSEINPUT), ('padding', ctypes.c_ubyte * 32)]


class INPUT(ctypes.Structure):
    _fields_ = [('type', wt.DWORD), ('u', INPUTUNION)]


def click_owned(u, hwnd, meta, pre, scale):
    require(ctypes.sizeof(INPUT) == 40 and ctypes.sizeof(MOUSEINPUT) == 32, 'not x64 SendInput ABI')
    u.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
    u.SetCursorPos.restype = wt.BOOL
    u.GetCursorPos.argtypes = [ctypes.POINTER(wt.POINT)]
    u.GetCursorPos.restype = wt.BOOL
    u.SendInput.argtypes = [wt.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
    u.SendInput.restype = wt.UINT
    require(H.pointer_value(u.GetForegroundWindow()) == hwnd, 'target is not foreground before opt-in input')
    p = meta['next_click']
    x = px(meta['view_bounds'][0] + p['x'], scale)
    y = px(meta['view_bounds'][1] + p['y'], scale)
    require(0 <= x < pre['client_width'] and 0 <= y < pre['client_height'], 'physical click outside client')
    sx, sy = pre['client_left'] + x, pre['client_top'] + y
    require(H.pointer_value(u.GetAncestor(u.WindowFromPoint(wt.POINT(sx, sy)), 2)) == hwnd, 'click target point is not owned')
    old = wt.POINT()
    require(u.GetCursorPos(ctypes.byref(old)) != 0, 'GetCursorPos failed')
    require(u.SetCursorPos(sx, sy) != 0, 'SetCursorPos failed')
    actual = wt.POINT()
    require(u.GetCursorPos(ctypes.byref(actual)) != 0 and (actual.x, actual.y) == (sx, sy), 'cursor did not reach exact owned point')
    require(H.inspect_window(u, hwnd) == pre, 'window identity/geometry changed before input')
    require(H.pointer_value(u.GetForegroundWindow()) == hwnd and H.pointer_value(u.GetAncestor(u.WindowFromPoint(actual), 2)) == hwnd, 'owner/foreground changed before SendInput')
    events = (INPUT * 2)()
    for i, flags in enumerate((2, 4)):
        events[i].type = 0
        events[i].u.mi = MOUSEINPUT(0, 0, 0, flags, 0, MAGIC)
    accepted = int(u.SendInput(2, events, ctypes.sizeof(INPUT)))
    require(accepted == 2, f'SendInput accepted {accepted}/2; partial input is failure')
    return {'submitted': True, 'accepted_events': accepted, 'marker': MAGIC, 'client_physical': [x, y], 'screen_physical': [sx, sy], 'previous_cursor': [old.x, old.y], 'cursor_restored': False, 'note': 'queue insertion is not GPUI/CEF completion; host must verify full attributed WM pair, component submit and real DOM/popup change'}


def run(args):
    case = args.case_dir.resolve()
    path = case / f'popup-stage-{args.stage}.json'
    record = {'schema': 1, 'kind': 'component-popup-owned-window', 'stage': args.stage, 'pid': args.pid, 'hwnd': 0, 'success': False, 'error': None, 'input': None}
    try:
        require(sys.platform == 'win32' and args.pid > 0, 'Windows owned positive PID required')
        meta = load(path)
        scale, view = validate_meta(meta, args)
        record.update(metadata=meta, metadata_sha256=sha(path))
        u = H.load_win32()
        record['thread_dpi_awareness'] = H.set_thread_dpi_awareness(u)
        windows = H.enum_visible_top_level_windows(u, args.pid)
        expected = meta.get('windows')
        require(type(expected) is list, 'missing complete owned window set')
        expected_set = set()
        for item in expected:
            require(item.get('slot') in TITLES and item.get('title') == TITLES[item['slot']], 'bad window slot/title')
            expected_set.add(integer(item.get('hwnd'), 'window hwnd', 1))
        require(len(expected_set) == (2 if args.stage < 4 else 1) and set(windows) == expected_set, 'actual visible owned windows differ from declared set')
        closed_a = None
        if args.stage >= 4:
            a_stage = load(case / 'popup-window-3.json')
            require(a_stage.get('success') is True and a_stage.get('pid') == args.pid and a_stage.get('metadata', {}).get('target') == 'a', 'missing prior passing A window identity')
            closed_a = integer(a_stage.get('hwnd'), 'former A hwnd', 1)
            record['former_a_hwnd'] = closed_a
            record['a_hwnd_exists_before'] = bool(u.IsWindow(closed_a))
            require(not record['a_hwnd_exists_before'], 'former A HWND still exists or was reused; visible-set absence is not destruction proof')
        hwnd = integer(meta['window'].get('hwnd'), 'target hwnd', 1)
        require(hwnd in expected_set, 'target is not registered owned window')
        pre = H.inspect_window(u, hwnd)
        require(pre['owner_pid'] == args.pid and pre['title'] == meta['title'] and pre['visible'] and not pre['iconic'], 'target identity/title/visibility mismatch')
        require(f32(pre['dpi'] / 96) == f32(scale) == f32(meta['diagnostics']['scale_factor']), 'DPI mismatch, no scale fallback')
        for k, m in [('client_left', 'left'), ('client_top', 'top'), ('client_width', 'width'), ('client_height', 'height'), ('dpi', 'dpi'), ('owner_pid', 'pid')]:
            require(pre[k] == meta['window'][m], f'host/OS window {k} mismatch')
        host_dip = (680, 440) if meta['target'] == 'a' else (520, 400)
        require(abs(pre['client_width'] - px(host_dip[0], scale)) <= 1 and abs(pre['client_height'] - px(host_dip[1], scale)) <= 1, 'owned client size differs from the fixed independent host geometry')
        record['window_pre'] = pre
        record['hwnd'] = hwnd
        record['owned_windows_pre'] = windows
        record['foreground_before'] = H.foreground_evidence(u, hwnd, args.pid)
        spec = probes(pre, view, scale, meta['popup'])
        before = H.run_probes(u, hwnd, pre, spec)
        record['probes_before'] = before
        require(all(p['owned'] for p in before), 'another window covers target before capture')
        bbox = [pre['client_left'], pre['client_top'], pre['client_right'], pre['client_bottom']]
        screen = H.virtual_screen(u)
        primary = H.primary_screen(u)
        def inside(area):
            return bbox[0] >= area['left'] and bbox[1] >= area['top'] and bbox[2] <= area['left'] + area['width'] and bbox[3] <= area['top'] + area['height']
        require(inside(screen), 'owned client is not wholly inside the actual desktop')
        all_screens = not inside(primary)
        record.update(virtual_screen=screen, primary_screen=primary, grab_all_screens=all_screens)
        image = H.ImageGrab.grab(bbox=tuple(bbox), all_screens=all_screens)
        png = case / f'popup-window-{args.stage}.png'
        H.save_png_atomic(image, png)
        record.update(png_path=str(png), png_sha256=sha(png), bbox_physical=bbox, image_mode=image.mode, image_size=list(image.size))
        require(image.mode == 'RGB' and image.size == (pre['client_width'], pre['client_height']), 'not unfiltered RGB owned-client capture')
        post = H.inspect_window(u, hwnd)
        after = H.run_probes(u, hwnd, pre, spec)
        record.update(window_post=post, probes_after=after, owned_windows_post=H.enum_visible_top_level_windows(u, args.pid))
        require(pre == post and set(record['owned_windows_post']) == expected_set and all(p['owned'] for p in after), 'owned identity/window-set/occlusion changed during capture')
        if closed_a is not None:
            record['a_hwnd_exists_after'] = bool(u.IsWindow(closed_a))
            require(not record['a_hwnd_exists_after'], 'former A HWND exists or was reused after capture; refusing to classify it as the old A')
        x0, y0 = px(view[0], scale), px(view[1], scale)
        x1, y1 = px(view[0] + view[2], scale), px(view[1] + view[3], scale)
        actual = image.crop((x0, y0, x1, y1))
        if args.stage == 8:
            require(meta.get('view_raw') is None and not meta['popup']['visible'], 'detached target still provides an active surface')
            expected_image = Image.new('RGB', actual.size, (0, 170, 68))
            raw_evidence = {}
        else:
            expected_image, raw_evidence = expected_view(case, meta, actual.width, actual.height)
        diff = ImageChops.difference(actual, expected_image)
        bboxdiff = diff.getbbox()
        record.update(viewport_rect=[x0, y0, x1, y1], composition=raw_evidence, pixel_difference_bbox=bboxdiff, comparison='all actual viewport RGB pixels vs 1:1 actual owned BGRA VIEW then clipped premultiplied POPUP; integer source-over round nearest; zero tolerance')
        require(bboxdiff is None, 'actual GPUI VIEW/POPUP composition pixels differ from owned sources')
        if args.stage in (1, 3):
            require(raw_evidence['popup_pixels_clipped'] > 0, 'A popup did not actually exercise out-of-content clipping')
        header_point = (pre['client_width'] - 20, px(24, scale))
        record['header_rgb'] = list(image.getpixel(header_point))
        require(tuple(record['header_rgb']) == (159, 34, 34), 'host bare red header changed')
        if args.stage > 0:
            previous = load(case / f'popup-window-{args.stage - 1}.json')
            require(previous.get('success') is True, 'previous stage failed/missing')
            require(previous['pid'] == args.pid, 'previous stage belongs to another PID')
            if TARGETS[args.stage] == TARGETS[args.stage - 1]:
                require(previous['hwnd'] == hwnd and previous['window_post'] == pre and previous['metadata']['browser'] == meta['browser'], 'same-target chain changed window/DPI/browser')
        if meta.get('next_click') is not None:
            record['input'] = click_owned(u, hwnd, meta, pre, scale)
        record['success'] = True
    except Exception as exc:
        record['error'] = f'{type(exc).__name__}: {exc}'
        record['traceback'] = traceback.format_exc()
    record['limits'] = 'Separate non-atomic owned probes/capture; not full/transparent overlay isolation or screenshot alpha. OSR raw only expected pixels. Input opt-in modifies cursor; no security/foreground-policy bypass. Actual WM/GPUI/CEF completion is independently verified by host.'
    H.write_json_atomic(case / f'popup-window-{args.stage}.json', record)
    print(f'popup-window-{args.stage}: {"PASS" if record["success"] else "FAIL"}: {record["error"]}', flush=True)
    return 0 if record['success'] else 1


def main():
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument('--pid', type=int, required=True)
    parser.add_argument('--case-dir', type=Path, required=True)
    parser.add_argument('--stage', type=int, choices=range(9), required=True)
    parser.add_argument('--interactive-input-approved', action='store_true')
    return run(parser.parse_args())


if __name__ == '__main__':
    sys.exit(main())
