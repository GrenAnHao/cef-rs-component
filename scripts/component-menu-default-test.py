"""Owned Win32 right-click -> default GPUI menu -> SelectAll witness.
No foreground request, clipboard, JS/CEF input injection or layout/IME changes.
Finite/non-atomic capture and qualification, not OCR/transparent overlay proof.
"""
import argparse,ctypes,ctypes.wintypes as wt,hashlib,importlib.util,json,math,ntpath,os,struct,sys,time
from pathlib import Path
spec=importlib.util.spec_from_file_location('menu_input_primitives',Path(__file__).with_name('component-input-test.py'))
I=importlib.util.module_from_spec(spec);spec.loader.exec_module(I)
H=I.H;require=I.require
TARGET_COMMAND=117
DEVTOOLS=False
FIND_FLOW=False
SUBMENU_FLOW=False
SKIP_TITLE=False
EXPECT_TITLE='menu-default-selected:1'
INPUT_STAGES=(0,1)
RIGHT_STAGES=(0,)
TITLE_EFFECT_STAGE=2
TERMINAL_STAGE=3
EXPECTED_ACTIONS=1


def session(pid):
 class Version(ctypes.Structure):
  _fields_=[('size',wt.DWORD),('major',wt.DWORD),('minor',wt.DWORD),('build',wt.DWORD),('platform',wt.DWORD),('service',wt.WCHAR*128)]
 version=Version();version.size=ctypes.sizeof(version)
 nt=ctypes.WinDLL('ntdll');nt.RtlGetVersion.argtypes=[ctypes.POINTER(Version)];nt.RtlGetVersion.restype=wt.LONG
 require(nt.RtlGetVersion(ctypes.byref(version))==0 and version.major>=10,'unknown Windows10+ session semantics')
 kernel=ctypes.WinDLL('kernel32');kernel.ProcessIdToSessionId.argtypes=[wt.DWORD,ctypes.POINTER(wt.DWORD)];kernel.ProcessIdToSessionId.restype=wt.BOOL
 own=wt.DWORD();target=wt.DWORD()
 require(kernel.ProcessIdToSessionId(os.getpid(),ctypes.byref(own)) and kernel.ProcessIdToSessionId(pid,ctypes.byref(target)) and own.value==target.value,'foreign/unknown process session')
 wts=ctypes.WinDLL('wtsapi32');wts.WTSQuerySessionInformationW.argtypes=[wt.HANDLE,wt.DWORD,ctypes.c_int,ctypes.POINTER(ctypes.c_void_p),ctypes.POINTER(wt.DWORD)];wts.WTSQuerySessionInformationW.restype=wt.BOOL
 wts.WTSFreeMemory.argtypes=[ctypes.c_void_p];wts.WTSFreeMemory.restype=None
 pointer=ctypes.c_void_p();size=wt.DWORD()
 try:
  require(wts.WTSQuerySessionInformationW(None,own.value,25,ctypes.byref(pointer),ctypes.byref(size)) and pointer.value and size.value>=232,'session readiness unknown')
  prefix=ctypes.string_at(pointer,20);level,padding,identity,state,flags=struct.unpack('<5I',prefix)
  require(level==1 and identity==own.value and state==0 and flags==1,'session locked/disconnected/unknown; no input')
  return dict(session=identity,level=level,state=state,flags=flags,returned_bytes=size.value,read_bytes=20)
 finally:
  if pointer.value:wts.WTSFreeMemory(pointer)

def process_api():
 kernel=ctypes.WinDLL('kernel32',use_last_error=True)
 kernel.OpenProcess.argtypes=[wt.DWORD,wt.BOOL,wt.DWORD];kernel.OpenProcess.restype=wt.HANDLE
 kernel.CloseHandle.argtypes=[wt.HANDLE];kernel.CloseHandle.restype=wt.BOOL
 kernel.GetProcessTimes.argtypes=[wt.HANDLE,*([ctypes.POINTER(wt.FILETIME)]*4)];kernel.GetProcessTimes.restype=wt.BOOL
 kernel.QueryFullProcessImageNameW.argtypes=[wt.HANDLE,wt.DWORD,wt.LPWSTR,ctypes.POINTER(wt.DWORD)];kernel.QueryFullProcessImageNameW.restype=wt.BOOL
 kernel.GetCurrentThreadId.argtypes=[];kernel.GetCurrentThreadId.restype=wt.DWORD
 return kernel

def verified_session(kernel,pid,original):
 verify_process(kernel,pid,original)
 return session(pid)

