#!/usr/bin/env python3
"""Partial11.3 owned desktop pixels, not full resize/drag/DPI acceptance.

The host drives its own public Window.resize. This observer only reads owned
window facts and desktop pixels. No activation, input, cursor or system changes.
"""
from __future__ import annotations
import argparse, hashlib, importlib.util, io, json, math, struct, sys, traceback
from pathlib import Path
from PIL import Image, ImageChops

spec = importlib.util.spec_from_file_location('resize_capture_host', Path(__file__).with_name('capture-component-host.py'))
H = importlib.util.module_from_spec(spec)
spec.loader.exec_module(H)
EXTENTS = [(320, 240), (16, 12), (500, 300), (64, 48), (320, 240), (320, 240)]
MAX_RECEIPT_BYTES = 128 * 1024


def require(value, message):
    if not value:
        raise H.CaptureError(message)


def f32(n):
    return struct.unpack('<f', struct.pack('<f', n))[0]


def integer(n, label, minimum=0):
    require(type(n) is int and n >= minimum, f'invalid {label}')
    return n


def target(extent, scale):
    return tuple(math.ceil(f32(f32(n) * f32(scale))) for n in extent)


def client_pixels(extent, scale):
    # Pinned GPUI Size::to_device_pixels rounds positive f32 products; the
    # CEF View target above has a different (ceil) contract.
    return tuple(math.floor(f32(f32(n) * f32(scale)) + 0.5) for n in extent)


def validate(meta, pid, stage):
    require(type(meta) is dict and all(type(meta.get(k)) is int for k in ('schema', 'pid', 'stage'))
            and meta['schema'] == 1 and meta['pid'] == pid and meta['stage'] == stage
            and meta.get('kind') == 'window-layout-resize-slice'
            and meta.get('task_11_3_accepted') is False, 'resize identity/scope mismatch')
    w, h = EXTENTS[stage]
    require(meta.get('view_bounds') == [0, 48, w, h] and meta.get('client_dip') == [max(w+80, 200), max(h+120, 180)], 'not fixed stage geometry')
    scale = meta.get('scale_factor')
    require(type(scale) in (int, float) and math.isfinite(scale) and scale > 0, 'invalid independent scale')
    ids = meta.get('browser')
    require(type(ids) is list and len(ids) == 2 and all(type(x) is int and x > 0 for x in ids), 'invalid full BrowserId')
    d = meta.get('diagnostics')
    require(type(d) is dict and d.get('last_error') is None and d.get('surface_bounds') == meta['view_bounds']
            and type(d.get('scale_factor')) in (int, float) and f32(d['scale_factor']) == f32(scale), 'view diagnostic geometry/error mismatch')
    for key in ('frame_width', 'frame_height', 'created_images', 'released_images', 'paint_calls', 'painted_generation', 'painted_sequence'):
        integer(d.get(key), f'diagnostics.{key}')
    require(meta.get('detached') is (stage == 5), 'detached flag mismatch')
    if stage == 5:
        require(meta.get('view_raw') is None and integer(d.get('created_images'), 'images', 1) == d.get('released_images')
                and d.get('frame_width') == d.get('frame_height') == 0, 'released View not actually empty/balanced')
        return scale
    raw = meta.get('view_raw')
    require(type(raw) is dict and raw.get('file') == f'resize-stage-{stage}-view.bgra'
            and raw.get('browser') == ids and raw.get('surface') == 'View', 'raw identity or local file mismatch')
    for key in ('width', 'height', 'bytes', 'generation', 'sequence'):
        integer(raw.get(key), f'raw.{key}', 1)
    fw, fh = target((w, h), scale)
    require(raw.get('width') == d.get('frame_width') == fw and raw.get('height') == d.get('frame_height') == fh
            and raw.get('bytes') == fw*fh*4, 'frame != independent f32 ceil target')
    require(integer(raw.get('generation'), 'generation', 1) == d.get('painted_generation')
            and integer(raw.get('sequence'), 'sequence', 1) == d.get('painted_sequence')
            and integer(d.get('paint_calls'), 'paint', 1) > 0 and d.get('pending_resync') is False,
            'source is not matching actual painted View')
    return scale


