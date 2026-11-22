#!/usr/bin/env python3
"""Only the changed production exhaustion metadata predicates, no desktop calls."""
import copy, importlib.util, unittest
from pathlib import Path
spec = importlib.util.spec_from_file_location('exhaustion_predicate', Path(__file__).with_name('component-exhaustion-test.py'))
M = importlib.util.module_from_spec(spec)
spec.loader.exec_module(M)
PID, SCALE = 10, 1.5
IDS = [1, 2]
WINDOW = {'hwnd': 4242, 'pid': PID, 'thread': 77, 'dpi': 144, 'left': 40, 'top': 40,
          'width': 900, 'height': 675}


def recovery(status, generation, width, height, attempts=0, requests=4, native=4, acks=3,
             exhaustions=0, offsets=(), native_offsets=(), queued=False, post_failures=0):
    return {'status': status, 'generation': generation, 'expected_width': width,
            'expected_height': height, 'attempts': attempts, 'requests_total': requests,
            'native_invalidate_entries': native, 'native_thread': 100,
            'native_offsets_us': list(native_offsets), 'request_offsets_us': list(offsets),
            'acks': acks, 'exhaustions': exhaustions, 'queued': queued,
            'post_failures': post_failures}


def diagnostics(bounds, painted=(1, 3), frame=(480, 360), created=1, released=0):
    return {'schema': 1, 'paint_calls': 2, 'painted_sequence': painted[1],
            'painted_generation': painted[0], 'frame_width': frame[0], 'frame_height': frame[1],
            'scale_factor': SCALE, 'surface_bounds': [float(x) for x in bounds],
            'created_images': created, 'released_images': released, 'consumed_updates': 1,
            'binding_retired': False, 'binding_revision': 1, 'resync_attempts': 0,
            'rejected_paints': 0, 'pending_resync': False, 'clicks_submitted': 0,
            'clicks_rejected': 0, 'pending_click': False, 'last_error': None,
            'popup': {'generation': 0, 'visible': False, 'bounds': [0, 0, 0, 0],
                      'frame_width': 0, 'frame_height': 0, 'created_images': 0,
                      'released_images': 0, 'paint_calls': 0, 'painted_sequence': 0,
                      'painted_generation': 0, 'painted_bounds': [0, 0, 0, 0]}}


def raw(stage, generation=1, sequence=3, width=480, height=360):
    return {'file': f'exhaustion-stage-{stage}-view.bgra', 'browser': list(IDS),
            'surface': 'View', 'width': width, 'height': height,
            'bytes': width * height * 4, 'generation': generation, 'sequence': sequence}


def meta(stage):
    bounds = [0, 48, 500, 300] if stage == 1 else [0, 48, 320, 240]
    base = recovery('Recovered', 1, 480, 360)
    offsets = [0, 100_000, 200_000, 300_000, 400_000, 500_000, 600_000, 700_000]
    facts = {'schema': 1, 'kind': 'recovery-exhaustion-slice', 'pid': PID, 'stage': stage,
             'scope': 'recovery-exhaustion-slice', 'input_sent': False,
             'foreground_forced': False, 'observer_resized': False,
             'initial_foreground': {'approved': False, 'attempts': 0, 'before': None, 'after': None, 'policy': 'passive'},
             'browser': list(IDS), 'window': copy.deepcopy(WINDOW), 'scale_factor': SCALE,
             'view_bounds': bounds, 'stale': stage == 1,
             'fault_label': 'STALE: resize recovery exhausted' if stage == 1 else None,
             'baseline_recovery': copy.deepcopy(base), 'quiet_observation_ms': 300 if stage == 1 else 0,
             'source_generation': 1, 'detached': stage == 3,
             'task_11_3_accepted': False,
             'probe': {'drop_view': stage == 1, 'applied': stage, 'dropped': 1 if stage == 1 else 0,
                       'incomplete_pending': False, 'incomplete_rejected': 0},
             'fault': {'count': 0 if stage == 0 else 1,
                       'browser': None if stage == 0 else list(IDS),
                       'event_sequence': None if stage == 0 else 5, 'retired': stage == 3}}
    if stage == 0:
        facts.update(diagnostics=diagnostics(bounds), view_raw=raw(0), recovery=copy.deepcopy(base))
    elif stage == 1:
        facts.update(view_raw=raw(1), source_generation=2,
                     diagnostics=diagnostics(bounds),
                     recovery=recovery('Exhausted', 2, 750, 450, attempts=8, requests=12, native=12,
                                       offsets=offsets, native_offsets=offsets, exhaustions=1))
    elif stage == 2:
        facts.update(view_raw=raw(2, generation=3, sequence=9), source_generation=3,
                     diagnostics=diagnostics(bounds, painted=(3, 9)),
                     recovery=recovery('Recovered', 3, 480, 360, requests=12, native=12,
                                       acks=4, exhaustions=1))
    else:
        facts.update(view_raw=None, source_generation=3,
                     diagnostics=diagnostics(bounds, painted=(3, 9), frame=(0, 0), created=5, released=5),
                     recovery=recovery('Idle', 3, 480, 360, requests=12, native=12, acks=4,
                                       exhaustions=1))
    return facts