def input_desktop(user,kernel):
 # Borrowed calling-thread desktop, scalar UOI_IO only: no name/SID, switch or CloseDesktop.
 value=wt.BOOL();needed=wt.DWORD();desktop=user.GetThreadDesktop(kernel.GetCurrentThreadId())
 require(desktop and user.GetUserObjectInformationW(desktop,6,ctypes.byref(value),ctypes.sizeof(value),ctypes.byref(needed)) and needed.value==ctypes.sizeof(value) and bool(value.value),'current desktop input eligibility unknown; no input/release')
 return dict(receives_input=True,bytes=needed.value,physical_release_proof=False)

def image_key(value):
 require(isinstance(value,str) and ntpath.isabs(value) and chr(0) not in value,'invalid original absolute process image')
 prefix=chr(92)*2+'?'+chr(92)
 if value.startswith(prefix+'UNC'+chr(92)):value=chr(92)*2+value[8:]
 elif value.startswith(prefix):value=value[4:]
 return ntpath.normcase(ntpath.normpath(value))

def verify_process(kernel,pid,original):
 expected=original['created_filetime'];require(type(expected) is int and expected>0,'original process creation unknown')
 wanted=image_key(original['image']);handle=kernel.OpenProcess(0x1000,False,pid)
 require(handle,'original menu process unreadable')
 try:
  times=[wt.FILETIME() for _ in range(4)]
  require(kernel.GetProcessTimes(handle,*[ctypes.byref(x) for x in times]),'original creation time unavailable')
  created=(times[0].dwHighDateTime<<32)|times[0].dwLowDateTime
  require(created==expected,'original menu PID reused; no image/window inspection')
  size=wt.DWORD(32768);text=ctypes.create_unicode_buffer(size.value)
  require(kernel.QueryFullProcessImageNameW(handle,0,text,ctypes.byref(size)) and 0<size.value<32768,'original menu image unavailable')
  require(image_key(text.value)==wanted,'original menu image mismatch')
  return dict(created_filetime=created,image=text.value)
 finally:kernel.CloseHandle(handle)

def inspect(user,fact,pid,kernel,original_process):
 verify_process(kernel,pid,original_process)
 hwnd=fact['hwnd'];owner=wt.DWORD()
 require(user.IsWindow(hwnd),'owned window no longer exists')
 thread=int(user.GetWindowThreadProcessId(hwnd,ctypes.byref(owner)))
 require(owner.value==pid==fact['pid'] and thread==fact['thread'],'owned HWND lifecycle/PID/thread changed')
 require(H.pointer_value(user.GetPropW(hwnd,'GPUI_CEF_Inp1'))!=0,'original input observer property absent')
 state=I.verify_owner(user,fact,I.TITLE,pid,thread)
 require(H.pointer_value(user.GetAncestor(hwnd,2))==hwnd,'owned window not root')
 return state

