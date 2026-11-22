#!/usr/bin/env python3
"""Only the changed production resize metadata predicate, no desktop calls."""
import copy, hashlib, importlib.util, tempfile, unittest
from pathlib import Path
from unittest import mock
spec = importlib.util.spec_from_file_location('resize_predicate', Path(__file__).with_name('component-resize-test.py'))
M = importlib.util.module_from_spec(spec)
spec.loader.exec_module(M)


def initial():
    return {'schema':1, 'kind':'window-layout-resize-slice', 'pid':10, 'stage':0,
            'task_11_3_accepted':False, 'browser':[1,2], 'scale_factor':1.5,
            'view_bounds':[0,48,320,240], 'client_dip':[400,360], 'detached':False,
            'diagnostics':{'last_error':None, 'surface_bounds':[0,48,320,240], 'scale_factor':1.5,
                           'frame_width':480, 'frame_height':360, 'created_images':1, 'released_images':0,
                           'paint_calls':2, 'painted_generation':1, 'painted_sequence':3, 'pending_resync':False},
            'view_raw':{'file':'resize-stage-0-view.bgra', 'browser':[1,2], 'surface':'View',
                        'generation':1, 'sequence':3, 'width':480, 'height':360, 'bytes':691200}}


class ResizePredicate(unittest.TestCase):
    def test_active_and_actual_release_are_distinct_and_scale_is_independent(self):
        self.assertEqual(M.validate(initial(), 10, 0), 1.5)
        self.assertEqual(M.target((64,48), 1.1), (71,53))
        end = initial(); end.update(stage=5, detached=True, view_raw=None)
        end['diagnostics'].update(frame_width=0, frame_height=0, released_images=1)
        self.assertEqual(M.validate(end, 10, 5), 1.5)
        end['diagnostics']['released_images']=0
        with self.assertRaises(M.H.CaptureError): M.validate(end, 10, 5)

    def test_native_client_rounding_does_not_change_ceil_view_requirement(self):
        dpi_scale = 106 / 96
        self.assertEqual(M.client_pixels((580, 420), dpi_scale), (640, 464))
        self.assertEqual(M.target((580, 420), dpi_scale), (641, 464))
        self.assertEqual(M.client_pixels((1, 3), 1.5), (2, 5))
        self.assertEqual(M.physical_view_rect(0, dpi_scale), (0, 53, 354, 318))
        self.assertEqual(M.client_pixels((580, 420), 1.5), (870, 630))

    def test_only_manual_stage_zero_can_use_foreground_observation(self):
        meta = dict(initial_foreground=dict(policy='manual-owned-observation',
                    approved=False, attempts=0, before=False, after=True))
        M.verify_initial_foreground(meta, True)
        M.verify_initial_foreground({}, False)
        with self.assertRaises(M.H.CaptureError):
            M.verify_initial_foreground(meta, False)
        with self.assertRaises(M.H.CaptureError):
            M.verify_initial_foreground({}, True)
        for bad in (dict(attempts=True), dict(attempts=1), dict(approved=True), dict(after=False)):
            with self.subTest(bad=bad), self.assertRaises(M.H.CaptureError):
                M.verify_initial_foreground(dict(initial_foreground=dict(meta['initial_foreground'], **bad)), True)

    def test_passive_resize_binds_original_owned_native_client_and_flags(self):
        facts = dict(hwnd=20, pid=10, thread=30, dpi=168, client=[700,630], foreground=False, resize_flags=0x16)
        def verify(value, enabled=True):
            M.verify_passive_window(value, enabled, 20, 10, 30, 168, (700,630))
        verify(dict(passive_window=facts))
        verify({}, False)
        for changes in (dict(hwnd=21), dict(pid=11), dict(thread=31), dict(dpi=144),
                        dict(client=[699,630]), dict(foreground=True), dict(foreground=0), dict(resize_flags=2)):
            with self.subTest(changes=changes), self.assertRaises(M.H.CaptureError):
                verify(dict(passive_window=dict(facts, **changes)))
        with self.assertRaises(M.H.CaptureError): verify({})
        with self.assertRaises(M.H.CaptureError): verify(dict(passive_window=facts), False)

    def test_scope_identity_geometry_and_raw_mismatch_fail_closed(self):
        for key, value in [('schema',True), ('stage',1), ('pid',11), ('task_11_3_accepted',True),
                           ('view_bounds',[0,48,321,240]), ('client_dip',[680,440]), ('scale_factor',True), ('browser',[1,True])]:
            bad=initial();bad[key]=value
            with self.assertRaises(M.H.CaptureError, msg=key):M.validate(bad,10,0)
        for key, value in [('file','../foreign.bgra'), ('browser',[1,3]), ('surface','Popup'),
                           ('width',481), ('width',480.0), ('sequence',4)]:
            bad=copy.deepcopy(initial());bad['view_raw'][key]=value
            with self.assertRaises(M.H.CaptureError, msg=key):M.validate(bad,10,0)
        bad=initial();bad['diagnostics']['last_error']='retained failure'
        with self.assertRaises(M.H.CaptureError):M.validate(bad,10,0)


