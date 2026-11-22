#!/usr/bin/env python3
"""Read-only 11.3 owned-window visibility observer.

Four independent stage invocations consume the host's PS-handshake metadata
``visibility-stage-<n>.json`` and emit one atomic, no-replace
``visibility-window-<n>.json``. The host, never this observer, performs the
approved non-default validation minimize / ordinary SW_RESTORE / explicit
SetVisible operations. This script never activates, sends input, moves the
cursor, changes foreground policy or touches a foreign window.

Stages 0, 2 and 3 read the one owned visible original top-level HWND and grab
its complete client with ImageGrab. They compare the entire 320x240 DIP VIEW
(top 48 DIP) against the exact opaque raw BGRA source (or detached green) with
zero RGB tolerance, retaining the full-client PNG and 18 owner probes before
and after the grab. Stage 1 is the explicit exception: a minimized window has
no client pixels, so it performs ONLY IsWindow / IsIconic /
GetWindowThreadProcessId on the original HWND and keeps the stage-0 window
identity. It never captures, inspects client/DPI, enumerates foreign windows or
claims a hidden-desktop image. Because the explicit SetVisible(false) clears the
presented image, the hidden stage requires an empty (0x0) presented frame plus a
positive historical paint count; it never claims the retained source frame was
the last GPUI-painted frame. Only stages 0 and 2 require the raw source to be
exactly the painted frame.

The natural popup did not exist before the hide, so this observer only proves
empty-after-hide at the fixed stages. ``popup_clear_transition_verified`` is
always false and no popup-transition proof is invented.
"""
from __future__ import annotations

import argparse
import ctypes
import importlib.util
import json
import math
import os
import sys
import traceback
from ctypes import wintypes as wt
from pathlib import Path

from PIL import Image, ImageChops

spec = importlib.util.spec_from_file_location(
    'visibility_exhaustion_base', Path(__file__).with_name('component-exhaustion-test.py'))
M = importlib.util.module_from_spec(spec)
spec.loader.exec_module(M)
I, H = M.I, M.H

VIEW_BOUNDS = [0, 48, 320, 240]
VIEW_DIP = (320, 240)
HOST_DIP = (600, 450)
GREEN = (0, 170, 68)
PAGE_LEFT_RGB = (190, 90, 20)   # 4 DIP orange bar at the VIEW's left edge
PAGE_TOP_RGB = (110, 40, 180)   # 4 DIP purple bar at the VIEW's top edge
PAGE_CENTER_RGB = (26, 76, 126)  # fixed static page background
PHASES = ('Ready', 'MinimizeRequested', 'HidePosted', 'Hidden', 'RestoreRequested',
          'ShowPosted', 'SourceRecovered', 'Retired', 'Failed')
PHASE_BY_STAGE = {0: 'Ready', 1: 'Hidden', 2: 'SourceRecovered', 3: 'Retired'}
REQUIRED = ('schema', 'kind', 'scope', 'pid', 'stage', 'browser', 'window', 'scale_factor',
            'view_bounds', 'diagnostics', 'view_raw', 'recovery', 'bridge', 'detached',
            'source_visible', 'source_generation', 'popup_visible', 'popup_frame_present',
            'task_11_3_accepted', 'input_sent', 'foreground_forced', 'observer_resized',
            'popup_clear_transition_verified')
BRIDGE_FIELDS = ('phase', 'browser', 'owner', 'viewport', 'minimize_requests',
                 'restore_requests', 'before_hide_generation', 'hidden_generation',
                 'hide_command', 'show_command', 'retained', 'hidden_recovery',
                 'last_hidden_observation', 'quiet_ms', 'quiet_ms_frozen',
                 'popup_clear_transition_verified')
RAW_FIELDS = ('file', 'browser', 'generation', 'sequence', 'width', 'height', 'bytes')
COMMAND_FIELDS = ('browser', 'sequence')
RETAINED_FIELDS = ('browser', 'generation', 'sequence', 'width', 'height')
OBSERVATION_FIELDS = ('source_generation', 'source_visible', 'popup_visible',
                      'popup_frame_present', 'retained_same_arc', 'recovery')
WINDOW_FIELDS = ('hwnd', 'pid', 'thread', 'dpi', 'left', 'top', 'width', 'height')

require = M.require
f32 = M.f32
target = M.target
integer = M.integer
boolean = M.boolean
typed_id = M.typed_id
recovery = M.recovery
diagnostics = M.diagnostics
RECOVERY_FIELDS = M.RECOVERY_FIELDS


def same_recovery(left, right, label='recovery'):
    """Same canonical recovery facts, tolerant only of extra wire fields."""
    recovery(left, f'{label}.left')
    recovery(right, f'{label}.right')
    require(all(left[key] == right[key] for key in RECOVERY_FIELDS),
            f'{label} facts differ')
    return left


def command(value, label):
    if value is None:
        return None
    require(type(value) is dict and set(value) == set(COMMAND_FIELDS),
            f'invalid {label} field set')
    typed_id(value['browser'], f'{label}.browser')
    integer(value['sequence'], f'{label}.sequence', 1)
    return value


def retained(value, label):
    if value is None:
        return None
    require(type(value) is dict and set(value) == set(RETAINED_FIELDS),
            f'invalid {label} field set')
    typed_id(value['browser'], f'{label}.browser')
    for key in ('generation', 'sequence', 'width', 'height'):
        integer(value[key], f'{label}.{key}', 1)
    return value


def last_hidden_observation(value, label):
    """The last actually successful Hidden observation, not a fresh sample."""
    if value is None:
        return None
    require(type(value) is dict and set(value) == set(OBSERVATION_FIELDS),
            f'invalid {label} field set')
    integer(value['source_generation'], f'{label}.source_generation', 1)
    require(value['source_visible'] is False, f'{label}.source_visible is not false')
    require(value['popup_visible'] is False and value['popup_frame_present'] is False,
            f'{label} reports a popup/frame while hidden')
    require(value['retained_same_arc'] is True,
            f'{label}.retained_same_arc is not the retained old frame')
    recovery(value['recovery'], f'{label}.recovery')
    require(value['recovery']['status'] == 'Hidden',
            f'{label}.recovery is not Hidden')
    return value


