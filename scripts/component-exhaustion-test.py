#!/usr/bin/env python3
"""Owned-window recovery-exhaustion slice: read-only real desktop pixels.

The host's non-default validation feature performs one controlled DropView
injection. This observer only reads the host's stage metadata, the one owned
original window and its on-screen client pixels. It never activates, sends
input, moves the cursor, queries IME, resizes, hides or restores a window, and
it never asks for foreground. The unchanged input helper is reused only for its
owner identity checks, never for its input/IME routines.
"""
from __future__ import annotations
import argparse, importlib.util, math, struct, sys, traceback
from pathlib import Path
from PIL import Image, ImageChops

spec = importlib.util.spec_from_file_location('exhaustion_capture_input', Path(__file__).with_name('component-input-test.py'))
I = importlib.util.module_from_spec(spec)
spec.loader.exec_module(I)
H = I.H

EXTENTS = [(320, 240), (500, 300), (320, 240), (320, 240)]
OLD_EXTENT = (320, 240)
HOST_DIP = (600, 450)
FAULT_LABEL = 'STALE: resize recovery exhausted'
MAX_ATTEMPTS = 8
MIN_SPACING_US = 100_000
HOST_BACKGROUND = (21, 21, 21)
DETACHED_GREEN = (0, 170, 68)
STATUSES = ('Idle', 'Pending', 'Recovered', 'Hidden', 'Closed', 'Exhausted')
REQUIRED = ('schema', 'kind', 'pid', 'stage', 'browser', 'window', 'scale_factor', 'view_bounds',
            'diagnostics', 'view_raw', 'stale', 'fault_label', 'recovery', 'baseline_recovery',
            'fault', 'probe', 'quiet_observation_ms', 'source_generation', 'detached',
            'task_11_3_accepted', 'scope', 'input_sent', 'foreground_forced', 'observer_resized', 'initial_foreground')
RECOVERY_FIELDS = ('status', 'generation', 'expected_width', 'expected_height', 'attempts',
                   'requests_total', 'native_invalidate_entries', 'native_thread',
                   'native_offsets_us', 'request_offsets_us', 'acks', 'exhaustions', 'queued',
                   'post_failures')
RECOVERY_INTS = ('generation', 'expected_width', 'expected_height', 'attempts', 'requests_total',
                 'native_invalidate_entries', 'native_thread', 'acks', 'exhaustions',
                 'post_failures')
DIAG_INTS = ('paint_calls', 'painted_sequence', 'painted_generation', 'frame_width',
             'frame_height', 'created_images', 'released_images', 'consumed_updates',
             'binding_revision', 'resync_attempts', 'rejected_paints', 'clicks_submitted',
             'clicks_rejected')
DIAG_BOOLS = ('binding_retired', 'pending_resync', 'pending_click')
POPUP_INTS = ('generation', 'frame_width', 'frame_height', 'created_images', 'released_images',
              'paint_calls', 'painted_sequence', 'painted_generation')
PROBE_FIELDS = ('drop_view', 'applied', 'dropped', 'incomplete_pending', 'incomplete_rejected')
FAULT_FIELDS = ('count', 'browser', 'event_sequence', 'retired')
RAW_FIELDS = ('file', 'browser', 'surface', 'width', 'height', 'bytes', 'generation', 'sequence')


def require(value, message):
    if not value:
        raise H.CaptureError(message)


def f32(n):
    return struct.unpack('<f', struct.pack('<f', n))[0]


def integer(value, label, minimum=0):
    require(type(value) is int and (minimum is None or value >= minimum), f'invalid {label}')
    return value


def boolean(value, label):
    require(type(value) is bool, f'invalid {label}')
    return value


def target(extent, scale):
    return tuple(math.ceil(f32(f32(n) * f32(scale))) for n in extent)


def typed_id(value, label):
    require(type(value) is list and len(value) == 2
            and all(type(x) is int and x > 0 for x in value), f'invalid {label}')
    return value


def integer_list(value, label):
    require(type(value) is list and len(value) <= MAX_ATTEMPTS, f'invalid {label}')
    for item in value:
        integer(item, label)
    return value


