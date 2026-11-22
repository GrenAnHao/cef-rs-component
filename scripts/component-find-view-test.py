"""Independent original-View Find footer/retirement witness; not OCR.
No input/focus/foreground request, movement, foreign title or path inspection.
Reuses only qualified capture primitives, not old SourceViewer scenario logic.
"""
import argparse,ctypes,ctypes.wintypes as wt,importlib.util,json,sys
from pathlib import Path
spec=importlib.util.spec_from_file_location('source_capture_primitives',Path(__file__).with_name('component-source-viewer-test.py'))
source=importlib.util.module_from_spec(spec);spec.loader.exec_module(source)
cap=source.cap;require=source.require

def run(args):
 out=dict(schema=1,kind='find-original-view-owned-window-witness',pid=args.pid,stage=args.stage,success=False,error=None,input_sent=False,foreground_requested=False,atomic_capture=False,ocr_or_exact_glyph_recognition=False,transparent_or_unsampled_overlay_isolation=False)
 try:
  case=args.case_dir;meta=json.loads((case/f'find-view-stage-{args.stage}.json').read_text(encoding='utf-8-sig'));require(meta['pid']==args.pid and meta['stage']==args.stage,'Find View stage identity mismatch')
  user=cap.load_win32();cap.set_thread_dpi_awareness(user);user.GetPropW.argtypes=[wt.HWND,wt.LPCWSTR];user.GetPropW.restype=ctypes.c_void_p;user.GetClassNameW.argtypes=[wt.HWND,wt.LPWSTR,ctypes.c_int];user.GetClassNameW.restype=ctypes.c_int
  windows=sorted(cap.enum_visible_top_level_windows(user,args.pid));original=source.inspect(user,meta['original'],args.pid);require(windows==[original['hwnd']],'Find View expected exactly original owned native window');out['hwnd']=original['hwnd'];out['owned_windows']=windows
  scale=original['dpi_scale'];ui=meta['find_ui'];require(not ui['faulted'] and ui['first_error'] is None,'Find original owner service fault')
  colors=[dict(name=f'body-{x}-{y}',client_px=[round(x*scale),round((48+y)*scale)]) for x in (80,160,240) for y in (80,120,180)]
  header=dict(name='host-header',client_px=[original['client_width']-round(20*scale),round(24*scale)])
  if args.stage==0:
   require(meta['first_result'] and meta['matches']==1 and meta['active_ordinal']==0 and meta['first_footer_renders']>0 and meta['footer']=='Find: 1 match, active 0; next search pending','actual Find result/footer render witness missing')
   require(ui['started']==2 and ui['finished']==1 and ui['active'] and not ui['retired'] and ui['operation']==2 and ui['outcome']=='None','menu terminal incorrectly drained second Find')
   require(not meta['os_input_sent'] and not meta['foreground_requested'],'Find View input contract changed')
   samples=[dict(name=f'footer-{x}-{y}',client_px=[x,y]) for y in range(round(289*scale),round(315*scale),3) for x in range(round(3*scale),original['client_width']-round(12*scale),5)]
  else:
   prior=json.loads((case/'find-view-window-0.json').read_text(encoding='utf-8-sig'));require(prior['success'] and prior['pid']==args.pid and prior['hwnd']==original['hwnd'],'Find original root chain missing')
   require(meta['weak_entity_released'] and meta['lease_retired'] and meta['browser_closed'] and meta['automatic_view_release_cleanup'] and not meta['find_dispatch_used_after_owner_drop'] and meta['host_alive'],'automatic original-View Find cleanup missing')
   require(ui['retired'] and ui['retire_submitted'] and not ui['active'] and ui['started']==ui['finished']==2 and ui['operation']==2 and ui['outcome']=='Some(Cancelled)','Find completion/actual Browser closure accounting mismatch')
   samples=[]
  record={};out['original']=record;image=source.capture(user,meta['original'],args.pid,colors+[header]+samples,case,f'find-view-window-{args.stage}.png',windows,record)
  expected=(26,76,126) if args.stage==0 else (0,170,68)
  require(all(image.getpixel(tuple(p['client_px']))==expected for p in colors),'actual original CEF/retired green surface mismatch')
  require(image.getpixel(tuple(header['client_px']))==(159,34,34),'original host chrome changed')
  if args.stage==0:
   lit=sum(all(v>=140 for v in image.getpixel(tuple(p['client_px']))) for p in samples);out['sampled_footer_light_pixels']=lit;require(lit>=15,'rendered Find footer has insufficient glyph-shaped bright witness')
  out['success']=True
 except Exception as error:out['error']=f'{type(error).__name__}: {error}'
 with (args.case_dir/f'find-view-window-{args.stage}.json').open('x',encoding='utf-8') as f:json.dump(out,f,ensure_ascii=False,indent=2)
 print(json.dumps({k:out[k] for k in ['stage','pid','success','error']},ensure_ascii=False));return 0 if out['success'] else 1
if __name__=='__main__':
 p=argparse.ArgumentParser(allow_abbrev=False);p.add_argument('--pid',type=int,required=True);p.add_argument('--case-dir',type=Path,required=True);p.add_argument('--stage',type=int,choices=(0,1),required=True);a=p.parse_args()
 if a.pid<=0:p.error('positive owned PID required')
 sys.exit(run(a))