def snapshot_bytes(path, limit, *, exact=False):
    with path.open('rb') as stream:
        data = stream.read(limit + 1)
    require(0 < len(data) <= limit and (not exact or len(data) == limit),
            'evidence file outside its expected byte bounds')
    return data, hashlib.sha256(data).hexdigest()


def snapshot_json(path):
    data, digest = snapshot_bytes(path, MAX_RECEIPT_BYTES)
    return json.loads(data), digest


def physical_view_rect(stage, scale):
    w, h = target(EXTENTS[stage], scale)
    y = math.floor(48 * scale + .5)
    return 0, y, w, y + h


def compare_view(case, stage, image, rect, raw):
    """Compare the entire unscaled crop and hash the exact BGRA bytes decoded."""
    left, top, right, bottom = rect
    w, h = right - left, bottom - top
    require(image.mode == 'RGB' and 0 <= left < right <= image.width
            and 0 <= top < bottom <= image.height, 'entire View not inside full RGB client')
    raw_record = None
    if stage == 5:
        require(raw is None, 'detached stage cannot retain a raw frame')
        expected = Image.new('RGB', (w, h), (0, 170, 68))
    else:
        name = f'resize-stage-{stage}-view.bgra'
        require(raw['file'] == name and (raw['width'], raw['height']) == (w, h), 'raw frame geometry or name changed')
        data, digest = snapshot_bytes(case / name, w*h*4, exact=True)
        require(all(x == 255 for x in data[3::4]), 'raw is not opaque full BGRA')
        expected = Image.frombytes('RGBA', (w, h), data, 'raw', 'BGRA').convert('RGB')
        require({(26,76,126), (190,90,20), (110,40,180)} <= set(expected.getdata()), 'missing nonuniform fixed edge pattern')
        raw_record = {'file':name, 'bytes':len(data), 'sha256':digest}
    difference = ImageChops.difference(image.crop(rect), expected).getbbox()
    require(difference is None, f'whole View differs from 1:1 full frame: {difference}')
    return raw_record


def compared_png_bytes(case, stage, image, rect, raw):
    """Encode once; verify the actual decoded payload that will be published."""
    with io.BytesIO() as stream:
        image.save(stream, format='PNG')
        payload = stream.getvalue()
    with Image.open(io.BytesIO(payload), formats=['PNG']) as decoded:
        require(decoded.mode == image.mode and decoded.size == image.size, 'PNG payload does not preserve client geometry')
        raw_record = compare_view(case, stage, decoded, rect, raw)
    return payload, raw_record


def publish_png_snapshot(path, payload):
    """Publish an already compared payload; do not hash a second path read."""
    require(not path.exists(), 'PNG evidence already exists')
    pending = path.with_suffix(path.suffix + '.pending')
    with pending.open('xb') as stream:
        stream.write(payload)
    # On this live Windows path rename cannot replace an existing destination.
    pending.rename(path)
    return hashlib.sha256(payload).hexdigest()


def verify_initial_foreground(meta, enabled):
    facts = meta.get('initial_foreground')
    if not enabled:
        require(facts is None, 'unexpected manual Resize readiness metadata')
        return
    require(type(facts) is dict and facts.get('policy') == 'manual-owned-observation'
            and facts.get('approved') is False and type(facts.get('attempts')) is int
            and facts['attempts'] == 0 and type(facts.get('before')) is bool
            and facts.get('after') is True, 'manual Resize readiness was not qualified')