def recovery(value, label):
    require(type(value) is dict and all(key in value for key in RECOVERY_FIELDS),
            f'{label} is missing a field')
    require(type(value['status']) is str and value['status'] in STATUSES, f'invalid {label}.status')
    for key in RECOVERY_INTS:
        integer(value[key], f'{label}.{key}')
    boolean(value['queued'], f'{label}.queued')
    integer_list(value['native_offsets_us'], f'{label}.native_offsets_us')
    integer_list(value['request_offsets_us'], f'{label}.request_offsets_us')
    return value


def diagnostics(value, view_bounds, scale, label='diagnostics'):
    require(type(value) is dict and all(key in value for key in DIAG_INTS + DIAG_BOOLS
                                        + ('schema', 'scale_factor', 'surface_bounds', 'popup', 'last_error')),
            f'{label} is missing a field')
    require(integer(value['schema'], f'{label}.schema') == 1, f'invalid {label}.schema')
    require(value['last_error'] is None, f'{label}.last_error is not null')
    require(value['surface_bounds'] is not None and len(value['surface_bounds']) == 4
            and all(type(a) in (int, float) and type(b) in (int, float)
                    and f32(a) == f32(b) for a, b in zip(value['surface_bounds'], view_bounds)),
            f'{label}.surface_bounds is not the stage VIEW bounds')
    require(type(value['scale_factor']) in (int, float) and f32(value['scale_factor']) == f32(scale),
            f'{label}.scale_factor mismatch')
    for key in DIAG_INTS:
        integer(value[key], f'{label}.{key}')
    for key in DIAG_BOOLS:
        boolean(value[key], f'{label}.{key}')
    popup = value['popup']
    require(type(popup) is dict and all(key in popup for key in POPUP_INTS
                                        + ('visible', 'bounds', 'painted_bounds')),
            f'{label}.popup is missing a field')
    boolean(popup['visible'], f'{label}.popup.visible')
    for key in POPUP_INTS:
        integer(popup[key], f'{label}.popup.{key}')
    for key in ('bounds', 'painted_bounds'):
        row = popup[key]
        require(type(row) is list and len(row) == 4
                and all(type(x) in (int, float) and math.isfinite(x) for x in row),
                f'invalid {label}.popup.{key}')
    return value


def foreground_policy(meta, approved=None, manual=None):
    facts = meta.get('initial_foreground')
    require(type(facts) is dict and all(k in facts for k in ('approved', 'attempts', 'before', 'after', 'policy')),
            'missing initial foreground policy evidence')
    boolean(facts['approved'], 'initial foreground approved')
    require(type(facts['attempts']) is int and facts['attempts'] in (0, 1), 'initial foreground request repeated')
    if approved is not None:
        require(facts['approved'] is approved, 'host/observer initial foreground opt-in mismatch')
    manual_policy = facts['policy'] == 'manual-owned-observation'
    if manual is not None:
        require(manual_policy is manual, 'host/observer manual foreground opt-in mismatch')
    if manual_policy:
        require(facts['approved'] is False and facts['attempts'] == 0 and
                type(facts['before']) is bool and facts['after'] is True,
                'manual foreground was not observed or made an ordinary request')
    elif facts['approved']:
        require(type(facts['before']) is bool and facts['after'] is True
                and facts['attempts'] == int(not facts['before'])
                and facts['policy'] == 'one-ordinary-owned-request', 'ordinary owned initial request not confirmed')
    else:
        require(facts['attempts'] == 0 and facts['before'] is None and facts['after'] is None
                and facts['policy'] == 'passive', 'passive host requested foreground')
    return facts


