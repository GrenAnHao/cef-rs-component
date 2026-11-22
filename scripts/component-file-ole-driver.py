"""One explicitly approved owned source->target LEFT OLE gesture. No foreign input.
Finite identity/point/session sampling is not atomic isolation or transparent-overlay proof.
"""
import argparse,ctypes,ctypes.wintypes as W,importlib.util,json,os,sys,time,struct
from pathlib import Path
spec=importlib.util.spec_from_file_location('ole_owned_primitives',Path(__file__).with_name('component-menu-default-test.py'))
M=importlib.util.module_from_spec(spec);spec.loader.exec_module(M)
I=M.I;H=I.H;require=I.require;MARKER=0x43464534
GESTURE_KEYS=(1,2,4,5,6,0x10,0x11,0x12,0x5b,0x5c)


MANUAL_FOCUS_SECONDS = 15.0


def manual_source_eligible(u, hwnd):
    return (H.pointer_value(u.GetForegroundWindow()) == hwnd
            and not any(u.GetAsyncKeyState(key) & 0x8000 for key in GESTURE_KEYS))


def require_manual_source_before_input(u, hwnd, *, left_pressed):
    require(H.pointer_value(u.GetForegroundWindow()) == hwnd,
            'manual source foreground lost before source input')
    pressed = [key for key in GESTURE_KEYS if u.GetAsyncKeyState(key) & 0x8000]
    require(pressed == ([1] if left_pressed else []),
            'manual source buttons lost required input state')


def wait_for_manual_source_focus(u, hwnd, verify, out):
    """One finite read-only observation; never inspect or activate a foreign window."""
    require(hwnd not in (0, 0xffff), 'specific owned source HWND required')
    start = time.monotonic()
    deadline = start + MANUAL_FOCUS_SECONDS
    quiet_since = None
    counts = dict(source_foreground=0, other_or_unknown=0, source_buttons_down=0)
    out['manual_source_focus_samples'] = counts
    while time.monotonic() < deadline:
        began = time.monotonic()
        focus = H.pointer_value(u.GetForegroundWindow()) == hwnd
        held = focus and any(u.GetAsyncKeyState(key) & 0x8000 for key in GESTURE_KEYS)
        finished = time.monotonic()
        counts['source_foreground' if focus else 'other_or_unknown'] += 1
        counts['source_buttons_down'] += int(held)
        if not focus or held:
            quiet_since = None
        elif quiet_since is None:
            verify()
            quiet_since = finished
        elif began - quiet_since >= 0.15:
            verify()
            require(manual_source_eligible(u, hwnd) and time.monotonic() < deadline,
                    'manual source focus lost before input; no foreground request')
            out['manual_source_focus_observed'] = True
            out['manual_source_focus_wait_ms'] = (time.monotonic()-start)*1000
            return
        time.sleep(.05)
    raise RuntimeError('manual owned source foreground timeout; no foreground request or input')


def validate_manual_gesture_ready(plan, receipt):
    expected = dict(pid=plan['pid'], nonce=plan['source_nonce'],
                    hwnd=plan['target']['hwnd'], input_epoch=plan['input_epoch'])
    require(type(receipt) is dict and receipt == expected and all(type(v) is int and v > 0 for v in receipt.values()),
            'manual OLE gesture ACK does not match original target PID/nonce/HWND/epoch')


def establish_foreground(u, hwnd, approved, verify, out):
    """One ordinary request plus one owned WM_NULL fence, never an input retry.

    Cross-input-queue activation is asynchronous (Microsoft Old New Thing,
    2016-11-18). Message completion is NOT actual foreground or atomic ownership.
    """
    require(hwnd not in (0, 0xffff), 'specific owned source HWND required')
    verify()
    out['foreground_before'] = H.pointer_value(u.GetForegroundWindow())
    if out['foreground_before'] != hwnd:
        require(approved, 'source not foreground; ordinary request not approved')
        out['foreground_requested'] = True
        out['foreground_attempts'] = 1
        out['foreground_call_return'] = bool(u.SetForegroundWindow(hwnd))
        require(out['foreground_call_return'], 'ordinary owned SOURCE SetForegroundWindow denied')
        verify()
        result = ctypes.c_size_t()
        ctypes.set_last_error(0)
        start = time.monotonic()
        status = int(u.SendMessageTimeoutW(hwnd, 0, 0, 0, 0x23, 1000, ctypes.byref(result)))
        error = ctypes.get_last_error()
        out['foreground_fence'] = dict(hwnd=hwnd, message=0, flags=0x23, timeout_ms=1000,
                                       status=status, message_result=result.value,
                                       last_error=error, elapsed_ms=(time.monotonic()-start)*1000)
        require(status != 0, 'owned source WM_NULL fence failed/timed out')
        verify()
    out['foreground_after'] = H.pointer_value(u.GetForegroundWindow())
    require(out['foreground_after'] == hwnd, 'owned source not actually foreground')
    verify()


