"""Only the frame comparator's alpha/legacy and fail-closed closure checks."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec=importlib.util.spec_from_file_location('component_frames',Path(__file__).with_name('compare-component-frames.py'))
frames=importlib.util.module_from_spec(spec)
spec.loader.exec_module(frames)

class FrameComparatorTests(unittest.TestCase):
    def test_alpha_conversion_and_original_unaligned_fingerprint(self):
        bgra=bytes((100,40,20,128,0,0,0,0,60,60,220,255))
        self.assertEqual(frames.premultiplied_bgra_to_straight_rgba(bgra),bytes((40,80,199,128,0,0,0,0,220,60,60,255)))
        self.assertEqual(frames.legacy_fingerprint(bytes((0,0,255,255,255,0,0,255))),bytes((0,0,255,255,255,255,255,0,0,255,0,0)))

    def test_missing_or_non_numeric_map_evidence_and_event_count_fail_closed(self):
        document={'expected_resize_exhausted_events':1,'native_close':{
            'capture':{'retired':True,'native_resources_live':False,'abandoned':False,'live_imports':0,'staging_bytes':0,'maps':1,'unmaps':1,'failures':0},
            'recovery':{'status':'Closed','queued':False},'cpu_used':0,'cpu_peak':307200,'cpu_limit':268435456}}
        frames.validate_native_close('fixture-only',document,{})
        for values in ({}, {'maps':None,'unmaps':None}, {'maps':True,'unmaps':True}, {'maps':-1,'unmaps':-1}, {'maps':1,'unmaps':0}):
            bad=copy.deepcopy(document)
            bad['native_close']['capture'].pop('maps');bad['native_close']['capture'].pop('unmaps')
            bad['native_close']['capture'].update(values)
            with self.subTest(values=values),self.assertRaises(frames.Failure):
                frames.validate_native_close('fixture-only',bad,{})
        for count in (None,True,0,2):
            bad=copy.deepcopy(document);bad['expected_resize_exhausted_events']=count
            with self.subTest(count=count),self.assertRaises(frames.Failure):
                frames.validate_native_close('fixture-only',bad,{})
        for failures in (None,True,-1,'0'):
            bad=copy.deepcopy(document);bad['native_close']['capture']['failures']=failures
            with self.subTest(failures=failures),self.assertRaises(frames.Failure):
                frames.validate_native_close('fixture-only',bad,{})
        kept=copy.deepcopy(document);kept['native_close']['capture']['failures']=2
        evidence={}
        frames.validate_native_close('fixture-only',kept,evidence)
        self.assertEqual(evidence['native_close']['failures'],2)

    def test_rejection_accounting_is_exact_and_fail_closed(self):
        sha=lambda ch: ch*64
        def document():
            return {
                'browser':'B','cef_ui_thread_id':7,
                'native_close':{'capture':{'failures':1,'last_failure':'IncompleteFrame','last_failure_code':None,'owner_thread':7,'retirement_thread':7}},
                'incomplete_capture_events':[{'browser':'B','event_sequence':8,'code':None}],
                'native_rejection_observations':{'native_rejections_overflow':False,'native_rejections':[{
                    'browser':'B','surface':'View','thread':7,'failure_ordinal':1,
                    'coded':[480,360],'visible':[0,0,480,360],'content':[60,0,360,360],
                    'source_generation_before':7,'source_generation_after':7,
                    'target_generation':7,'target':[20,20],
                    'frame_before':[5,3,480,360],'frame_after':[5,3,480,360],'same_frame':True,
                    'recovery_status_before':'Pending','recovery_status_after':'Pending',
                    'acks_before':2,'acks_after':2,
                    'native_io_before':[3,3,3,3],'native_io_after':[3,3,3,3],
                    'frame_sha256':sha('a')}]},
            }
        def stages():
            return [
                {'generation':5,'sequence':3,'width':480,'height':360,'sha256':sha('a'),'recovery_status':'Recovered'},
                {'generation':7,'sequence':4,'width':20,'height':20,'sha256':sha('b'),'recovery_status':'Recovered',
                 'recovery':{'status':'Recovered','generation':7,'expected_width':20,'expected_height':20,'acks':3}},
            ]
        # Synthetic fixture: validator regression only, never Native evidence.
        self.assertEqual(frames.validate_rejection_accounting('fixture-only',document(),stages())['verified_failures'],1)
        empty=document()
        empty['native_close']['capture'].update({'failures':0,'last_failure':None})
        empty['incomplete_capture_events']=[]
        empty['native_rejection_observations']['native_rejections']=[]
        self.assertEqual(frames.validate_rejection_accounting('fixture-only',empty,stages())['verified_failures'],0)

        def case(mutation):
            bad=document(); mutation(bad); return bad
        record=lambda document:document['native_rejection_observations']['native_rejections'][0]
        bad_cases={
            'missing_record': lambda d:d['native_rejection_observations'].update({'native_rejections':[]}),
            'duplicate_record': lambda d:(d['native_rejection_observations']['native_rejections'].append(copy.deepcopy(record(d))),
                                          d['incomplete_capture_events'].append({'browser':'B','event_sequence':9,'code':None})),
            'duplicate_event_sequence': lambda d:(d['native_rejection_observations']['native_rejections'].append(copy.deepcopy(record(d))),
                                                  d['incomplete_capture_events'].append({'browser':'B','event_sequence':8,'code':None})),
            'descending_event_sequence': lambda d:d['incomplete_capture_events'][0].update({'event_sequence':0}),
            'event_browser_mismatch': lambda d:d['incomplete_capture_events'][0].update({'browser':'other'}),
            'foreign_pair': lambda d:(record(d).update({'browser':'other'}),d['incomplete_capture_events'][0].update({'browser':'other'})),
            'wrong_surface': lambda d:record(d).update({'surface':'Popup'}),
            'wrong_ui': lambda d:record(d).update({'thread':8}),
            'wrong_owner': lambda d:d['native_close']['capture'].update({'owner_thread':8}),
            'wrong_retirement': lambda d:d['native_close']['capture'].update({'retirement_thread':8}),
            'event_code_not_null': lambda d:d['incomplete_capture_events'][0].update({'code':3}),
            'event_sequence_bool': lambda d:d['incomplete_capture_events'][0].update({'event_sequence':True}),
            'ordinal_gap': lambda d:record(d).update({'failure_ordinal':2}),
            'overflow': lambda d:d['native_rejection_observations'].update({'native_rejections_overflow':True}),
            'last_failure_kind': lambda d:d['native_close']['capture'].update({'last_failure':'ResizeExhausted'}),
            'last_failure_code': lambda d:d['native_close']['capture'].update({'last_failure_code':0}),
            'failures_bool': lambda d:d['native_close']['capture'].update({'failures':True}),
            'failures_excess': lambda d:d['native_close']['capture'].update({'failures':33}),
            'acks_changed': lambda d:record(d).update({'acks_after':3}),
            'io_changed': lambda d:record(d).update({'native_io_after':[3,3,4,3]}),
            'io_bool': lambda d:record(d).update({'native_io_before':[True,3,3,3],'native_io_after':[True,3,3,3]}),
            'sha_mismatch': lambda d:record(d).update({'frame_sha256':sha('c')}),
            'sha_truncated': lambda d:record(d).update({'frame_sha256':sha('a')[:63]}),
            'frame_identity_changed': lambda d:record(d).update({'frame_after':[5,3,480,361]}),
            'same_frame_false': lambda d:record(d).update({'same_frame':False}),
            'retained_frame_null': lambda d:record(d).update({'frame_before':None,'frame_after':None}),
            'frame_generation_not_older': lambda d:record(d).update({'frame_before':[7,3,480,360],'frame_after':[7,3,480,360]}),
            'source_generation_drift': lambda d:record(d).update({'source_generation_after':8}),
            'status_not_pending': lambda d:record(d).update({'recovery_status_after':'Recovered'}),
            'coded_zero': lambda d:record(d).update({'coded':[0,360]}),
            'coded_bool': lambda d:record(d).update({'coded':[True,360]}),
            'visible_outside_coded': lambda d:record(d).update({'visible':[-1,0,480,360]}),
            'content_outside_visible': lambda d:record(d).update({'content':[0,0,481,360]}),
            'empty_rect': lambda d:record(d).update({'content':[60,0,0,360]}),
            'content_equals_visible': lambda d:record(d).update({'content':[0,0,480,360]}),
            'target_missing': lambda d:record(d).pop('target'),
            'records_missing': lambda d:d.pop('native_rejection_observations'),
            'events_missing': lambda d:d.pop('incomplete_capture_events'),
        }
        for name,mutation in bad_cases.items():
            with self.subTest(name=name),self.assertRaises(frames.Failure):
                frames.validate_rejection_accounting('fixture-only',case(mutation),stages())
        no_good_stage=stages()[1:]
        with self.assertRaises(frames.Failure):
            frames.validate_rejection_accounting('fixture-only',document(),no_good_stage)
        earlier_identical_pixels = stages()
        earlier_identical_pixels[0]['sequence'] = 2
        with self.assertRaises(frames.Failure):
            frames.validate_rejection_accounting('fixture-only',document(),earlier_identical_pixels)
        no_successor=stages()[:1]
        with self.assertRaises(frames.Failure):
            frames.validate_rejection_accounting('fixture-only',document(),no_successor)
        for name,successor in {
            'successor_wrong_target':dict(stages()[1],width=21),
            'successor_stale_generation':dict(stages()[1],generation=6),
            'successor_newer_generation':dict(stages()[1],generation=8),
            'successor_missing_recovery_details':dict(stages()[1],recovery=None),
            'successor_not_later':dict(stages()[1],sequence=3),
        }.items():
            with self.subTest(name=name),self.assertRaises(frames.Failure):
                frames.validate_rejection_accounting('fixture-only',document(),[stages()[0],successor])
        for field,value in (('generation',8),('expected_width',21),('expected_height',19),('acks',2),('acks',4),('acks',3.0)):
            bad = stages()
            bad[1]['recovery'][field] = value
            with self.subTest(field=field,value=value),self.assertRaises(frames.Failure):
                frames.validate_rejection_accounting('fixture-only',document(),bad)

if __name__=='__main__':
    unittest.main()