def validate(meta, pid, stage):
    """Pure metadata predicate: no window, file or desktop access."""
    require(stage in range(4), 'unsupported exhaustion stage')
    require(type(meta) is dict and all(key in meta for key in REQUIRED),
            'exhaustion metadata is missing a required field')
    require(all(type(meta[key]) is int for key in ('schema', 'pid', 'stage'))
            and meta['schema'] == 1 and meta['pid'] == pid and meta['stage'] == stage,
            'exhaustion stage identity mismatch')
    require(meta['kind'] == 'recovery-exhaustion-slice' and meta['task_11_3_accepted'] is False,
            'wrong scene kind or task acceptance')
    require(meta['scope'] == 'recovery-exhaustion-slice'
            and all(meta[k] is False for k in ('input_sent', 'foreground_forced', 'observer_resized')),
            'host scope or passive observer declaration mismatch')
    foreground_policy(meta)
    scale = meta['scale_factor']
    require(type(scale) in (int, float) and math.isfinite(scale) and scale > 0,
            'invalid independent scale_factor')
    ids = meta['browser']
    typed_id(ids, 'full BrowserId')
    win = meta['window']
    require(type(win) is dict and all(key in win for key in ('hwnd', 'pid', 'thread', 'dpi',
                                                             'left', 'top', 'width', 'height')),
            'window facts are missing a field')
    require(integer(win['hwnd'], 'window.hwnd', 1) > 0, 'invalid owned HWND')
    require(integer(win['pid'], 'window.pid', 1) == pid, 'window PID mismatch')
    integer(win['thread'], 'window.thread', 1)
    require(integer(win['dpi'], 'window.dpi', 1) > 0, 'invalid window DPI')
    integer(win['left'], 'window.left', None)
    integer(win['top'], 'window.top', None)
    require(integer(win['width'], 'window.width', 1) > 0
            and integer(win['height'], 'window.height', 1) > 0, 'invalid client size')
    require((win['width'], win['height']) == target(HOST_DIP, scale),
            'client size is not the fixed 600x450 DIP target')
    require(f32(win['dpi'] / 96) == f32(scale), 'metadata DPI does not match the host scale')
    w, h = EXTENTS[stage]
    bounds = meta['view_bounds']
    require(bounds == [0, 48, w, h], 'not the fixed stage VIEW bounds')
    vw, vh = target((w, h), scale)
    vx, vy = I.px(bounds[0], scale), I.px(bounds[1], scale)
    require(vx >= 0 and vy >= 0 and win['width'] >= vx + vw and win['height'] >= vy + vh,
            'metadata client cannot contain the entire VIEW')
    d = diagnostics(meta['diagnostics'], bounds, scale)
    require(meta['detached'] is (stage == 3), 'detached flag does not match the release stage')
    boolean(meta['stale'], 'stale')
    require(meta['stale'] is (stage == 1), 'stale flag does not match the exhausted stage')
    if stage == 1:
        require(meta['fault_label'] == FAULT_LABEL, 'stale label text mismatch')
    else:
        require(meta['fault_label'] is None, 'non-exhausted stage reports a stale label')
    probe = meta['probe']
    require(type(probe) is dict and all(key in probe for key in PROBE_FIELDS),
            'probe state is missing a field')
    boolean(probe['drop_view'], 'probe.drop_view')
    boolean(probe['incomplete_pending'], 'probe.incomplete_pending')
    for key in ('applied', 'dropped', 'incomplete_rejected'):
        integer(probe[key], f'probe.{key}')
    require(probe['incomplete_pending'] is False, 'unexpected incomplete-frame injection pending')
    if stage == 1:
        require(probe['drop_view'] is True and probe['dropped'] >= 1 and probe['applied'] >= 1,
                'DropView was not applied before exhaustion')
    else:
        require(probe['drop_view'] is False, 'DropView is still armed outside the exhausted stage')
    rec = recovery(meta['recovery'], 'recovery')
    base = recovery(meta['baseline_recovery'], 'baseline_recovery')
    fault = meta['fault']
    require(type(fault) is dict and all(key in fault for key in FAULT_FIELDS),
            'fault record is missing a field')
    count = integer(fault['count'], 'fault.count')
    boolean(fault['retired'], 'fault.retired')
    if stage == 0:
        require(count == 0 and fault['browser'] is None and fault['retired'] is False,
                'initial stage already reports a stale or retired fault')
        require(fault['event_sequence'] is None
                or (type(fault['event_sequence']) is int and fault['event_sequence'] == 0),
                'initial fault sequence must be empty')
        require(base == rec, 'initial baseline recovery differs from the observed recovery')
    else:
        require(count == 1, 'stale fault count mismatch')
        typed_id(fault['browser'], 'fault.browser')
        require(fault['browser'] == ids, 'stale fault names a different Browser')
        integer(fault['event_sequence'], 'fault.event_sequence', 1)
        require(fault['retired'] is (stage == 3),
                'fault retirement does not match the View release stage')
    quiet = meta['quiet_observation_ms']
    if stage == 1:
        require(integer(quiet, 'quiet_observation_ms') >= 250,
                'quiet observation shorter than 250 ms')
    else:
        require(quiet is None or (type(quiet) is int and quiet >= 0),
                'invalid quiet_observation_ms')
    source_generation = integer(meta['source_generation'], 'source_generation', 1)
    if stage == 3:
        require(meta['view_raw'] is None, 'released stage still reports a raw View frame')
        require(rec['status'] in ('Idle', 'Closed', 'Recovered', 'Hidden'),
                'released View still reports pending/exhausted recovery')
        require(d['frame_width'] == 0 and d['frame_height'] == 0,
                'released View still has a non-empty frame size')
        require(integer(d['created_images'], 'diagnostics.created_images', 1) == d['released_images'],
                'released View images are not balanced')
        return float(scale)
    raw = meta['view_raw']
    require(type(raw) is dict and all(key in raw for key in RAW_FIELDS),
            'raw View frame is missing a field')
    require(raw['file'] == f'exhaustion-stage-{stage}-view.bgra',
            'raw View frame is not the fixed stage-local file')
    typed_id(raw['browser'], 'view_raw.browser')
    require(raw['browser'] == ids and raw['surface'] == 'View',
            'raw frame is not this Browser VIEW')
    for key in ('width', 'height', 'bytes', 'generation', 'sequence'):
        integer(raw[key], f'view_raw.{key}', 1)
    rw, rh = target(OLD_EXTENT, scale)
    require((raw['width'], raw['height']) == (rw, rh),
            'raw View frame is not the fixed 320x240 device target')
    require(raw['bytes'] == rw * rh * 4, 'raw View frame byte length mismatch')
    if stage == 1:
        require(vw > rw and vh > rh,
                'stale layout does not expose the surrounding host background')
    require(d['frame_width'] == rw and d['frame_height'] == rh,
            'painted frame size differs from the raw frame')
    require(integer(d['paint_calls'], 'diagnostics.paint_calls', 1) > 0,
            'raw frame was never actually painted')
    require(raw['generation'] == d['painted_generation']
            and raw['sequence'] == d['painted_sequence'],
            'raw frame is not the actual painted source frame')
    if stage == 1:
        require(rec['status'] == 'Exhausted', 'exhausted stage did not report Exhausted')
        require((rec['expected_width'], rec['expected_height']) == target(EXTENTS[1], scale),
                'exhausted target is not the 500x300 layout')
        require(rec['attempts'] == MAX_ATTEMPTS, 'exhaustion did not stop at exactly 8 attempts')
        requests = rec['request_offsets_us']
        native = rec['native_offsets_us']
        require(len(requests) == MAX_ATTEMPTS and len(native) == MAX_ATTEMPTS,
                'missing exactly 8 request/native offsets')
        for offsets, label in ((requests, 'request'), (native, 'native')):
            require(all(offsets[index] - offsets[index - 1] >= MIN_SPACING_US
                        for index in range(1, len(offsets))),
                    f'{label} offsets are closer than 100 ms')
        require(integer(rec['native_thread'], 'recovery.native_thread', 1) > 0,
                'native Invalidate thread is missing')
        require(rec['requests_total'] == base['requests_total'] + MAX_ATTEMPTS,
                'requests_total is not the baseline plus 8')
        require(rec['native_invalidate_entries']
                == base['native_invalidate_entries'] + MAX_ATTEMPTS,
                'native Invalidate entries are not the baseline plus 8')
        require(rec['acks'] == base['acks'], 'ACK advanced during exhaustion')
        require(rec['exhaustions'] == base['exhaustions'] + 1,
                'exhaustion was not reported exactly once')
        require(rec['queued'] is False and rec['post_failures'] == 0,
                'recovery is still queued or reported post failures')
        require(rec['generation'] == source_generation > raw['generation'],
                'exhausted target generation is not the newer source generation')
    elif stage == 0 and rec['status'] == 'Idle':
        # Initial matching viewport need not arm the resize coordinator.
        # This exception is only the untouched initial baseline, never recovery.
        require(all(rec[key] == 0 for key in RECOVERY_INTS)
                and not rec['native_offsets_us'] and not rec['request_offsets_us']
                and rec['queued'] is False, 'initial Idle coordinator was already used')
        require(source_generation == raw['generation'], 'initial good frame is not current')
    else:
        require(rec['status'] == 'Recovered', 'stage is not Recovered')
        require(rec['generation'] == raw['generation'],
                'recovery generation differs from the painted generation')
        require((rec['expected_width'], rec['expected_height']) == (rw, rh),
                'recovered target differs from the painted frame')
        require(source_generation == raw['generation'],
                'source generation differs from the painted frame')
        if stage == 2:
            require(rec['acks'] > base['acks'], 'natural recovery advanced no ACK')
            require(rec['exhaustions'] == base['exhaustions'] + 1,
                    'recovery did not preserve the single exhaustion')
            require(rec['post_failures'] == 0 and rec['queued'] is False,
                    'recovered View is still queued or reported post failures')
    return float(scale)