def capture(user,meta,pid,case,stage,out,kernel):
 fact=meta['original'];before=inspect(user,fact,pid,kernel,meta['process_identity']);hwnd=fact['hwnd'];scale=meta['scale']
 require(math.isfinite(scale) and scale>0 and abs(scale-before['dpi']/96)<.0001,'scale/DPI mismatch')
 points=[dict(name=f'grid-{x}-{y}',client_px=[int(before['client_width']*x/6),int(before['client_height']*y/6)]) for x in (1,3,5) for y in (1,3,5)]
 rect=None
 if stage==1:
  q=meta['visual'];layout=q['layout'];raw=meta['original_capture']
  require(q['first_error'] is None and q['open_sequence']==layout['sequence']==raw['sequence'] and raw['frame_id_bytes']>0 and raw['selection_bytes']==0,'original native menu scope absent')
  rect=layout['menu'];require(isinstance(rect,list) and len(rect)==4 and all(type(v) in (float,int) and math.isfinite(v) for v in rect),'actual menu prepaint bounds absent')
  left,top,width,height=rect
  require(width>0 and height>0 and left>=0 and top>=0 and (left+width)*scale<=before['client_width'] and (top+height)*scale<=before['client_height'],'menu outside original client')
  require(abs(left-raw['anchor'][0])<=1 and abs(top-(48+raw['anchor'][1]))<=1,'actual menu anchor does not match original VIEW DIP once')
  target=TARGET_COMMAND
  row=next(c for c in layout['commands'] if c['id']==target and c['enabled']);b=row['bounds'];require(b is not None,'actual target label bounds absent')
  center=[I.px(b[0]+b[2]*.5,scale),I.px(b[1]+b[3]*.5,scale)]
  require(center==meta['select_point'] and left*scale<=center[0]<(left+width)*scale and top*scale<=center[1]<(top+height)*scale,'SelectAll point not inside actual current menu')
  points += [dict(name=f'menu-{x}-{y}',client_px=[x,y]) for y in range(I.px(top+3,scale),I.px(top+height-3,scale),4) for x in range(I.px(left+3,scale),I.px(left+width-3,scale),4)]
 else:
  points += [dict(name=f'body-{x}-{y}',client_px=[I.px(x,scale),I.px(48+y,scale)]) for x in (80,160,240) for y in (130,180)]
 if (DEVTOOLS and stage>=2) or (FIND_FLOW and stage in (2,3)):
  # The auxiliary DevTools popup is expected to overlap the owner at this point;
  # the passive occlusion witness cannot be authoritative there. Identity,
  # session and foreground checks above still run, and the action stages keep the
  # full strict probe set.
  out['capture']={'before':before,'probes_before':[],'devtools_overlap_expected':True}
  pre=[]
 else:
  pre=H.run_probes(user,hwnd,before,points);out['capture']={'before':before,'probes_before':pre}
  require(all(p['owned'] for p in pre),'foreign window covers original client/menu before capture')
 virtual=H.virtual_screen(user);box=(before['client_left'],before['client_top'],before['client_right'],before['client_bottom'])
 require(box[0]>=virtual['left'] and box[1]>=virtual['top'] and box[2]<=virtual['left']+virtual['width'] and box[3]<=virtual['top']+virtual['height'],'owned client outside desktop')
 image=H.ImageGrab.grab(bbox=box,all_screens=True).convert('RGB')
 after=inspect(user,fact,pid,kernel,meta['process_identity'])
 if DEVTOOLS and stage>=2:
  out['capture']['after']=after
 else:
  post=H.run_probes(user,hwnd,after,points)
  out['capture'].update(after=after,probes_after=post)
  require(before==after and all(p['owned'] for p in post),'owned window became occluded/changed during capture')
 wins=sorted(H.enum_visible_top_level_windows(user,pid))
 if DEVTOOLS and stage>=2:
  # The opt-in row really created a second window owned by this process; record
  # it instead of pretending the owner is alone, and keep requiring exactly one
  # auxiliary beside the owner.
  # The auxiliary may already be gone once the owner Browser retires, so accept
  # owner alone or owner plus exactly one auxiliary; anything else is foreign.
  require(hwnd in wins and len(wins) in (1,2),'expected owner (+optional one DevTools window), got %r'%(wins,))
  out['auxiliary_hwnds']=[w for w in wins if w!=hwnd]
 else:
  require(wins==[hwnd],'unexpected extra owned native window')
 path=case/f'menu-default-window-{stage}.png'
 with path.open('xb') as f:image.save(f,format='PNG')
 out['capture'].update(path=path.name,sha256=hashlib.sha256(path.read_bytes()).hexdigest(),rgb_sha256=hashlib.sha256(image.tobytes()).hexdigest(),size=list(image.size),bbox=list(box))
 if stage==1:
  values=[image.getpixel(tuple(p['client_px'])) for p in points if p['name'].startswith('menu-')]
  luma=[sum(v)/3 for v in values];require(luma,'empty actual menu samples');base=max(luma)
  contrasts=sum(base-v>=80 for v in luma);out['menu_contrast_samples']=contrasts;require(contrasts>=12,'actual menu has insufficient contrast/glyph witness; not OCR')
 else:
  expected=(0,170,68) if stage==TERMINAL_STAGE else (26,76,126)
  if (DEVTOOLS and stage>=2) or (FIND_FLOW and stage in (2,3)):
   # Body pixels are under the owned auxiliary popup now; record the samples
   # instead of asserting a colour the overlay can legitimately change.
   out['body_samples']=[list(image.getpixel(tuple(p['client_px']))) for p in points if p['name'].startswith('body-')]
  else:
   require(all(image.getpixel(tuple(p['client_px']))==expected for p in points if p['name'].startswith('body-')),'original CEF/retired green body mismatch')
 if stage==TITLE_EFFECT_STAGE and not DEVTOOLS and not SKIP_TITLE:require(meta['title']==EXPECT_TITLE,'SelectAll effect missing before the second menu')
 if stage==TITLE_EFFECT_STAGE and not DEVTOOLS and not SKIP_TITLE:require(meta['title']==EXPECT_TITLE and meta['native']['actions']==EXPECTED_ACTIONS and meta['native']['dismissed']==EXPECTED_ACTIONS,'actual original selection effect/terminal missing')
 if stage==TERMINAL_STAGE:require(meta['weak_released'] and meta['browser_closed'] and meta['host_alive'] and meta['visual']['retired'],'original View/Browser did not retire')
 return before

