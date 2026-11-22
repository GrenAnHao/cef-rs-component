#!/usr/bin/env python3
"""Opt-in 11.1 input driver: separate real owned-client RGB and attributed OS input.

One --stage invocation per live host stage, one PID per mode/backend case. No
clipboard, Unicode SendInput, IME claim, eval/CDP, deliberately targeted
foreign windows, foreground-policy bypass or unmarked synthetic ALT. Discrete
pre/post owner probes cannot exclude an intervening transparent occluder. The host, not SendInput's
return code, checks raw WM messages, component admission, native execution,
cleanup and fixed-fixture DOM counters/digest.
"""
from __future__ import annotations

import argparse
import ctypes
from ctypes import wintypes as wt
import hashlib
import importlib.util
import json
import math
import struct
import sys
import time
import traceback
from pathlib import Path
from PIL import Image, ImageChops

spec = importlib.util.spec_from_file_location('component_host_capture', Path(__file__).with_name('capture-component-host.py'))
H = importlib.util.module_from_spec(spec)
spec.loader.exec_module(H)
MARKER = 0x43464532  # positive full-width exact marker; distinct from popup 0x43464530
TITLE = 'GPUI-CEF public host'
PEER = 'GPUI-CEF input peer'
ACTIONS = ['focus-a', 'type-b-drag-wheel', 'focus-peer', 'focus-a-again', 'none', 'none']


def require(ok, message):
    if not ok:
        raise H.CaptureError(message)


def integer(value, label, minimum=None):
    require(type(value) is int and (minimum is None or value >= minimum), f'invalid {label}')
    return value


def number(value, label):
    require(type(value) in (int, float) and math.isfinite(value), f'invalid {label}')
    return value


def px(value, scale):
    return math.floor(value * scale + .5)


def f32(v):
    return struct.unpack('<f', struct.pack('<f', v))[0]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(path):
    return json.loads(path.read_text(encoding='utf-8-sig'))


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [('dx', wt.LONG), ('dy', wt.LONG), ('mouseData', wt.DWORD), ('dwFlags', wt.DWORD), ('time', wt.DWORD), ('dwExtraInfo', ctypes.c_size_t)]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [('wVk', wt.WORD), ('wScan', wt.WORD), ('dwFlags', wt.DWORD), ('time', wt.DWORD), ('dwExtraInfo', ctypes.c_size_t)]


class INPUTUNION(ctypes.Union):
    _fields_ = [('mi', MOUSEINPUT), ('ki', KEYBDINPUT), ('padding', ctypes.c_ubyte * 32)]


class INPUT(ctypes.Structure):
    _fields_ = [('type', wt.DWORD), ('u', INPUTUNION)]


