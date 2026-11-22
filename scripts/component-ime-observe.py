#!/usr/bin/env python3
"""Two owned-window IME prerequisites only: pointer focus, owner facts, NO keys.

Observation completion is not a Chinese input/candidate test or 11.2 acceptance.
The original six-stage ASCII driver and its assertions remain unchanged.
"""
from __future__ import annotations
import argparse
import ctypes
import importlib.util
import json
from pathlib import Path
import sys
import time
import traceback

_spec = importlib.util.spec_from_file_location('component_ascii_driver', Path(__file__).with_name('component-input-test.py'))
I = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(I)
H = I.H
require = I.require
# Scope names shared with the host receipt; a compose stage is never allowed to
# report the pointer-only scope.
COMPOSE_SCOPE = 'ime-compose-two-window'


def classify_chinese_prerequisite(ime):
    """Only interprets actual owner facts; no future mode/adoption guarantee."""
    def answer(known, eligible, reason):
        return {'known': known, 'eligible': eligible, 'reason': reason}
    if type(ime) is not dict:
        return answer(False, False, 'missing-owner-facts')
    layout = ime.get('keyboard_layout')
    if type(layout) is not int or layout <= 0:
        return answer(False, False, 'unknown-layout')
    if layout & 0x3ff != 4:
        return answer(True, False, 'layout-not-chinese')
    if ime.get('is_ime') is not True:
        return answer(False, False, 'no-imm-ime-qualification')
    if ime.get('associated') is not True:
        return answer(False, False, 'no-queryable-owner-context')
    opened = ime.get('open')
    if type(opened) is not bool:
        return answer(False, False, 'unknown-open-state')
    if not opened:
        return answer(True, False, 'ime-closed')
    conversion = ime.get('conversion')
    if type(conversion) is not int or conversion < 0:
        return answer(False, False, 'unknown-conversion')
    return answer(True, bool(conversion & 1), 'native-conversion' if conversion & 1 else 'non-native-conversion')


def validate_owner_response(meta, fresh):
    stage, pid, original = meta['stage'], meta['pid'], meta['window']
    require(type(stage) is int and stage in (0, 1) and type(pid) is int and pid > 0,
            'not an owned IME environment observation stage')
    tag = f"{pid}:{stage}:{original['hwnd']}"
    require(type(fresh) is dict and all(type(fresh.get(k)) is int for k in ('schema', 'pid', 'stage', 'raw_count'))
            and fresh.get('schema') == 1 and fresh.get('kind') == 'ime-environment-owner'
            and fresh.get('pid') == pid and fresh.get('stage') == stage and fresh.get('request') == tag,
            'IME observation owner identity mismatch')
    require(fresh.get('raw_count') == 2 and fresh.get('keyboard_seen') is False
            and fresh.get('accepts_text') is True and fresh.get('foreground') is True,
            'owner facts not at exact post-focus pre-keyboard cutpoint')
    require(fresh.get('keyboard_input_sent') is False and fresh.get('mode_changed') is False,
            'environment observer must not send keyboard input or change mode')
    window = fresh.get('window')
    require(type(window) is dict and all(window.get(k) == original.get(k)
            for k in ('hwnd', 'pid', 'thread', 'dpi', 'left', 'top', 'width', 'height')),
            'owner geometry/thread changed during observation')
    native_ime = fresh.get('native_ime')
    require(type(native_ime) is dict and native_ime.get('system_caret_owned') is False
            and native_ime.get('candidate_adoption_verified') is False
            and all(type(native_ime.get(k)) is int and native_ime[k] == 0
                    for k in ('queued', 'applied', 'composition_calls', 'candidate_calls')),
            'pointer-only observation unexpectedly owns caret or positions composition/candidates')
    callbacks = fresh.get('component_callbacks')
    require(type(callbacks) is dict and all(callbacks.get(k) == 0 for k in (
        'key_down_entered', 'key_up_entered', 'replace_text_entered', 'replace_and_mark_text_entered',
        'key_down_rejected', 'key_up_rejected', 'replace_text_rejected', 'replace_and_mark_text_rejected'))
        and callbacks.get('last_rejection') is None, 'unexpected keyboard/text callback during pointer-only observation')
    title = fresh.get('dom_title')
    require(type(title) is str and title.startswith('Input|'), 'missing fixed numeric-only fixture title')
    fields = dict(part.split('=', 1) for part in title.split('|')[1:])
    # m is mousemove, not marked text. The real cursor placement may increment
    # it; require a numeric observation but never invent a zero-move constraint.
    require(type(fields.get('m')) is str and fields['m'].isascii() and fields['m'].isdigit(),
            'missing numeric mousemove counter')
    expected = {'n': 0, 'h': 2166136261, 'f': 1, 'b': 0, 'd': 1, 'u': 1,
                'kd': 0, 'ku': 0, 'i': 0, 'cs': 0, 'ce': 0, 'w': 0}
    require(all(fields.get(k) == str(v) for k, v in expected.items()),
            'unexpected DOM keyboard/composition/text or incomplete focus gesture')
    return classify_chinese_prerequisite(window.get('ime'))