def publish_marker(control, name, payload):
    # Same-directory Windows rename publishes complete bytes, without overwrite.
    pending = control / (name + '.pending')
    target = control / name
    with pending.open('x', encoding='ascii') as stream:
        stream.write(payload)
    os.rename(pending, target)


def publish_source_stop(control, pid, nonce):
    publish_marker(control, 'ole-source-stop.request', f'ole-stop:{pid}:{nonce}')


def validate_active_close_receipt(plan, receipt, target_pid, nonce):
    def positive(value):
        return type(value) is int and value > 0

    target, browser, epoch = plan['target'], plan['browser'], plan['input_epoch']
    require(type(browser) is list and len(browser)==2 and all(map(positive,browser)) and positive(epoch),
            'original Close plan identity missing')
    require(type(receipt) is dict and all(positive(receipt.get(k)) for k in ('pid','nonce','hwnd'))
            and (receipt['pid'],receipt['nonce'],receipt['hwnd'])==(target_pid,nonce,target['hwnd']),
            'original OLE target close receipt identity mismatch')
    r = receipt.get('retirement')
    require(type(r) is dict and all(r.get(k) is True for k in
             ('closed','source_closed','view_released','idle_after_close','root_repainted'))
            and r.get('drop_observed') is False, 'original OLE target did not close cleanly before release')
    ticket = r.get('ticket')
    require(type(ticket) is list and len(ticket)==3 and all(map(positive,ticket)) and ticket[:2]==browser
            and positive(r.get('before_epoch')) and r['before_epoch']==epoch
            and positive(r.get('after_epoch')) and r['after_epoch']>epoch,
            'original OLE CloseTicket or input epoch mismatch')


def validate_cancelled_source(result, pid, nonce):
    require(type(result) is dict and type(result.get('pid')) is int and result['pid']==pid
            and type(result.get('nonce')) is int and result['nonce']==nonce
            and type(result.get('effect')) is int and result['effect']==0
            and result.get('error','missing') is None and result.get('aborted') is False
            and type(result.get('queries')) is int and result['queries']>0
            and result.get('physical_source') is True and result.get('marked_left_down') is True
            and result.get('terminal_action')=='Cancel' and result.get('terminal_cancel_on_release') is True
            and type(result.get('terminal_key_state')) is int and result['terminal_key_state']&1==0,
            'native source did not report the armed Cancel after LEFT release')


def integrity_rid(buffer, length):
    """Read only the integrity RID, after bounding the returned inline SID."""
    require(16 <= length <= ctypes.sizeof(buffer), 'invalid owned token buffer length')
    base = ctypes.addressof(buffer)
    sid = ctypes.c_void_p.from_buffer(buffer).value
    offset = (sid or 0) - base
    header = ctypes.sizeof(ctypes.c_void_p) * 2  # x64 SID_AND_ATTRIBUTES
    require(header <= offset and offset + 8 <= length, 'token SID outside owned buffer')
    data = buffer.raw[:length]
    count = data[offset + 1]
    require(data[offset] == 1 and 0 < count <= 15 and offset + 8 + 4*count <= length,
            'invalid bounded integrity SID')
    return struct.unpack_from('<I', data, offset + 8 + 4*(count-1))[0]