def same_observation(left, right, label='last hidden observation'):
    """The last successful Hidden snapshot is kept, never refreshed."""
    for value, side in ((left, 'left'), (right, 'right')):
        last_hidden_observation(value, f'{label}.{side}')
    for key in ('source_generation', 'source_visible', 'popup_visible',
                'popup_frame_present', 'retained_same_arc'):
        require(left[key] == right[key], f'{label}.{key} changed')
    return same_recovery(left['recovery'], right['recovery'], label)


def window_facts(win, pid, stage, scale):
    require(type(win) is dict, 'window facts are missing')
    require(all(key in win for key in ('hwnd', 'pid', 'thread')),
            'window identity is missing a field')
    require(integer(win['hwnd'], 'window.hwnd', 1) > 0, 'invalid owned HWND')
    require(integer(win['pid'], 'window.pid', 1) == pid, 'window PID mismatch')
    integer(win['thread'], 'window.thread', 1)
    if stage == 1:
        # A minimized window has no usable client. The stage-1 gate must not
        # reject on an empty/absent DPI or client; stage 0 already proved the
        # original full identity.
        return win
    require(all(key in win for key in WINDOW_FIELDS), 'window facts are missing a field')
    require(integer(win['dpi'], 'window.dpi', 1) > 0, 'invalid window DPI')
    integer(win['left'], 'window.left', None)
    integer(win['top'], 'window.top', None)
    require(integer(win['width'], 'window.width', 1) > 0
            and integer(win['height'], 'window.height', 1) > 0, 'invalid client size')
    require((win['width'], win['height']) == target(HOST_DIP, scale),
            'client size is not the fixed 600x450 DIP target')
    require(f32(win['dpi'] / 96) == f32(scale),
            'metadata DPI does not match the independent host scale')
    vx, vy = I.px(VIEW_BOUNDS[0], scale), I.px(VIEW_BOUNDS[1], scale)
    vw, vh = target(VIEW_DIP, scale)
    require(vx >= 0 and vy >= 0 and vx + vw <= win['width'] and vy + vh <= win['height'],
            'metadata client cannot contain the entire VIEW')
    return win


def bridge_facts(value, meta, pid, stage, ids, scale):
    require(type(value) is dict and set(value) == set(BRIDGE_FIELDS),
            'bridge evidence field set mismatch')
    require(type(value['phase']) is str and value['phase'] in PHASES,
            'invalid bridge phase')
    require(value['phase'] == PHASE_BY_STAGE[stage],
            'bridge phase does not match the handshake stage')
    require(value['browser'] == ids, 'bridge names a different Browser')
    win = meta['window']
    owner = value['owner']
    require(type(owner) is dict and set(owner) == {'hwnd', 'pid', 'thread'},
            'bridge owner field set mismatch')
    require(owner['hwnd'] == win['hwnd'] and owner['pid'] == pid
            and owner['thread'] == win['thread'], 'bridge owner identity mismatch')
    viewport = value['viewport']
    require(type(viewport) is dict
            and set(viewport) == {'width', 'height', 'scale'},
            'bridge viewport field set mismatch')
    require(viewport['width'] == VIEW_DIP[0] and viewport['height'] == VIEW_DIP[1],
            'bridge viewport is not the fixed 320x240 DIP VIEW')
    require(type(viewport['scale']) in (int, float) and math.isfinite(viewport['scale'])
            and f32(viewport['scale']) == f32(scale),
            'bridge viewport scale mismatch')
    minimize_requests = integer(value['minimize_requests'], 'minimize_requests', 0)
    restore_requests = integer(value['restore_requests'], 'restore_requests', 0)
    expected = {0: (0, 0), 1: (1, 0), 2: (1, 1), 3: (1, 1)}[stage]
    require((minimize_requests, restore_requests) == expected,
            'bridge minimize/restore request count mismatch')
    before = integer(value['before_hide_generation'], 'before_hide_generation', 0)
    hidden = integer(value['hidden_generation'], 'hidden_generation', 0)
    if stage == 0:
        require(before == 0 and hidden == 0, 'initial bridge already used a hide generation')
    else:
        require(before > 0 and hidden > before,
                'bridge hidden generation is not newer than the pre-hide source')
    hide = command(value['hide_command'], 'hide_command')
    show = command(value['show_command'], 'show_command')
    for label, item in (('hide_command', hide), ('show_command', show)):
        if item is not None:
            require(item['browser'] == ids, f'{label} names a different Browser')
    if stage == 0:
        require(hide is None and show is None, 'initial bridge already submitted visibility commands')
    elif stage == 1:
        require(hide is not None and show is None,
                'hidden stage must have exactly the hide command and no show command')
    else:
        require(hide is not None and show is not None and show['sequence'] > hide['sequence'],
                'show command is missing or does not follow the hide command')
    held = retained(value['retained'], 'retained')
    if held is not None:
        require(held['browser'] == ids, 'retained frame names a different Browser')
        require(held['generation'] == before,
                'retained frame is not the actual pre-hide source generation')
    if stage == 0:
        require(held is None, 'initial bridge already retains a hidden frame')
    elif stage in (1, 2):
        require(held is not None, 'hidden/recovered stage does not retain the old frame lease')
    else:
        require(held is None, 'retired stage still retains a hidden frame lease')
    hidden_recovery = value['hidden_recovery']
    if stage == 0:
        require(hidden_recovery is None, 'initial bridge already has a hidden recovery baseline')
    else:
        recovery(hidden_recovery, 'bridge.hidden_recovery')
        require(hidden_recovery['status'] == 'Hidden',
                'bridge hidden recovery baseline is not Hidden')
        # Hiding cancels recovery without advancing its target generation, so
        # the Hidden baseline can never be newer than the pre-hide source.
        require(hidden_recovery['generation'] <= before,
                'bridge hidden recovery generation postdates the pre-hide source')
        require(hidden_recovery['attempts'] == 0
                and not hidden_recovery['request_offsets_us'],
                'bridge hidden baseline still has in-flight retry budget')
        require(hidden_recovery['queued'] is False
                and hidden_recovery['post_failures'] == 0,
                'bridge hidden baseline is queued or reports post failures')
    quiet = value['quiet_ms']
    frozen = value['quiet_ms_frozen']
    boolean(frozen, 'bridge.quiet_ms_frozen')
    observed = last_hidden_observation(value['last_hidden_observation'],
                                       'bridge.last_hidden_observation')
    if stage == 0:
        require(quiet is None, 'initial bridge already reports a quiet interval')
        require(frozen is False, 'initial bridge already froze a hidden duration')
        require(observed is None, 'initial bridge already has a hidden observation')
    elif stage == 1:
        require(integer(quiet, 'bridge.quiet_ms', 0) >= 250,
                'hidden quiet observation is shorter than 250 ms')
        require(frozen is False, 'hidden observation already froze the restore duration')
        require(observed is not None,
                'hidden stage has no successful observation snapshot')
        require(observed['source_generation'] == meta['source_generation'],
                'last hidden observation generation differs from the hidden source')
        require(same_recovery(observed['recovery'], meta['recovery'],
                              'last hidden observation'),
                'last hidden observation is not the current Hidden recovery snapshot')
        base = hidden_recovery
        require(all(observed['recovery'][key] == base[key]
                    for key in ('status', 'generation', 'requests_total',
                                'native_invalidate_entries', 'acks', 'exhaustions')),
                'hidden observation differs from the hidden baseline counters')
    else:
        # Frozen at the actual ordinary restore: the same real hidden duration
        # and the same last successful Hidden snapshot are kept by
        # SourceRecovered and Retired, never recomputed or refreshed.
        require(integer(quiet, 'bridge.quiet_ms', 0) >= 250,
                'frozen hidden duration is shorter than 250 ms')
        require(frozen is True, 'post-restore stage did not freeze the hidden duration')
        require(observed is not None,
                'post-restore stage has no kept hidden observation')
        require(same_recovery(observed['recovery'], hidden_recovery,
                              'kept hidden observation'),
                'post-restore stage does not keep the last hidden observation baseline')
        require(observed['source_generation'] == hidden,
                'last hidden observation generation differs from the hidden source')
    require(value['popup_clear_transition_verified'] is False,
            'popup clear transition is unproven and must stay false')
    return value