class ExhaustionPredicate(unittest.TestCase):
    def test_manual_foreground_is_observed_not_requested(self):
        row = meta(0)
        manual = {'approved': False, 'attempts': 0, 'before': False, 'after': True,
                  'policy': 'manual-owned-observation'}
        row['initial_foreground'] = copy.deepcopy(manual)
        self.assertEqual(M.validate(row, PID, 0), SCALE)
        self.assertEqual(M.foreground_policy(row, False, True), manual)
        with self.assertRaises(M.H.CaptureError):
            M.foreground_policy(row, False, False)
        with self.assertRaises(M.H.CaptureError):
            M.foreground_policy(meta(0), False, True)
        for key, value in [('approved', True), ('attempts', 1), ('attempts', False),
                           ('before', None), ('before', 0), ('after', None),
                           ('after', False), ('after', 1), ('policy', 'passive')]:
            with self.subTest(key=key, value=value):
                row['initial_foreground'] = dict(manual, **{key: value})
                with self.assertRaises(M.H.CaptureError):
                    M.validate(row, PID, 0)

    def test_manual_chain_rejects_geometry_and_foreground_drift_before_io(self):
        first, current = meta(0), meta(1)
        policy = dict(approved=False, attempts=0, before=False, after=True,
                      policy='manual-owned-observation')
        first['initial_foreground'] = current['initial_foreground'] = policy
        prior = dict(success=True, browser=IDS, metadata=first, owner_thread=WINDOW['thread'],
                     manual_foreground_pre=WINDOW['hwnd'], manual_foreground_post=WINDOW['hwnd'])
        for key in ('left', 'top', 'width', 'height'):
            moved = copy.deepcopy(current)
            moved['window'][key] += 1
            with self.assertRaisesRegex(M.H.CaptureError, 'geometry changed'):
                M.chain(moved, prior, 1, None)
        for key in ('manual_foreground_pre', 'manual_foreground_post'):
            for value in (None, True, WINDOW['hwnd'] + 1):
                with self.assertRaisesRegex(M.H.CaptureError, 'original foreground HWND'):
                    M.chain(current, dict(prior, **{key: value}), 1, None)

    def test_stage_identity_geometry_and_scale_fail_closed(self):
        for stage in range(4):
            self.assertEqual(M.validate(meta(stage), PID, stage), SCALE)
        idle = meta(0)
        idle['recovery'] = recovery('Idle', 0, 0, 0, requests=0, native=0, acks=0)
        idle['recovery']['native_thread'] = 0
        idle['baseline_recovery'] = copy.deepcopy(idle['recovery'])
        self.assertEqual(M.validate(idle, PID, 0), SCALE)
        idle['recovery']['attempts'] = 1
        idle['baseline_recovery'] = copy.deepcopy(idle['recovery'])
        with self.assertRaises(M.H.CaptureError):
            M.validate(idle, PID, 0)
        for before in (False, True):
            approved = meta(0)
            approved['initial_foreground'] = {'approved': True, 'attempts': int(not before), 'before': before, 'after': True, 'policy': 'one-ordinary-owned-request'}
            self.assertEqual(M.validate(approved, PID, 0), SCALE)
            M.foreground_policy(approved, True)
            with self.assertRaises(M.H.CaptureError):
                M.foreground_policy(approved, False)
        for field, value in [('approved', True), ('attempts', 1), ('attempts', True), ('after', True), ('policy', 'forced')]:
            bad = meta(0)
            bad['initial_foreground'][field] = value
            with self.assertRaises(M.H.CaptureError):
                M.validate(bad, PID, 0)
        with self.assertRaises(M.H.CaptureError):
            M.foreground_policy(meta(0), True)
        self.assertEqual(M.target((320, 240), 1.5), (480, 360))
        self.assertEqual(M.target((16, 12), 1.1), (18, 14))
        for key, value in [('schema', True), ('pid', 11), ('task_11_3_accepted', True),
                           ('scope', 'other'), ('input_sent', True),
                           ('foreground_forced', True), ('observer_resized', True),
                           ('kind', 'window-layout-resize-slice'), ('scale_factor', True),
                           ('scale_factor', 0), ('scale_factor', float('inf')),
                           ('browser', [1, True]), ('browser', [1]), ('view_bounds', [0, 48, 321, 240]),
                           ('view_bounds', [0, 49, 320, 240]), ('stale', False),
                           ('detached', True), ('quiet_observation_ms', 249),
                           ('window', {k: v for k, v in WINDOW.items() if k != 'dpi'}),
                           ('fault_label', None), ('source_generation', 0),
                           ('window', dict(WINDOW, width=901)),
                           ('window', dict(WINDOW, height=674))]:
            bad = meta(1)
            bad[key] = value
            with self.assertRaises(M.H.CaptureError, msg=key):
                M.validate(bad, PID, 1)
        bad = meta(0)
        del bad['probe']
        with self.assertRaises(M.H.CaptureError):
            M.validate(bad, PID, 0)
        bad = meta(0)
        bad['window']['dpi'] = 120
        with self.assertRaises(M.H.CaptureError):
            M.validate(bad, PID, 0)
        bad = meta(0)
        del bad['diagnostics']['popup']['painted_bounds']
        with self.assertRaises(M.H.CaptureError):
            M.validate(bad, PID, 0)
        bad = meta(3)
        bad['diagnostics']['released_images'] = 4
        with self.assertRaises(M.H.CaptureError):
            M.validate(bad, PID, 3)
        bad = meta(3)
        bad['view_raw'] = raw(3)
        with self.assertRaises(M.H.CaptureError):
            M.validate(bad, PID, 3)
        bad = meta(2)
        bad['recovery']['status'] = 'Exhausted'
        with self.assertRaises(M.H.CaptureError):
            M.validate(bad, PID, 2)

    def test_exhaustion_accounting_raw_and_early_recovery_fail_closed(self):
        good = meta(1)
        self.assertEqual(M.validate(good, PID, 1), SCALE)
        for mutate in [
            lambda d: d['recovery'].update(attempts=7),
            lambda d: d['recovery'].update(attempts=9),
            lambda d: d['recovery']['request_offsets_us'].pop(),
            lambda d: d['recovery']['native_offsets_us'].append(800_000),
            lambda d: d['recovery']['native_offsets_us'].__setitem__(1, 50_000),
            lambda d: d['recovery'].update(requests_total=11),
            lambda d: d['recovery'].update(native_invalidate_entries=11),
            lambda d: d['recovery'].update(acks=4),
            lambda d: d['recovery'].update(exhaustions=2),
            lambda d: d['recovery'].update(queued=True),
            lambda d: d['recovery'].update(post_failures=1),
            lambda d: d['recovery'].update(generation=1),
            lambda d: d['recovery'].update(expected_width=480, expected_height=360),
            lambda d: d['probe'].update(drop_view=False),
            lambda d: d['probe'].update(dropped=0),
            lambda d: d['probe'].update(incomplete_pending=True),
            lambda d: d.update(source_generation=1),
            lambda d: d['view_raw'].update(width=750, height=450, bytes=1_350_000),
            lambda d: d['view_raw'].update(file='exhaustion-stage-2-view.bgra'),
            lambda d: d['view_raw'].update(browser=[1, True]),
            lambda d: d['view_raw'].update(browser=[1, 3]),
            lambda d: d['view_raw'].update(generation=2),
            lambda d: d['view_raw'].update(sequence=4),
            lambda d: d.update(fault={'count': 1, 'browser': None, 'event_sequence': 5, 'retired': False}),
            lambda d: d.update(fault={'count': 1, 'browser': [1, True], 'event_sequence': 5, 'retired': False}),
            lambda d: d.update(fault={'count': 1, 'browser': IDS, 'event_sequence': 5, 'retired': True}),
            lambda d: d.update(fault={'count': 1, 'browser': IDS, 'event_sequence': 0, 'retired': False}),
            lambda d: d.update(fault={'count': 0, 'browser': None, 'event_sequence': None, 'retired': False}),
            lambda d: d.update(fault_label=None),
        ]:
            bad = copy.deepcopy(good)
            mutate(bad)
            with self.assertRaises(M.H.CaptureError):
                M.validate(bad, PID, 1)
        early = meta(0)
        early.update(stale=True, fault_label='STALE: resize recovery exhausted',
                     quiet_observation_ms=300, source_generation=2,
                     view_raw=raw(0, generation=2), recovery=meta(1)['recovery'])
        with self.assertRaises(M.H.CaptureError):
            M.validate(early, PID, 0)
        late = meta(2)
        late['recovery']['status'] = 'Pending'
        with self.assertRaises(M.H.CaptureError):
            M.validate(late, PID, 2)
        late = meta(2)
        late['recovery']['expected_width'] = 750
        with self.assertRaises(M.H.CaptureError):
            M.validate(late, PID, 2)
        for key, value in [('acks', 3), ('exhaustions', 0), ('post_failures', 1), ('queued', True),
                           ('generation', 2)]:
            late = meta(2)
            late['recovery'][key] = value
            with self.assertRaises(M.H.CaptureError, msg=key):
                M.validate(late, PID, 2)
        no_ack = meta(2)
        no_ack['recovery']['acks'] = no_ack['baseline_recovery']['acks']
        with self.assertRaises(M.H.CaptureError):
            M.validate(no_ack, PID, 2)
        retired_early = meta(1)
        retired_early['fault']['retired'] = True
        with self.assertRaises(M.H.CaptureError):
            M.validate(retired_early, PID, 1)
        still_live = meta(3)
        still_live['fault']['retired'] = False
        with self.assertRaises(M.H.CaptureError):
            M.validate(still_live, PID, 3)
        unretired_bid = meta(3)
        unretired_bid['fault']['browser'] = None
        with self.assertRaises(M.H.CaptureError):
            M.validate(unretired_bid, PID, 3)
        green = meta(3)
        green['recovery']['status'] = 'Exhausted'
        with self.assertRaises(M.H.CaptureError):
            M.validate(green, PID, 3)


if __name__ == '__main__':
    unittest.main()
