#!/usr/bin/env python3
"""Test OLE focus and receipt predicates without SendInput or native windows."""
from copy import deepcopy
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('ole_driver', Path(__file__).with_name('component-file-ole-driver.py'))
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


class Clock:
    def __init__(self):
        self.now = 0.0

    def monotonic(self):
        return self.now

    def sleep(self, delta):
        self.now += delta


class WindowSamples:
    def __init__(self, hwnd, buttons=lambda: False):
        self.hwnd, self.buttons = hwnd, buttons

    def GetForegroundWindow(self):
        return self.hwnd()

    def GetAsyncKeyState(self, key):
        return 0x8000 if key == 1 and self.buttons() else 0


class ManualSourceFocus(unittest.TestCase):
    def setUp(self):
        self.clock = Clock()
        self.out = {'manual_source_focus_observed': False}

    def check(self, u, verify):
        with patch.object(driver, 'time', self.clock):
            driver.wait_for_manual_source_focus(u, 101, verify, self.out)

    def test_exact_owned_source_waits_for_title_press_to_end(self):
        u = WindowSamples(lambda: 101 if self.clock.now >= 0.2 else 808,
                          lambda: self.clock.now < 0.5)
        qualified = []
        self.check(u, lambda: qualified.append(self.clock.now))
        self.assertEqual(len(qualified), 2)
        self.assertGreaterEqual(qualified[0], 0.5)
        self.assertGreaterEqual(qualified[1]-qualified[0], 0.15)
        self.assertIs(self.out['manual_source_focus_observed'], True)

    def test_foreign_or_unknown_foreground_never_admits_input(self):
        for hwnd in (0, 808):
            self.clock = Clock()
            with self.subTest(hwnd=hwnd), self.assertRaisesRegex(RuntimeError, 'foreground timeout'):
                self.check(WindowSamples(lambda: hwnd), lambda: self.fail('not the original source'))
            self.assertLess(self.clock.now, 15.06)
            self.assertGreaterEqual(self.clock.now, 15.0)
            self.assertEqual(self.out['manual_source_focus_samples']['source_foreground'], 0)
            self.assertIs(self.out['manual_source_focus_observed'], False)

    def test_kickoff_after_old_eight_seconds_can_qualify(self):
        self.check(WindowSamples(lambda: 101 if self.clock.now >= 10 else 0), lambda: None)
        self.assertTrue(self.out['manual_source_focus_observed'])
        self.assertGreaterEqual(self.out['manual_source_focus_wait_ms'], 10150)
        self.assertGreater(self.out['manual_source_focus_samples']['other_or_unknown'], 0)

    def test_slow_first_sample_does_not_count_as_quiet_time(self):
        def read():
            if self.clock.now == 0:
                self.clock.sleep(0.3)
            return 101
        self.check(WindowSamples(read), lambda: None)
        self.assertGreaterEqual(self.out['manual_source_focus_wait_ms'], 450)

    def test_held_button_never_qualifies(self):
        with self.assertRaisesRegex(RuntimeError, 'foreground timeout'):
            self.check(WindowSamples(lambda: 101, lambda: True), lambda: self.fail('button still held'))
        self.assertIs(self.out['manual_source_focus_observed'], False)

    def test_last_identity_check_cannot_authorize_lost_foreground(self):
        hwnd, counts = 101, 0

        def verify():
            nonlocal hwnd, counts
            counts += 1
            if counts == 2:
                hwnd = 808

        with self.assertRaisesRegex(driver.H.CaptureError, 'manual source focus lost'):
            self.check(WindowSamples(lambda: hwnd), verify)
        self.assertEqual(counts, 2)
        self.assertIs(self.out['manual_source_focus_observed'], False)


class ManualGestureReceipt(unittest.TestCase):
    def test_source_required_at_cursor_and_left_down_boundaries(self):
        for hwnd in (0, 102):
            for left_pressed in (False, True):
                with self.assertRaises(driver.H.CaptureError):
                    driver.require_manual_source_before_input(
                        WindowSamples(lambda: hwnd), 101, left_pressed=left_pressed)
        for left_pressed in (False, True):
            driver.require_manual_source_before_input(
                WindowSamples(lambda: 101, lambda: left_pressed), 101, left_pressed=left_pressed)
            with self.assertRaisesRegex(driver.H.CaptureError, 'buttons lost'):
                driver.require_manual_source_before_input(
                    WindowSamples(lambda: 101, lambda: not left_pressed), 101, left_pressed=left_pressed)

    def test_requires_exact_target_and_epoch_after_focus(self):
        plan = {'pid': 80, 'source_nonce': 90, 'target': {'hwnd': 101}, 'input_epoch': 2}
        receipt = dict(pid=80, nonce=90, hwnd=101, input_epoch=2)
        driver.validate_manual_gesture_ready(plan, receipt)
        for update in (dict(pid=81), dict(nonce=0), dict(hwnd=102), dict(input_epoch=3),
                       dict(input_epoch=2.0), dict(unknown=True)):
            with self.assertRaises(driver.H.CaptureError):
                driver.validate_manual_gesture_ready(plan, {**receipt, **update})
        with self.assertRaises(driver.H.CaptureError):
            driver.validate_manual_gesture_ready(plan, [receipt])


class ActiveCloseReceipts(unittest.TestCase):
    def setUp(self):
        self.plan = {'target': {'hwnd': 101}, 'browser': [1, 2], 'input_epoch': 3}
        self.close = {'pid': 80, 'nonce': 90, 'hwnd': 101, 'retirement': {
            'closed': True, 'source_closed': True, 'view_released': True, 'idle_after_close': True,
            'root_repainted': True, 'drop_observed': False, 'ticket': [1, 2, 1],
            'before_epoch': 3, 'after_epoch': 4}}
        self.source = {'pid': 81, 'nonce': 90, 'effect': 0, 'error': None, 'aborted': False,
            'queries': 3, 'physical_source': True, 'marked_left_down': True,
            'terminal_action': 'Cancel', 'terminal_cancel_on_release': True, 'terminal_key_state': 0}

    def test_requires_original_ticket_cleanup_and_no_drop(self):
        driver.validate_active_close_receipt(self.plan, self.close, 80, 90)
        for field, value in [('ticket', [1, 3, 1]), ('ticket', [1, 2, True]), ('after_epoch', 3),
                             ('before_epoch', 2), ('view_released', 1), ('root_repainted', False),
                             ('idle_after_close', False), ('drop_observed', True)]:
            bad = deepcopy(self.close)
            bad['retirement'][field] = value
            with self.subTest(field=field, value=value), self.assertRaises(driver.H.CaptureError):
                driver.validate_active_close_receipt(self.plan, bad, 80, 90)
        for field, value in [('pid', 81), ('nonce', 0), ('hwnd', 102)]:
            with self.subTest(field=field), self.assertRaises(driver.H.CaptureError):
                driver.validate_active_close_receipt(self.plan, {**self.close, field: value}, 80, 90)

    def test_does_not_accept_drop_generic_abort_or_unknown_release(self):
        driver.validate_cancelled_source(self.source, 81, 90)
        for field, value in [('effect', 1), ('effect', False), ('aborted', True), ('queries', 0),
                             ('terminal_action', 'Drop'), ('terminal_cancel_on_release', False),
                             ('terminal_key_state', 1), ('terminal_key_state', None), ('pid', 80),
                             ('nonce', 99), ('marked_left_down', 1)]:
            with self.subTest(field=field, value=value), self.assertRaises(driver.H.CaptureError):
                driver.validate_cancelled_source({**self.source, field: value}, 81, 90)


if __name__ == '__main__':
    unittest.main()