def raw_facts(raw, stage, ids, scale):
    # Fixed wire shape for this scene: file/browser/generation/sequence/
    # width/height/bytes, stage-local name, no extra surface/kind field.
    require(type(raw) is dict and set(raw) == set(RAW_FIELDS),
            'raw View frame field set mismatch')
    require(type(raw['file']) is str and '/' not in raw['file'] and '\\' not in raw['file']
            and Path(raw['file']).name == raw['file']
            and raw['file'] == f'visibility-stage-{stage}-view.bgra',
            'raw View frame is not the fixed stage-local file')
    typed_id(raw['browser'], 'view_raw.browser')
    require(raw['browser'] == ids, 'raw frame is not this Browser VIEW')
    for key in ('width', 'height', 'bytes', 'generation', 'sequence'):
        integer(raw[key], f'view_raw.{key}', 1)
    vw, vh = target(VIEW_DIP, scale)
    require((raw['width'], raw['height']) == (vw, vh),
            'raw View frame is not the fixed 320x240 device target')
    require(raw['bytes'] == vw * vh * 4, 'raw View frame byte length mismatch')
    return raw


def page_fingerprint(raw, data):
    """Require the fixed page's three colors before trusting a source frame.

    A complete-length, fully opaque all-white first frame is not the fixed
    ready page. Positions are device pixels inside the 4 DIP bars: left-edge
    middle (orange), top-edge middle (purple) and center (blue).
    """
    width, height = raw['width'], raw['height']
    samples = ((0, height // 2, PAGE_LEFT_RGB, 'left-orange'),
               (width // 2, 0, PAGE_TOP_RGB, 'top-purple'),
               (width // 2, height // 2, PAGE_CENTER_RGB, 'center-blue'))
    for x, y, expected, label in samples:
        at = (y * width + x) * 4
        blue, green, red, alpha = data[at:at + 4]
        require(alpha == 255, f'raw page {label} sample is not opaque')
        require((red, green, blue) == expected,
                f'raw page {label} sample is {red, green, blue}, not {expected}')
    return data


def raw_pixels(raw, data, scale):
    """Strict opaque full-BGRA length/alpha and fixed-page fingerprint gate."""
    vw, vh = target(VIEW_DIP, scale)
    require((raw['width'], raw['height']) == (vw, vh),
            'raw View frame is not the fixed 320x240 device target')
    require(type(data) is bytes and len(data) == raw['bytes'] == vw * vh * 4,
            'raw View frame byte length mismatch')
    require(all(alpha == 255 for alpha in data[3::4]),
            'raw View frame is not opaque full BGRA')
    return page_fingerprint(raw, data)


def read_raw(case, raw, ids, scale, stage):
    """Read only the expected opaque BGRA source; never a desktop screenshot."""
    raw_facts(raw, stage, ids, scale)
    path = case / raw['file']
    require(path.is_file(), f'missing raw View frame {raw["file"]}')
    data = path.read_bytes()
    raw_pixels(raw, data, scale)
    return path, data


def validate(meta, pid, stage):
    """Pure metadata predicate: no window, screenshot or raw-file access."""
    require(stage in range(4), 'unsupported visibility stage')
    require(type(meta) is dict and all(key in meta for key in REQUIRED),
            'visibility metadata is missing a required field')
    require(all(type(meta[key]) is int for key in ('schema', 'pid', 'stage'))
            and meta['schema'] == 1 and meta['pid'] == pid and meta['stage'] == stage,
            'visibility stage identity mismatch')
    require(meta['kind'] == 'visibility-stage'
            and meta['scope'] == 'host-owned-visibility-slice',
            'wrong visibility metadata kind or scope')
    require(meta['task_11_3_accepted'] is False and meta['input_sent'] is False
            and meta['foreground_forced'] is False and meta['observer_resized'] is False,
            'host declared an unapproved task/input/foreground/resize effect')
    require(meta['popup_clear_transition_verified'] is False,
            'popup clear transition is unproven and must stay false')
    scale = meta['scale_factor']
    require(type(scale) in (int, float) and math.isfinite(scale) and scale > 0,
            'invalid independent scale_factor')
    ids = meta['browser']
    typed_id(ids, 'full BrowserId')
    win = window_facts(meta['window'], pid, stage, scale)
    require(meta['view_bounds'] == VIEW_BOUNDS,
            'not the fixed 320x240 VIEW at top 48 DIP')
    diag = diagnostics(meta['diagnostics'], VIEW_BOUNDS, scale)
    require(diag['popup']['visible'] is False,
            'diagnostics reports a visible natural popup')
    for key in ('detached', 'source_visible', 'popup_visible', 'popup_frame_present'):
        boolean(meta[key], key)
    require(meta['detached'] is (stage == 3),
            'detached flag does not match the release stage')
    require(meta['source_visible'] is (stage in (0, 2)),
            'source_visible flag does not match the stage')
    require(meta['popup_visible'] is False and meta['popup_frame_present'] is False,
            'a natural popup/frame is present; empty-after-hide is not proven')
    source_generation = integer(meta['source_generation'], 'source_generation', 1)
    bridge = bridge_facts(meta['bridge'], meta, pid, stage, ids, scale)
    rec = recovery(meta['recovery'], 'recovery')
    raw = meta['view_raw']
    if stage == 3:
        require(raw is None, 'retired stage still reports a raw View frame')
        # The host must wait for the real Closed/Closing release before this
        # sample; source_visible=false is therefore observed, not inferred from
        # the retirement request alone.
        require(rec['status'] == 'Closed',
                'retired View was not observed Closed; source visibility is unproven')
        require(rec['queued'] is False and rec['post_failures'] == 0,
                'retired View is still queued or reports post failures')
        require(diag['frame_width'] == 0 and diag['frame_height'] == 0,
                'retired View still has a non-empty frame size')
        require(integer(diag['created_images'], 'diagnostics.created_images', 1)
                == diag['released_images'],
                'retired View images are not balanced')
        require(source_generation > bridge['hidden_generation'],
                'retired source generation does not postdate the hidden source')
        return float(scale)

    raw = raw_facts(raw, stage, ids, scale)
    rw, rh = raw['width'], raw['height']
    # An actual SetVisible(false) through UiComposition clears the presented
    # image, so the Hidden stage has a real 0x0 frame size and cannot claim
    # its retained source was the last GPUI-painted frame. Only the historical
    # paint count must still show that the View really painted before.
    require(integer(diag['paint_calls'], 'diagnostics.paint_calls', 1) > 0,
            'no View paint was ever recorded')
    if stage == 1:
        require(diag['frame_width'] == 0 and diag['frame_height'] == 0,
                'hidden View is not actually cleared to an empty frame')
        require(source_generation == bridge['hidden_generation']
                and source_generation > raw['generation'],
                'hidden source generation is not newer than the retained frame')
        require(bridge['before_hide_generation'] == raw['generation'],
                'pre-hide generation differs from the retained raw frame')
        held = bridge['retained']
        require(held['generation'] == raw['generation']
                and held['sequence'] == raw['sequence']
                and held['width'] == rw and held['height'] == rh,
                'bridge retained frame differs from the stage raw frame')
        require(rec['status'] == 'Hidden',
                'hidden stage did not report Hidden recovery')
        require(rec['queued'] is False and rec['post_failures'] == 0,
                'hidden recovery is queued or reports post failures')
        require(same_recovery(rec, bridge['hidden_recovery']),
                'current hidden recovery differs from the bridge baseline')
        require(rec['attempts'] == 0 and not rec['request_offsets_us'],
                'hidden recovery still has in-flight retry budget')
        require(rec['generation'] <= bridge['before_hide_generation'],
                'hidden recovery generation postdates the pre-hide source')
        return float(scale)

    require(diag['frame_width'] == rw and diag['frame_height'] == rh,
            'painted frame size differs from the raw frame')
    require(diag['painted_generation'] == raw['generation']
            and diag['painted_sequence'] == raw['sequence'],
            'raw frame is not the actual painted source frame')
    if stage == 0:
        require(source_generation == raw['generation'],
                'initial source generation differs from the raw frame')
        require(bridge['before_hide_generation'] == 0
                and bridge['hidden_generation'] == 0,
                'initial bridge already has a hidden generation')
        require(rec['status'] in ('Idle', 'Recovered'),
                'initial View reports a hidden/pending/exhausted state')
        require(rec['queued'] is False and rec['post_failures'] == 0,
                'initial recovery is queued or reports post failures')
        if rec['status'] == 'Recovered':
            require(rec['generation'] == source_generation,
                    'initial recovered generation differs from the source')
            require((rec['expected_width'], rec['expected_height']) == (rw, rh),
                    'initial recovered target differs from the painted frame')
        else:
            # Initial matching viewport need not arm the resize coordinator;
            # this exception is only the untouched zero baseline.
            require(all(rec[key] == 0 for key in M.RECOVERY_INTS)
                    and not rec['native_offsets_us']
                    and not rec['request_offsets_us'],
                    'initial Idle coordinator was already used')
    else:
        require(source_generation == raw['generation']
                and source_generation > bridge['hidden_generation'],
                'recovered source generation is not newer than the hidden source')
        held = bridge['retained']
        require(held['generation'] < raw['generation']
                and held['sequence'] < raw['sequence'],
                'recovered frame did not advance beyond the retained old frame')
        require(rec['status'] == 'Recovered',
                'recovered stage did not report Recovered recovery')
        require(rec['generation'] == source_generation,
                'recovery generation differs from the painted generation')
        require((rec['expected_width'], rec['expected_height']) == (rw, rh),
                'recovered target differs from the painted frame')
        hidden_rec = bridge['hidden_recovery']
        require(rec['acks'] > hidden_rec['acks'],
                'natural recovery advanced no ACK beyond the hidden baseline')
        require(rec['exhaustions'] == hidden_rec['exhaustions'],
                'recovery changed the exhaustion count')
        require(rec['queued'] is False and rec['post_failures'] == 0,
                'recovered View is still queued or reports post failures')
    return float(scale)


def chain(meta, prior, stage, case):
    """Same original run and monotonic host bridge history across stages."""
    require(stage in (1, 2, 3), 'chain has no previous stage')
    require(type(prior) is dict and prior.get('success') is True,
            'previous stage did not pass')
    require(type(prior.get('pid')) is int and prior.get('pid') == meta['pid']
            and type(prior.get('stage')) is int and prior.get('stage') == stage - 1,
            'previous stage belongs to another PID/stage')
    prev = prior.get('metadata')
    require(type(prev) is dict, 'previous stage has no metadata')
    prev_scale = validate(prev, meta['pid'], stage - 1)
    require(f32(prev_scale) == f32(meta['scale_factor']),
            'scale changed across stages')
    require(prior.get('hwnd') == meta['window']['hwnd'],
            'previous record HWND differs from the current original HWND')
    require(prior.get('owner_thread') == meta['window']['thread'],
            'previous owner thread differs from the current original thread')
    require(prior.get('browser') == meta['browser'], 'Browser changed across stages')
    require(type(prior.get('scale_factor')) in (int, float)
            and math.isfinite(prior.get('scale_factor'))
            and prior.get('scale_factor') > 0,
            'previous record has no independent scale')
    require(f32(prior.get('scale_factor')) == f32(meta['scale_factor']),
            'recorded scale changed across stages')
    for key in ('hwnd', 'pid', 'thread'):
        require(prev['window'][key] == meta['window'][key],
                f'original window {key} changed across stages')
    require(prev['bridge']['phase'] == PHASE_BY_STAGE[stage - 1],
            'previous bridge phase does not match its stage')
    if stage == 1:
        prev_raw = prev['view_raw']
        raw = meta['view_raw']
        require(type(prev_raw) is dict and type(raw) is dict,
                'missing raw frames in the hidden transition')
        # A natural complete frame may appear between stage 0 and the actual
        # minimize/pre-hide, so the retained old frame is not required to be
        # Arc- or pixel-identical to the stage-0 raw. It must still be the
        # actual pre-hide source: same bridge.retained metadata, generation ==
        # before_hide_generation and a sequence no older than stage 0.
        require(raw['generation'] >= prev_raw['generation']
                and raw['sequence'] >= prev_raw['sequence'],
                'hidden retained frame predates the stage-0 source frame')
        require(meta['bridge']['before_hide_generation'] == raw['generation'],
                'pre-hide generation differs from the retained raw generation')
        held = meta['bridge']['retained']
        require((held['generation'], held['sequence'], held['width'], held['height'])
                == (raw['generation'], raw['sequence'], raw['width'], raw['height']),
                'hidden raw frame differs from the bridge retained metadata')
        require(meta['bridge']['hidden_generation'] == meta['source_generation']
                and meta['source_generation'] > raw['generation'],
                'hidden generation is not a newer source generation')
        require(prev['bridge']['hide_command'] is None
                and prev['bridge']['show_command'] is None,
                'visibility commands existed before the hidden transition')
        for key in ('requests_total', 'native_invalidate_entries', 'acks', 'exhaustions'):
            require(meta['recovery'][key] == prev['recovery'][key],
                    f'hidden interval changed recovery {key}')
        require(same_recovery(meta['recovery'], meta['bridge']['hidden_recovery'],
                              'hidden transition recovery'),
                'hidden interval counter baseline differs from the actual recovery')
    elif stage == 2:
        prev_raw = prev['view_raw']
        raw = meta['view_raw']
        require(type(prev_raw) is dict and type(raw) is dict,
                'missing raw frames in the recovery transition')
        require(raw['generation'] > prev_raw['generation']
                and raw['sequence'] > prev_raw['sequence'],
                'recovery frame did not advance generation/sequence')
        require(meta['source_generation'] == raw['generation']
                and meta['source_generation'] > prev['source_generation'],
                'recovered source generation is not a newer actual frame')
        require(meta['bridge']['before_hide_generation']
                == prev['bridge']['before_hide_generation']
                and meta['bridge']['hidden_generation']
                == prev['bridge']['hidden_generation'],
                'hidden generation changed during recovery')
        require(meta['bridge']['hide_command'] == prev['bridge']['hide_command'],
                'hide command identity changed during recovery')
        require(meta['bridge']['retained'] == prev['bridge']['retained'],
                'recovery released or replaced the hidden old-frame lease early')
        require(same_recovery(meta['bridge']['hidden_recovery'],
                              prev['bridge']['hidden_recovery'],
                              'hidden baseline'),
                'hidden recovery baseline changed during recovery')
        require(meta['bridge']['quiet_ms_frozen'] is True
                and meta['bridge']['quiet_ms'] >= prev['bridge']['quiet_ms'],
                'restore did not freeze the actual hidden duration')
        require(same_observation(meta['bridge']['last_hidden_observation'],
                                 prev['bridge']['last_hidden_observation']),
                'recovery replaced or refreshed the last hidden observation')
        require(prev['bridge']['restore_requests'] == 0
                and meta['bridge']['restore_requests'] == 1,
                'ordinary restore was not requested exactly once after hide')
        require(meta['recovery']['acks'] > prev['recovery']['acks'],
                'recovery did not advance ACK beyond the hidden baseline')
        require(meta['recovery']['exhaustions'] == prev['recovery']['exhaustions'],
                'recovery changed the exhaustion count')
        require(prev['source_visible'] is False and meta['source_visible'] is True,
                'source visibility did not transition Hidden->visible')
        require(meta['bridge']['show_command']['sequence']
                > prev['bridge']['hide_command']['sequence'],
                'show command does not follow the hide command')
    else:
        require(prev['view_raw'] is not None and meta['view_raw'] is None,
                'release stage still reports a View frame')
        require(prev['bridge']['phase'] == 'SourceRecovered'
                and meta['bridge']['phase'] == 'Retired',
                'release did not retire the recovered bridge')
        require(prev['bridge']['retained'] is not None
                and meta['bridge']['retained'] is None,
                'retirement did not release the old-frame lease')
        require(meta['bridge']['hide_command'] == prev['bridge']['hide_command']
                and meta['bridge']['show_command'] == prev['bridge']['show_command'],
                'visibility command identity changed during retirement')
        require(meta['bridge']['before_hide_generation']
                == prev['bridge']['before_hide_generation']
                and meta['bridge']['hidden_generation']
                == prev['bridge']['hidden_generation'],
                'hidden generation changed during retirement')
        require(same_recovery(meta['bridge']['hidden_recovery'],
                              prev['bridge']['hidden_recovery'],
                              'hidden baseline'),
                'hidden recovery baseline changed during retirement')
        require(meta['bridge']['quiet_ms_frozen'] is True
                and meta['bridge']['quiet_ms'] == prev['bridge']['quiet_ms'],
                'retirement did not keep the frozen hidden duration')
        require(same_observation(meta['bridge']['last_hidden_observation'],
                                 prev['bridge']['last_hidden_observation']),
                'retirement replaced or refreshed the last hidden observation')
        require(prev['source_visible'] is True and meta['source_visible'] is False,
                'source visibility did not transition recovered->released')
        require(prev['detached'] is False and meta['detached'] is True,
                'release stage is not detached')
        require(meta['bridge']['retained'] is None,
                'retired stage still retains a frame')


def verify_manual_initial(meta, enabled):
    facts = meta.get('initial_foreground')
    if not enabled:
        require(facts is None or (type(facts) is dict and facts.get('policy') == 'passive'),
                'unexpected manual Visibility foreground metadata')
        return
    require(type(facts) is dict and facts.get('policy') == 'manual-owned-observation'
            and facts.get('approved') is False and type(facts.get('attempts')) is int
            and facts['attempts'] == 0 and type(facts.get('before')) is bool
            and facts.get('after') is True, 'manual Visibility readiness was not qualified')


def capture_view(u, case, stage, meta, scale, state, hwnd, thread, roots, record,
                 manual_initial=False):
    """Full owned-client RGB and zero-tolerance whole-VIEW comparison."""
    bounds = meta['view_bounds']
    vx, vy = I.px(bounds[0], scale), I.px(bounds[1], scale)
    vw, vh = target(VIEW_DIP, scale)
    cw, ch = state['client_width'], state['client_height']
    require(0 <= vx and 0 <= vy and vx + vw <= cw and vy + vh <= ch,
            'entire VIEW is not inside the owned client')
    points = [{'name': f'grid-{a}-{b}',
               'client_px': [min(cw - 1, int(cw * a)), min(ch - 1, int(ch * b))]}
              for a in (1 / 6, .5, 5 / 6) for b in (1 / 6, .5, 5 / 6)]
    points += [{'name': f'view-{a}-{b}',
                'client_px': [vx + min(vw - 1, int(vw * a)),
                              vy + min(vh - 1, int(vh * b))]}
               for a in (.05, .5, .95) for b in (.05, .5, .95)]
    require(len(points) == 18, 'owner probe plan is not the fixed 18 points')
    before = H.run_probes(u, hwnd, state, points)
    record['capture_evidence'] = {'owner_pre': state, 'probes_pre': before}
    require(all(p['owned'] for p in before),
            'owned client is occluded before capture')
    box = (state['client_left'], state['client_top'],
           state['client_right'], state['client_bottom'])
    desktop, primary = H.virtual_screen(u), H.primary_screen(u)
    require(box[0] >= desktop['left'] and box[1] >= desktop['top']
            and box[2] <= desktop['left'] + desktop['width']
            and box[3] <= desktop['top'] + desktop['height'],
            'owned client is outside the actual desktop')
    on_primary = (box[0] >= primary['left'] and box[1] >= primary['top']
                  and box[2] <= primary['left'] + primary['width']
                  and box[3] <= primary['top'] + primary['height'])
    if manual_initial:
        require(H.pointer_value(u.GetForegroundWindow()) == hwnd,
                'manual Visibility lost original foreground before initial capture')
        record['manual_foreground_pre'] = hwnd
    image = H.ImageGrab.grab(bbox=box, all_screens=not on_primary)
    if manual_initial:
        require(H.pointer_value(u.GetForegroundWindow()) == hwnd,
                'manual Visibility lost original foreground during initial capture')
        record['manual_foreground_post'] = hwnd
    after = H.inspect_window(u, hwnd)
    probes = H.run_probes(u, hwnd, state, points)
    after_roots = H.enum_visible_top_level_windows(u, meta['pid'])
    record['capture_evidence'].update(owner_post=after, probes_post=probes,
                                      image_mode=image.mode,
                                      image_size=[int(image.width), int(image.height)])
    require(after == state and all(p['owned'] for p in probes)
            and after_roots == roots
            and int(u.GetWindowThreadProcessId(hwnd, None)) == thread,
            'owner/geometry/occlusion changed during capture')
    require(image.mode == 'RGB' and image.size == (cw, ch),
            'not an unfiltered full owned-client RGB capture')
    png = case / f'visibility-window-{stage}.png'
    H.save_png_atomic(image, png)
    png_sha = H.sha256_file(png)
    evidence = {'png': png.name, 'png_path': str(png), 'sha256': png_sha,
                'image_mode': image.mode, 'image_size': [cw, ch],
                'viewport_physical': [vx, vy, vx + vw, vy + vh],
                'raw': None, 'pixel_difference_bbox': None,
                'owner_pre': state, 'owner_post': after,
                'probes_pre': before, 'probes_post': probes}
    # The PNG path/hash survives a later pixel or raw failure as evidence.
    record['capture_evidence'] = evidence
    actual = image.crop((vx, vy, vx + vw, vy + vh))
    require(actual.size == (vw, vh),
            'VIEW crop is not the independent f32-ceil device target')
    raw_record = None
    if stage == 3:
        expected = Image.new('RGB', (vw, vh), GREEN)
    else:
        raw = meta['view_raw']
        path, data = read_raw(case, raw, meta['browser'], scale, stage)
        require((raw['width'], raw['height']) == (vw, vh),
                'raw View frame does not cover the whole VIEW')
        expected = Image.frombytes(
            'RGBA', (raw['width'], raw['height']), data, 'raw', 'BGRA').convert('RGB')
        raw_record = {'file': raw['file'], 'bytes': len(data),
                      'sha256': H.sha256_file(path),
                      'generation': raw['generation'], 'sequence': raw['sequence']}
    difference = ImageChops.difference(actual, expected).getbbox()
    evidence.update(raw=raw_record, pixel_difference_bbox=difference)
    # One canonical success block, matching the Exhaustion record shape. The
    # earlier partial capture_evidence remains only on a failed stage.
    record.pop('capture_evidence', None)
    record['capture'] = evidence
    require(difference is None,
            f'whole VIEW differs from expected pixels: {difference}')
    return evidence


def stage1_record_facts(record, meta, prior):
    """Minimized record evidence: real native identity, no capture/PNG."""
    require(type(record) is dict, 'missing stage-1 record')
    require(record.get('capture') is None,
            'minimized stage must not claim a client capture')
    identity = record.get('hidden_identity')
    require(type(identity) is dict
            and set(identity) == {'hwnd', 'pid', 'thread', 'is_window', 'is_iconic'},
            'stage-1 hidden identity field set mismatch')
    require(identity['hwnd'] == meta['window']['hwnd']
            and identity['pid'] == meta['pid']
            and identity['thread'] == meta['window']['thread'],
            'stage-1 hidden identity does not name the original HWND/PID/thread')
    require(identity['is_window'] is True and identity['is_iconic'] is True,
            'stage-1 hidden identity does not show a real minimized window')
    identity_facts = record.get('window_identity')
    require(type(identity_facts) is dict,
            'stage-1 does not keep the original stage-0 window identity')
    require(identity_facts == prior['metadata']['window'],
            'stage-1 changed the original stage-0 window identity')
    require(all(identity_facts.get(key) == value
                for key, value in (('hwnd', meta['window']['hwnd']),
                                   ('pid', meta['window']['pid']),
                                   ('thread', meta['window']['thread']))),
            'stage-1 kept identity is not this original HWND/PID/thread')
    held = record.get('retained_source_raw')
    raw = meta['view_raw']
    require(type(held) is dict
            and set(held) == {'file', 'bytes', 'sha256', 'generation', 'sequence',
                              'width', 'height'},
            'stage-1 retained source evidence field set mismatch')
    require((held['file'], held['bytes'], held['generation'], held['sequence'],
             held['width'], held['height'])
            == (raw['file'], raw['bytes'], raw['generation'], raw['sequence'],
                raw['width'], raw['height']),
            'stage-1 retained source evidence differs from the actual raw frame')
    require(type(held['sha256']) is str and len(held['sha256']) == 64,
            'stage-1 retained source hash is missing')
    return record


def publish_no_replace(path, record):
    """Atomic hard-link publication; an existing output is never replaced."""
    text = json.dumps(record, indent=2, ensure_ascii=False) + '\n'
    stream = None
    temporary = None
    for attempt in range(8):
        candidate = path.with_name(f'{path.name}.{os.getpid()}.{attempt}.tmp')
        try:
            stream = open(candidate, 'x', encoding='utf-8', newline='\n')
        except FileExistsError:
            continue
        temporary = candidate
        break
    if stream is None:
        raise H.CaptureError('cannot create a unique temporary output file')
    try:
        with stream:
            stream.write(text)
            stream.flush()
            os.fsync(stream.fileno())
        try:
            os.link(temporary, path)
        except FileExistsError as exc:
            raise H.CaptureError(
                f'output {path.name} already exists; no-replace publication '
                'refused') from exc
    finally:
        try:
            temporary.unlink()
        except OSError:
            pass


def run(args):
    case, stage = args.case_dir.resolve(), args.stage
    record = {'schema': 1, 'kind': 'visibility-owned-window',
              'scope': 'host-owned-visibility-slice', 'pid': args.pid, 'stage': stage,
              'success': False, 'error': None, 'task_11_3_accepted': False,
              'input_sent': False, 'foreground_forced': False,
              'observer_resized': False, 'popup_clear_transition_verified': False}
    output = case / f'visibility-window-{stage}.json'
    try:
        require(sys.platform == 'win32', 'real Windows desktop required')
        require(args.pid > 0, 'positive owned PID required')
        # No-replace output: refuse before any desktop access so a rerun cannot
        # overwrite an earlier record or its PNG evidence.
        require(not output.exists(),
                f'output {output.name} already exists; refusing to replace it')
        if stage != 1:
            require(not (case / f'visibility-window-{stage}.png').exists(),
                    'existing stage PNG; refusing to replace capture evidence')
        path = case / f'visibility-stage-{stage}.json'
        meta = I.load(path)
        record.update(metadata=meta, metadata_sha256=H.sha256_file(path))
        scale = validate(meta, args.pid, stage)
        manual_initial = args.manual_foreground and stage == 0
        if stage == 0:
            verify_manual_initial(meta, manual_initial)
        prior = None
        if stage:
            # Validate the complete previous metadata and bridge chain before
            # any desktop/window access. A failed prior stage stops here.
            prior_path = case / f'visibility-window-{stage - 1}.json'
            require(prior_path.is_file(), 'missing previous stage record')
            prior = I.load(prior_path)
            require(type(prior.get('pid')) is int and prior.get('pid') == args.pid
                    and type(prior.get('stage')) is int and prior.get('stage') == stage - 1,
                    'previous stage belongs to another run')
            require(prior.get('success') is True,
                    'previous stage did not pass; stopping before desktop access')
            require(integer(prior.get('hwnd'), 'previous hwnd', 1)
                    == meta['window']['hwnd'],
                    'previous stage HWND differs; stopping before desktop access')
            chain(meta, prior, stage, case)
        if stage == 1:
            # Minimized: exactly IsWindow / IsIconic /
            # GetWindowThreadProcessId, no client/DPI inspection, no capture,
            # no enumeration of foreign or other windows.
            u = H.load_win32()
            hwnd = meta['window']['hwnd']
            thread = meta['window']['thread']
            actual_pid = wt.DWORD()
            is_window = bool(u.IsWindow(hwnd))
            is_iconic = bool(u.IsIconic(hwnd))
            actual_thread = int(u.GetWindowThreadProcessId(hwnd, ctypes.byref(actual_pid)))
            hidden_identity = {'hwnd': hwnd, 'pid': int(actual_pid.value),
                               'thread': actual_thread, 'is_window': is_window,
                               'is_iconic': is_iconic}
            record['hidden_identity'] = hidden_identity
            require(is_window, 'original owned HWND no longer exists')
            require(is_iconic, 'original owned HWND is not actually minimized')
            require(actual_thread == thread and actual_pid.value == args.pid,
                    'original HWND/PID/thread identity changed')
            raw = meta['view_raw']
            raw_path, data = read_raw(case, raw, meta['browser'], scale, stage)
            record['retained_source_raw'] = {
                'file': raw['file'], 'bytes': len(data),
                'sha256': H.sha256_file(raw_path),
                'generation': raw['generation'], 'sequence': raw['sequence'],
                'width': raw['width'], 'height': raw['height']}
            record['window_identity'] = prior['metadata']['window']
            record.update(hwnd=hwnd, owner_thread=thread, thread=thread,
                          browser=meta['browser'], scale_factor=scale,
                          capture=None)
            stage1_record_facts(record, meta, prior)
            record['success'] = True
        else:
            u = H.load_win32()
            record['thread_dpi_awareness'] = H.set_thread_dpi_awareness(u)
            roots = H.enum_visible_top_level_windows(u, args.pid)
            require(len(roots) == 1,
                    'not exactly one owned visible top-level window')
            hwnd = roots[0]
            require(meta['window']['hwnd'] == hwnd,
                    'metadata HWND is not the owned original root')
            thread = meta['window']['thread']
            state = I.verify_owner(u, meta['window'], I.TITLE, args.pid, thread)
            require(f32(state['dpi'] / 96) == f32(scale),
                    'actual HWND DPI != host GPUI scale')
            require((state['client_width'], state['client_height'])
                    == target(HOST_DIP, scale),
                    'actual client is not the fixed 600x450 DIP target')
            record.update(hwnd=hwnd, owner_thread=thread, thread=thread,
                          browser=meta['browser'], scale_factor=scale)
            record['capture'] = capture_view(
                u, case, stage, meta, scale, state, hwnd, thread, roots, record,
                manual_initial=manual_initial)
            record['success'] = True
    except Exception as exc:
        record.update(error=f'{type(exc).__name__}: {exc}',
                      traceback=traceback.format_exc())
    record['limits'] = (
        'Read-only observer of one physical DPI and one owned original window. '
        'Stage 1 performs only IsWindow/IsIconic/GetWindowThreadProcessId on the '
        'original HWND: no client screenshot, DPI/client gate, foreign-window '
        'enumeration or hidden-desktop image is claimed, and the retained '
        'pre-hide source is not claimed to be the last painted frame. Owner '
        'probes, state '
        'snapshots and ImageGrab are separate point-in-time operations, so a '
        'transient overlay can be missed; screenshot RGB is not alpha proof. '
        'The raw BGRA frame is an expected source, never the captured image; '
        'it must be the complete opaque page with the fixed left-orange, '
        'top-purple and center-blue fingerprint, so a blank/all-white first '
        'frame cannot be accepted as the ready page. '
        'The natural popup did not exist before the hide, so only '
        'empty-after-hide at these fixed stages is observed and '
        'popup_clear_transition_verified stays false; no popup transition is '
        'proven. No input, IME, activation, foreground request, resize, '
        'minimize/restore, policy change or foreign-window operation is '
        'performed here.')
    try:
        publish_no_replace(output, record)
    except Exception as exc:
        print(f'Visibility stage {stage}: FAILED: output not published: '
              f'{type(exc).__name__}: {exc}', file=sys.stderr, flush=True)
        return 1
    print(f'Visibility stage {stage}: '
          f'{"PASSED" if record["success"] else "FAILED"}: {record["error"]}',
          flush=True)
    return 0 if record['success'] else 1


def main():
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument('--pid', required=True, type=int)
    parser.add_argument('--case-dir', required=True, type=Path)
    parser.add_argument('--stage', required=True, type=int, choices=range(4))
    parser.add_argument('--manual-foreground', action='store_true')
    return run(parser.parse_args())


if __name__ == '__main__':
    sys.exit(main())
