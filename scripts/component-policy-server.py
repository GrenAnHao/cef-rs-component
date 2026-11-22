#!/usr/bin/env python3
"""8.3 policy fixture. Fixed origins, fixed labels; never logs URLs, query, body or input."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import ssl
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

BODY_LIMIT = HEADER_LIMIT = 16384
CASE_RE = re.compile(r"^[a-z0-9]{1,64}$")
MAX_CASES, MAX_RECORDS, MAX_REQUESTS = 128, 256, 512
MAX_CONTROL_REQUESTS, MAX_ERRORS, MAX_THREADS = 4096, 64, 64
SOCKET_TIMEOUT = 2.0
ROLES = ("allowed", "forbidden", "tls")
ACT = ("is_trusted", "has_been_active", "is_active")
NETWORK = ("attempted", "popupAttempted", "externalAttempted", "fetchAttempted",
           "iframeAttempted", "imageAttempted", "redirectAttempted", "downloadAttempted",
           "mainAttempted", "fetchRejected", "redirectRejected", "imageRejected")
GEO = {"granted", "denied", "unavailable", "timeout", "error", "unsupported"}
MEDIA = {"granted", "denied", "unavailable", "notreadable", "error", "unsupported"}
NOTIFY = {"granted", "denied", "default", "error", "unsupported"}
WORKER_REASONS = {"unsupported", "registration_failed", "no_active", "timeout", "other"}
ROUTE_KEYS = ("data_target", "data_attempt", "download", "favicon", "health", "page", "plain", "record",
              "redirect", "register", "state", "stop", "sw_js", "unknown", "unload")
ROUTE_BY_PATH = {"/data-target": "data_target", "/data-attempt": "data_attempt", "/download": "download",
                 "/favicon.ico": "favicon", "/health": "health", "/page": "page",
                 "/plain": "plain", "/record": "record", "/redirect": "redirect",
                 "/register": "register", "/state": "state", "/stop": "stop",
                 "/sw.js": "sw_js", "/unload": "unload"}
GET_PAGES = ("/page", "/unload", "/plain", "/sw.js", "/redirect", "/download", "/data-attempt")
KNOWN_PATHS = GET_PAGES + ("/health", "/state", "/data-target", "/register", "/record", "/stop")

def atomic_json(path, data):
    path = Path(path)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(data, separators=(",", ":")), encoding="utf-8")
    os.replace(tmp, path)

def bools(obj, keys):
    return all(type(obj[key]) is bool for key in keys)

def build_rules():
    """Exact labels with exact scalar fields; anything else is a fixed error."""
    flags = {"activation": ACT, "network_done": NETWORK,
             "js_dialogs": ("confirm", "prompt_null"),
             "before_unload": ("has_been_active", "is_trusted"),
             "tls_rejected": ("attempted", "rejected"),
             "worker": ("attempted", "blocked"), "main_attempt": ("attempted",),
             "data_attempt": ("attempted",), "data_attempt_survived": ("survived",)}
    rules = {label: (("label",) + keys, lambda o, k=keys: bools(o, k))
             for label, keys in flags.items()}
    rules["click"] = (("label", "step") + ACT, lambda o: type(o["step"]) is int
                      and 1 <= o["step"] <= 4 and bools(o, ACT))
    for label, statuses in (("geolocation", GEO), ("media", MEDIA),
                            ("notification", NOTIFY)):
        rules[label] = (("label", "attempted", "supported", "status"),
                        lambda o, s=statuses: bools(o, ("attempted", "supported"))
                        and o["status"] in s)
    rules["worker_error"] = (("label", "reason"), lambda o: o["reason"] in WORKER_REASONS)
    for label in ("data_loaded", "file_attempt", "file_change", "file_cancel",
                  "unload_armed"):
        rules[label] = (("label",), lambda o: True)
    return rules
RECORD_RULES = build_rules()

def normalize_record(obj):
    if not isinstance(obj, dict):
        return None, "record-type"
    label = obj.get("label")
    rule = RECORD_RULES.get(label) if isinstance(label, str) else None
    if rule is None:
        return None, "record-label"
    keys, check = rule
    if set(obj) != set(keys) or not check(obj):
        return None, "record-fields"
    return obj, None

def limit_error(errors, category):
    if len(errors) < MAX_ERRORS:
        errors.append(category)
    elif errors[-1] != "additional-errors-omitted":
        errors.append("additional-errors-omitted")

class Core:
    def __init__(self):
        self.lock = threading.RLock()
        self.cases, self.errors = {}, []
        self.totals = {role + "_http": 0 for role in ROLES}
        self.origins, self.stop_event = {}, threading.Event()
    def error(self, category, case_id=None):
        with self.lock:
            case = self.cases.get(case_id) if case_id else None
            limit_error(case["errors"] if case else self.errors, category)
    def register(self, case_id, data_uri=None):
        with self.lock:
            case = self.cases.get(case_id)
            if case is None:
                if len(self.cases) >= MAX_CASES:
                    limit_error(self.errors, "case-limit")
                    return False
                case = self.cases[case_id] = {
                    "records": [], "errors": [], "requests": 0, "data_uri": None, "data_revision": 0,
                    "routes": {key: 0 for key in ROUTE_KEYS},
                    "counters": {role + "_http": 0 for role in ROLES}}
            if data_uri is not None:
                case["data_uri"] = data_uri
                case["data_revision"] += 1
            return True
    def note(self, role, case_id, route, control=False):
        """Count every request; /state has a separate budget and is never page traffic."""
        with self.lock:
            self.totals[role + "_http"] += 1
            case = self.cases.get(case_id) if case_id else None
            if case is None:
                return True
            if control:
                route = route or "state"
                if case["routes"][route] >= MAX_CONTROL_REQUESTS:
                    return False
                case["routes"][route] += 1
                return True
            case["counters"][role + "_http"] += 1
            if role != "allowed":
                return True  # forbidden/TLS traffic is evidence, not a page budget
            case["routes"][route or "unknown"] += 1
            if case["requests"] >= MAX_REQUESTS:
                if "request-limit" not in case["errors"]:
                    limit_error(case["errors"], "request-limit")
                return False
            case["requests"] += 1
            return True
    def add_record(self, case_id, record):
        with self.lock:
            case = self.cases[case_id]
            if len(case["records"]) >= MAX_RECORDS:
                if "record-limit" not in case["errors"]:
                    limit_error(case["errors"], "record-limit")
                return False
            case["records"].append(record)
            return True
    def state(self, case_id):
        with self.lock:
            case = self.cases[case_id]
            return {"records": [dict(r) for r in case["records"]],
                    "counts": dict(case["routes"]),
                    "forbidden_requests": case["counters"]["forbidden_http"],
                    "tls_requests": case["counters"]["tls_http"],
                    "errors": list(case["errors"])}
    def report(self, stopped=False):
        with self.lock:
            return {"ok": not self.errors, "pid": os.getpid(), "stopped": bool(stopped),
                    "cases": len(self.cases),
                    "records": sum(len(c["records"]) for c in self.cases.values()),
                    "counters": dict(self.totals), "origins": dict(self.origins),
                    "errors": list(self.errors)}

class Server(ThreadingHTTPServer):
    daemon_threads = False
    block_on_close = True
    allow_reuse_address = True
    request_queue_size = 64
    def __init__(self, role, core, tls_context=None):
        self.role, self.core, self.tls_context = role, core, tls_context
        self.slots = threading.BoundedSemaphore(MAX_THREADS)
        super().__init__(("127.0.0.1", 0), Handler)
        self.origin = "%s://127.0.0.1:%d" % (
            "https" if role == "tls" else "http", self.server_port)
    def get_request(self):
        connection, address = super().get_request()
        connection.settimeout(SOCKET_TIMEOUT)
        return connection, address
    def process_request(self, request, client_address):
        if not self.slots.acquire(blocking=False):
            self.core.error("request-capacity")
            return self.shutdown_request(request)
        try:
            super().process_request(request, client_address)
        except BaseException:
            self.slots.release()
            raise
    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self.slots.release()
    def finish_request(self, request, client_address):
        if self.tls_context is None:
            return super().finish_request(request, client_address)
        try:
            request.settimeout(SOCKET_TIMEOUT)
            tls_socket = self.tls_context.wrap_socket(request, server_side=True)
        except (ssl.SSLError, OSError):
            return  # Certificate rejection and transport cancellation are expected.
        try:
            super().finish_request(tls_socket, client_address)
        finally:
            self.shutdown_request(tls_socket)
    def handle_error(self, request, client_address):
        error_type = sys.exc_info()[0]
        if error_type is None or not issubclass(error_type, OSError):
            self.core.error("internal-handler-error")

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *args):
        pass
    def handle_one_request(self):
        self._counted = self._limit_hit = self._control_limit = False
        self._route = "unknown"
        original = self.rfile
        self.rfile = LimitedHeaders(original)
        try:
            super().handle_one_request()
        except ValueError:
            self.count_request()
            if self.server.role != "forbidden":
                self.server.core.error("headers-too-large")
            self.close_connection = True
        except OSError:
            self.close_connection = True
        finally:
            self.rfile = original
    def parse_request(self):
        ok = super().parse_request()
        try:
            self._route = ROUTE_BY_PATH.get(urlsplit(self.path).path, "unknown")
        except (AttributeError, ValueError):
            self._route = "unknown"
        self.count_request()
        return ok
    def count_request(self):
        if getattr(self, "_counted", False):
            return
        self._counted = True
        route = self._route
        case_id, control = self.case_id(), route in ("state", "data_target")
        if route in ("register", "stop") and self.server.role == "allowed":
            case_id = None  # control endpoints are not page traffic
        if not self.server.core.note(self.server.role, case_id, route, control):
            if control:
                self._control_limit = True
            else:
                self._limit_hit = True
    def case_id(self):
        try:
            values = parse_qs(urlsplit(getattr(self, "path", "")).query,
                              keep_blank_values=True).get("case", [])
        except ValueError:
            return None
        return values[0] if len(values) == 1 and CASE_RE.match(values[0]) else None
    def known_case(self, case_id):
        return case_id is not None and case_id in self.server.core.cases
    def respond(self, status, obj=None, content_type="application/json", headers=None):
        body = obj or b""
        if isinstance(obj, str):
            body = obj.encode("utf-8")
        elif obj is not None and not isinstance(obj, bytes):
            body = json.dumps(obj, separators=(",", ":")).encode()
        headers = dict(headers or {})
        if self._route in ("record", "data_target"):
            headers.setdefault("Access-Control-Allow-Origin", "*")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        for key, value in headers.items():
            self.send_header(key, value)
        self.end_headers()
        self.close_connection = True
        if body:
            self.wfile.write(body)
        return True  # sentinel: every fail()/respond() call returns handled=True
    def send_error(self, code, message=None, explain=None):
        self.count_request()
        if self.server.role == "forbidden":
            return self.respond(200, b"forbidden-target", "text/plain")
        category = "http-error-%d" % int(code)
        self.server.core.error(category, self.case_id())
        self.respond(int(code), {"error": category})
    def fail(self, case_id, category, status=400):
        self.server.core.error(category, case_id)
        self.respond(status, {"error": category})
    def read_json(self):
        length = self.headers.get("Content-Length")
        if (self.headers.get("Transfer-Encoding") or not length or not length.isdigit()
                or int(length) > BODY_LIMIT):
            raise ValueError("body-length")
        raw = self.rfile.read(int(length))
        if len(raw) != int(length):
            raise ValueError("body-truncated")
        try:
            value = json.loads(raw)
        except (ValueError, UnicodeError):
            raise ValueError("body-json") from None
        if not isinstance(value, dict):
            raise ValueError("body-type")
        return value
    def body(self, case_id):
        try:
            return self.read_json(), None
        except ValueError as exc:
            category = str(exc)
            if category in ("body-truncated", "body-read"):
                # Navigation can cancel a keepalive body; transport noise only.
                self.respond(400, {"error": category})
            else:
                self.fail(case_id, category)
            return None, True
        except OSError:
            self.respond(400, {"error": "body-read"})
            return None, True
    def do_GET(self):
        self.dispatch("GET")
    def do_POST(self):
        self.dispatch("POST")
    def do_OPTIONS(self):
        self.dispatch("OPTIONS")
    def dispatch(self, method):
        try:
            path = urlsplit(self.path).path
        except ValueError:
            return self.fail(None, "invalid-path")
        case_id = self.case_id()
        if self.server.role == "forbidden":
            # Every forbidden request was counted above, unknown paths included.
            return self.respond(200, b"forbidden-target", "text/plain")
        if self.server.role == "tls":
            if method == "GET" and path == "/certificate" and self.known_case(case_id):
                return self.respond(200, {"ok": True})
            return self.fail(case_id, "unknown-path", 404)
        if path == "/favicon.ico":
            return self.respond(204)
        if method == "GET" and path == "/health":
            return self.respond(200, self.server.core.report())
        if method == "POST" and path == "/stop":
            self.respond(200, {"stopping": True})
            self.server.core.stop_event.set()
            return
        if method == "POST" and path == "/register":
            return self.register(case_id)
        if method == "GET" and path == "/state":
            return (self.respond(429, {"error": "control-limit"})
                    if self._control_limit else self.state(case_id))
        if method == "GET" and path == "/data-target":
            if self._control_limit:
                return self.respond(429, {"error": "control-limit"})
            if self.require_case(case_id):
                return True
            with self.server.core.lock:
                case = self.server.core.cases[case_id]
                target = {"uri": case["data_uri"], "revision": case["data_revision"]}
            return self.respond(200, target)
        if self._limit_hit:
            return self.fail(case_id, "request-limit", 429)
        if method == "OPTIONS" and path == "/record":
            return self.respond(204, None, headers={
                "Access-Control-Allow-Methods": "POST, OPTIONS",
                "Access-Control-Allow-Headers": "Content-Type",
                "Access-Control-Max-Age": "600"})
        if method == "POST" and path == "/record":
            return self.record(case_id)
        if method == "GET" and path in GET_PAGES:
            return self.get_route(path, case_id)
        return self.fail(case_id, "method-not-allowed" if path in KNOWN_PATHS
                         else "unknown-path", 405 if path in KNOWN_PATHS else 404)
    def register(self, case_id):
        obj, error = self.body(case_id)
        if error:
            return True
        if set(obj) not in ({"case"}, {"case", "data_uri"}):
            return self.fail(case_id, "invalid-fields")
        new_case = obj.get("case")
        if not isinstance(new_case, str) or not CASE_RE.match(new_case):
            return self.fail(case_id, "invalid-case")
        data_uri = obj.get("data_uri")
        if "data_uri" in obj and (not isinstance(data_uri, str)
                                  or not data_uri.startswith("data:")
                                  or len(data_uri) > BODY_LIMIT):
            return self.fail(case_id, "invalid-data-uri")
        if not self.server.core.register(new_case, data_uri):
            return self.fail(case_id, "case-limit", 429)
        origins = self.server.core.origins
        return self.respond(200, {"allowed_origin": origins["allowed"],
                                  "forbidden_origin": origins["forbidden"],
                                  "tls_origin": origins["tls"]})
    def require_case(self, case_id, sticky=True):
        if not CASE_RE.match(case_id or ""):
            return self.fail(None, "invalid-case")
        if not self.known_case(case_id):
            if not sticky:
                return self.respond(404, {"error": "unknown-case"})
            return self.fail(None, "unknown-case", 404)
        return None
    def state(self, case_id):
        if self.require_case(case_id, sticky=False):
            return True
        return self.respond(200, self.server.core.state(case_id))
    def record(self, case_id):
        if self.require_case(case_id):
            return True
        obj, error = self.body(case_id)
        if error:
            return True
        record, category = normalize_record(obj)
        if record is None:
            return self.fail(case_id, category)
        if not self.server.core.add_record(case_id, record):
            return self.fail(case_id, "record-limit", 429)
        return self.respond(200, {"ok": True})
    def get_route(self, path, case_id):
        if not self.known_case(case_id):
            return self.fail(case_id, "unknown-case", 404)
        origins = self.server.core.origins
        if path == "/sw.js":
            return self.respond(200, sw_script(case_id, origins), "application/javascript",
                                {"Service-Worker-Allowed": "/"})
        if path == "/data-attempt":
            data_uri = self.server.core.cases[case_id]["data_uri"]
            if data_uri is None:
                return self.fail(case_id, "no-data-uri", 404)
            html = data_html(case_id, origins, data_uri)
        elif path == "/redirect":
            target = origins["forbidden"] + "/redirect-target?case=" + case_id
            return self.respond(302, None, "text/plain", {"Location": target})
        elif path == "/download":
            return self.respond(200, b"policy-probe\n", "application/octet-stream",
                                {"Content-Disposition": 'attachment; filename="policy-probe.bin"'})
        else:
            title, js = {"/page": ("Policy Probe", PAGE_JS),
                         "/unload": ("Unload Probe", UNLOAD_JS),
                         "/plain": ("Plain Probe", PLAIN_JS)}[path]
            html = page_html(case_id, origins, title, js)
        return self.respond(200, html, "text/html; charset=utf-8")

class LimitedHeaders:
    """Count request line plus headers, then pass body reads through unchanged."""
    def __init__(self, wrapped):
        self.wrapped, self.remaining, self.headers_done = wrapped, HEADER_LIMIT, False
    def readline(self, size=-1):
        if self.headers_done:
            return self.wrapped.readline(size)
        line = self.wrapped.readline(min(size if size >= 0 else HEADER_LIMIT + 1,
                                         self.remaining + 1))
        self.remaining -= len(line)
        if self.remaining < 0:
            raise ValueError("headers-too-large")
        if line in (b"\r\n", b"\n"):
            self.headers_done = True
        return line
    def read(self, size=-1):
        return self.wrapped.read(size)
STYLE = """<style>html,body{margin:0;width:320px;height:240px;background:#1a4c7e;overflow:hidden}
body{position:relative}#activate{position:absolute;left:120px;top:90px;width:80px;height:60px}</style>"""
BUTTON = ('<button id="activate" style="position:absolute;left:120px;top:90px;'
          'width:80px;height:60px">Activate</button>')
COMMON = """const C=%(case)s,A=%(allowed)s,F=%(forbidden)s,T=%(tls)s;
const rec=o=>fetch('/record?case='+encodeURIComponent(C),{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(o),keepalive:true,cache:'no-store'}).catch(()=>{});
const act=()=>{const u=navigator.userActivation||{};return {has_been_active:!!u.hasBeenActive,is_active:!!u.isActive};};
const beacon=o=>{try{if(navigator.sendBeacon)navigator.sendBeacon('/record?case='+encodeURIComponent(C),new Blob([JSON.stringify(o)],{type:'application/json'}));}catch(e){}};
const unloadHandler=ev=>{const x=act();beacon({label:'before_unload',has_been_active:x.has_been_active,is_trusted:ev.isTrusted===true});ev.preventDefault();ev.returnValue='';};
"""
PAGE_JS = """(function(){
const btn=document.getElementById('activate');let step=0;
const file=document.createElement('input');file.type='file';file.style.cssText='position:absolute;left:-10000px;top:-10000px';document.body.appendChild(file);
file.addEventListener('change',()=>rec({label:'file_change'}));file.addEventListener('cancel',()=>rec({label:'file_cancel'}));
const clicked=e=>{const x=act();rec({label:'click',step:step,is_trusted:e.isTrusted===true,has_been_active:x.has_been_active,is_active:x.is_active});};
const link=(href,extra)=>{const a=document.createElement('a');a.href=href;a.style.cssText='position:absolute;left:-20px';Object.assign(a,extra||{});document.body.appendChild(a);a.click();};
function setupWorker(){
 if(!('serviceWorker' in navigator)){rec({label:'worker_error',reason:'unsupported'});return;}
 (async()=>{try{
  const regs=await navigator.serviceWorker.getRegistrations();
  await Promise.all(regs.filter(r=>r.scope===location.origin+'/').map(r=>r.unregister().catch(()=>false)));
  await navigator.serviceWorker.register('/sw.js?case='+encodeURIComponent(C),{scope:'/'});
  const reg=await Promise.race([navigator.serviceWorker.ready,new Promise((_,rej)=>setTimeout(()=>rej('timeout'),5000))]);
  const worker=reg.active||navigator.serviceWorker.controller;
  if(worker)worker.postMessage('probe');else rec({label:'worker_error',reason:'no_active'});
 }catch(e){rec({label:'worker_error',reason:e==='timeout'?'timeout':'registration_failed'});}})();
}
function networkPhase(){
 let fetchRejected=false,redirectRejected=false,imageRejected=false;
 try{window.open(F+'/popup?case='+encodeURIComponent(C),'_blank');}catch(e){}
 try{link('gpui-cef-denied://test');}catch(e){}
 fetch(F+'/fetch?case='+encodeURIComponent(C),{cache:'no-store',mode:'no-cors'}).catch(()=>{fetchRejected=true;});
 fetch(A+'/redirect?case='+encodeURIComponent(C),{cache:'no-store',mode:'no-cors'}).catch(()=>{redirectRejected=true;});
 try{const fr=document.createElement('iframe');fr.src=F+'/iframe?case='+encodeURIComponent(C);fr.style.cssText='position:absolute;left:-20px;width:1px;height:1px';document.body.appendChild(fr);}catch(e){}
 try{const im=new Image();im.onerror=()=>{imageRejected=true;};im.src=F+'/image?case='+encodeURIComponent(C);}catch(e){}
 try{link(A+'/download?case='+encodeURIComponent(C),{download:'policy-probe.bin'});}catch(e){}
 setupWorker();beacon({label:'main_attempt',attempted:true});
 setTimeout(()=>rec({label:'network_done',attempted:true,popupAttempted:true,externalAttempted:true,fetchAttempted:true,iframeAttempted:true,imageAttempted:true,redirectAttempted:true,downloadAttempted:true,mainAttempted:true,fetchRejected:fetchRejected,redirectRejected:redirectRejected,imageRejected:imageRejected}),1000);
 window.location.href=F+'/main?case='+encodeURIComponent(C);
}
const verdict=(label,statuses)=>(attempted,supported,status)=>rec({label:label,attempted:attempted,supported:supported,status:statuses.includes(status)?status:'error'});
const geo=verdict('geolocation',%(geo)s),media=verdict('media',%(media)s),notify=verdict('notification',%(notify)s);
function permissionPhase(){
 const g=navigator.geolocation;
 if(!g)geo(true,false,'unsupported');
 else try{g.getCurrentPosition(()=>geo(true,true,'granted'),e=>geo(true,true,e&&e.code===1?'denied':e&&e.code===2?'unavailable':e&&e.code===3?'timeout':'error'),{timeout:5000,maximumAge:0});}catch(e){geo(true,true,'error');}
 const md=navigator.mediaDevices;
 if(!md||!md.getUserMedia)media(true,false,'unsupported');
 else md.getUserMedia({audio:true}).then(s=>{s.getTracks().forEach(t=>t.stop());media(true,true,'granted');}).catch(e=>{const n=e&&e.name;media(true,true,n==='NotAllowedError'?'denied':n==='NotFoundError'?'unavailable':n==='NotReadableError'?'notreadable':'error');});
 if(!window.Notification||typeof Notification.requestPermission!=='function')notify(true,false,'unsupported');
 else{let done=false;const finish=st=>{if(!done){done=true;notify(true,true,st||'default');}};try{const p=Notification.requestPermission(finish);if(p&&p.then)p.then(finish).catch(()=>finish('error'));}catch(e){finish('error');}}
 const ac=typeof AbortController==='function'?new AbortController():null;
 const timer=ac?setTimeout(()=>ac.abort(),2500):null;
 fetch(T+'/certificate?case='+encodeURIComponent(C),{cache:'no-store',mode:'no-cors',signal:ac?ac.signal:undefined}).then(()=>{if(timer)clearTimeout(timer);rec({label:'tls_rejected',attempted:true,rejected:false});}).catch(()=>{if(timer)clearTimeout(timer);rec({label:'tls_rejected',attempted:true,rejected:!ac||!ac.signal.aborted});});
}
function dialogPhase(){
 let c=false,pn=true;
 try{alert('policy probe');}catch(e){}
 try{c=confirm('policy probe')===true;}catch(e){c=false;}
 try{pn=prompt('policy probe')===null;}catch(e){pn=true;}
 rec({label:'js_dialogs',confirm:c,prompt_null:pn});
 window.addEventListener('beforeunload',unloadHandler);rec({label:'unload_armed'});
}
btn.addEventListener('click',e=>{step++;if(step>4)return;clicked(e);
 if(step===1)networkPhase();
 else if(step===2){rec({label:'file_attempt'});try{file.click();}catch(err){}}
 else if(step===3)permissionPhase();
 else dialogPhase();});
})();""" % {"geo": json.dumps(sorted(GEO)), "media": json.dumps(sorted(MEDIA)),
              "notify": json.dumps(sorted(NOTIFY))}
UNLOAD_JS = """(function(){document.getElementById('activate').addEventListener('click',e=>{
const x=act();rec({label:'activation',is_trusted:e.isTrusted===true,has_been_active:x.has_been_active,is_active:x.is_active});
window.addEventListener('beforeunload',unloadHandler);rec({label:'unload_armed'});});})();"""
PLAIN_JS = """(function(){document.getElementById('activate').addEventListener('click',e=>{
const x=act();rec({label:'activation',is_trusted:e.isTrusted===true,has_been_active:x.has_been_active,is_active:x.is_active});});})();"""

def page_html(case_id, origins, title, js, button=True):
    common = COMMON % {"case": json.dumps(case_id), "allowed": json.dumps(origins["allowed"]),
                       "forbidden": json.dumps(origins["forbidden"]),
                       "tls": json.dumps(origins["tls"])}
    return ('<!doctype html><html><head><meta charset="utf-8">'
            '<meta name="viewport" content="width=320,height=240">'
            '<title>%s</title>%s</head><body>%s<script>%s%s</script></body></html>'
            % (title, STYLE, BUTTON if button else "", common, js))

def data_html(case_id, origins, data_uri):
    target = json.dumps(data_uri).replace("</", r"<\/")
    # A data: document has an opaque origin, so its witnesses must use the
    # absolute allowed origin; the served page itself stays on the HTTP origin.
    js = ("(function(){beacon({label:'data_attempt',attempted:true});"
          "setTimeout(()=>rec({label:'data_attempt_survived',survived:true}),300);"
          "window.location.href=%s;})();" % target)
    return page_html(case_id, origins, "Data Attempt", js, button=False)

def sw_script(case_id, origins):
    return """const C=%(case)s,A=%(allowed)s,F=%(forbidden)s;
self.addEventListener('install',e=>e.waitUntil(self.skipWaiting()));
self.addEventListener('activate',e=>e.waitUntil(self.clients.claim()));
self.addEventListener('message',e=>{if(e.data!=='probe')return;e.waitUntil((async()=>{
 let blocked=false;
 try{await fetch(F+'/worker?case='+encodeURIComponent(C),{cache:'no-store',mode:'no-cors'});}catch(err){blocked=true;}
 try{await fetch(A+'/record?case='+encodeURIComponent(C),{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({label:'worker',attempted:true,blocked:blocked})});}catch(err){}
})());});""" % {"case": json.dumps(case_id), "allowed": json.dumps(origins["allowed"]),
                "forbidden": json.dumps(origins["forbidden"])}

def serve_forever(server, core):
    try:
        server.serve_forever(poll_interval=0.1)
    except Exception:
        core.error("serve-loop-error")
    finally:
        if not core.stop_event.is_set():
            core.error("serve-loop-exit")
            core.stop_event.set()

def main():
    parser = argparse.ArgumentParser()
    for name in ("ready", "final", "cert", "key"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.load_cert_chain(certfile=str(args.cert), keyfile=str(args.key))
    core = Core()
    for number in (signal.SIGINT, signal.SIGTERM):
        signal.signal(number, lambda *_: core.stop_event.set())
    servers = [Server("allowed", core), Server("forbidden", core),
               Server("tls", core, context)]
    if len({server.server_port for server in servers}) != 3:
        raise RuntimeError("port collision")
    core.origins = {"allowed": servers[0].origin, "forbidden": servers[1].origin,
                    "tls": servers[2].origin}
    threads = []
    try:
        for server in servers:
            thread = threading.Thread(target=serve_forever, args=(server, core),
                                      daemon=False)
            thread.start()
            threads.append(thread)
        # Ready is published only once all three listeners are accepting.
        atomic_json(args.ready, {"pid": os.getpid(),
                                 "allowed_origin": servers[0].origin,
                                 "forbidden_origin": servers[1].origin,
                                 "tls_origin": servers[2].origin})
        core.stop_event.wait()
    finally:
        for server in servers:
            server.shutdown()
        for thread in threads:
            thread.join(timeout=5)
        for server in servers:
            try:
                server.server_close()
            except OSError:
                pass
        atomic_json(args.final, core.report(core.stop_event.is_set()))
if __name__ == "__main__":
    main()