def verify_passive_window(meta, enabled, hwnd, pid, thread, dpi, client):
    facts = meta.get('passive_window')
    if not enabled:
        require(facts is None, 'unexpected passive Resize window metadata')
        return
    require(type(facts) is dict, 'missing passive Resize original window evidence')
    for key, value in (('hwnd', hwnd), ('pid', pid), ('thread', thread), ('dpi', dpi), ('resize_flags', 0x0016)):
        require(type(facts.get(key)) is int and facts[key] == value, f'passive Resize {key} mismatch')
    require(facts.get('client') == list(client) and facts.get('foreground') is False,
            'passive Resize client or foreground mismatch')


def run(args):
    case, stage = args.case_dir.resolve(), args.stage
    record = {'schema': 1, 'kind': 'resize-owned-window', 'scope': 'window-layout-resize-slice',
              'pid': args.pid, 'stage': stage, 'success': False, 'error': None,
              'task_11_3_accepted': False, 'input_sent': False, 'observer_resized_window': False,
              'foreground_forced': False}
    try:
        require(sys.platform == 'win32', 'real Windows desktop required')
        path = case / f'resize-stage-{stage}.json'
        meta, meta_digest = snapshot_json(path)
        scale = validate(meta, args.pid, stage)
        manual_initial = args.manual_foreground and stage == 0
        verify_initial_foreground(meta, manual_initial)
        u = H.load_win32()
        record['thread_dpi_awareness'] = H.set_thread_dpi_awareness(u)
        roots = H.enum_visible_top_level_windows(u, args.pid)
        require(len(roots) == 1, 'not one owned root window')
        hwnd = roots[0]
        state = H.inspect_window(u, hwnd)
        thread = int(u.GetWindowThreadProcessId(hwnd, None))
        require(state['owner_pid'] == args.pid and state['title'] == H.TITLE and state['visible'] and not state['iconic'] and thread > 0, 'owned window not eligible')
        require(f32(state['dpi']/96) == f32(scale), 'real HWND DPI != host GPUI scale')
        cw, ch = client_pixels(meta['client_dip'], scale)
        require((state['client_width'], state['client_height']) == (cw, ch), 'actual native client did not reach requested size')
        verify_passive_window(meta, args.passive_window, hwnd, args.pid, thread, state['dpi'], (cw, ch))
        if manual_initial:
            expected = dict(hwnd=hwnd, pid=args.pid, thread=thread, dpi=state['dpi'],
                            left=state['client_left'], top=state['client_top'], width=cw, height=ch)
            require(meta.get('window') == expected, 'manual Resize original owner or geometry changed')
        record.update(hwnd=hwnd, owner_thread=thread, scale_factor=scale, browser=meta['browser'],
                      metadata_sha256=meta_digest, metadata=meta, owner_pre=state)
        if stage:
            previous, _ = snapshot_json(case / f'resize-window-{stage-1}.json')
            require(previous.get('success') is True and previous.get('pid') == args.pid and previous.get('stage') == stage-1
                    and previous.get('hwnd') == hwnd and previous.get('owner_thread') == thread and previous.get('browser') == meta['browser'], 'broken same-window/Browser stage chain')
            old = previous['owner_pre']
            record['actual_client_size_changed'] = (old['client_width'], old['client_height']) != (cw, ch)
            require(record['actual_client_size_changed'] is (stage < 5), 'no actual native resize or unexpected detached resize')
            if stage < 5:
                oldraw, raw = previous['metadata']['view_raw'], meta['view_raw']
                require(raw['generation'] > oldraw['generation'] and raw['sequence'] > oldraw['sequence'], 'no new-generation frame after changed geometry')
        rect = physical_view_rect(stage, scale)
        _, y, w, bottom = rect
        h = bottom - y
        require(rect[2] <= cw and rect[3] <= ch, 'entire View not inside client')
        points = [{'name':f'grid-{a}-{b}', 'client_px':[min(cw-1, int(cw*a)), min(ch-1, int(ch*b))]}
                  for a in (1/6, .5, 5/6) for b in (1/6, .5, 5/6)]
        points += [{'name':f'view-{a}-{b}', 'client_px':[min(w-1, int(w*a)), y+min(h-1, int(h*b))]}
                   for a in (.05, .5, .95) for b in (.05, .5, .95)]
        before = H.run_probes(u, hwnd, state, points)
        record['probes_pre'] = before
        require(all(p['owned'] for p in before), 'owned client occluded before capture')
        box = (state['client_left'], state['client_top'], state['client_right'], state['client_bottom'])
        desktop, primary = H.virtual_screen(u), H.primary_screen(u)
        require(box[0] >= desktop['left'] and box[1] >= desktop['top'] and box[2] <= desktop['left']+desktop['width'] and box[3] <= desktop['top']+desktop['height'], 'client outside actual desktop')
        on_primary = box[0] >= 0 and box[1] >= 0 and box[2] <= primary['width'] and box[3] <= primary['height']
        if manual_initial:
            require(H.pointer_value(u.GetForegroundWindow()) == hwnd, 'manual Resize lost foreground before initial capture')
            record['manual_foreground_pre'] = hwnd
        if args.passive_window:
            require(H.pointer_value(u.GetForegroundWindow()) != hwnd, 'passive Resize became foreground before capture')
            record['passive_unfocused_pre'] = True
        image = H.ImageGrab.grab(bbox=box, all_screens=not on_primary)
        if args.passive_window:
            require(H.pointer_value(u.GetForegroundWindow()) != hwnd, 'passive Resize became foreground during capture')
            record['passive_unfocused_post'] = True
        if manual_initial:
            require(H.pointer_value(u.GetForegroundWindow()) == hwnd, 'manual Resize lost foreground during initial capture')
            record['manual_foreground_post'] = hwnd
        after = H.inspect_window(u, hwnd)
        probes = H.run_probes(u, hwnd, state, points)
        require(after == state and all(p['owned'] for p in probes) and H.enum_visible_top_level_windows(u, args.pid) == roots
                and int(u.GetWindowThreadProcessId(hwnd, None)) == thread, 'owner/geometry/occlusion changed during capture')
        require(image.mode == 'RGB' and image.size == (cw, ch), 'not unfiltered full owned-client RGB')
        png_bytes, raw_record = compared_png_bytes(case, stage, image, rect, meta['view_raw'])
        png = case / f'resize-window-{stage}.png'
        png_digest = publish_png_snapshot(png, png_bytes)
        record.update(owner_post=after, probes_pre=before, probes_post=probes, viewport_physical=list(rect),
                      capture={'png':png.name, 'sha256':png_digest, 'raw':raw_record, 'pixel_difference_bbox':None},
                      success=True)
    except Exception as exc:
        record.update(error=f'{type(exc).__name__}: {exc}', traceback=traceback.format_exc())
    record['limits'] = 'One current physical DPI, static pattern and settled public-window/layout sizes only. No actual drag, unfocused animation, minimize/restore, 110% extrapolation, recovery-exhaustion injection or full-task acceptance. Discrete owner/occlusion probes are not atomic or all-overlay isolation; screenshot RGB is not alpha proof. Raw is expected pixels, never the captured image.'
    H.write_json_atomic(case / f'resize-window-{stage}.json', record)
    print(f'Resize stage {stage}: {"PASSED" if record["success"] else "FAILED"}: {record["error"]}', flush=True)
    return 0 if record['success'] else 1


def main():
    p = argparse.ArgumentParser(allow_abbrev=False)
    p.add_argument('--pid', required=True, type=int)
    p.add_argument('--case-dir', required=True, type=Path)
    p.add_argument('--stage', required=True, type=int, choices=range(6))
    focus = p.add_mutually_exclusive_group()
    focus.add_argument('--manual-foreground', action='store_true')
    focus.add_argument('--passive-window', action='store_true')
    return run(p.parse_args())


if __name__ == '__main__':
    sys.exit(main())
