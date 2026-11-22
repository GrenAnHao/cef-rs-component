#!/usr/bin/env python3
"""Pure validator/chain predicates for the 11.3 visibility observer.

No window, desktop, screenshot, raw-file or publication access: fixtures are
plain records only. The production module is imported as-is; nothing here
replaces or relaxes its assertions.
"""
import copy
import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    'visibility_predicate', Path(__file__).with_name('component-visibility-test.py'))
V = importlib.util.module_from_spec(spec)
spec.loader.exec_module(V)
H = V.H

PID, SCALE = 10, 1.5
IDS = [1, 2]
HWND, THREAD = 4242, 77
WINDOW = {'hwnd': HWND, 'pid': PID, 'thread': THREAD, 'dpi': 144, 'left': 40,
          'top': 40, 'width': 900, 'height': 675}
BOUNDS = [0, 48, 320, 240]


def rec(status='Recovered', generation=1, width=480, height=360, requests=4,
        native=4, acks=3, exhaustions=0):
    return {'status': status, 'generation': generation, 'expected_width': width,
            'expected_height': height, 'attempts': 0, 'requests_total': requests,
            'native_invalidate_entries': native, 'native_thread': 100,
            'native_offsets_us': [], 'request_offsets_us': [], 'acks': acks,
            'exhaustions': exhaustions, 'queued': False, 'post_failures': 0}


def diag(painted=(1, 3), frame=(480, 360), created=1, released=0):
    return {'schema': 1, 'paint_calls': 2, 'painted_sequence': painted[1],
            'painted_generation': painted[0], 'frame_width': frame[0],
            'frame_height': frame[1], 'scale_factor': SCALE,
            'surface_bounds': [float(x) for x in BOUNDS], 'created_images': created,
            'released_images': released, 'consumed_updates': 1,
            'binding_retired': False, 'binding_revision': 1, 'resync_attempts': 0,
            'rejected_paints': 0, 'pending_resync': False, 'clicks_submitted': 0,
            'clicks_rejected': 0, 'pending_click': False, 'last_error': None,
            'popup': {'generation': 0, 'visible': False, 'bounds': [0, 0, 0, 0],
                      'frame_width': 0, 'frame_height': 0, 'created_images': 0,
                      'released_images': 0, 'paint_calls': 0, 'painted_sequence': 0,
                      'painted_generation': 0, 'painted_bounds': [0, 0, 0, 0]}}


def raw(stage, generation, sequence):
    return {'file': f'visibility-stage-{stage}-view.bgra', 'browser': list(IDS),
            'generation': generation, 'sequence': sequence, 'width': 480,
            'height': 360, 'bytes': 480 * 360 * 4}


def bridge(**overrides):
    value = {'phase': 'Ready', 'browser': list(IDS),
             'owner': {'hwnd': HWND, 'pid': PID, 'thread': THREAD},
             'viewport': {'width': 320, 'height': 240, 'scale': SCALE},
             'minimize_requests': 0, 'restore_requests': 0,
             'before_hide_generation': 0, 'hidden_generation': 0,
             'hide_command': None, 'show_command': None, 'retained': None,
             'hidden_recovery': None, 'last_hidden_observation': None,
             'quiet_ms': None, 'quiet_ms_frozen': False,
             'popup_clear_transition_verified': False}
    value.update(overrides)
    return value


def hidden_bridge(hidden_recovery, quiet=310, frozen=False):
    retained_frame = {'browser': list(IDS), 'generation': 2, 'sequence': 5,
                      'width': 480, 'height': 360}
    return bridge(phase='Hidden', minimize_requests=1, before_hide_generation=2,
                  hidden_generation=3,
                  hide_command={'browser': list(IDS), 'sequence': 7},
                  retained=retained_frame,
                  hidden_recovery=copy.deepcopy(hidden_recovery),
                  last_hidden_observation={
                      'source_generation': 3, 'source_visible': False,
                      'popup_visible': False, 'popup_frame_present': False,
                      'retained_same_arc': True,
                      'recovery': copy.deepcopy(hidden_recovery)},
                  quiet_ms=quiet, quiet_ms_frozen=frozen)