def win32(u):
    require(sys.platform == 'win32' and ctypes.sizeof(INPUT) == 40 and ctypes.sizeof(MOUSEINPUT) == 32 and ctypes.sizeof(KEYBDINPUT) == 24, 'Windows x64 SendInput ABI required')
    u.SendInput.argtypes = [wt.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
    u.SendInput.restype = wt.UINT
    u.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
    u.SetCursorPos.restype = wt.BOOL
    u.GetCursorPos.argtypes = [ctypes.POINTER(wt.POINT)]
    u.GetCursorPos.restype = wt.BOOL
    u.SetForegroundWindow.argtypes = [wt.HWND]
    u.SetForegroundWindow.restype = wt.BOOL
    u.GetKeyboardLayout.argtypes = [wt.DWORD]
    u.GetKeyboardLayout.restype = ctypes.c_void_p
    u.VkKeyScanExW.argtypes = [wt.WCHAR, ctypes.c_void_p]
    u.VkKeyScanExW.restype = ctypes.c_short
    u.MapVirtualKeyExW.argtypes = [wt.UINT, wt.UINT, ctypes.c_void_p]
    u.MapVirtualKeyExW.restype = wt.UINT
    u.GetAsyncKeyState.argtypes = [ctypes.c_int]
    u.GetAsyncKeyState.restype = ctypes.c_short
    u.GetKeyState.argtypes = [ctypes.c_int]
    u.GetKeyState.restype = ctypes.c_short


def verify_owner(u, fact, title, pid, expected_thread):
    hwnd = integer(fact.get('hwnd'), 'HWND', 1)
    state = H.inspect_window(u, hwnd)
    thread_pid = wt.DWORD()
    owner_thread = int(u.GetWindowThreadProcessId(hwnd, ctypes.byref(thread_pid)))
    require(owner_thread > 0 and owner_thread == integer(fact.get('thread'), 'thread', 1) == expected_thread and thread_pid.value == pid, 'owner thread/PID changed')
    require(state['owner_pid'] == pid and state['title'] == title and state['visible'] and not state['iconic'], 'owned HWND/title/visibility mismatch')
    for real, declared in [('client_left', 'left'), ('client_top', 'top'), ('client_width', 'width'), ('client_height', 'height'), ('dpi', 'dpi'), ('owner_pid', 'pid')]:
        require(state[real] == fact.get(declared), f'owned {real} changed')
    return state


def root_at(u, x, y):
    found = H.pointer_value(u.WindowFromPoint(wt.POINT(x, y)))
    return H.pointer_value(u.GetAncestor(found, 2)) if found else 0


def assert_point(u, hwnd, state, x, y):
    require(0 <= x < state['client_width'] and 0 <= y < state['client_height'], 'input outside owned client')
    sx, sy = state['client_left'] + x, state['client_top'] + y
    require(root_at(u, sx, sy) == hwnd, 'input point covered by foreign HWND')
    return sx, sy


def foreground(u, hwnd):
    if H.pointer_value(u.GetForegroundWindow()) != hwnd:
        require(u.SetForegroundWindow(hwnd), 'ordinary SetForegroundWindow denied')
    require(H.pointer_value(u.GetForegroundWindow()) == hwnd, 'target is not foreground')


def send(u, inp, hwnd, state, x, y):
    sx, sy = assert_point(u, hwnd, state, x, y)
    require(H.pointer_value(u.GetForegroundWindow()) == hwnd, 'foreground changed before attributed SendInput')
    require(root_at(u, sx, sy) == hwnd, 'target changed before attributed SendInput')
    arr = (INPUT * 1)(inp)
    require(int(u.SendInput(1, arr, ctypes.sizeof(INPUT))) == 1, 'partial/rejected SendInput; never count as completion')


def mouse(flags, data=0, dx=0, dy=0):
    result = INPUT()
    result.type = 0
    result.u.mi = MOUSEINPUT(dx, dy, data, flags, 0, MARKER)
    return result


def key_input(vk, scan, up):
    result = INPUT()
    result.type = 1
    result.u.ki = KEYBDINPUT(vk, scan, 2 if up else 0, 0, MARKER)
    return result


def position(u, hwnd, state, x, y):
    sx, sy = assert_point(u, hwnd, state, x, y)
    require(u.SetCursorPos(sx, sy), 'cannot place cursor in owned client')
    actual = wt.POINT()
    require(u.GetCursorPos(ctypes.byref(actual)) and (actual.x, actual.y) == (sx, sy), 'cursor failed to reach exact owned point')
    assert_point(u, hwnd, state, x, y)
    return sx, sy


def require_released_ascii_keys(u):
    # A previously refused B release must stop even the next A, not wait until
    # B's own stage. Read only; never synthesize a release or change Caps Lock.
    protected = (0x41, 0x42, 0x43, 0x10, 0x11, 0x12, 0x5b, 0x5c)
    held = [vk for vk in protected if u.GetAsyncKeyState(vk) & 0x8000]
    require(not held, f'fixture/modifier keys already held {held}; release manually; no key sent')
    require(not (u.GetKeyState(0x14) & 1), 'Caps Lock is on; fixed lowercase ASCII is not proven; no key sent')


def key_ascii(u, hwnd, state, x, y, char, owner_thread):
    require(char in 'abc', 'only three fixed ASCII fixture keys are permitted')
    require_released_ascii_keys(u)
    layout = u.GetKeyboardLayout(owner_thread)
    require(H.pointer_value(layout) != 0, 'owning thread has no keyboard layout')
    mapped = u.VkKeyScanExW(char, layout) & 0xffff
    vk = ord(char.upper())
    require(mapped == vk, f'fixed ASCII key {vk} requires modifiers/dead key in active owner layout; refusing to switch layout or IME')
    scan = int(u.MapVirtualKeyExW(vk, 0, layout))
    require(0 < scan <= 0xff, 'VK to physical scan mapping unavailable')
    require(not (u.GetAsyncKeyState(vk) & 0x8000), 'fixture key already held by another source')
    send(u, key_input(vk, scan, False), hwnd, state, x, y)
    try:
        # Let the owner process TranslateMessage's real WM_CHAR before KEYUP.
        # This is not a substitute for the host's marked WM/DOM/native checks.
        time.sleep(.05)
    finally:
        require(H.pointer_value(u.GetForegroundWindow()) == hwnd and root_at(u, state['client_left']+x, state['client_top']+y) == hwnd, 'owned foreground/point lost while key held; refusing foreign KEYUP')
        send(u, key_input(vk, scan, True), hwnd, state, x, y)


def click(u, hwnd, state, x, y):
    position(u, hwnd, state, x, y)
    send(u, mouse(2), hwnd, state, x, y)
    require(H.pointer_value(u.GetForegroundWindow()) == hwnd and root_at(u, state['client_left']+x, state['client_top']+y) == hwnd, 'owned foreground/point lost while mouse held; refusing foreign UP')
    send(u, mouse(4), hwnd, state, x, y)


def drag(u, hwnd, state, x0, y0, x1, y1):
    position(u, hwnd, state, x0, y0)
    send(u, mouse(2), hwnd, state, x0, y0)
    try:
        sx, sy = assert_point(u, hwnd, state, x1, y1)
        virtual = H.virtual_screen(u)
        require(virtual['width'] > 1 and virtual['height'] > 1, 'invalid virtual screen for absolute move')
        ax = round((sx - virtual['left']) * 65535 / (virtual['width'] - 1))
        ay = round((sy - virtual['top']) * 65535 / (virtual['height'] - 1))
        require(0 <= ax <= 65535 and 0 <= ay <= 65535, 'absolute move outside owned desktop')
        send(u, mouse(0x0001 | 0x8000 | 0x4000, 0, ax, ay), hwnd, state, x1, y1)
        point = wt.POINT()
        require(u.GetCursorPos(ctypes.byref(point)) and (point.x, point.y) == (sx, sy), 'absolute move rounded away from owned target; cannot count WM_MOVE')
    finally:
        if H.pointer_value(u.GetForegroundWindow()) == hwnd:
            cursor = wt.POINT()
            if u.GetCursorPos(ctypes.byref(cursor)):
                cx, cy = cursor.x - state['client_left'], cursor.y - state['client_top']
                if 0 <= cx < state['client_width'] and 0 <= cy < state['client_height'] and root_at(u, cursor.x, cursor.y) == hwnd:
                    send(u, mouse(4), hwnd, state, cx, cy)


def capture(u, case, stage, meta, a, a_hwnd, record):
    scale = number(meta.get('scale_factor'), 'scale_factor')
    require(scale > 0 and f32(scale) == f32(a['dpi'] / 96), 'owned window DPI mismatch')
    bounds = meta.get('view_bounds')
    require(bounds == [0, 48, 320, 240], 'unexpected fixed Input VIEW geometry')
    spec_points = [
        {'name': f'client-{i}', 'client_px': [int(a['client_width'] * fraction), int(a['client_height'] * fraction)]}
        for i, fraction in enumerate((1/6, .5, 5/6))
    ] + [{'name': f'view-{i}', 'client_px': [px(70 + i * 65, scale), px(48 + 50 + i * 50, scale)]} for i in range(3)]
    before = H.run_probes(u, a_hwnd, a, spec_points)
    record['capture_evidence'] = {'owner_pre': a, 'probes_pre': before, 'png_saved': False}
    require(all(p['owned'] for p in before), 'foreign window covers owned client before capture')
    box = (a['client_left'], a['client_top'], a['client_right'], a['client_bottom'])
    virtual, primary = H.virtual_screen(u), H.primary_screen(u)
    primary['left'], primary['top'] = 0, 0
    def contained(s):
        return box[0] >= s['left'] and box[1] >= s['top'] and box[2] <= s['left'] + s['width'] and box[3] <= s['top'] + s['height']
    require(contained(virtual), 'owned client outside desktop')
    image = H.ImageGrab.grab(bbox=box, all_screens=not contained(primary))
    png = case / f'input-window-{stage}.png'
    post = H.inspect_window(u, a_hwnd)
    after = H.run_probes(u, a_hwnd, a, spec_points)
    record['capture_evidence'].update(owner_post=post, probes_post=after,
                                      mode=image.mode, size=list(image.size))
    require(post == a and all(p['owned'] for p in after), 'owned geometry/occlusion changed during capture')
    require(image.mode == 'RGB' and image.size == (a['client_width'], a['client_height']), 'not unfiltered owned-client RGB capture')
    x0, y0 = px(0, scale), px(48, scale)
    x1, y1 = px(320, scale), px(288, scale)
    actual = image.crop((x0, y0, x1, y1))
    require(actual.width > 0 and actual.height > 0, 'empty real client VIEW crop')
    raw = meta.get('view_raw')
    if stage == 5:
        require(raw is None and meta.get('detached') is True, 'green host still has attached VIEW')
        expected = Image.new('RGB', actual.size, (0, 170, 68))
        raw_record = None
    else:
        require(type(raw) is dict and raw.get('surface') == 'View' and raw.get('browser') == meta.get('browser'), 'raw frame not attached to this Browser VIEW')
        name = raw.get('file')
        require(type(name) is str and Path(name).name == name and name == f'input-stage-{stage}-view.bgra', 'invalid case-local source name')
        path = case / name
        w = integer(raw.get('width'), 'raw width', 1)
        h = integer(raw.get('height'), 'raw height', 1)
        require(w == math.ceil(f32(f32(320) * f32(scale))) and h == math.ceil(f32(f32(240) * f32(scale))), 'raw VIEW != exact f32 ceil device target')
        require(w >= actual.width and h >= actual.height and raw.get('bytes') == w*h*4, 'raw VIEW geometry/length mismatch')
        diag = meta.get('diagnostics')
        require(type(diag) is dict and diag.get('surface_bounds') == bounds and diag.get('frame_width') == w and diag.get('frame_height') == h and diag.get('painted_sequence') == raw.get('sequence') and diag.get('painted_generation') == raw.get('generation'), 'authoritative source differs from GPUI painted VIEW')
        data = path.read_bytes()
        require(len(data) == w*h*4 and all(alpha == 255 for alpha in data[3::4]), 'source VIEW is not opaque owned BGRA')
        pixels = bytearray(actual.width * actual.height * 3)
        for row in range(actual.height):
            for col in range(actual.width):
                pos = (row*w+col)*4
                dst = (row*actual.width+col)*3
                pixels[dst:dst+3] = bytes((data[pos+2], data[pos+1], data[pos]))
        expected = Image.frombytes('RGB', actual.size, bytes(pixels))
        raw_record = {'file': name, 'sha256': sha(path), 'bytes': len(data)}
    diff = ImageChops.difference(actual, expected).getbbox()
    chrome = tuple(image.getpixel((a['client_width'] - 20, px(24, scale))))
    record['capture_evidence'].update(viewport_physical=[x0, y0, x1, y1],
                                      pixel_difference_bbox=diff, chrome_rgb=list(chrome),
                                      raw=raw_record)
    require(diff is None, f'complete owned-client VIEW differs at {diff}; zero tolerance')
    require(chrome == (159, 34, 34), 'host chrome changed')
    # Never save a PNG before both identity checks AND strict pixel comparison.
    # The grab itself is non-atomic; failure evidence intentionally has no PNG.
    H.save_png_atomic(image, png)
    record['capture_evidence']['png_saved'] = True
    return {'png': str(png), 'sha256': sha(png), 'raw': raw_record, 'viewport_physical': [x0, y0, x1, y1], 'comparison': 'entire real owned-client VIEW RGB versus 1:1 opaque authoritative BGRA; zero tolerance', 'owner_pre': a, 'owner_post': post, 'probes_pre': before, 'probes_post': after, 'pixel_difference_bbox': diff}


def owner_ime_before_key(case, meta, record):
    """One read-only owner request AFTER any real focus click, before key-down.

    A pre-focus unassociated HIMC is not evidence for either ASCII or native
    conversion. The host responds on the HWND owner thread after an actual UI
    turn/draw and the expected pointer prefix. Unknown/native modes still fail.
    """
    stage, pid, hwnd = meta['stage'], meta['pid'], meta['window']['hwnd']
    require(stage in (0, 1, 3), 'no keyboard precondition request in pointer-only stage')
    request = case / f'input-owner-ime-{stage}.request'
    response = case / f'input-owner-ime-{stage}.json'
    require(not request.exists() and not response.exists(), 'duplicate owner IME request/reused stage')
    tag = f'{pid}:{stage}:{hwnd}'
    temporary = request.with_suffix('.pending')
    temporary.write_text(tag, encoding='ascii')
    temporary.replace(request)
    until = time.monotonic() + 2.0
    while not response.is_file():
        require(time.monotonic() < until, 'fresh owner-thread IME response missing before key; no key sent')
        time.sleep(.01)
    fresh = load(response)
    record['owner_ime_precondition'] = {'file': response.name, 'sha256': sha(response), 'response': fresh}
    validate_owner_ime_response(meta, fresh)


def validate_owner_ime_response(meta, fresh):
    stage, pid, hwnd = meta['stage'], meta['pid'], meta['window']['hwnd']
    require(stage in (0, 1, 3), 'owner reply is not for an ASCII keyboard stage')
    tag = f'{pid}:{stage}:{hwnd}'
    expected_raw = 0 if stage == 1 else 2
    require(type(fresh) is dict and fresh.get('schema') == 1 and fresh.get('kind') == 'input-owner-ime'
            and fresh.get('pid') == pid and fresh.get('stage') == stage and fresh.get('request') == tag,
            'fresh owner-thread IME response identity mismatch; no key sent')
    require(fresh.get('raw_count') == expected_raw and fresh.get('keyboard_seen') is False,
            'IME response was not before keyboard and after the exact pointer prefix')
    window = fresh.get('window')
    require(type(window) is dict and all(window.get(k) == meta['window'].get(k)
            for k in ('hwnd', 'pid', 'thread', 'dpi', 'left', 'top', 'width', 'height')),
            'IME response belongs to a different or moved HWND; no key sent')
    require(fresh.get('accepts_text') is True, 'CEF text readiness not confirmed before key')
    require_ascii_owner_ime_snapshot(window.get('ime'))


def owner_peer_after_capture(case, meta, record):
    """Normal peer activation by the already-foreground owner, after capture.

    No synthetic activation keys, foreign activation, policy change or retry.
    """
    require(meta['stage'] == 2 and record.get('capture'), 'peer request must follow original stage-2 capture')
    tag = f"{meta['pid']}:2:{meta['window']['hwnd']}:{meta['peer']['hwnd']}"
    request, response = case / 'input-owner-foreground-2.request', case / 'input-owner-foreground-2.json'
    require(not request.exists() and not response.exists(), 'duplicate peer activation request/reused stage')
    pending = request.with_suffix('.pending')
    pending.write_text(tag, encoding='ascii')
    pending.replace(request)
    until = time.monotonic() + 2.0
    while not response.is_file():
        require(time.monotonic() < until, 'ordinary owner peer activation not confirmed; no peer click sent')
        time.sleep(.01)
    fresh = load(response)
    record['owner_peer_activation'] = {'file': response.name, 'sha256': sha(response), 'response': fresh}
    validate_owner_peer_response(meta, fresh)


def validate_owner_peer_response(meta, fresh):
    tag = f"{meta['pid']}:2:{meta['window']['hwnd']}:{meta['peer']['hwnd']}"
    require(meta['stage'] == 2 and type(fresh) is dict and fresh.get('schema') == 1
            and fresh.get('kind') == 'input-owner-foreground' and fresh.get('pid') == meta['pid']
            and fresh.get('stage') == 2 and fresh.get('request') == tag,
            'owner peer activation response identity mismatch; no peer click sent')
    require(fresh.get('source_was_foreground') is True and fresh.get('target_is_foreground') is True
            and fresh.get('raw_count') == 0, 'peer request did not start at untouched owned foreground cutpoint')
    for key, original in [('source', meta['window']), ('target', meta['peer'])]:
        facts = fresh.get(key)
        require(type(facts) is dict and all(facts.get(k) == original.get(k)
                for k in ('hwnd', 'pid', 'thread', 'dpi', 'left', 'top', 'width', 'height')),
                'peer activation belongs to a different or moved HWND; no peer click sent')


def perform(u, stage, a_hwnd, b_hwnd, a, b, sa, sb, thread, before_key, before_peer):
    require(not (u.GetAsyncKeyState(1) & 0x8000), 'left button already held before fixture')
    if stage == 0:
        foreground(u, a_hwnd)
        x, y = px(90, sa), px(94, sa)
        click(u, a_hwnd, a, x, y)
        before_key()
        key_ascii(u, a_hwnd, a, x, y, 'a', thread)
    elif stage == 1:
        foreground(u, a_hwnd)
        x, y = px(90, sa), px(94, sa)
        before_key()
        key_ascii(u, a_hwnd, a, x, y, 'b', thread)
        drag(u, a_hwnd, a, x, y, px(90, sa), px(24, sa))
        wx, wy = px(250, sa), px(208, sa)
        position(u, a_hwnd, a, wx, wy)
        send(u, mouse(0x800, (-120) & 0xffffffff), a_hwnd, a, wx, wy)
    elif stage == 2:
        before_peer()
        foreground(u, b_hwnd)
        click(u, b_hwnd, b, px(440, sb), px(24, sb))
    elif stage == 3:
        foreground(u, a_hwnd)
        x, y = px(90, sa), px(94, sa)
        click(u, a_hwnd, a, x, y)
        before_key()
        key_ascii(u, a_hwnd, a, x, y, 'c', thread)
    return {'action': ACTIONS[stage], 'marker': MARKER if stage <= 3 else None, 'submitted_only': stage <= 3}


def require_ascii_owner_ime_snapshot(ime):
    require(type(ime) is dict, 'missing owner-thread IME snapshot; external null HIMC is not target-mode proof')
    integer(ime.get('keyboard_layout'), 'owner keyboard layout', 1)
    require(type(ime.get('is_ime')) is bool and type(ime.get('associated')) is bool, 'unknown owner IME association')
    if ime['associated']:
        require(type(ime.get('open')) is bool, 'owner IME open state unavailable')
        conversion = integer(ime.get('conversion'), 'owner IME conversion', 0)
        require(not (ime['open'] and conversion & 1), 'owner reports native IME conversion; use English input manually before ASCII fixture; no key sent')
    else:
        require(not ime['is_ime'], 'owner IME has no queryable context yet; cannot prove ASCII mode; refusing input')


def run(args):
    case = args.case_dir.resolve()
    stage = args.stage
    path = case / f'input-stage-{stage}.json'
    record = {'schema': 1, 'kind': 'component-real-input', 'pid': args.pid, 'stage': stage, 'success': False, 'error': None}
    try:
        require(args.interactive_input_approved, 'real SendInput requires --interactive-input-approved (even for release-only stages)')
        require(sys.platform == 'win32' and args.pid > 0, 'owned Windows PID required')
        meta = load(path)
        require(type(meta) is dict and integer(meta.get('schema'), 'schema') == 1 and integer(meta.get('stage'), 'stage') == stage and integer(meta.get('pid'), 'pid', 1) == args.pid, 'host Input stage identity mismatch')
        require(meta.get('title') == TITLE and meta.get('peer_title') == PEER and meta.get('action') == ACTIONS[stage], 'Input fixture title/action mismatch')
        u = H.load_win32()
        record['thread_dpi_awareness'] = H.set_thread_dpi_awareness(u)
        win32(u)
        actual_set = set(H.enum_visible_top_level_windows(u, args.pid))
        declared = [meta['window']['hwnd'], meta['peer']['hwnd']]
        require(len(set(declared)) == 2 and actual_set == set(declared), 'owned HWNDs are not the exact two unique visible windows')
        thread = integer(meta['window'].get('thread'), 'host owner thread', 1)
        a_hwnd, b_hwnd = declared
        a = verify_owner(u, meta['window'], TITLE, args.pid, thread)
        b = verify_owner(u, meta['peer'], PEER, args.pid, thread)
        require(f32(number(meta['scale_factor'], 'A scale')) == f32(a['dpi'] / 96) and f32(number(meta['peer_scale'], 'B scale')) == f32(b['dpi'] / 96), 'DPI / host scales disagree')
        if stage > 0:
            prior = load(case / f'input-window-{stage-1}.json')
            require(prior.get('success') is True and prior.get('pid') == args.pid and prior.get('hwnds') == declared, 'missing previous same-process passing owned-window stage')
        record.update(metadata_sha256=sha(path), hwnds=declared, owner_thread=thread, foreground_pre=H.foreground_evidence(u, a_hwnd, args.pid))
        record['capture'] = capture(u, case, stage, meta, a, a_hwnd, record)
        require(set(H.enum_visible_top_level_windows(u, args.pid)) == set(declared), 'owned window set changed during stage')
        require(verify_owner(u, meta['window'], TITLE, args.pid, thread) == a and verify_owner(u, meta['peer'], PEER, args.pid, thread) == b, 'owned geometry changed before input')
        if stage <= 3:
            require_released_ascii_keys(u)
        # Pointer focus is allowed only in an already owned foreground window.
        # IME/ASCII proof is refreshed after that focus, before EVERY keyboard
        # down; pointer-only peer focus does not pretend to be an IME test.
        record['input'] = perform(u, stage, a_hwnd, b_hwnd, a, b, meta['scale_factor'], meta['peer_scale'], thread,
                                  lambda: owner_ime_before_key(case, meta, record),
                                  lambda: owner_peer_after_capture(case, meta, record))
        record['foreground_post'] = H.foreground_evidence(u, b_hwnd if stage == 2 else a_hwnd, args.pid)
        record['success'] = True
    except Exception as exc:
        record['error'] = f'{type(exc).__name__}: {exc}'
        record['traceback'] = traceback.format_exc()
        # A failed partial physical gesture may still be held. Never repair it
        # by sending an UP to an unverified foreign foreground/desktop target.
        record['possible_physical_input_held'] = 'unknown; no foreign UP sent'
        if 'u' in locals() and hasattr(u, 'GetAsyncKeyState'):
            record['observed_async_held'] = {
                'left_button': bool(u.GetAsyncKeyState(1) & 0x8000),
                'fixture_key_vks': [vk for vk in (0x41, 0x42, 0x43)
                                    if u.GetAsyncKeyState(vk) & 0x8000],
            }
    record['limits'] = 'Only approved fixed ASCII input. WM_CHAR is associated with marked RAWKEYDOWN, not claimed independently marked; no Chinese IME/caret claim. SendInput acceptance is queue insertion, not native or DOM completion. Discrete owner probes/image grab cannot exclude intervening transparent/foreign pixels; screenshot is not proof of atomic isolation. Peer is only real focus target, not fully input-validated second Browser.'
    H.write_json_atomic(case / f'input-window-{stage}.json', record)
    print(f'input-window-{stage}: {"PASS" if record["success"] else "FAIL"}: {record["error"]}', flush=True)
    return 0 if record['success'] else 1


def main():
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument('--pid', type=int, required=True)
    parser.add_argument('--case-dir', type=Path, required=True)
    parser.add_argument('--stage', type=int, choices=range(6), required=True)
    parser.add_argument('--interactive-input-approved', action='store_true')
    return run(parser.parse_args())


if __name__ == '__main__':
    sys.exit(main())