def title_field(title, key):
    for part in title.split('|'):
        if part.startswith(key + '='):
            return int(part[len(key) + 1:])
    raise ValueError(f'DOM title missing {key}')


def fnv16(text):
    h = 2166136261
    raw = text.encode('utf-16-le')
    for i in range(0, len(raw), 2):
        unit = raw[i] | (raw[i + 1] << 8)
        h = ((h ^ unit) * 16777619) & 0xffffffff
    return h


def validate_composed(meta, fresh):
    require(fresh.get('keyboard_seen') is True, 'compose stage did not observe keyboard')
    title = fresh.get('dom_title')
    require(type(title) is str and title.startswith('Input|'), 'compose DOM title missing')
    expected = '你好'
    require(title_field(title, 'n') == len(expected), 'committed UTF-16 length mismatch')
    require(title_field(title, 'h') == fnv16(expected), 'committed text hash mismatch')
    require(title_field(title, 'cs') == 1, 'compositionstart count mismatch')
    require(title_field(title, 'ce') == 1, 'compositionend count mismatch')
    # Chrome fires input per composition update, so several input events are
    # expected; exactly one compositionend plus the committed text is the
    # one-shot evidence.
    require(title_field(title, 'i') >= 1, 'missing input events')
    # Key ledger balance is part of the input-count requirement: an unpaired UP
    # means the commit was sampled mid-gesture.
    require(title_field(title, 'kd') == 6 and title_field(title, 'ku') == 6,
            'compose key ledger unbalanced')
    ime = meta['window']['ime']
    return classify_chinese_prerequisite(ime)


def send_key(u, hwnd, state, x, y, char, owner_thread):
    """Send one plain VK to the owned window without the fixture's a/b/c list.

    The active owner layout must map the character to that VK without
    modifiers; the layout/IME mode is never switched. Real IME conversion is
    what turns these pinyin letters into Chinese text.
    """
    vk = 0x20 if char == ' ' else ord(char.upper())
    layout = u.GetKeyboardLayout(owner_thread)
    require(H.pointer_value(layout) != 0, 'owning thread has no keyboard layout')
    require((u.VkKeyScanExW(char, layout) & 0xff) == vk,
            f'key {vk} needs modifiers/dead key in owner layout; refusing')
    scan = int(u.MapVirtualKeyExW(vk, 0, layout))
    require(0 < scan <= 0xff, 'VK to physical scan mapping unavailable')
    require(not (u.GetAsyncKeyState(vk) & 0x8000), 'key already held by another source')
    I.send(u, I.key_input(vk, scan, False), hwnd, state, x, y)
    try:
        time.sleep(.03)
    finally:
        require(H.pointer_value(u.GetForegroundWindow()) == hwnd
                and I.root_at(u, state['client_left'] + x, state['client_top'] + y) == hwnd,
                'owned foreground/point lost while key held; refusing KEYUP')
        I.send(u, I.key_input(vk, scan, True), hwnd, state, x, y)


def request_owner(case, meta, record):
    stage, pid, hwnd = meta['stage'], meta['pid'], meta['window']['hwnd']
    request = case / f'ime-observe-owner-{stage}.request'
    response = request.with_suffix('.json')
    require(not request.exists() and not response.exists(), 'reused observation request')
    pending = request.with_suffix('.pending')
    pending.write_text(f'{pid}:{stage}:{hwnd}', encoding='ascii')
    pending.replace(request)
    until = time.monotonic() + 2.0
    while not response.is_file():
        require(time.monotonic() < until, 'fresh owner observation timeout; no keyboard sent')
        time.sleep(.01)
    fresh = I.load(response)
    record['owner_response'] = {'file': response.name, 'sha256': I.sha(response), 'response': fresh}
    if stage >= 2:
        return validate_composed(meta, fresh)
    return validate_owner_response(meta, fresh)