def recovered_bridge(hidden_recovery, quiet=310):
    hidden = hidden_bridge(hidden_recovery, quiet=quiet, frozen=True)
    hidden.update(phase='SourceRecovered', restore_requests=1,
                  show_command={'browser': list(IDS), 'sequence': 9})
    return hidden


def retired_bridge(hidden_recovery, quiet=310):
    retired = recovered_bridge(hidden_recovery, quiet=quiet)
    retired.update(phase='Retired', retained=None)
    return retired


def hidden_rec():
    # Hiding cancels recovery without advancing its target generation, so the
    # Hidden diagnostics still name the pre-hide generation; the mailbox VIEW
    # generation is the newer hidden_generation.
    return rec(status='Hidden', generation=2)


def meta(stage):
    facts = {'schema': 1, 'kind': 'visibility-stage',
             'scope': 'host-owned-visibility-slice', 'pid': PID, 'stage': stage,
             'browser': list(IDS), 'window': copy.deepcopy(WINDOW),
             'scale_factor': SCALE, 'view_bounds': list(BOUNDS),
             'task_11_3_accepted': False, 'input_sent': False,
             'foreground_forced': False, 'observer_resized': False,
             'popup_clear_transition_verified': False, 'popup_visible': False,
             'popup_frame_present': False}
    if stage == 0:
        facts.update(view_raw=raw(0, 1, 3), source_generation=1,
                     detached=False, source_visible=True,
                     diagnostics=diag(painted=(1, 3)), recovery=rec(),
                     bridge=bridge())
    elif stage == 1:
        # A real SetVisible(false) through UiComposition clears the presented
        # image: the Hidden stage has a 0x0 frame size even though the paint
        # history and the retained pre-hide source remain.
        facts.update(view_raw=raw(1, 2, 5), source_generation=3,
                     detached=False, source_visible=False,
                     diagnostics=diag(painted=(2, 5), frame=(0, 0)),
                     recovery=hidden_rec(),
                     bridge=hidden_bridge(hidden_rec()))
    elif stage == 2:
        facts.update(view_raw=raw(2, 4, 8), source_generation=4,
                     detached=False, source_visible=True,
                     diagnostics=diag(painted=(4, 8)),
                     recovery=rec(generation=4, requests=6, native=6, acks=4),
                     bridge=recovered_bridge(hidden_rec()))
    else:
        facts.update(view_raw=None, source_generation=4,
                     detached=True, source_visible=False,
                     diagnostics=diag(painted=(4, 8), frame=(0, 0), created=3,
                                      released=3),
                     recovery=rec(status='Closed', generation=4, requests=6,
                                  native=6, acks=4),
                     bridge=retired_bridge(hidden_rec()))
    return facts


def prior_record(stage):
    record = {'schema': 1, 'kind': 'visibility-owned-window',
              'scope': 'host-owned-visibility-slice', 'pid': PID, 'stage': stage,
              'success': True, 'hwnd': HWND, 'owner_thread': THREAD,
              'browser': list(IDS), 'scale_factor': SCALE,
              'metadata': meta(stage)}
    if stage == 1:
        record['capture'] = None
    return record


