"""Opt-in real ESC/outside dismissal slice; no SelectAll replay or direct CEF reply.
Only owned foreground/points and one marked Escape DOWN/UP. Derived WM_CHAR is
observed by the host, never synthesized. Finite checks are not OS authentication.
"""
import argparse,ctypes,ctypes.wintypes as wt,hashlib,importlib.util,json,math,time
from pathlib import Path
spec=importlib.util.spec_from_file_location('menu_owned_guards',Path(__file__).with_name('component-menu-default-test.py'))
M=importlib.util.module_from_spec(spec);spec.loader.exec_module(M)
I,H,require=M.I,M.H,M.require

def plan(stage):
 require(type(stage) is int and 0<=stage<=4,'invalid dismissal stage')
 return ('right','escape','right','outside','none')[stage]

def capture(user,kernel,meta,pid,case,stage,out):
 fact=meta['original'];before=M.inspect(user,fact,pid,kernel,meta['process_identity']);hwnd=fact['hwnd'];scale=meta['scale']
 require(math.isfinite(scale) and scale>0 and abs(scale-before['dpi']/96)<.0001,'original scale/DPI mismatch')
 points=[dict(name=f'grid-{x}-{y}',client_px=[int(before['client_width']*x/6),int(before['client_height']*y/6)]) for x in (1,3,5) for y in (1,3,5)]
 menu_visible=stage in (1,3)
 if menu_visible:
  q=meta['visual'];layout=q['layout'];raw=meta['original_capture']
  require(q['first_error'] is None and q['open_sequence']==layout['sequence']==raw['sequence'] and raw['frame_id_bytes']>0 and raw['selection_bytes']==0,'original native menu scope missing')
  rect=layout['menu'];require(isinstance(rect,list) and len(rect)==4 and all(type(v) in (int,float) and math.isfinite(v) for v in rect),'actual menu geometry missing')
  left,top,width,height=rect
  require(width>0 and height>0 and left>=0 and top>=0 and (left+width)*scale<=before['client_width'] and (top+height)*scale<=before['client_height'],'original menu outside client')
  require(abs(left-raw['anchor'][0])<=1 and abs(top-(48+raw['anchor'][1]))<=1,'menu VIEW DIP anchor mismatch')
  points += [dict(name=f'menu-{x}-{y}',client_px=[x,y]) for y in range(I.px(top+3,scale),I.px(top+height-3,scale),4) for x in range(I.px(left+3,scale),I.px(left+width-3,scale),4)]
  if stage==3:
   x,y=meta['select_point'];require(not(left*scale<=x<(left+width)*scale and top*scale<=y<(top+height)*scale),'outside click lies in current menu')
 else:
  points += [dict(name=f'body-{x}-{y}',client_px=[I.px(x,scale),I.px(48+y,scale)]) for x in (80,160,240) for y in (130,180)]
 pre=H.run_probes(user,hwnd,before,points);require(all(p['owned'] for p in pre),'foreign/unknown cover before capture')
 virtual=H.virtual_screen(user);box=(before['client_left'],before['client_top'],before['client_right'],before['client_bottom'])
 require(box[0]>=virtual['left'] and box[1]>=virtual['top'] and box[2]<=virtual['left']+virtual['width'] and box[3]<=virtual['top']+virtual['height'],'owned client outside desktop')
 image=H.ImageGrab.grab(bbox=box,all_screens=True).convert('RGB')
 after=M.inspect(user,fact,pid,kernel,meta['process_identity']);post=H.run_probes(user,hwnd,after,points)
 require(before==after and all(p['owned'] for p in post),'original client changed/covered during capture')
 require(sorted(H.enum_visible_top_level_windows(user,pid))==[hwnd],'unexpected extra owned native window')
 path=case/f'menu-dismiss-window-{stage}.png'
 with path.open('xb') as f:image.save(f,format='PNG')
 out['capture']=dict(before=before,after=after,probes_before=pre,probes_after=post,path=path.name,sha256=hashlib.sha256(path.read_bytes()).hexdigest(),rgb_sha256=hashlib.sha256(image.tobytes()).hexdigest(),size=list(image.size),bbox=list(box))
 if menu_visible:
  values=[image.getpixel(tuple(p['client_px'])) for p in points if p['name'].startswith('menu-')];luma=[sum(v)/3 for v in values]
  require(luma,'empty actual menu samples');out['menu_contrast_samples']=sum(max(luma)-v>=80 for v in luma);require(out['menu_contrast_samples']>=12,'actual menu glyph/contrast witness missing; not OCR')
 else:
  color=(0,170,68) if stage==4 else (26,76,126)
  require(all(image.getpixel(tuple(p['client_px']))==color for p in points if p['name'].startswith('body-')),'original CEF/green body mismatch')
 if stage==2:require(meta['title']=='menu-default-ready' and meta['native']['cancelled']==meta['native']['dismissed']==1 and meta['native']['actions']==0,'ESC cancellation result missing')
 if stage==4:require(meta['weak_released'] and meta['browser_closed'] and meta['host_alive'] and meta['visual']['retired'] and meta['native']['cancelled']==meta['native']['dismissed']==2 and meta['native']['actions']==0,'outside dismissal/retirement missing')
 return before