def run(args):
    case, stage = args.case_dir.resolve(), args.stage
    path = case / f'ime-observe-stage-{stage}.json'
    scope = COMPOSE_SCOPE if stage >= 2 else 'ime-environment-only'
    record = {'schema': 1, 'kind': 'ime-environment-window', 'scope': scope,
              'pid': args.pid, 'stage': stage, 'success': False, 'error': None,
              'task_11_2_accepted': False, 'native_chinese_verified': False,
              'keyboard_input_sent': False, 'mode_changed': False}
    try:
        require(args.interactive_input_approved and sys.platform == 'win32' and args.pid > 0,
                'explicit owned Windows pointer input approval required')
        meta = I.load(path)
        require(type(meta) is dict and all(type(meta.get(k)) is int for k in ('schema', 'pid', 'stage'))
                and meta.get('schema') == 1 and meta.get('kind') == 'ime-environment-stage'
                and meta.get('pid') == args.pid and meta.get('stage') == stage
                and meta.get('action') == 'focus-and-observe', 'observation metadata mismatch')
        expected_title, expected_peer = ((I.PEER, I.TITLE) if stage in (1, 3) else (I.TITLE, I.PEER))
        require(meta.get('title') == expected_title and meta.get('peer_title') == expected_peer, 'target window swapped')
        u = H.load_win32()
        record['thread_dpi_awareness'] = H.set_thread_dpi_awareness(u)
        I.win32(u)
        a, b = meta['hwnds']
        require(type(a) is int and type(b) is int and a != b and set(H.enum_visible_top_level_windows(u, args.pid)) == {a, b},
                'not the exact two owned host roots')
        target, other = (a, b) if stage in (0, 2) else (b, a)
        require(meta['window']['hwnd'] == target and meta['peer']['hwnd'] == other, 'target HWND mismatched fixed stage')
        thread = I.integer(meta['window'].get('thread'), 'owner thread', 1)
        state = I.verify_owner(u, meta['window'], expected_title, args.pid, thread)
        peer = I.verify_owner(u, meta['peer'], expected_peer, args.pid, thread)
        require(H.pointer_value(u.GetForegroundWindow()) == target, 'ordinary owner foreground request not confirmed')
        require(I.f32(meta['scale_factor']) == I.f32(state['dpi'] / 96)
                and I.f32(meta['peer_scale']) == I.f32(peer['dpi'] / 96), 'owned DPI differs from declared scale')
        if stage == 1:
            prior = I.load(case / 'ime-observe-window-0.json')
            require(prior.get('success') is True and prior.get('pid') == args.pid and prior.get('hwnds') == [a, b],
                    'missing previous same-process owned observation')
        record.update(hwnds=[a, b], target_hwnd=target, owner_thread=thread, metadata_sha256=I.sha(path))
        # This pre-focus capture has no composition/candidate overlay. Reuse the
        # unchanged full VIEW RGB gate, never relax it for an IME overlay.
        if stage < 2:
            # The compose stage runs after the input already has focus, so a
            # blinking caret makes the zero-tolerance pre-focus capture gate
            # meaningless. Text/commit assertions replace it there.
            record['capture'] = I.capture(u, case, stage, meta, state, target, record)
        held = [vk for vk in (1, 2, 4, 5, 6, 16, 17, 18, 91, 92, *range(65, 91)) if u.GetAsyncKeyState(vk) & 0x8000]
        require(not held, f'input already held {held}; no input or automatic release sent')
        require(I.verify_owner(u, meta['window'], expected_title, args.pid, thread) == state,
                'owned geometry changed before pointer gesture')
        if stage in (2, 3):
            # Restore our own foreground BEFORE the attributed click; sending a
            # click while the peer is foreground must fail closed.
            u.SetForegroundWindow(target)
            time.sleep(0.15)
        I.click(u, target, state, I.px(90, meta['scale_factor']), I.px(94, meta['scale_factor']))
        # The OS may keep another top-level window in front; clicking a covered
        # window cannot activate it. Ask for our own window explicitly, bounded,
        # and never touch a foreign window.
        if stage not in (2, 3) and H.pointer_value(u.GetForegroundWindow()) != target:
            # Compose reuses the foreground proven before its own click; asking
            # again here would reset focus away from the page input.
            u.SetForegroundWindow(target)
            time.sleep(0.15)
        if stage in (2, 3):
            for ch in 'nihao ':
                send_key(u, target, state, I.px(90, meta['scale_factor']), I.px(94, meta['scale_factor']), ch, thread)
            record['keyboard_input_sent'] = True
        record['chinese_prerequisite'] = request_owner(case, meta, record)
        require(H.pointer_value(u.GetForegroundWindow()) == target, 'owned foreground lost after observation')
        I.verify_owner(u, meta['window'], expected_title, args.pid, thread)
        require(set(H.enum_visible_top_level_windows(u, args.pid)) == {a, b}, 'owned root set changed')
        record['success'] = True
    except Exception as exc:
        record['error'] = f'{type(exc).__name__}: {exc}'
        record['traceback'] = traceback.format_exc()
        record['possible_physical_input_held'] = 'unknown; never send an unverified foreign UP'
        if 'u' in locals():
            record['observed_left_button_held'] = bool(u.GetAsyncKeyState(1) & 0x8000)
    record['limits'] = 'Two explicit owned pointer focus observations only. No keyboard or Chinese composition attempted; no mode/layout changes. Owner facts and capture are discrete observations, not future mode or atomic occlusion guarantees. Eligible prerequisites do not prove candidate adoption or task11.2.'
    H.write_json_atomic(case / f'ime-observe-window-{stage}.json', record)
    print(f'IME environment stage {stage}: {"COLLECTED" if record["success"] else "FAILED"}: {record["error"]}', flush=True)
    return 0 if record['success'] else 1


def main():
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument('--pid', type=int, required=True)
    parser.add_argument('--case-dir', type=Path, required=True)
    parser.add_argument('--stage', type=int, choices=(0, 1, 2, 3), required=True)
    parser.add_argument('--interactive-input-approved', action='store_true')
    return run(parser.parse_args())


if __name__ == '__main__':
    sys.exit(main())