def run(args):
 out=dict(schema=1,kind='real-default-menu-select-all-slice',pid=args.pid,stage=args.stage,success=False,error=None,hwnd=None,metadata=None,input_sent=False,foreground_requested=False,possibly_held=None,menu_contrast_samples=0,session_checks=[],atomic_capture=False,ocr=False,button_observations=[],process_identity_verified=False,desktop_checks=[],physical_release_proof=False,async_zero_authoritative=False,possibly_held_semantics='interrupted owned input sequence; null is not global physical-release proof')
 try:
  require(args.interactive_input_approved,'explicit input approval required')
  case=args.case_dir;meta=I.load(case/f'menu-default-stage-{args.stage}.json');out['metadata']=meta
  require(meta['pid']==args.pid and meta['stage']==args.stage,'owned stage/PID mismatch')
  fg=meta['initial_foreground'];require(type(fg['approved']) is bool and type(fg['attempts']) is int and fg['attempts'] in (0,1),'foreground approval/count unknown')
  if fg['approved']:require(fg['after'] is True and type(fg['before']) is bool and fg['policy']=='one-ordinary-owned-request','approved owned request not actually ready')
  else:require(fg['attempts']==0 and fg['before'] is None and fg['after'] is None and fg['policy']=='passive','passive observer policy changed')
  user=H.load_win32();H.set_thread_dpi_awareness(user);I.win32(user);kernel=process_api()
  user.GetPropW.argtypes=[wt.HWND,wt.LPCWSTR];user.GetPropW.restype=ctypes.c_void_p
  user.GetThreadDesktop.argtypes=[wt.DWORD];user.GetThreadDesktop.restype=wt.HANDLE
  user.GetUserObjectInformationW.argtypes=[wt.HANDLE,ctypes.c_int,ctypes.c_void_p,wt.DWORD,ctypes.POINTER(wt.DWORD)];user.GetUserObjectInformationW.restype=wt.BOOL
  fact=meta['original'];out['hwnd']=fact['hwnd']
  out['session_checks'].append(verified_session(kernel,args.pid,meta['process_identity']));out['process_identity_verified']=True
  capture(user,meta,args.pid,case,args.stage,out,kernel)
  if args.stage in INPUT_STAGES:
   scale=meta['scale'];hwnd=fact['hwnd']
   if args.stage in RIGHT_STAGES:
    x,y=I.px(24,scale),I.px(68,scale)
   else:
    sp=case/f'menu-default-select-point-{args.stage-1}.json'
    if not sp.is_file(): sp=case/'menu-default-select-point.json'
    require(sp.is_file(),'validated select point missing; refusing input')
    sj=json.loads(sp.read_text(encoding='utf-8-sig'))
    x,y=int(sj['x']),int(sj['y'])
   def qualify():
    out['session_checks'].append(verified_session(kernel,args.pid,meta['process_identity']))
    out['desktop_checks'].append(input_desktop(user,kernel))
    state=inspect(user,fact,args.pid,kernel,meta['process_identity'])
    require(H.pointer_value(user.GetForegroundWindow())==hwnd,'owned window not foreground; no request/bypass allowed')
    I.assert_point(user,hwnd,state,x,y);return state
   if SUBMENU_FLOW and args.stage==1:
    sp=case/f'menu-default-submenu-point-{args.stage-1}.json'
    require(sp.is_file(),'submenu parent point missing; refusing input')
    sj=json.loads(sp.read_text(encoding='utf-8-sig'))
    x,y=int(sj['x']),int(sj['y'])
   state=qualify();held=[v for v in (1,2,4,5,6,0x10,0x11,0x12,0x5b,0x5c) if user.GetAsyncKeyState(v)&0x8000]
   require(not held,f'buttons/modifiers already held {held}; no input')
   I.position(user,hwnd,state,x,y)
   if SUBMENU_FLOW and args.stage==1:
    # Hover the one-level submenu's parent row, then click the child row the
    # host publishes only once the submenu popup has actually laid out.
    cp=case/f'menu-default-submenu-child-{args.stage}.json'
    end=time.monotonic()+6;child=None
    while time.monotonic()<end:
     if cp.is_file():
      cj=json.loads(cp.read_text(encoding='utf-8-sig'));child=(int(cj['x']),int(cj['y']));break
     time.sleep(.05)
    require(child is not None,'submenu child row never published; refusing input')
    out['submenu_hover']={'parent':[x,y],'child':list(child)}
    x,y=child
    state=qualify()
    I.position(user,hwnd,state,x,y)
   down,up,name=(8,16,'right') if args.stage in RIGHT_STAGES else (2,4,'left')
   if args.stage==2:
    I.position(user,hwnd,state,x,y)
   vk=2 if name=='right' else 1
   def button(flags,is_down):
    state=qualify();sx,sy=I.assert_point(user,hwnd,state,x,y);actual=wt.POINT()
    require(user.GetCursorPos(ctypes.byref(actual)) and (actual.x,actual.y)==(sx,sy),'actual cursor changed; no unattributed input/release')
    require(I.root_at(user,actual.x,actual.y)==hwnd,'actual cursor root lost; no foreign input/release')
    require(bool(user.GetAsyncKeyState(vk)&0x8000)==(not is_down),'button precondition changed; no input/release')
    if is_down:out['possibly_held']=name
    I.send(user,I.mouse(flags),hwnd,state,x,y);out['input_sent']=True
    row=dict(button=name,flags=flags,accepted=True,pressed_after=None);out['button_observations'].append(row)
    until=time.monotonic()+2
    while time.monotonic()<until:
     row['pressed_after']=bool(user.GetAsyncKeyState(vk)&0x8000)
     if row['pressed_after']==is_down:break
     time.sleep(.01)
    require(row['pressed_after']==is_down,'accepted menu button did not reach expected async state')
    # A zero sample alone is not release evidence. Requalify the original scope
    # and readable input-desktop after the sample, without claiming atomicity.
    qualify()
    if not is_down:out['possibly_held']=None
   button(down,True)
   button(up,False)
   if args.stage in RIGHT_STAGES:
    probe=[]
    for _ in range(6):
     probe.append({'t':round(time.monotonic(),3),'pressed':bool(user.GetAsyncKeyState(vk)&0x8000)})
     time.sleep(.05)
    out['post_right_probe']=probe
  out['success']=True
 except Exception as e:out['error']=f'{type(e).__name__}: {e}'
 with (args.case_dir/f'menu-default-window-{args.stage}.json').open('x',encoding='utf-8') as f:json.dump(out,f,ensure_ascii=False,indent=2)
 print(json.dumps({k:out[k] for k in ['stage','pid','success','error','possibly_held']},ensure_ascii=False));return 0 if out['success'] else 1