def chain(meta, prior, stage, case):
    """Same original window/Browser and monotonic frame history across stages."""
    require(type(prior) is dict and prior.get('success') is True,
            'previous stage did not pass')
    prev = prior.get('metadata')
    require(type(prev) is dict, 'previous stage has no metadata')
    require(prior.get('browser') == meta['browser'], 'Browser changed across stages')
    require(prior.get('owner_thread') == meta['window']['thread'], 'owner thread changed across stages')
    require(all(prev.get('window', {}).get(key) == meta['window'][key]
                for key in ('hwnd', 'pid', 'thread', 'dpi')),
            'original HWND/PID/thread/DPI changed across stages')
    require(f32(prev.get('scale_factor')) == f32(meta['scale_factor']), 'scale changed across stages')
    require(prev.get('initial_foreground') == meta['initial_foreground'], 'initial foreground policy changed or repeated')
    if meta['initial_foreground']['policy'] == 'manual-owned-observation':
        require(prev.get('window') == meta['window'], 'manual Exhaustion client geometry changed across stages')
        for key in ('manual_foreground_pre', 'manual_foreground_post'):
            require(type(prior.get(key)) is int and prior[key] == meta['window']['hwnd'],
                    'previous manual Exhaustion capture did not observe the original foreground HWND')
    require(prev.get('baseline_recovery') == meta['baseline_recovery'],
            'baseline recovery object changed across stages')
    prev_raw, raw = prev.get('view_raw'), meta['view_raw']
    prev_rec = prev.get('recovery')
    require(type(prev_rec) is dict, 'previous stage has no recovery facts')
    if stage == 1:
        require(type(prev_raw) is dict and type(raw) is dict,
                'missing raw frames in the stale transition')
        require(raw['generation'] == prev_raw['generation']
                and raw['sequence'] == prev_raw['sequence'],
                'stale stage replaced the retained source frame')
        require(H.sha256_file(case / prev_raw['file']) == H.sha256_file(case / raw['file']),
                'stale stage retained different pixels than stage 0')
    elif stage == 2:
        require(type(prev_raw) is dict and type(raw) is dict,
                'missing raw frames in the recovery transition')
        require(raw['generation'] > prev_raw['generation']
                and raw['sequence'] > prev_raw['sequence'],
                'recovery frame did not advance generation/sequence')
        require(raw['generation'] > integer(prev_rec.get('generation'), 'prior target generation', 0),
                'recovered frame does not postdate the exhausted target generation')
        prev_probe = prev.get('probe')
        require(type(prev_probe) is dict, 'previous stage has no probe state')
        require(integer(meta['probe']['applied'], 'probe.applied')
                > integer(prev_probe.get('applied'), 'prior probe.applied'),
                'DropView release was not applied between stages')
    elif stage == 3:
        require(type(prev_raw) is dict and raw is None,
                'release stage still reports a View frame')
        prev_probe = prev.get('probe')
        require(type(prev_probe) is dict and prev_probe.get('drop_view') is False,
                'fault injection was still armed before release')


