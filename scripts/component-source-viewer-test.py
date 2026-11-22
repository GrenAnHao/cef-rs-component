"""Owned source-viewer desktop witness, not OCR/full-overlay isolation.
No input, focus request, move/resize, foreign title/path or clipboard access.
Only PID/thread/property-qualified original/viewer clients may be captured.
"""
from __future__ import annotations
import argparse
import ctypes
import ctypes.wintypes as wt
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
from PIL import ImageGrab

spec=importlib.util.spec_from_file_location("capture_host",Path(__file__).with_name("capture-component-host.py"))
cap=importlib.util.module_from_spec(spec)
spec.loader.exec_module(cap)

def require(ok,message):
    if not ok:
        raise cap.CaptureError(message)

def inspect(user,expected,pid):
    hwnd=expected["hwnd"]
    require(user.IsWindow(hwnd),"expected owned window no longer exists")
    owner=wt.DWORD()
    thread=int(user.GetWindowThreadProcessId(hwnd,ctypes.byref(owner)))
    require(owner.value==pid==expected["pid"] and thread==expected["thread"],"owned PID/thread/lifecycle qualification failed")
    require(user.GetAncestor(hwnd,2)==hwnd,"owned HWND is not its own root")
    require(user.GetPropW(hwnd,"GPUI_CEF_PtrPrb"),"owned pointer observer property missing")
    require(user.IsWindowVisible(hwnd) and not user.IsIconic(hwnd),"owned window hidden/iconic")
    rect=wt.RECT(); origin=wt.POINT(0,0)
    require(user.GetClientRect(hwnd,ctypes.byref(rect)) and user.ClientToScreen(hwnd,ctypes.byref(origin)),"owned client geometry unavailable")
    dpi=int(user.GetDpiForWindow(hwnd)); width=rect.right-rect.left; height=rect.bottom-rect.top
    require([dpi,origin.x,origin.y,width,height]==[expected[k] for k in ("dpi","left","top","width","height")],"owned geometry/DPI changed")
    name=ctypes.create_unicode_buffer(256)
    require(user.GetClassNameW(hwnd,name,256)>0,"owned class unavailable")
    return {"hwnd":hwnd,"owner_pid":int(owner.value),"thread":thread,"dpi":dpi,"dpi_scale":dpi/96.,"class":name.value,"probe_property_present":True,"visible":True,"iconic":False,"client_left":origin.x,"client_top":origin.y,"client_width":width,"client_height":height,"client_right":origin.x+width,"client_bottom":origin.y+height}

def grid(state):
    return [{"name":f"grid-{x}-{y}","client_px":[int(state["client_width"]*x/6),int(state["client_height"]*y/6)]} for x in (1,3,5) for y in (1,3,5)]

def capture(user,expected,pid,specs,case,name,owned_set,record):
    before=inspect(user,expected,pid); hwnd=before["hwnd"]
    virtual=cap.virtual_screen(user)
    require(before["client_left"]>=virtual["left"] and before["client_top"]>=virtual["top"] and before["client_right"]<=virtual["left"]+virtual["width"] and before["client_bottom"]<=virtual["top"]+virtual["height"],"owned client is outside virtual screen")
    points=grid(before)+specs
    pre=cap.run_probes(user,hwnd,before,points)
    record["before"]=before; record["probes_before"]=pre
    require(all(p["owned"] for p in pre),"another window covers the owned source/original client before capture")
    bbox=(before["client_left"],before["client_top"],before["client_right"],before["client_bottom"])
    image=ImageGrab.grab(bbox=bbox,all_screens=True).convert("RGB")
    after=inspect(user,expected,pid)
    post=cap.run_probes(user,hwnd,after,points)
    record["after"]=after;record["probes_after"]=post
    require(before==after and all(p["owned"] for p in post),"owned window changed or became occluded during capture")
    current=sorted(cap.enum_visible_top_level_windows(user,pid))
    require(current==owned_set,"owned visible-window set changed during capture")
    path=case/name
    require(not path.exists(),"refuse PNG overwrite")
    with path.open("xb") as stream:
        image.save(stream,format="PNG")
    record["capture"]={"path":path.name,"bbox":list(bbox),"size":list(image.size),"sha256":hashlib.sha256(path.read_bytes()).hexdigest(),"rgb_sha256":hashlib.sha256(image.tobytes()).hexdigest()}
    return image