if __name__=='__main__':
 p=argparse.ArgumentParser(allow_abbrev=False);p.add_argument('--pid',type=int,required=True);p.add_argument('--case-dir',type=Path,required=True);p.add_argument('--stage',type=int,choices=range(6),required=True);p.add_argument('--interactive-input-approved',action='store_true');p.add_argument('--command',type=int,default=117);p.add_argument('--devtools',action='store_true');p.add_argument('--find-flow',action='store_true');p.add_argument('--skip-selectall-title',action='store_true');p.add_argument('--expect-title',default='menu-default-selected:1');p.add_argument('--submenu-flow',action='store_true');a=p.parse_args()
 if a.pid<=0 or not a.case_dir.is_absolute() or not a.case_dir.is_dir():p.error('positive PID and existing absolute case directory required')
 globals()['TARGET_COMMAND']=a.command
 globals()['SUBMENU_FLOW']=a.submenu_flow
 globals()['DEVTOOLS']=bool(getattr(a,'devtools',False))
 globals()['FIND_FLOW']=bool(getattr(a,'find_flow',False))
 globals()['INPUT_STAGES']=(0,1,2,3) if globals()['FIND_FLOW'] else (0,1)
 globals()['RIGHT_STAGES']=(0,2) if globals()['FIND_FLOW'] else (0,)
 globals()['TITLE_EFFECT_STAGE']=4 if globals()['FIND_FLOW'] else 2
 globals()['TERMINAL_STAGE']=5 if globals()['FIND_FLOW'] else 3
 globals()['EXPECTED_ACTIONS']=2 if globals()['FIND_FLOW'] else 1
 globals()['SKIP_TITLE']=bool(getattr(a,'skip_selectall_title',False))
 globals()['EXPECT_TITLE']=getattr(a,'expect_title','menu-default-selected:1')
 sys.exit(run(a))
