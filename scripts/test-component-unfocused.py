#!/usr/bin/env python3
"""Only new production observation predicates, no desktop/Native execution."""
import copy,importlib.util,unittest
from pathlib import Path
spec=importlib.util.spec_from_file_location('unfocused_predicate',Path(__file__).with_name('component-unfocused-test.py'))
M=importlib.util.module_from_spec(spec);spec.loader.exec_module(M)


def samples():
    return [{'elapsed_ms':100+500*i,'sequence':3+i,'generation':1,'digest':f'{i+1:016x}',
             'file':f'unfocused-sample-{3+i}.bgra','width':480,'height':360,'painted_sequence':3+i,'painted_generation':1,'paint_calls':3+i,'observed_document_tick':1+11*i,
             'active':False,'visible':True,'source_visible':True} for i in range(3)]


def meta(stage=0):
    return {'schema':1,'kind':'unfocused-animation-slice','pid':10,'stage':stage,'task_11_3_accepted':False,'keyboard_or_pointer_sent':False,
            'active':stage==0,'visible':True,'detached':stage==2,'peer_title':M.PEER,'view_bounds':[0,48,320,240],'scale_factor':1.5,
            'browser':[1,2],'hwnds':[100,200],'window':{'hwnd':100,'pid':10,'thread':30},'peer':{'hwnd':200,'pid':10,'thread':30},
            'samples':[] if stage==0 else samples(),
            'diagnostics':{'last_error':None,'surface_bounds':[0,48,320,240],'scale_factor':1.5,'frame_width':480 if stage<2 else 0,'frame_height':360 if stage<2 else 0,'paint_calls':10,'created_images':5,'released_images':5 if stage==2 else 4,'painted_generation':1,'painted_sequence':10},
            'view_raw':None if stage==2 else {'file':f'input-stage-{stage}-view.bgra','browser':[1,2],'surface':'View','width':480,'height':360,'bytes':691200,'generation':1,'sequence':10}}


class UnfocusedPredicates(unittest.TestCase):
    def test_elapsed_saved_frame_progress_never_accepts_focus_or_hidden_or_duplicates(self):
        self.assertEqual(M.validate_samples(samples()),samples())
        for key,value in [('active',True),('visible',False),('source_visible',False),('elapsed_ms',1099),('sequence',4),('digest','0000000000000002'),('observed_document_tick',96),('file','../foreign.bgra'),('painted_sequence',10)]:
            bad=copy.deepcopy(samples());bad[2][key]=value
            with self.assertRaises(M.H.CaptureError,msg=key):M.validate_samples(bad)
        self.assertEqual(M.fnv(b''),'cbf29ce484222325')

    def test_original_window_scope_actual_paint_and_release_stages_are_separate(self):
        for stage in range(3):self.assertEqual(M.validate(meta(stage),10,stage),1.5)
        for key,value in [('schema',True),('task_11_3_accepted',True),('active',True),('visible',False),('hwnds',[100,100]),('browser',[1,True])]:
            bad=meta(1);bad[key]=value
            with self.assertRaises(M.H.CaptureError,msg=key):M.validate(bad,10,1)
        bad=meta(1);bad['view_raw']['sequence']=5;bad['diagnostics']['painted_sequence']=5
        with self.assertRaises(M.H.CaptureError):M.validate(bad,10,1)
        bad=meta(2);bad['diagnostics']['released_images']=4
        with self.assertRaises(M.H.CaptureError):M.validate(bad,10,2)


if __name__=='__main__':unittest.main()