class ResizePixelEvidence(unittest.TestCase):
    def test_snapshot_digest_binds_parsed_and_bounded_bytes(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / 'receipt.json'
            data = b'\xef\xbb\xbf{"ok":true}'
            path.write_bytes(data)
            value, digest = M.snapshot_json(path)
            self.assertEqual(value, {'ok': True})
            self.assertEqual(digest, hashlib.sha256(data).hexdigest())
            for count in (0, M.MAX_RECEIPT_BYTES + 1):
                path.write_bytes(b' ' * count)
                with self.assertRaisesRegex(M.H.CaptureError, 'expected byte bounds'):
                    M.snapshot_json(path)

    def test_comparison_rejects_changed_pixel_alpha_and_raw_lengths(self):
        with tempfile.TemporaryDirectory() as d:
            case = Path(d)
            rect = (1, 1, 4, 2)
            raw = {'file': 'resize-stage-0-view.bgra', 'width': 3, 'height': 1}
            path = case / raw['file']
            pixels = [(26, 76, 126), (190, 90, 20), (110, 40, 180)]
            data = bytes(c for r,g,b in pixels for c in (b,g,r,255))
            path.write_bytes(data)
            image = M.Image.new('RGB', (5, 3))
            for i, pixel in enumerate(pixels, 1):
                image.putpixel((i, 1), pixel)
            record = M.compare_view(case, 0, image, rect, raw)
            self.assertEqual(record, {'file':raw['file'], 'bytes':len(data), 'sha256':hashlib.sha256(data).hexdigest()})
            # A single mismatched edge pixel fails; there is no sampling or scaling.
            changed = image.copy()
            changed.putpixel((3, 1), (110, 41, 180))
            with self.assertRaisesRegex(M.H.CaptureError, 'whole View differs'):
                M.compare_view(case, 0, changed, rect, raw)
            path.write_bytes(data[:3] + b'\x00' + data[4:])
            with self.assertRaisesRegex(M.H.CaptureError, 'not opaque'):
                M.compare_view(case, 0, image, rect, raw)
            for invalid in (data[:-1], data + b'x'):
                path.write_bytes(invalid)
                with self.assertRaisesRegex(M.H.CaptureError, 'expected byte bounds'):
                    M.compare_view(case, 0, image, rect, raw)

    def test_detached_compare_requires_contained_unscaled_green_without_raw(self):
        green = M.Image.new('RGB', (2, 2), (0, 170, 68))
        self.assertIsNone(M.compare_view(Path('.'), 5, green, (0, 0, 2, 2), None))
        with self.assertRaisesRegex(M.H.CaptureError, 'inside full RGB client'):
            M.compare_view(Path('.'), 5, green, (-1, 0, 2, 2), None)
        with self.assertRaisesRegex(M.H.CaptureError, 'cannot retain a raw frame'):
            M.compare_view(Path('.'), 5, green, (0, 0, 2, 2), {})
        with self.assertRaisesRegex(M.H.CaptureError, 'whole View differs'):
            M.compare_view(Path('.'), 5, M.Image.new('RGB', (2, 2)), (0, 0, 2, 2), None)

    def test_published_png_hash_binds_compared_bytes_without_overwrite(self):
        with tempfile.TemporaryDirectory() as d:
            case = Path(d)
            green = M.Image.new('RGB', (2, 2), (0, 170, 68))
            payload, raw = M.compared_png_bytes(case, 5, green, (0, 0, 2, 2), None)
            self.assertIsNone(raw)
            output = case / 'resize-window-5.png'
            digest = M.publish_png_snapshot(output, payload)
            self.assertEqual(output.read_bytes(), payload)
            self.assertEqual(digest, hashlib.sha256(payload).hexdigest())
            with self.assertRaisesRegex(M.H.CaptureError, 'already exists'):
                M.publish_png_snapshot(output, b'different PNG')
            self.assertEqual(output.read_bytes(), payload)

    def test_compare_png_uses_serialized_pixels_not_the_input_object(self):
        green = M.Image.new('RGB', (2, 2), (0, 170, 68))
        different = M.Image.new('RGB', (2, 2), (0, 169, 68))
        with mock.patch.object(green, 'save', side_effect=different.save):
            with self.assertRaisesRegex(M.H.CaptureError, 'whole View differs'):
                M.compared_png_bytes(Path('.'), 5, green, (0, 0, 2, 2), None)


if __name__=='__main__':unittest.main()
