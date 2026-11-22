#!/usr/bin/env python3
"""Owned-window loss of foreground + finite animation, not minimize/full11.3.

No input, activation, cursor or display changes here. Reuses the unchanged
fixed-320 real desktop capture helper, never its input routines or IME queries.
"""
from __future__ import annotations
import argparse, importlib.util, json, re, sys, traceback
from pathlib import Path
spec=importlib.util.spec_from_file_location('unfocused_capture',Path(__file__).with_name('component-input-test.py'))
I=importlib.util.module_from_spec(spec);spec.loader.exec_module(I)
H=I.H
PEER='GPUI-CEF animation peer'


def require(v,message):
    if not v:raise H.CaptureError(message)


def validate_samples(items):
    require(type(items) is list and len(items)==3,'three bounded animation samples required')
    previous=None
    for s in items:
        require(type(s) is dict and s.get('active') is False and s.get('visible') is True and s.get('source_visible') is True,'inactive != hidden qualification failed')
        for key in ('sequence','generation','width','height','painted_sequence','painted_generation','paint_calls','observed_document_tick'):
            I.integer(s.get(key),key,1)
        I.integer(s.get('elapsed_ms'),'elapsed_ms',0)
        require(s['observed_document_tick']<96 and s['painted_sequence']==s['sequence'] and s['painted_generation']==s['generation'],'sample is not an in-flight actually painted fixture frame')
        require(type(s.get('digest')) is str and re.fullmatch('[0-9a-f]{16}',s['digest']) is not None,'invalid frame digest')
        require(s.get('file')==f"unfocused-sample-{s['sequence']}.bgra",'not fixed case-local sample file')
        if previous:
            require(s['elapsed_ms']-previous['elapsed_ms']>=500 and s['sequence']>previous['sequence']
                    and s['observed_document_tick']>previous['observed_document_tick'] and s['paint_calls']>previous['paint_calls']
                    and s['generation']==previous['generation'] and s['width']==previous['width'] and s['height']==previous['height']
                    and s['digest']!=previous['digest'],'no distinct painted-frame progression over elapsed observation')
        previous=s
    return items


def validate(meta,pid,stage):
    require(type(meta) is dict and all(type(meta.get(k)) is int for k in ('schema','pid','stage')) and meta['schema']==1 and meta['pid']==pid and meta['stage']==stage
            and meta.get('kind')=='unfocused-animation-slice' and meta.get('task_11_3_accepted') is False and meta.get('keyboard_or_pointer_sent') is False,'scene identity/scope mismatch')
    require(meta.get('active') is (stage==0) and meta.get('visible') is True and meta.get('detached') is (stage==2),'wrong active/visible/detached facts')
    require(meta.get('peer_title')==PEER and meta.get('view_bounds')==[0,48,320,240],'unexpected fixture')
    scale=I.number(meta.get('scale_factor'),'scale');require(scale>0,'nonpositive scale')
    ids=meta.get('browser');require(type(ids) is list and len(ids)==2 and all(type(x) is int and x>0 for x in ids),'invalid full BrowserId')
    hwnds=meta.get('hwnds');require(type(hwnds) is list and len(hwnds)==2 and len(set(hwnds))==2 and all(type(x) is int and x>0 for x in hwnds),'not fixed pair of roots')
    for key,hwnd in zip(('window','peer'),hwnds):
        fact=meta.get(key);require(type(fact) is dict and fact.get('hwnd')==hwnd and fact.get('pid')==pid,'wrong window identity')
        I.integer(fact.get('thread'),'owner thread',1)
    require(meta['window']['thread']==meta['peer']['thread'],'different UI owner thread')
    d=meta.get('diagnostics');require(type(d) is dict and d.get('last_error') is None and d.get('surface_bounds')==meta['view_bounds'] and I.f32(d.get('scale_factor'))==I.f32(scale),'paint geometry/error mismatch')
    if stage==2:
        require(meta.get('view_raw') is None and d.get('frame_width')==d.get('frame_height')==0 and I.integer(d.get('created_images'),'images',1)==d.get('released_images'),'released View not empty/balanced')
    else:
        raw=meta.get('view_raw');w,h=I.math.ceil(I.f32(I.f32(320)*I.f32(scale))),I.math.ceil(I.f32(I.f32(240)*I.f32(scale)))
        require(type(raw) is dict and raw.get('file')==f'input-stage-{stage}-view.bgra' and raw.get('browser')==ids and raw.get('surface')=='View','raw frame identity/local name mismatch')
        for key in ('width','height','bytes','generation','sequence'):I.integer(raw.get(key),key,1)
        require(raw['width']==d.get('frame_width')==w and raw['height']==d.get('frame_height')==h and raw['bytes']==w*h*4
                and raw['generation']==d.get('painted_generation') and raw['sequence']==d.get('painted_sequence') and I.integer(d.get('paint_calls'),'paint',1)>0,'raw frame not matching actual paint')
    if stage:
        samples=validate_samples(meta.get('samples'))
        if stage==1:
            last=samples[-1]
            require(raw['generation']==last['generation'] and raw['sequence']>last['sequence'] and d['paint_calls']>last['paint_calls'],'no later settled frame after measured animation')
    else:require(meta.get('samples')==[],'initial static capture already reports animation')
    return scale