def page(width, height):
    """Fixed page: 4 DIP left orange bar, top purple bar, blue body."""
    pixels = bytearray(bytes((126, 76, 26, 255)) * (width * height))
    for x, y, rgb in ((0, height // 2, V.PAGE_LEFT_RGB),
                      (width // 2, 0, V.PAGE_TOP_RGB),
                      (width // 2, height // 2, V.PAGE_CENTER_RGB)):
        at = (y * width + x) * 4
        red, green, blue = rgb
        pixels[at:at + 4] = bytes((blue, green, red, 255))
    return pixels


class ManualInitialForeground(unittest.TestCase):
    def test_manual_initial_requires_explicit_matching_observation(self):
        facts = dict(policy='manual-owned-observation', approved=False,
                     attempts=0, before=False, after=True)
        V.verify_manual_initial({'initial_foreground': facts}, True)
        V.verify_manual_initial({}, False)
        for changes in (dict(attempts=1), dict(attempts=False), dict(approved=True),
                        dict(before=None), dict(after=False), dict(policy='passive')):
            with self.subTest(changes=changes), self.assertRaises(H.CaptureError):
                V.verify_manual_initial({'initial_foreground': dict(facts, **changes)}, True)
        with self.assertRaises(H.CaptureError):
            V.verify_manual_initial({}, True)
        with self.assertRaises(H.CaptureError):
            V.verify_manual_initial({'initial_foreground': facts}, False)


class VisibilityPredicate(unittest.TestCase):
    def test_all_four_stages_accept_the_full_valid_contract(self):
        idle = meta(0)
        idle['recovery'] = rec(status='Idle', generation=0, width=0, height=0,
                               requests=0, native=0, acks=0)
        idle['recovery']['native_thread'] = 0
        self.assertEqual(V.validate(idle, PID, 0), SCALE)
        used = copy.deepcopy(idle)
        used['recovery']['attempts'] = 1
        with self.assertRaises(H.CaptureError):
            V.validate(used, PID, 0)
        used_native = copy.deepcopy(idle)
        used_native['recovery']['native_invalidate_entries'] = 1
        with self.assertRaises(H.CaptureError):
            V.validate(used_native, PID, 0)
        hidden_early = meta(0)
        hidden_early['recovery'] = hidden_rec()
        with self.assertRaises(H.CaptureError):
            V.validate(hidden_early, PID, 0)
        pending = meta(0)
        pending['recovery']['status'] = 'Pending'
        with self.assertRaises(H.CaptureError):
            V.validate(pending, PID, 0)
        recovered = meta(0)
        recovered['recovery']['expected_width'] = 750
        with self.assertRaises(H.CaptureError):
            V.validate(recovered, PID, 0)

        for stage in range(4):
            record = meta(stage)
            self.assertEqual(V.validate(record, PID, stage), SCALE)
        # Stage-1 is a minimized gate: an absent/empty client and DPI must not
        # cause a false rejection, while stage 0 still requires that identity.
        minimized = meta(1)
        for key in ('dpi', 'left', 'top', 'width', 'height'):
            del minimized['window'][key]
        self.assertEqual(V.validate(minimized, PID, 1), SCALE)
        with self.assertRaises(H.CaptureError):
            V.validate(meta(0), PID, 1)
        self.assertEqual(V.validate(meta(0), PID, 0), SCALE)
        # Stage-1 reports no capture, but keeps the real hidden identity.
        with self.assertRaises(H.CaptureError):
            V.validate(meta(1), PID, 0)
        missing_dpi = meta(0)
        del missing_dpi['window']['dpi']
        with self.assertRaises(H.CaptureError):
            V.validate(missing_dpi, PID, 0)
        captured_early = meta(0)
        captured_early['detached'] = True
        with self.assertRaises(H.CaptureError):
            V.validate(captured_early, PID, 0)

        for key, value in [
                ('schema', True), ('schema', 2), ('pid', 11), ('stage', 0),
                ('kind', 'visibility-owned-window'), ('kind', 'recovery-exhaustion-slice'),
                ('scope', 'window-layout-resize-slice'), ('task_11_3_accepted', True),
                ('input_sent', True), ('foreground_forced', True),
                ('observer_resized', True), ('detached', True), ('source_visible', False),
                ('popup_visible', True), ('popup_frame_present', True),
                ('popup_clear_transition_verified', True),
                ('view_bounds', [0, 47, 320, 240]), ('view_bounds', [0, 48, 321, 240]),
                ('view_bounds', [0, 48, 320, 241]), ('scale_factor', True),
                ('scale_factor', 0), ('scale_factor', float('inf')),
                ('browser', [1, True]), ('browser', [1]), ('browser', [0, 2]),
                ('source_generation', 0), ('source_generation', True),
                ('window', {k: v for k, v in WINDOW.items() if k != 'dpi'}),
                ('window', dict(WINDOW, dpi=120)),
                ('window', dict(WINDOW, width=899)),
                ('window', dict(WINDOW, height=676)),
                ('window', dict(WINDOW, pid=11)),
                ('window', dict(WINDOW, thread=78))]:
            bad = meta(2)
            bad[key] = value
            with self.assertRaises(H.CaptureError, msg=f'{key}={value}'):
                V.validate(bad, PID, 2)

        no_popup_bound = meta(2)
        del no_popup_bound['diagnostics']['popup']['painted_bounds']
        with self.assertRaises(H.CaptureError):
            V.validate(no_popup_bound, PID, 2)
        visible_popup = meta(2)
        visible_popup['diagnostics']['popup']['visible'] = True
        with self.assertRaises(H.CaptureError):
            V.validate(visible_popup, PID, 2)
        for key in ('recovery', 'bridge', 'view_raw', 'diagnostics', 'window',
                    'view_bounds', 'scale_factor', 'source_visible', 'detached'):
            bad = meta(2)
            del bad[key]
            with self.assertRaises(H.CaptureError, msg=key):
                V.validate(bad, PID, 2)

    def test_raw_frame_recovery_and_retirement_fail_closed(self):
        good = meta(2)
        self.assertEqual(V.validate(good, PID, 2), SCALE)
        for mutate in [
            lambda d: d['view_raw'].update(file='visibility-stage-1-view.bgra'),
            lambda d: d['view_raw'].update(file='../visibility-stage-2-view.bgra'),
            lambda d: d['view_raw'].update(file='visibility-stage-2-view.bgra\\x'),
            lambda d: d['view_raw'].update(browser=[1, 3]),
            lambda d: d['view_raw'].update(browser=[1, True]),
            lambda d: d['view_raw'].update(width=750, height=450, bytes=1_350_000),
            lambda d: d['view_raw'].update(bytes=480 * 360 * 4 - 4),
            lambda d: d['view_raw'].update(generation=3),
            lambda d: d['view_raw'].update(sequence=7),
            lambda d: d['view_raw'].update(generation=True),
            lambda d: d['view_raw'].update(surface='Popup'),
            lambda d: d['diagnostics'].update(frame_width=479),
            lambda d: d['diagnostics'].update(painted_generation=3),
            lambda d: d['diagnostics'].update(painted_sequence=7),
            lambda d: d['diagnostics'].update(paint_calls=0),
            lambda d: d.update(source_generation=3),
            lambda d: d.update(source_generation=5),
            lambda d: d['recovery'].update(status='Hidden'),
            lambda d: d['recovery'].update(status='Exhausted'),
            lambda d: d['recovery'].update(status='Pending'),
            lambda d: d['recovery'].update(generation=3),
            lambda d: d['recovery'].update(expected_width=750),
            lambda d: d['recovery'].update(acks=3),
            lambda d: d['recovery'].update(acks='4'),
            lambda d: d['recovery'].update(exhaustions=1),
            lambda d: d['recovery'].update(queued=True),
            lambda d: d['recovery'].update(post_failures=1),
            lambda d: d['bridge']['retained'].update(generation=4),
            lambda d: d['bridge']['retained'].update(sequence=8),
            lambda d: d['bridge'].update(retained=None),
            lambda d: d['bridge']['show_command'].update(sequence=7),
            lambda d: d['bridge']['show_command'].update(browser=[1, 3]),
            lambda d: d['bridge'].update(show_command=None),
            lambda d: d['bridge'].update(restore_requests=0),
            lambda d: d['bridge'].update(minimize_requests=0),
            lambda d: d['bridge'].update(phase='ShowPosted'),
            lambda d: d['bridge'].update(quiet_ms=None),
            lambda d: d['bridge'].update(quiet_ms=249),
            lambda d: d['bridge'].update(quiet_ms=True),
            lambda d: d['bridge'].update(quiet_ms_frozen=False),
            lambda d: d['bridge'].update(last_hidden_observation=None),
            lambda d: d['bridge']['last_hidden_observation'].update(source_generation=4),
            lambda d: d['bridge']['last_hidden_observation'].update(retained_same_arc=False),
            lambda d: d['bridge']['last_hidden_observation'].update(popup_visible=True),
            lambda d: d['bridge']['last_hidden_observation'].update(popup_frame_present=True),
            lambda d: d['bridge']['last_hidden_observation'].update(source_visible=True),
            lambda d: d['bridge']['last_hidden_observation']['recovery'].update(acks=4),
            lambda d: d['bridge']['last_hidden_observation']['recovery'].update(requests_total=6),
            lambda d: d['bridge']['last_hidden_observation']['recovery'].update(status='Recovered'),
            lambda d: d['bridge'].update(hidden_recovery=None),
            lambda d: d['bridge']['hidden_recovery'].update(acks=4),
            lambda d: d['bridge']['hidden_recovery'].update(status='Pending'),
            lambda d: d['bridge']['hidden_recovery'].update(generation=4),
            lambda d: d['bridge']['hidden_recovery'].update(queued=True),
            lambda d: d['bridge']['hidden_recovery'].update(post_failures=1),
            lambda d: d['bridge'].update(before_hide_generation=3),
            lambda d: d['bridge'].update(hidden_generation=4),
            lambda d: d['bridge'].update(hidden_generation=2),
            lambda d: d['bridge'].update(browser=[1, 3]),
            lambda d: d['bridge'].update(viewport={'width': 321, 'height': 240, 'scale': SCALE}),
            lambda d: d['bridge'].update(viewport={'width': 320, 'height': 241, 'scale': SCALE}),
            lambda d: d['bridge'].update(viewport={'width': 320, 'height': 240, 'scale': 1.25}),
            lambda d: d['bridge']['owner'].update(hwnd=4243),
            lambda d: d['bridge']['owner'].update(pid=11),
            lambda d: d['bridge']['owner'].update(thread=78),
            lambda d: d['bridge'].update(popup_clear_transition_verified=True),
            lambda d: d['bridge'].update(stale=True),
            lambda d: d['bridge'].pop('quiet_ms_frozen'),
        ]:
            bad = copy.deepcopy(good)
            mutate(bad)
            with self.assertRaises(H.CaptureError):
                V.validate(bad, PID, 2)

        for field in ('phase', 'browser', 'owner', 'viewport', 'minimize_requests',
                      'restore_requests', 'before_hide_generation', 'hidden_generation',
                      'hide_command', 'show_command', 'retained', 'hidden_recovery',
                      'last_hidden_observation', 'quiet_ms', 'quiet_ms_frozen',
                      'popup_clear_transition_verified'):
            bad = meta(2)
            del bad['bridge'][field]
            with self.assertRaises(H.CaptureError, msg=field):
                V.validate(bad, PID, 2)

        initial = meta(0)
        initial['bridge']['quiet_ms'] = 300
        with self.assertRaises(H.CaptureError):
            V.validate(initial, PID, 0)
        initial = meta(0)
        initial['bridge']['hide_command'] = {'browser': list(IDS), 'sequence': 1}
        with self.assertRaises(H.CaptureError):
            V.validate(initial, PID, 0)
        initial = meta(0)
        initial['bridge']['last_hidden_observation'] = {
            'source_generation': 1, 'source_visible': False,
            'popup_visible': False, 'popup_frame_present': False,
            'retained_same_arc': True, 'recovery': rec()}
        with self.assertRaises(H.CaptureError):
            V.validate(initial, PID, 0)

        hidden = meta(1)
        hidden['bridge']['quiet_ms'] = 249
        with self.assertRaises(H.CaptureError):
            V.validate(hidden, PID, 1)
        hidden = meta(1)
        hidden['bridge']['quiet_ms_frozen'] = True
        with self.assertRaises(H.CaptureError):
            V.validate(hidden, PID, 1)
        hidden = meta(1)
        hidden['bridge']['show_command'] = {'browser': list(IDS), 'sequence': 8}
        with self.assertRaises(H.CaptureError):
            V.validate(hidden, PID, 1)
        hidden = meta(1)
        hidden['recovery']['status'] = 'Recovered'
        with self.assertRaises(H.CaptureError):
            V.validate(hidden, PID, 1)
        hidden = meta(1)
        hidden['recovery']['acks'] = 4
        with self.assertRaises(H.CaptureError):
            V.validate(hidden, PID, 1)
        hidden = meta(1)
        hidden['source_generation'] = 2
        with self.assertRaises(H.CaptureError):
            V.validate(hidden, PID, 1)
        hidden = meta(1)
        hidden['bridge']['retained'] = None
        with self.assertRaises(H.CaptureError):
            V.validate(hidden, PID, 1)
        # The cleared hidden image must be empty; the retained source is never
        # claimed to be the last painted frame at this stage.
        hidden = meta(1)
        hidden['diagnostics']['frame_width'] = 480
        hidden['diagnostics']['frame_height'] = 360
        with self.assertRaises(H.CaptureError):
            V.validate(hidden, PID, 1)
        # Historical paint identity may be stale at Hidden; it is not the
        # claim under test, so only the cleared geometry/history gate applies.
        stale_paint = meta(1)
        stale_paint['diagnostics']['painted_generation'] = 3
        stale_paint['diagnostics']['painted_sequence'] = 9
        self.assertEqual(V.validate(stale_paint, PID, 1), SCALE)
        hidden = meta(1)
        hidden['diagnostics']['paint_calls'] = 0
        with self.assertRaises(H.CaptureError):
            V.validate(hidden, PID, 1)
        # Stage 0/2 keep the strict raw == painted relation.
        for stage in (0, 2):
            painted = meta(stage)
            painted['diagnostics']['frame_width'] = 0
            with self.assertRaises(H.CaptureError, msg=stage):
                V.validate(painted, PID, stage)

        released = meta(3)
        released['view_raw'] = raw(3, 4, 8)
        with self.assertRaises(H.CaptureError):
            V.validate(released, PID, 3)
        released = meta(3)
        released['diagnostics']['released_images'] = 2
        with self.assertRaises(H.CaptureError):
            V.validate(released, PID, 3)
        released = meta(3)
        released['diagnostics']['frame_width'] = 480
        with self.assertRaises(H.CaptureError):
            V.validate(released, PID, 3)
        released = meta(3)
        released['bridge']['retained'] = {'browser': list(IDS), 'generation': 2,
                                          'sequence': 5, 'width': 480, 'height': 360}
        with self.assertRaises(H.CaptureError):
            V.validate(released, PID, 3)
        released = meta(3)
        released['bridge'].update(phase='SourceRecovered')
        with self.assertRaises(H.CaptureError):
            V.validate(released, PID, 3)
        released = meta(3)
        released['source_generation'] = 3
        with self.assertRaises(H.CaptureError):
            V.validate(released, PID, 3)
        released = meta(3)
        released['recovery']['queued'] = True
        with self.assertRaises(H.CaptureError):
            V.validate(released, PID, 3)
        # source_visible=false must follow an observed Closed release, never a
        # guessed retirement.
        released = meta(3)
        released['recovery']['status'] = 'Recovered'
        with self.assertRaises(H.CaptureError):
            V.validate(released, PID, 3)
        released = meta(3)
        released['recovery']['status'] = 'Hidden'
        with self.assertRaises(H.CaptureError):
            V.validate(released, PID, 3)

        # Fabricated tmp raw files only: the expected source must be the
        # complete opaque fixed page, never a blank/all-white first frame.
        import tempfile
        from pathlib import Path as _Path
        import sys as _sys
        _sys.path.insert(0, str(_Path(__file__).resolve().parent))
        import poc_paths
        poc_paths.temp_dir().mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=str(poc_paths.temp_dir())) as directory:
            stage = 2
            width, height = V.target(V.VIEW_DIP, SCALE)
            path = Path(directory) / f'visibility-stage-{stage}-view.bgra'
            raw_frame = {'file': path.name, 'browser': list(IDS),
                         'generation': 4, 'sequence': 8, 'width': width,
                         'height': height, 'bytes': width * height * 4}
            opaque = page(width, height)
            path.write_bytes(bytes(opaque))
            read_path, data = V.read_raw(Path(directory), raw_frame, IDS, SCALE, stage)
            self.assertEqual(read_path, path)
            self.assertEqual(len(data), raw_frame['bytes'])
            path.write_bytes(bytes(bytes((255, 255, 255, 255)) * (width * height)))
            with self.assertRaises(H.CaptureError):
                V.read_raw(Path(directory), raw_frame, IDS, SCALE, stage)
            path.write_bytes(bytes(opaque))
            # left-orange, top-purple and center-blue sample offsets.
            for at in (((height // 2) * width) * 4,
                       (width // 2) * 4,
                       ((height // 2) * width + width // 2) * 4):
                wrong_color = bytearray(opaque)
                wrong_color[at:at + 4] = bytes((0, 0, 0, 255))
                path.write_bytes(bytes(wrong_color))
                with self.assertRaises(H.CaptureError):
                    V.read_raw(Path(directory), raw_frame, IDS, SCALE, stage)
            path.write_bytes(bytes(opaque))
            translucent = bytearray(opaque)
            translucent[3] = 254
            path.write_bytes(bytes(translucent))
            with self.assertRaises(H.CaptureError):
                V.read_raw(Path(directory), raw_frame, IDS, SCALE, stage)
            for payload in (bytes(opaque[:-4]), bytes(opaque) + bytes(4)):
                path.write_bytes(payload)
                with self.assertRaises(H.CaptureError):
                    V.read_raw(Path(directory), raw_frame, IDS, SCALE, stage)
            path.write_bytes(bytes(opaque))
            for foreign in (dict(raw_frame, file='visibility-stage-1-view.bgra'),
                            dict(raw_frame, browser=[1, 3]),
                            dict(raw_frame, width=750, height=450,
                                 bytes=750 * 450 * 4)):
                with self.assertRaises(H.CaptureError):
                    V.read_raw(Path(directory), foreign, IDS, SCALE, stage)
            missing = dict(raw_frame, file='visibility-stage-3-view.bgra')
            with self.assertRaises(H.CaptureError):
                V.read_raw(Path(directory), missing, IDS, SCALE, 3)


class VisibilityChain(unittest.TestCase):
    def case(self):
        # chain() never opens a file; a plain Path is enough for stage chaining.
        return Path('.')

    def test_stage_chain_accepts_one_monotonic_original_run(self):
        for stage in (1, 2, 3):
            V.chain(meta(stage), prior_record(stage - 1), stage, self.case())
        # A natural newer complete frame may appear between stage 0 and the
        # actual minimize; the retained pre-hide source may advance.
        newer = meta(1)
        newer['view_raw'] = raw(1, 2, 6)
        newer['diagnostics']['painted_sequence'] = 6
        newer['bridge']['retained']['sequence'] = 6
        V.chain(newer, prior_record(0), 1, self.case())

    def test_stage_chain_rejects_split_runs_and_broken_bridge_history(self):
        for mutate in [
            lambda r: r.update(success=False), lambda r: r.update(success=1),
            lambda r: r.update(pid=11), lambda r: r.update(stage=1),
            lambda r: r.update(pid=True), lambda r: r.update(hwnd=4243),
            lambda r: r.update(owner_thread=78), lambda r: r.update(browser=[1, 3]),
            lambda r: r.update(scale_factor=1.25),
            lambda r: r['metadata']['window'].update(hwnd=4243),
            lambda r: r['metadata']['window'].update(thread=78),
            lambda r: r['metadata']['window'].update(pid=11),
            lambda r: r.update(metadata=None),
            lambda r: r['metadata']['bridge'].update(phase='Hidden'),
            lambda r: r['metadata'].update(source_generation=2)]:
            broken = copy.deepcopy(prior_record(0))
            mutate(broken)
            with self.assertRaises(H.CaptureError):
                V.chain(meta(1), broken, 1, self.case())

        # Stage 1: the retained raw frame must be the actual pre-hide source
        # named by bridge.retained, never an older/mismatched frame.
        stale = meta(1)
        stale['view_raw'] = raw(1, 1, 1)
        stale['diagnostics']['painted_generation'] = 1
        stale['diagnostics']['painted_sequence'] = 1
        with self.assertRaises(H.CaptureError):
            V.chain(stale, prior_record(0), 1, self.case())
        replaced = meta(1)
        replaced['bridge']['retained']['sequence'] = 4
        with self.assertRaises(H.CaptureError):
            V.chain(replaced, prior_record(0), 1, self.case())
        no_new_generation = meta(1)
        no_new_generation['source_generation'] = 2
        no_new_generation['bridge']['hidden_generation'] = 2
        no_new_generation['recovery']['generation'] = 2
        no_new_generation['bridge']['hidden_recovery']['generation'] = 2
        no_new_generation['bridge']['last_hidden_observation']['source_generation'] = 2
        with self.assertRaises(H.CaptureError):
            V.chain(no_new_generation, prior_record(0), 1, self.case())
        missing_hidden = meta(1)
        missing_hidden['bridge']['hidden_recovery'] = None
        with self.assertRaises(H.CaptureError):
            V.chain(missing_hidden, prior_record(0), 1, self.case())
        regressed = meta(1)
        regressed['recovery']['acks'] = 2
        regressed['bridge']['hidden_recovery']['acks'] = 2
        regressed['bridge']['last_hidden_observation']['recovery']['acks'] = 2
        with self.assertRaises(H.CaptureError):
            V.chain(regressed, prior_record(0), 1, self.case())

        # Stage 2 must keep the hidden baseline frozen and advance naturally.
        prior1 = prior_record(1)
        for mutate in [
            lambda d: d['view_raw'].update(generation=3),
            lambda d: d['view_raw'].update(sequence=5),
            lambda d: d['bridge'].update(before_hide_generation=3),
            lambda d: d['bridge'].update(hidden_generation=4),
            lambda d: d['bridge']['hide_command'].update(sequence=8),
            lambda d: d['bridge'].update(retained=None),
            lambda d: d['bridge']['retained'].update(generation=3),
            lambda d: d['bridge']['hidden_recovery'].update(acks=4),
            lambda d: d['bridge'].update(quiet_ms=300),
            lambda d: d['bridge'].update(quiet_ms_frozen=False),
            lambda d: d['bridge']['last_hidden_observation'].update(source_generation=4),
            lambda d: d['bridge']['show_command'].update(sequence=7),
            lambda d: d['bridge'].update(restore_requests=2),
            lambda d: d['recovery'].update(acks=3),
            lambda d: d['recovery'].update(exhaustions=1),
            lambda d: d.update(source_visible=False),
            lambda d: d.update(source_generation=5)]:
            broken = copy.deepcopy(meta(2))
            mutate(broken)
            with self.assertRaises(H.CaptureError):
                V.chain(broken, prior1, 2, self.case())

        # Stage 3 must retire exactly once without touching identity/counters.
        prior2 = prior_record(2)
        for mutate in [
            lambda d: d.update(view_raw=raw(3, 4, 8)),
            lambda d: d['bridge'].update(phase='SourceRecovered'),
            lambda d: d['bridge'].update(
                retained={'browser': list(IDS), 'generation': 2, 'sequence': 5,
                          'width': 480, 'height': 360}),
            lambda d: d['bridge']['hide_command'].update(sequence=8),
            lambda d: d['bridge']['show_command'].update(sequence=10),
            lambda d: d['bridge'].update(before_hide_generation=3),
            lambda d: d['bridge'].update(hidden_generation=4),
            lambda d: d['bridge']['hidden_recovery'].update(acks=4),
            lambda d: d['bridge'].update(quiet_ms_frozen=False),
            lambda d: d['bridge'].update(quiet_ms=311),
            lambda d: d['bridge']['last_hidden_observation'].update(source_generation=4),
            lambda d: d.update(source_visible=True),
            lambda d: d.update(detached=False)]:
            broken = copy.deepcopy(meta(3))
            mutate(broken)
            with self.assertRaises(H.CaptureError):
                V.chain(broken, prior2, 3, self.case())
        with self.assertRaises(H.CaptureError):
            V.chain(meta(3), prior_record(1), 3, self.case())


if __name__ == '__main__':
    unittest.main()
