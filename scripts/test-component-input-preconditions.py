#!/usr/bin/env python3
"""Only the production ASCII-mode predicate; no window, input or IME mutation."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('input_driver', Path(__file__).with_name('component-input-test.py'))
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


class OwnerImePrecondition(unittest.TestCase):
    def test_prior_b_or_modifier_hold_blocks_even_the_next_a(self):
        class Keys:
            def __init__(self, held=(), caps=False):
                self.held, self.caps = held, caps

            def GetAsyncKeyState(self, vk):
                return 0x8000 if vk in self.held else 0

            def GetKeyState(self, vk):
                return int(self.caps) if vk == 0x14 else 0

        driver.require_released_ascii_keys(Keys())
        for keys in (Keys((0x42,)), Keys((0x10,)), Keys((0x5c,)), Keys(caps=True)):
            with self.subTest(held=keys.held, caps=keys.caps), self.assertRaises(driver.H.CaptureError):
                driver.require_released_ascii_keys(keys)

    def test_fresh_owner_reply_requires_exact_post_focus_pre_key_cutpoint(self):
        from copy import deepcopy
        window = {'hwnd': 101, 'pid': 77, 'thread': 9, 'dpi': 144,
                  'left': 60, 'top': 77, 'width': 1020, 'height': 660,
                  'ime': {'keyboard_layout': 0x04090409, 'is_ime': False,
                          'associated': False, 'open': None, 'conversion': None}}
        meta = {'stage': 0, 'pid': 77, 'window': window}
        good = {'schema': 1, 'kind': 'input-owner-ime', 'pid': 77, 'stage': 0,
                'request': '77:0:101', 'window': deepcopy(window), 'raw_count': 2,
                'keyboard_seen': False, 'accepts_text': True}
        driver.validate_owner_ime_response(meta, good)
        for key, value in [('pid', 78), ('stage', 3), ('request', '77:0:102'),
                           ('raw_count', 1), ('keyboard_seen', True), ('accepts_text', False)]:
            with self.subTest(key=key), self.assertRaises(driver.H.CaptureError):
                driver.validate_owner_ime_response(meta, {**good, key: value})
        for key, value in [('hwnd', 102), ('thread', 10), ('left', 61)]:
            bad = deepcopy(good)
            bad['window'][key] = value
            with self.subTest(window_key=key), self.assertRaises(driver.H.CaptureError):
                driver.validate_owner_ime_response(meta, bad)
        bad = deepcopy(good)
        bad['window']['ime']['is_ime'] = True  # still unknown, never accepted as ASCII
        with self.assertRaises(driver.H.CaptureError):
            driver.validate_owner_ime_response(meta, bad)

    def test_peer_reply_requires_original_foreground_pair_before_any_input(self):
        from copy import deepcopy
        source = {'hwnd': 101, 'pid': 77, 'thread': 9, 'dpi': 144,
                  'left': 60, 'top': 77, 'width': 1020, 'height': 660}
        target = {**source, 'hwnd': 102, 'left': 1140, 'width': 780, 'height': 600}
        meta = {'pid': 77, 'stage': 2, 'window': source, 'peer': target}
        good = {'schema': 1, 'kind': 'input-owner-foreground', 'pid': 77, 'stage': 2,
                'request': '77:2:101:102', 'source': source, 'target': target,
                'source_was_foreground': True, 'target_is_foreground': True, 'raw_count': 0}
        driver.validate_owner_peer_response(meta, good)
        for key, value in [('pid', 78), ('stage', 3), ('request', '77:2:102:101'),
                           ('source_was_foreground', False), ('target_is_foreground', False), ('raw_count', 1)]:
            with self.subTest(key=key), self.assertRaises(driver.H.CaptureError):
                driver.validate_owner_peer_response(meta, {**good, key: value})
        for which in ('source', 'target'):
            bad = deepcopy(good)
            bad[which]['hwnd'] += 1
            with self.subTest(which=which), self.assertRaises(driver.H.CaptureError):
                driver.validate_owner_peer_response(meta, bad)

    def test_only_known_owner_ascii_mode_is_admitted(self):
        known = {'keyboard_layout': 0x04090409, 'is_ime': False,
                 'associated': False, 'open': None, 'conversion': None}
        driver.require_ascii_owner_ime_snapshot(known)
        driver.require_ascii_owner_ime_snapshot({**known, 'is_ime': True,
                                               'associated': True, 'open': True, 'conversion': 0})
        for state in (None, {**known, 'is_ime': True},
                      {**known, 'associated': True, 'open': True, 'conversion': 1},
                      {**known, 'associated': True, 'open': False, 'conversion': None}):
            with self.subTest(state=state), self.assertRaises(driver.H.CaptureError):
                driver.require_ascii_owner_ime_snapshot(state)


if __name__ == '__main__':
    unittest.main()