def fnv(data):
    value=14695981039346656037
    for b in data:value=((value^b)*1099511628211)&0xffffffffffffffff
    return f'{value:016x}'


def run(args):
    case=args.case_dir.resolve();stage=args.stage
    record={'schema':1,'kind':'unfocused-owned-window','scope':'unfocused-animation-slice','pid':args.pid,'stage':stage,'success':False,'error':None,'task_11_3_accepted':False,'keyboard_or_pointer_sent':False,'foreground_forced':False}
    try:
        require(sys.platform=='win32','real Windows desktop required')
        path=case/f'unfocused-stage-{stage}.json';meta=I.load(path);scale=validate(meta,args.pid,stage)
        u=H.load_win32();record['thread_dpi_awareness']=H.set_thread_dpi_awareness(u)
        a,b=meta['hwnds'];require(set(H.enum_visible_top_level_windows(u,args.pid))=={a,b},'not exact two owned visible roots')
        thread=meta['window']['thread'];owner=I.verify_owner(u,meta['window'],I.TITLE,args.pid,thread);peer=I.verify_owner(u,meta['peer'],PEER,args.pid,thread)
        require(not owner['iconic'] and not peer['iconic'] and owner['visible'] and peer['visible'],'unexpected actual hidden/minimized window')
        require(H.pointer_value(u.GetForegroundWindow())==(a if stage==0 else b),'expected owned foreground not observed')
        require(I.f32(owner['dpi']/96)==I.f32(scale),'actual HWND DPI != GPUI scale')
        if stage:
            prior=I.load(case/f'unfocused-window-{stage-1}.json')
            require(prior.get('success') is True and prior.get('pid')==args.pid and prior.get('hwnds')==[a,b] and prior.get('browser')==meta['browser'] and prior.get('owner_thread')==thread,'broken original-window/Browser chain')
            if stage==2:require(prior['metadata']['samples']==meta['samples'],'sample history changed after View release')
        record.update(hwnd=a,hwnds=[a,b],owner_thread=thread,browser=meta['browser'],metadata=meta,metadata_sha256=I.sha(path),foreground_hwnd=a if stage==0 else b)
        artifacts=[]
        if stage:
            for sample in meta['samples']:
                rawpath=case/sample['file'];data=rawpath.read_bytes()
                require((sample['width'],sample['height'])==(I.math.ceil(I.f32(I.f32(320)*I.f32(scale))),I.math.ceil(I.f32(I.f32(240)*I.f32(scale)))) and len(data)==sample['width']*sample['height']*4 and all(x==255 for x in data[3::4]),'not complete opaque sampled source frame')
                require(fnv(data)==sample['digest'],'saved source sample digest differs from original observation')
                artifacts.append({'file':sample['file'],'sha256':I.sha(rawpath),'bytes':len(data),'digest':sample['digest']})
        record['sample_artifacts']=artifacts
        # Existing capture helper is fixed320 and read-only. Its green phase is
        # numbered5; record this mapping, without invoking any Input driver action.
        capture_stage=5 if stage==2 else stage
        record['capture_stage']=capture_stage;record['capture']=I.capture(u,case,capture_stage,meta,owner,a,record)
        require(H.pointer_value(u.GetForegroundWindow())==(a if stage==0 else b),'foreground changed during passive capture')
        I.verify_owner(u,meta['window'],I.TITLE,args.pid,thread);I.verify_owner(u,meta['peer'],PEER,args.pid,thread)
        require(set(H.enum_visible_top_level_windows(u,args.pid))=={a,b},'owned window pair changed')
        record['success']=True
    except Exception as exc:record.update(error=f'{type(exc).__name__}: {exc}',traceback=traceback.format_exc())
    record['limits']='Current physical DPI, one mode/backend only. Three saved actually painted source observations over >=1s while original window inactive/visible, plus final stopped-fixture desktop image. Not per-frame atomic screen proof, not native frame/DOM transaction identity, all-overlay isolation, minimize/restore, resize/drag/exhaustion or full11.3. No input, IME query or foreground changes by this observer.'
    H.write_json_atomic(case/f'unfocused-window-{stage}.json',record)
    print(f'Unfocused stage {stage}: {"PASSED" if record["success"] else "FAILED"}: {record["error"]}',flush=True)
    return 0 if record['success'] else 1


def main():
    p=argparse.ArgumentParser(allow_abbrev=False);p.add_argument('--pid',required=True,type=int);p.add_argument('--case-dir',required=True,type=Path);p.add_argument('--stage',required=True,type=int,choices=range(3));return run(p.parse_args())
if __name__=='__main__':sys.exit(main())