def input_environment(u, kernel, owned_creation):
    """Read-only diagnostics for exactly this fixture's processes; no privileges changed."""
    require(isinstance(owned_creation, dict) and 0 < len(owned_creation) <= 3 and all(pid > 0 and created > 0 for pid, created in owned_creation.items()), 'bounded original process creation identities required')
    adv = ctypes.WinDLL('advapi32', use_last_error=True)
    adv.OpenProcessToken.argtypes = [W.HANDLE, W.DWORD, ctypes.POINTER(W.HANDLE)]
    adv.OpenProcessToken.restype = W.BOOL
    adv.GetTokenInformation.argtypes = [W.HANDLE, ctypes.c_int, ctypes.c_void_p, W.DWORD, ctypes.POINTER(W.DWORD)]
    adv.GetTokenInformation.restype = W.BOOL
    result = {'tokens': [], 'driver_desktop': None, 'diagnostic_only': True}
    for pid, original_creation in sorted(owned_creation.items()):
        row = {'pid': pid, 'creation_ticks': None, 'integrity_rid': None, 'elevated': None, 'ui_access': None, 'error': None}
        process = None
        token = W.HANDLE()
        try:
            process = kernel.OpenProcess(0x1000, False, pid)
            require(process, 'owned token process unreadable')
            times = [W.FILETIME() for _ in range(4)]
            require(kernel.GetProcessTimes(process, *[ctypes.byref(t) for t in times]), 'owned token process creation unavailable')
            row['creation_ticks'] = (times[0].dwHighDateTime << 32) | times[0].dwLowDateTime
            require(row['creation_ticks'] == original_creation, 'token process PID reused; no TOKEN_QUERY')
            require(adv.OpenProcessToken(process, 8, ctypes.byref(token)), 'owned TOKEN_QUERY unavailable')
            for kind, key in [(20, 'elevated'), (26, 'ui_access')]:
                value, needed = W.DWORD(), W.DWORD()
                require(adv.GetTokenInformation(token, kind, ctypes.byref(value), 4, ctypes.byref(needed)) and needed.value == 4, 'owned scalar token info unavailable')
                row[key] = bool(value.value)
            needed = W.DWORD()
            ctypes.set_last_error(0)
            status = adv.GetTokenInformation(token, 25, None, 0, ctypes.byref(needed))
            require(not status and ctypes.get_last_error() == 122 and 16 <= needed.value <= 65536, 'bounded integrity size unavailable')
            buffer = ctypes.create_string_buffer(needed.value)
            capacity = needed.value
            require(adv.GetTokenInformation(token, 25, buffer, capacity, ctypes.byref(needed)) and 16 <= needed.value <= capacity, 'owned integrity info unavailable')
            row['integrity_rid'] = integrity_rid(buffer, needed.value)
        except Exception as error:
            row['error'] = type(error).__name__ + ': ' + str(error)
        finally:
            if token: kernel.CloseHandle(token)
            if process: kernel.CloseHandle(process)
        result['tokens'].append(row)
    try:
        kernel.GetCurrentThreadId.argtypes = []; kernel.GetCurrentThreadId.restype = W.DWORD
        u.GetThreadDesktop.argtypes = [W.DWORD]; u.GetThreadDesktop.restype = W.HANDLE
        u.GetUserObjectInformationW.argtypes = [W.HANDLE, ctypes.c_int, ctypes.c_void_p, W.DWORD, ctypes.POINTER(W.DWORD)]
        u.GetUserObjectInformationW.restype = W.BOOL
        # Borrowed current-thread desktop: do NOT close/switch it or read its name/SID.
        desktop = u.GetThreadDesktop(kernel.GetCurrentThreadId())
        value, needed = W.BOOL(), W.DWORD()
        require(desktop and u.GetUserObjectInformationW(desktop, 6, ctypes.byref(value), ctypes.sizeof(value), ctypes.byref(needed)) and needed.value == ctypes.sizeof(value), 'current desktop input flag unavailable')
        result['driver_desktop'] = {'receives_input': bool(value.value), 'bytes': needed.value}
    except Exception as error:
        result['driver_desktop'] = {'error': type(error).__name__ + ': ' + str(error)}
    return result