def run(args):
 out=dict(kind='real-menu-escape-outside-slice',schema=1,pid=args.pid,stage=args.stage,success=False,error=None,input_sent=False,foreground_requested=False,possibly_held=None,process_identity_verified=False,session_checks=[],desktop_checks=[],events=[],cursor_positions=0,menu_contrast_samples=0,physical_release_proof=False,async_zero_authoritative=False,atomic_capture=False,ocr=False,escape_prefix_partial_reads=0)
 try:
  require(args.interactive_input_approved and args.dismiss_input_approved,'fresh explicit dismissal mouse/Escape approval required')
  case=args.case_dir;meta=I.load(case/f'menu-default-stage-{args.stage}.json');out['metadata']=meta
  require(meta['pid']==args.pid and meta['stage']==args.stage and meta['dismiss_probe'] is True,'original dismissal stage identity mismatch')
  fg=meta['initial_foreground'];require(fg['approved'] is True and fg['attempts'] in (0,1) and fg['after'] is True and fg['policy']=='one-ordinary-owned-request','fresh bounded owned foreground not ready')
  user=H.load_win32();H.set_thread_dpi_awareness(user);I.win32(user);kernel=M.process_api()
  user.GetPropW.argtypes=[wt.HWND,wt.LPCWSTR];user.GetPropW.restype=ctypes.c_void_p
  user.GetThreadDesktop.argtypes=[wt.DWORD];user.GetThreadDesktop.restype=wt.HANDLE
  user.GetUserObjectInformationW.argtypes=[wt.HANDLE,ctypes.c_int,ctypes.c_void_p,wt.DWORD,ctypes.POINTER(wt.DWORD)];user.GetUserObjectInformationW.restype=wt.BOOL
  fact=meta['original'];hwnd=fact['hwnd'];out['hwnd']=hwnd
  out['session_checks'].append(M.verified_session(kernel,args.pid,meta['process_identity']));out['process_identity_verified']=True
  capture(user,kernel,meta,args.pid,case,args.stage,out)
  action=plan(args.stage);scale=meta['scale'];x,y=meta['select_point'] if action=='outside' else [I.px(24,scale),I.px(68,scale)]
  def qualify():
   out['session_checks'].append(M.verified_session(kernel,args.pid,meta['process_identity']));out['desktop_checks'].append(M.input_desktop(user,kernel))
   state=M.inspect(user,fact,args.pid,kernel,meta['process_identity']);require(H.pointer_value(user.GetForegroundWindow())==hwnd,'owned foreground lost; no request/bypass')
   I.assert_point(user,hwnd,state,x,y);return state
  def await_state(vk,pressed,row):
   until=time.monotonic()+2
   while time.monotonic()<until:
    row['pressed_after']=bool(user.GetAsyncKeyState(vk)&0x8000)
    if row['pressed_after']==pressed:break
    time.sleep(.01)
   require(row['pressed_after']==pressed,'accepted dismissal input did not reach expected async state')
   qualify() # Zero alone is not proof; requalify readable owned input scope.
  if action!='none':
   state=qualify();held=[v for v in (1,2,4,5,6,0x10,0x11,0x12,0x1b,0x5b,0x5c) if user.GetAsyncKeyState(v)&0x8000]
   require(not held,'button/modifier/Escape already held; no input')
   if action=='escape':
    for up in (False,True):
     if up:
      prefix=case/'menu-dismiss-key-prefix.json';until=time.monotonic()+2;ack=None
      while time.monotonic()<until:
       if prefix.is_file():
        try:ack=I.load(prefix);break
        except json.JSONDecodeError:out['escape_prefix_partial_reads']+=1
       time.sleep(.01)
      require(ack is not None and ack['pid']==args.pid and ack['stage']==1 and ack['window']['hwnd']==hwnd,'original Escape DOWN/derived CHAR acknowledgement missing')
      raw=ack['trace']['raw'];require(len(raw)==2 and [v['message'] for v in raw]==[256,258] and all(v['accepted'] and v['observation_point']=='removed-queue' and not v['during_manual_pump'] for v in raw),'original Escape prefix not observed')
      out['escape_prefix']=ack
     state=qualify();require(bool(user.GetAsyncKeyState(0x1b)&0x8000)==up,'Escape state changed; no key input/release')
     if not up:out['possibly_held']='escape'
     I.send(user,I.key_input(0x1b,1,up),hwnd,state,x,y);out['input_sent']=True
     row=dict(kind='key',virtual_key=27,up=up,accepted=True,pressed_after=None);out['events'].append(row);await_state(27,not up,row)
     if up:out['possibly_held']=None
   else:
    I.position(user,hwnd,state,x,y);out['cursor_positions']+=1
    down,up,vk,name=(8,16,2,'right') if action=='right' else (2,4,1,'left')
    for flags,pressed in [(down,True),(up,False)]:
     state=qualify();sx,sy=I.assert_point(user,hwnd,state,x,y);actual=wt.POINT()
     require(user.GetCursorPos(ctypes.byref(actual)) and (actual.x,actual.y)==(sx,sy) and I.root_at(user,actual.x,actual.y)==hwnd,'actual owned cursor lost; no mouse input/release')
     require(bool(user.GetAsyncKeyState(vk)&0x8000)==(not pressed),'button state changed; no input/release')
     if pressed:out['possibly_held']=name
     I.send(user,I.mouse(flags),hwnd,state,x,y);out['input_sent']=True
     row=dict(kind='mouse',button=name,flags=flags,accepted=True,pressed_after=None);out['events'].append(row);await_state(vk,pressed,row)
     if not pressed:out['possibly_held']=None
  out['success']=True
 except Exception as e:out['error']=f'{type(e).__name__}: {e}'
 with (args.case_dir/f'menu-dismiss-window-{args.stage}.json').open('x',encoding='utf-8') as f:json.dump(out,f,ensure_ascii=False,indent=2)
 print(json.dumps({k:out[k] for k in ['pid','stage','success','error','possibly_held']},ensure_ascii=False));return 0 if out['success'] else 1
if __name__=='__main__':
 p=argparse.ArgumentParser(allow_abbrev=False);p.add_argument('--pid',type=int,required=True);p.add_argument('--case-dir',type=Path,required=True);p.add_argument('--stage',type=int,choices=range(5),required=True);p.add_argument('--interactive-input-approved',action='store_true');p.add_argument('--dismiss-input-approved',action='store_true');a=p.parse_args()
 if a.pid<=0 or not a.case_dir.is_absolute() or not a.case_dir.is_dir():p.error('positive original PID and existing absolute case directory required')
 raise SystemExit(run(a))
