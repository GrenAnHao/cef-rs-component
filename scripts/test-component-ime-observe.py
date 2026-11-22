#!/usr/bin/env python3
"""Only production owner-response predicates, never native/Chinese evidence."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec=importlib.util.spec_from_file_location('ime_observe',Path(__file__).with_name('component-ime-observe.py'))
M=importlib.util.module_from_spec(spec)
spec.loader.exec_module(M)


class ObservationPredicates(unittest.TestCase):
    def test_native_mode_requires_complete_owner_facts_not_null_or_ascii_assumptions(self):
        ime={'keyboard_layout':0x08040804,'is_ime':True,'associated':True,'open':True,'conversion':1}
        self.assertEqual(M.classify_chinese_prerequisite(ime),{'known':True,'eligible':True,'reason':'native-conversion'})
        for changes,known in [({'associated':False},False),({'open':None},False),({'conversion':None},False),({'is_ime':False},False),({'keyboard_layout':True},False),({'open':False},True),({'conversion':0},True),({'keyboard_layout':0x04090409},True)]:
            actual=M.classify_chinese_prerequisite(dict(ime,**changes))
            self.assertFalse(actual['eligible'],changes)
            self.assertEqual(actual['known'],known,changes)
        self.assertFalse(M.classify_chinese_prerequisite(None)['known'])

    def test_owner_response_binds_original_identity_exact_post_focus_cutpoint(self):
        window={'hwnd':400,'pid':50,'thread':60,'dpi':144,'left':100,'top':120,'width':780,'height':600,
                'ime':{'keyboard_layout':0x08040804,'is_ime':True,'associated':True,'open':True,'conversion':1}}
        meta={'stage':1,'pid':50,'window':window}
        callbacks={key:0 for key in ('key_down_entered','key_up_entered','replace_text_entered','replace_and_mark_text_entered',
                                    'key_down_rejected','key_up_rejected','replace_text_rejected','replace_and_mark_text_rejected')}
        callbacks['last_rejection']=None
        fresh={'schema':1,'kind':'ime-environment-owner','stage':1,'pid':50,'request':'50:1:400','window':copy.deepcopy(window),
               'raw_count':2,'keyboard_seen':False,'accepts_text':True,'foreground':True,'keyboard_input_sent':False,'mode_changed':False,
               'native_ime':{'system_caret_owned':False,'candidate_adoption_verified':False,'queued':0,'applied':0,'composition_calls':0,'candidate_calls':0},
               'component_callbacks':callbacks,'dom_title':'Input|n=0|h=2166136261|f=1|b=0|d=1|u=1|kd=0|ku=0|i=0|m=0|w=0|cs=0|ce=0'}
        self.assertTrue(M.validate_owner_response(meta,fresh)['eligible'])
        moved=copy.deepcopy(fresh);moved['dom_title']=moved['dom_title'].replace('|m=0','|m=2')
        self.assertTrue(M.validate_owner_response(meta,moved)['eligible'])
        missing=copy.deepcopy(fresh);missing['dom_title']=missing['dom_title'].replace('|m=0','')
        with self.assertRaises(M.H.CaptureError):M.validate_owner_response(meta,missing)
        for key,value in [('schema',True),('pid',51),('stage',True),('stage',0),('request','50:1:401'),('raw_count',1),('keyboard_seen',True),('accepts_text',False),('foreground',False),('keyboard_input_sent',True),('mode_changed',True)]:
            bad=copy.deepcopy(fresh);bad[key]=value
            with self.assertRaises(M.H.CaptureError,msg=key):M.validate_owner_response(meta,bad)
        for field in ('hwnd','thread','dpi','left'):
            bad=copy.deepcopy(fresh);bad['window'][field]+=1
            with self.assertRaises(M.H.CaptureError,msg=field):M.validate_owner_response(meta,bad)
        bad=copy.deepcopy(fresh);bad['native_ime']['system_caret_owned']=True
        with self.assertRaises(M.H.CaptureError):M.validate_owner_response(meta,bad)
        bad=copy.deepcopy(fresh);bad['native_ime']['composition_calls']=1
        with self.assertRaises(M.H.CaptureError):M.validate_owner_response(meta,bad)
        bad=copy.deepcopy(fresh);bad['component_callbacks']['replace_and_mark_text_entered']=1
        with self.assertRaises(M.H.CaptureError):M.validate_owner_response(meta,bad)
        bad=copy.deepcopy(fresh);bad['dom_title']=bad['dom_title'].replace('|cs=0','|cs=1')
        with self.assertRaises(M.H.CaptureError):M.validate_owner_response(meta,bad)


if __name__=='__main__':unittest.main()