def text_pixels(pixels):
    white = yellow = 0
    for r, g, b in pixels:
        if r >= 180 and g >= 180 and b >= 180:
            white += 1
        elif r >= 170 and g >= 120 and b <= 160 and r - b >= 50:
            yellow += 1
    return white, yellow


def run(args):
    case, stage = args.case_dir.resolve(), args.stage
    record = {'schema': 1, 'kind': 'recovery-exhaustion-owned-window',
              'scope': 'recovery-exhaustion-slice', 'pid': args.pid, 'stage': stage,
              'success': False, 'error': None, 'task_11_3_accepted': False,
              'input_sent': False, 'foreground_forced': False, 'observer_resized': False}
    try:
        require(sys.platform == 'win32', 'real Windows desktop required')
        path = case / f'exhaustion-stage-{stage}.json'
        meta = I.load(path)
        scale = validate(meta, args.pid, stage)
        record['initial_foreground'] = foreground_policy(meta, args.initial_foreground_approved, args.manual_foreground)
        if stage:
            # Read the previous passing record before any desktop access: a
            # failed stage has no metadata/capture and must stop here.
            prior_path = case / f'exhaustion-window-{stage - 1}.json'
            require(prior_path.is_file(), 'missing previous stage record')
            prior = I.load(prior_path)
            require(prior.get('pid') == args.pid and prior.get('stage') == stage - 1,
                    'previous stage belongs to another run')
            require(prior.get('success') is True,
                    'previous stage did not pass; stopping before capture')
            require(integer(prior.get('hwnd'), 'previous hwnd', 1) == meta['window']['hwnd'],
                    'previous stage HWND is missing or different; stopping before capture')
            chain(meta, prior, stage, case)
        u = H.load_win32()
        record['thread_dpi_awareness'] = H.set_thread_dpi_awareness(u)
        roots = H.enum_visible_top_level_windows(u, args.pid)
        require(len(roots) == 1, 'not exactly one owned visible top-level window')
        hwnd = roots[0]
        require(meta['window']['hwnd'] == hwnd, 'metadata HWND is not the owned root')
        thread = meta['window']['thread']
        state = I.verify_owner(u, meta['window'], I.TITLE, args.pid, thread)
        require(f32(state['dpi'] / 96) == f32(scale), 'actual HWND DPI != host GPUI scale')
        record.update(hwnd=hwnd, owner_thread=thread, thread=thread, browser=meta['browser'],
                      scale_factor=scale, metadata_sha256=H.sha256_file(path))
        bounds = meta['view_bounds']
        vx, vy = I.px(bounds[0], scale), I.px(bounds[1], scale)
        vw, vh = target((bounds[2], bounds[3]), scale)
        cw, ch = state['client_width'], state['client_height']
        require(0 <= vx and 0 <= vy and vy + vh <= ch and vx + vw <= cw,
                'entire VIEW is not inside the owned client')
        points = [{'name': f'grid-{a}-{b}',
                   'client_px': [min(cw - 1, int(cw * a)), min(ch - 1, int(ch * b))]}
                  for a in (1 / 6, .5, 5 / 6) for b in (1 / 6, .5, 5 / 6)]
        points += [{'name': f'view-{a}-{b}',
                    'client_px': [vx + min(vw - 1, int(vw * a)), vy + min(vh - 1, int(vh * b))]}
                   for a in (.05, .5, .95) for b in (.05, .5, .95)]
        before = H.run_probes(u, hwnd, state, points)
        # Retain the actual failed-point witnesses before fail-closed refusal.
        # This is diagnostics only, never an activation or retry instruction.
        record['capture_evidence'] = {'owner_pre': state, 'probes_pre': before}
        require(all(p['owned'] for p in before), 'owned client is occluded before capture')
        box = (state['client_left'], state['client_top'], state['client_right'], state['client_bottom'])
        desktop, primary = H.virtual_screen(u), H.primary_screen(u)
        require(box[0] >= desktop['left'] and box[1] >= desktop['top']
                and box[2] <= desktop['left'] + desktop['width']
                and box[3] <= desktop['top'] + desktop['height'],
                'owned client is outside the actual desktop')
        on_primary = (box[0] >= 0 and box[1] >= 0
                      and box[2] <= primary['width'] and box[3] <= primary['height'])
        if args.manual_foreground:
            require(H.pointer_value(u.GetForegroundWindow()) == hwnd,
                    'manual Exhaustion original window lost foreground before capture')
            record['manual_foreground_pre'] = hwnd
        image = H.ImageGrab.grab(bbox=box, all_screens=not on_primary)
        if args.manual_foreground:
            require(H.pointer_value(u.GetForegroundWindow()) == hwnd,
                    'manual Exhaustion original window lost foreground during capture')
            record['manual_foreground_post'] = hwnd
        after = H.inspect_window(u, hwnd)
        probes = H.run_probes(u, hwnd, state, points)
        require(after == state and all(p['owned'] for p in probes)
                and H.enum_visible_top_level_windows(u, args.pid) == roots
                and int(u.GetWindowThreadProcessId(hwnd, None)) == thread,
                'owner/geometry/occlusion changed during capture')
        require(image.mode == 'RGB' and image.size == (cw, ch),
                'not an unfiltered full owned-client RGB capture')
        actual = image.crop((vx, vy, vx + vw, vy + vh))
        require(actual.size == (vw, vh), 'VIEW crop is not the independent f32-ceil target')
        raw_record = None
        if stage == 3:
            expected = Image.new('RGB', (vw, vh), DETACHED_GREEN)
        else:
            raw = meta['view_raw']
            rawpath = case / raw['file']
            data = rawpath.read_bytes()
            rw, rh = raw['width'], raw['height']
            require(len(data) == rw * rh * 4 and all(alpha == 255 for alpha in data[3::4]),
                    'raw View frame is not opaque full BGRA')
            frame = Image.frombytes('RGBA', (rw, rh), data, 'raw', 'BGRA').convert('RGB')
            if stage == 1:
                # Old pixels stay 1:1 at the top-left; the rest is host background.
                expected = Image.new('RGB', (vw, vh), HOST_BACKGROUND)
                expected.paste(frame, (0, 0))
            else:
                require((rw, rh) == (vw, vh), 'raw View frame does not cover the whole VIEW')
                expected = frame
            raw_record = {'file': raw['file'], 'bytes': len(data),
                          'sha256': H.sha256_file(rawpath)}
        difference = ImageChops.difference(actual, expected).getbbox()
        require(difference is None, f'whole VIEW differs from expected pixels: {difference}')
        footer = (0, vy + vh, cw, ch)
        require(ch > vy + vh + 4, 'no footer band below the VIEW')
        white, yellow = text_pixels(image.crop(footer).getdata())
        if stage == 1:
            # Real rendered label pixels in the footer band, not OCR and not a
            # substitute for the metadata-gated exact label string.
            require(white + yellow >= 8,
                    'footer band has no rendered label text pixels')
        png = case / f'exhaustion-window-{stage}.png'
        H.save_png_atomic(image, png)
        evidence = {'pixel_difference_bbox': difference, 'png': png.name,
                    'sha256': H.sha256_file(png), 'raw': raw_record,
                    'viewport_physical': [vx, vy, vx + vw, vy + vh],
                    'owner_pre': state, 'owner_post': after,
                    'probes_pre': before, 'probes_post': probes,
                    'footer': {'region': list(footer), 'white_pixels': white,
                               'yellow_pixels': yellow,
                               'note': 'rendered footer text pixels only; the exact stale string is metadata-gated'}}
        record.update(metadata=meta, capture=evidence, success=True)
    except Exception as exc:
        record.update(error=f'{type(exc).__name__}: {exc}', traceback=traceback.format_exc())
    record['limits'] = ('One physical DPI, one owned original window and readonly pixels. Pre/post probes and '
                        'the ImageGrab are not atomic, so a transient overlay between them can be missed; the '
                        'screenshot RGB cannot prove alpha or exclude every overlay, and no multi-monitor, '
                        '110% extrapolation or backend/mode matrix is claimed. DropView(true) is a controlled '
                        'test-support injection, not a natural device loss. The exact stale label string is '
                        'metadata-gated; this script only proves the footer band contains rendered white/yellow '
                        'text pixels and keeps the full client PNG for the parent visual check, without OCR or '
                        'any system configuration change. No input, IME query, activation, window resize, hide, '
                        'restore or foreground change is performed here.')
    H.write_json_atomic(case / f'exhaustion-window-{stage}.json', record)
    print(f'Exhaustion stage {stage}: {"PASSED" if record["success"] else "FAILED"}: {record["error"]}',
          flush=True)
    return 0 if record['success'] else 1


def main():
    p = argparse.ArgumentParser(allow_abbrev=False)
    p.add_argument('--pid', required=True, type=int)
    p.add_argument('--case-dir', required=True, type=Path)
    p.add_argument('--stage', required=True, type=int, choices=range(4))
    foreground = p.add_mutually_exclusive_group()
    foreground.add_argument('--initial-foreground-approved', action='store_true')
    foreground.add_argument('--manual-foreground', action='store_true')
    return run(p.parse_args())


if __name__ == '__main__':
    sys.exit(main())