def run(a):
 out=dict(success=False,error=None,source_pid=a.source_pid,target_pid=a.target_pid,foreground_requested=False,foreground_attempts=0,foreground_before=None,foreground_after=None,foreground_call_return=None,foreground_fence=None,input_sent=False,possibly_held=None,observations=[],session_checks=[],partial_json_reads=0,physical_ole_driver=True,manual_source_focus_approved=a.manual_source_focus_approved,manual_source_focus_observed=False,manual_source_focus_wait_ms=None,close_while_dragging=a.close_while_dragging)
 manual_enter_deadline=None
 def wait_json(name,seconds=20,deadline=None):
  until=time.monotonic()+seconds;p=a.control/name
  if deadline is not None:until=min(until,deadline)
  while time.monotonic()<until:
   if p.is_file():
    try:
     value=json.loads(p.read_text(encoding='utf-8-sig'))
     if deadline is None or time.monotonic()<until:return value
    except json.JSONDecodeError:out['partial_json_reads']+=1
   time.sleep(.01)
  raise RuntimeError('owned complete metadata timeout: '+name)
 try:
  require(a.ole_input_approved,'separate explicit physical OLE approval required')
  require(not (a.manual_source_focus_approved and a.ordinary_source_foreground_approved),
          'manual source focus and ordinary foreground request are mutually exclusive')
  require(a.control.is_absolute() and a.control.is_dir() and a.nonce>0 and a.source_pid>0 and a.target_pid>0,'explicit owned identities required')
  u=H.load_win32();H.set_thread_dpi_awareness(u);I.win32(u)
  u.GetPropW.argtypes=[W.HWND,W.LPCWSTR];u.GetPropW.restype=ctypes.c_void_p
  u.IsWindow.argtypes=[W.HWND];u.IsWindow.restype=W.BOOL
  u.IsWindowVisible.argtypes=[W.HWND];u.IsWindowVisible.restype=W.BOOL
  u.IsIconic.argtypes=[W.HWND];u.IsIconic.restype=W.BOOL
  u.SendMessageTimeoutW.argtypes=[W.HWND,W.UINT,ctypes.c_size_t,ctypes.c_ssize_t,W.UINT,W.UINT,ctypes.POINTER(ctypes.c_size_t)]
  u.SendMessageTimeoutW.restype=ctypes.c_ssize_t
  u.GetSystemMetrics.argtypes=[ctypes.c_int];u.GetSystemMetrics.restype=ctypes.c_int
  require(len({a.source_pid,a.target_pid,os.getpid()})==3,'independent source/target/driver processes required')
  kernel=ctypes.WinDLL('kernel32',use_last_error=True)
  kernel.OpenProcess.argtypes=[W.DWORD,W.BOOL,W.DWORD];kernel.OpenProcess.restype=W.HANDLE
  kernel.CloseHandle.argtypes=[W.HANDLE];kernel.CloseHandle.restype=W.BOOL
  kernel.GetProcessTimes.argtypes=[W.HANDLE,*([ctypes.POINTER(W.FILETIME)]*4)];kernel.GetProcessTimes.restype=W.BOOL
  kernel.QueryFullProcessImageNameW.argtypes=[W.HANDLE,W.DWORD,W.LPWSTR,ctypes.POINTER(W.DWORD)];kernel.QueryFullProcessImageNameW.restype=W.BOOL
  def process(pid,expected):
   handle=kernel.OpenProcess(0x1000,False,pid);require(handle,'owned process unreadable')
   try:
    size=W.DWORD(32768);text=ctypes.create_unicode_buffer(size.value)
    require(kernel.QueryFullProcessImageNameW(handle,0,text,ctypes.byref(size)),'owned image unconfirmed')
    require(os.path.normcase(text.value)==os.path.normcase(str(expected)),'owned image mismatch')
    times=[W.FILETIME() for _ in range(4)]
    require(kernel.GetProcessTimes(handle,*[ctypes.byref(x) for x in times]),'owned process creation time absent')
    return (times[0].dwHighDateTime<<32)|times[0].dwLowDateTime
   finally:kernel.CloseHandle(handle)
  source_creation=process(a.source_pid,a.source_executable);target_creation=process(a.target_pid,a.target_executable)
  plan=wait_json('ole-target-ready.json');source=wait_json('ole-source-ready.json')
  require(plan.get('close_while_dragging') is a.close_while_dragging,'owned target OLE case mismatch')
  require(plan.get('manual_source_focus') is a.manual_source_focus_approved and source.get('manual_source_focus') is a.manual_source_focus_approved,'owned manual focus policy mismatch')
  target=plan['target'];require(plan['pid']==target['pid']==a.target_pid and plan['source_nonce']==a.nonce and source['pid']==a.source_pid,'plan PID/nonce mismatch')
  require(source['creation_ticks']==source_creation+504911232000000000,'source process creation stamp mismatch')
  require(source['nonce']==target['nonce']==a.nonce and source['property']=='GPUI_CEF_OLE_Source_R58' and target['property']=='GPUI_CEF_OLE_Target_R58','owned property mismatch')
  require(source['hwnd']!=target['hwnd'],'source and target must be separate processes/windows')
  expected_creation={a.source_pid:(source_creation,a.source_executable),a.target_pid:(target_creation,a.target_executable)}
  def qualify(f):
   pid=f['pid'];out['session_checks'].append(M.session(pid));require(process(pid,expected_creation[pid][1])==expected_creation[pid][0],'owned PID reused')
   hwnd=f['hwnd'];p=W.DWORD();thread=int(u.GetWindowThreadProcessId(hwnd,ctypes.byref(p)))
   require(u.IsWindow(hwnd) and p.value==pid and thread==f['thread'] and H.pointer_value(u.GetAncestor(hwnd,2))==hwnd,'owned HWND identity/lifecycle changed')
   require(H.pointer_value(u.GetPropW(hwnd,f['property']))==a.nonce,'owned property retired or changed')
   require(u.IsWindowVisible(hwnd) and not u.IsIconic(hwnd),'owned window not visible/noniconic')
   r=W.RECT();point=W.POINT(0,0)
   require(u.GetClientRect(hwnd,ctypes.byref(r)) and u.ClientToScreen(hwnd,ctypes.byref(point)),'owned client geometry unknown')
   real=[point.x,point.y,r.right-r.left,r.bottom-r.top,int(u.GetDpiForWindow(hwnd))]
   declared=[f['client_left'],f['client_top'],f['client_width'],f['client_height'],f['dpi']]
   require(real==declared and real[2]>0 and real[3]>0 and real[4]>0,'owned geometry/DPI changed')
   return real
  def family():
   qualify(source);qualify(target)
   unexpected=[v for v in GESTURE_KEYS[1:] if u.GetAsyncKeyState(v)&0x8000]
   require(not unexpected,'unexpected button/modifier changed; no input/release')
   fg=H.pointer_value(u.GetForegroundWindow());require(fg in (source['hwnd'],target['hwnd']),'owned foreground family lost; no input/release')
   return fg
  def owned_point(f,x,y):
   r=qualify(f);require(0<=x<r[2] and 0<=y<r[3],'point outside owned client')
   sx,sy=r[0]+x,r[1]+y
   require(I.root_at(u,sx,sy)==f['hwnd'],'point covered by unknown/foreign root')
   return sx,sy
  qualify(source);qualify(target)
  out['input_environment_before_request']=input_environment(u,kernel,{a.source_pid:source_creation,a.target_pid:target_creation,os.getpid():process(os.getpid(),Path(sys.executable))})
  if a.manual_source_focus_approved:
   def verify_pair():
    qualify(source);qualify(target)
   identity=f'{a.target_pid}:{a.nonce}'
   publish_marker(a.control,'ole-driver-manual-ready.request',f'ole-manual-ready:{identity}')
   wait_for_manual_source_focus(u,source['hwnd'],verify_pair,out)
   # Starts before target sees the focus marker, so it cannot extend the
   # target's subsequent 20 s Enter deadline. This is not an input retry.
   manual_enter_deadline=time.monotonic()+18.0
   publish_marker(a.control,'ole-driver-manual-focus.request',f'ole-manual-focus:{identity}')
   receipt=wait_json('ole-target-gesture.ready.json',5,manual_enter_deadline)
   validate_manual_gesture_ready(plan,receipt)
   verify_pair()
   require(manual_source_eligible(u,source['hwnd']),'manual source focus lost after target ACK; no input')
   out['manual_gesture_ready']=receipt
  establish_foreground(u,source['hwnd'],a.ordinary_source_foreground_approved,lambda:qualify(source),out)
  require(ctypes.sizeof(I.INPUT)==40,'unexpected x64 INPUT ABI')
  held=[v for v in GESTURE_KEYS if u.GetAsyncKeyState(v)&0x8000]
  require(not held,'buttons/modifiers already held; no input')
  def position(f,x,y,label):
   family();sx,sy=owned_point(f,x,y)
   if manual_enter_deadline is not None:
    if f is source:
     left_pressed=label=='start-native-drag'
     require(not left_pressed or out['possibly_held']=='left','no original marked LEFT drag to start')
     require_manual_source_before_input(u,source['hwnd'],left_pressed=left_pressed)
    require(time.monotonic()<manual_enter_deadline,'manual OLE Enter budget expired before cursor placement')
   require(u.SetCursorPos(sx,sy),'owned cursor placement failed')
   p=W.POINT();require(u.GetCursorPos(ctypes.byref(p)) and (p.x,p.y)==(sx,sy),'cursor did not reach exact owned pixel')
   family();owned_point(f,x,y);out['observations'].append(dict(event=label,hwnd=f['hwnd'],client=[x,y],screen=[sx,sy]))
  def button(f,x,y,flags,label):
   family();sx,sy=owned_point(f,x,y);actual=W.POINT()
   require(u.GetCursorPos(ctypes.byref(actual)) and (actual.x,actual.y)==(sx,sy),'actual cursor moved away; refusing unattributed button input/release')
   require(I.root_at(u,actual.x,actual.y)==f['hwnd'],'actual cursor root lost; refusing foreign input/release')
   before_raw=int(u.GetAsyncKeyState(1))&0xffff
   before=bool(before_raw&0x8000)
   require(flags in (2,4) and before==(flags==4),'LEFT precondition changed; no unattributed DOWN/UP')
   inp=I.mouse(flags);inp.u.mi.dwExtraInfo=MARKER;arr=(I.INPUT*1)(inp)
   if manual_enter_deadline is not None and flags==2:
    require_manual_source_before_input(u,source['hwnd'],left_pressed=False)
    require(time.monotonic()<manual_enter_deadline,'manual OLE Enter budget expired before LEFT DOWN')
   require(int(u.SendInput(1,arr,ctypes.sizeof(I.INPUT)))==1,'partial/rejected owned SendInput')
   out['last_owned_input_abi_hex']=ctypes.string_at(ctypes.addressof(arr),ctypes.sizeof(I.INPUT)).hex()
   out['input_sent']=True
   if flags==2:out['possibly_held']='left'
   observation=dict(event=label,hwnd=f['hwnd'],client=[x,y],marker=MARKER,left_before=before,left_after=None,left_before_raw=before_raw,left_after_raw=None,right_after_raw=None,swap_buttons=int(u.GetSystemMetrics(23)))
   out['observations'].append(observation)
   until=time.monotonic()+2
   while time.monotonic()<until:
    raw=int(u.GetAsyncKeyState(1))&0xffff;after=bool(raw&0x8000);observation['left_after']=after;observation['left_after_raw']=raw;observation['right_after_raw']=int(u.GetAsyncKeyState(2))&0xffff
    if after==(flags==2):break
    time.sleep(.005)
   require(observation['left_after']==(flags==2),'accepted button event did not reach expected async state')
   if flags==4:out['possibly_held']=None
  # Reject a covered destination before holding LEFT, not only after OLE starts.
  owned_point(target,*plan['point']);owned_point(target,*plan['over_point'])
  sx,sy=source['client_width']//2,source['client_height']//2
  position(source,sx,sy,'source-position');button(source,sx,sy,2,'left-down')
  position(source,sx+32,sy,'start-native-drag')
  started=wait_json('ole-source-started.json',8,manual_enter_deadline);require(started['identity']['pid']==a.source_pid and started['marked_left_down'] and started['offered_format']=='FileDrop' and started['allowed_effect']=='Copy','source did not start owned native file drag')
  native=wait_json('ole-source-native-ready.json',8,manual_enter_deadline)
  require(native['pid']==a.source_pid and native['nonce']==a.nonce and native['query']==1 and native['key_state']&1 and not native['escape_pressed'],'native source query did not confirm original held LEFT gesture')
  tx,ty=plan['point'];position(target,tx,ty,'target-enter')
  enter=wait_json('ole-target-enter.ready.json',deadline=manual_enter_deadline);require(enter['pid']==a.target_pid and enter['nonce']==a.nonce,'target enter ACK foreign')
  manual_enter_deadline=None
  ox,oy=plan['over_point'];position(target,ox,oy,'target-over')
  over=wait_json('ole-target-over.ready.json');require(over['pid']==a.target_pid and over['nonce']==a.nonce,'target over ACK foreign')
  if a.close_while_dragging:
   family();owned_point(target,ox,oy)
   require(u.GetAsyncKeyState(1)&0x8000,'original LEFT no longer held before arm')
   publish_marker(a.control,'ole-source-cancel-on-release.request',f'ole-arm-cancel:{a.source_pid}:{a.nonce}')
   armed=wait_json('ole-source-cancel-armed.json',8)
   require(armed==dict(pid=a.source_pid,nonce=a.nonce,in_drag=True) and armed['in_drag'] is True,'foreign OLE Cancel arm receipt')
   family();owned_point(target,ox,oy)
   require(u.GetAsyncKeyState(1)&0x8000,'original LEFT no longer held before requesting Close')
   publish_marker(a.control,'ole-target-close.request',f'ole-close:{a.target_pid}:{a.nonce}')
   closed=wait_json('ole-target-close.ready.json',12)
   validate_active_close_receipt(plan,closed,a.target_pid,a.nonce)
   out['active_close']=closed
   # The original target HWND is still alive; the same point and source must
   # qualify again. Do not release elsewhere if the source stopped early.
   button(target,ox,oy,4,'left-up-after-close')
   result=wait_json('ole-source-result.json');validate_cancelled_source(result,a.source_pid,a.nonce)
   require(not (a.control/'ole-target-drop.ready.json').exists(),'Drop was reported during the cancellation case')
   out['source_result']=result
   publish_marker(a.control,'ole-cancel-verified.ack',f'ole-cancel:{a.target_pid}:{a.nonce}')
  else:
   button(target,ox,oy,4,'left-up')
   drop=wait_json('ole-target-drop.ready.json');require(drop['pid']==a.target_pid and drop['nonce']==a.nonce,'target drop ACK foreign')
   result=wait_json('ole-source-result.json');require(result['pid']==a.source_pid and result['nonce']==a.nonce and result['effect']==1 and result['error'] is None and not result['aborted'] and result['queries']>0 and result['physical_source'] and result['marked_left_down'] and result['terminal_action']=='Drop' and result['terminal_key_state']&1==0,'native source did not complete COPY')
   out['source_result']=result
   # Finite trusted-fixture completion fence; not OS identity authentication.
   publish_marker(a.control,'ole-copy-verified.ack',f'ole-copy:{a.target_pid}:{a.nonce}')
  out['success']=True
 except Exception as e:
  out['success']=False;out['error']=type(e).__name__+': '+str(e)
  if a.manual_source_focus_approved and not out['input_sent'] and a.control.is_absolute() and a.target_pid>0 and a.nonce>0:
   try:
    publish_marker(a.control,'ole-driver-manual-failed.request',f'ole-manual-failed:{a.target_pid}:{a.nonce}')
   except Exception as cleanup:out['manual_failure_marker_error']=type(cleanup).__name__+': '+str(cleanup)
  # Ask ONLY our original source to stop. Its UI timer/actual OLE query owns cancellation.
  try:
   publish_source_stop(a.control,a.source_pid,a.nonce)
   out['source_stop_requested']=True
  except Exception as cleanup:out['source_stop_error']=type(cleanup).__name__+': '+str(cleanup)
 # Never synthesize a foreign LEFT-UP in an error/finally block.
 with (a.control/'ole-driver-result.json').open('x',encoding='utf-8') as f:json.dump(out,f,ensure_ascii=False,indent=2)
 print(json.dumps({k:out[k] for k in ['success','error','foreground_requested','foreground_attempts','input_sent','possibly_held']}));return 0 if out['success'] else 1
if __name__=='__main__':
 p=argparse.ArgumentParser(allow_abbrev=False);p.add_argument('--control',type=Path,required=True);p.add_argument('--source-pid',type=int,required=True);p.add_argument('--target-pid',type=int,required=True);p.add_argument('--nonce',type=int,required=True);p.add_argument('--source-executable',type=Path,required=True);p.add_argument('--target-executable',type=Path,required=True);p.add_argument('--ole-input-approved',action='store_true');p.add_argument('--close-while-dragging',action='store_true');focus=p.add_mutually_exclusive_group();focus.add_argument('--ordinary-source-foreground-approved',action='store_true');focus.add_argument('--manual-source-focus-approved',action='store_true');sys.exit(run(p.parse_args()))