def run(args):
    out={"schema":1,"kind":"source-viewer-owned-window-witness","stage":args.stage,"pid":args.pid,"success":False,"error":None,"input_sent":False,"foreground_requested":False,"window_moved":False,"atomic_capture":False,"ocr_or_exact_glyph_recognition":False,"transparent_or_unsampled_overlay_isolation":False}
    try:
        case=args.case_dir
        meta=json.loads((case/f"source-viewer-stage-{args.stage}.json").read_text(encoding="utf-8-sig"))
        require(meta["pid"]==args.pid and meta["stage"]==args.stage,"source viewer stage identity mismatch")
        user=cap.load_win32();cap.set_thread_dpi_awareness(user)
        user.GetPropW.argtypes=[wt.HWND,wt.LPCWSTR];user.GetPropW.restype=ctypes.c_void_p
        user.GetClassNameW.argtypes=[wt.HWND,wt.LPWSTR,ctypes.c_int];user.GetClassNameW.restype=ctypes.c_int
        windows=sorted(cap.enum_visible_top_level_windows(user,args.pid));out["owned_windows"]=windows
        original=inspect(user,meta["original"],args.pid);out["hwnd"]=original["hwnd"]
        if args.stage==0:
            viewer=inspect(user,meta["viewer"],args.pid)
            require(windows==sorted([original["hwnd"],viewer["hwnd"]]) and len(set(windows))==2,"expected two distinct owned native windows")
            require(original["class"]==viewer["class"] and original["thread"]==viewer["thread"],"source viewer is not on the original GPUI owner thread/class")
            require(meta["source_bytes"]==392 and meta["source_hash"]==14904330143978622504 and meta["source_witness"] and meta["renders"]>0 and meta["native_hold_released"] and meta["host_hold_live"],"original-frame source data/lifetime witness missing")
            require(meta["source_ui"]["created"]==1 and meta["source_ui"]["acknowledged"]==0 and meta["source_ui"]["first_error"] is None,"source viewer was prematurely acknowledged or faulted")
            scale=viewer["dpi_scale"]
            # Only these deterministic client pixels are interpreted. Every one
            # is qualified by WindowFromPoint/root both before and after grab.
            samples=[{"name":f"text-{x}-{y}","client_px":[x,y]} for y in range(round(54*scale),round(80*scale),4) for x in range(round(12*scale),viewer["client_width"]-round(12*scale),8)]
            background=[{"name":f"background-{x}","client_px":[int(viewer["client_width"]*x/4),int(viewer["client_height"]*.8)]} for x in (1,2,3)]
            record={};out["viewer"]=record
            image=capture(user,meta["viewer"],args.pid,samples+background,case,"source-viewer-window-0.png",windows,record)
            lit=sum(all(v>=140 for v in image.getpixel(tuple(p["client_px"]))) for p in samples)
            out["sampled_text_light_pixels"]=lit
            require(lit>=20,"source plaintext display has no sufficient glyph-shaped bright pixels")
            require(all(image.getpixel(tuple(p["client_px"]))==(23,27,34) for p in background),"source read-only UI background mismatch")
            out["viewer_hwnd"]=viewer["hwnd"]
            original_probes=cap.run_probes(user,original["hwnd"],original,grid(original));out["original_probes"]=original_probes
            require(all(p["owned"] for p in original_probes),"another window covers original client")
        else:
            prior=json.loads((case/"source-viewer-window-0.json").read_text(encoding="utf-8-sig"))
            require(prior["success"] and prior["pid"]==args.pid and meta["former_viewer"]["hwnd"]==prior["viewer_hwnd"],"source retirement chain missing")
            former=prior["viewer_hwnd"]
            # Reuse is NOT treated as success. Do not inspect a potentially
            # foreign recycled HWND or capture its former desktop region.
            require(not user.IsWindow(former),"former source HWND still exists or has been reused")
            require(windows==[original["hwnd"]] and original["hwnd"]==prior["hwnd"],"original owner did not survive source Entity release")
            require(meta["weak_entity_released"] and meta["lease_retired"] and meta["automatic_view_release_cleanup"] and not meta["source_dispatch_used_after_owner_drop"] and meta["host_alive"],"original-View release did not drive automatic viewer cleanup")
            ui=meta["source_ui"]
            require(ui["retired"] and ui["operations"]==ui["windows"]==0 and ui["created"]==ui["window_closed"]==ui["entities_released"]==ui["payloads_retired"]==ui["acknowledged"]==1 and ui["first_error"] is None and ui["outcome"]=="Some(Cancelled)","actual viewer/payload retirement accounting mismatch")
            scale=original["dpi_scale"]
            specs=[{"name":f"green-{x}-{y}","client_px":[round(x*scale),round((48+y)*scale)]} for x in (80,160,240) for y in (60,120,180)]
            header={"name":"host-header","client_px":[round((600-20)*scale),round(24*scale)]}
            record={};out["original"]=record
            image=capture(user,meta["original"],args.pid,specs+[header],case,"source-viewer-window-1.png",windows,record)
            require(all(image.getpixel(tuple(p["client_px"]))==(0,170,68) for p in specs),"original host did not repaint its retired CEF surface green")
            require(image.getpixel(tuple(header["client_px"]))==(159,34,34),"original host chrome changed")
            out["former_viewer_hwnd"]=former;out["former_viewer_is_window"]=False
        out["success"]=True
    except Exception as error:
        out["error"]=f"{type(error).__name__}: {error}"
    path=args.case_dir/f"source-viewer-window-{args.stage}.json"
    with path.open("x",encoding="utf-8") as stream:
        json.dump(out,stream,ensure_ascii=False,indent=2)
    print(json.dumps({k:out.get(k) for k in ("stage","pid","success","error")},ensure_ascii=False))
    return 0 if out["success"] else 1

if __name__=="__main__":
    parser=argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--pid",type=int,required=True)
    parser.add_argument("--case-dir",type=Path,required=True)
    parser.add_argument("--stage",type=int,choices=(0,1),required=True)
    args=parser.parse_args()
    if args.pid<=0:parser.error("positive owned PID required")
    sys.exit(run(args))
