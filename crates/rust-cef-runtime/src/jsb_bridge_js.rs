//! Renderer-side V8 bootstrap script implementing `window['cef-rs-gpui']`
//! Object Repository (`BindObjectAsync`, `DeleteBoundObject`, `PostMessage`,
//! `boundAsync`, `boundAsync2`, and `bound`) on top of origin-gated `window.rustCef` IPC.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

pub(crate) fn bootstrap_js(legacy_binding: bool) -> String {
    let legacy_flag = if legacy_binding { "true" } else { "false" };
    format!(
        "if (typeof globalThis !== 'undefined' && globalThis.rustCef) {{\n\
         if (Object.freeze) {{ Object.freeze(globalThis.rustCef); }}\n\
         (function(legacyBinding) {{\n\
         {CEF_RS_GPUI_BOOTSTRAP_BODY_JS}\n\
         }})({legacy_flag});\n\
         }}"
    )
}

const CEF_RS_GPUI_BOOTSTRAP_BODY_JS: &str = r##"
if (globalThis.__cefRsGpuiInitialized) { return; }
globalThis.__cefRsGpuiInitialized = true;

var ctxId = Math.random().toString(36).slice(2) + '_' + Date.now().toString(36);
var nextReqId = 1;
var nextCbId = 1;
var pending = new Map();
var callbacks = new Map();
var boundSet = new Set();

function encodeVal(v) {
  if (v === undefined || v === null) return null;
  if (typeof v === 'function') {
    var cbId = 'cb_' + (nextCbId++);
    callbacks.set(cbId, v);
    return { __cefCbId: cbId };
  }
  if (v instanceof Date) {
    return { __cefDateMs: v.getTime() };
  }
  if (Array.isArray(v)) {
    return v.map(encodeVal);
  }
  if (typeof v === 'object') {
    var out = {};
    var keys = Object.keys(v);
    for (var i = 0; i < keys.length; i++) {
      var k = keys[i];
      out[k] = encodeVal(v[k]);
    }
    return out;
  }
  return v;
}

function decodeVal(v) {
  if (v === null || v === undefined || typeof v !== 'object') return v;
  if (Array.isArray(v)) {
    return v.map(decodeVal);
  }
  if (Object.prototype.hasOwnProperty.call(v, '__cefDateMs')) {
    return new Date(v.__cefDateMs);
  }
  if (Object.prototype.hasOwnProperty.call(v, '__cefLocalDate')) {
    var a = v.__cefLocalDate;
    return new Date(a[0], a[1] - 1, a[2], a[3], a[4], a[5]);
  }
  var out = {};
  var keys = Object.keys(v);
  for (var i = 0; i < keys.length; i++) {
    var k = keys[i];
    out[k] = decodeVal(v[k]);
  }
  return out;
}

function callHostRpc(target, method, rawArgs) {
  var id = nextReqId++;
  var encodedArgs = [];
  for (var i = 0; i < rawArgs.length; i++) {
    encodedArgs.push(encodeVal(rawArgs[i]));
  }
  return new Promise(function(resolve, reject) {
    pending.set(id, { resolve: resolve, reject: reject });
    try {
      globalThis.rustCef.postMessage(JSON.stringify({
        __cefKind: 'rpcCall',
        ctxId: ctxId,
        id: id,
        target: target,
        method: method,
        args: encodedArgs
      }));
    } catch (e) {
      pending.delete(id);
      reject(e);
    }
  });
}

globalThis.rustCef.onMessage(function(rawJson) {
  var msg;
  try {
    msg = JSON.parse(rawJson);
  } catch (_) {
    return;
  }
  if (!msg || typeof msg !== 'object') return;
  if (msg.ctxId && msg.ctxId !== ctxId) return;

  if (msg.__cefKind === 'rpcResult') {
    var entry = pending.get(msg.id);
    if (!entry) return;
    pending.delete(msg.id);
    if (msg.ok) {
      entry.resolve(decodeVal(msg.result));
    } else {
      entry.reject(new Error(String(msg.error || 'Host RPC error')));
    }
    return;
  }

  if (msg.__cefKind === 'invokeCallback') {
    var fn = callbacks.get(msg.cbId);
    if (msg.dispose) {
      callbacks.delete(msg.cbId);
    }
    if (typeof fn !== 'function') {
      if (msg.replyId) {
        globalThis.rustCef.postMessage(JSON.stringify({
          __cefKind: 'cbResult',
          ctxId: ctxId,
          replyId: msg.replyId,
          ok: false,
          error: 'Callback is unavailable'
        }));
      }
      return;
    }
    var args = Array.isArray(msg.args) ? msg.args.map(decodeVal) : [];
    try {
      var ret = fn.apply(undefined, args);
      if (msg.replyId) {
        Promise.resolve(ret).then(function(resolved) {
          globalThis.rustCef.postMessage(JSON.stringify({
            __cefKind: 'cbResult',
            ctxId: ctxId,
            replyId: msg.replyId,
            ok: true,
            result: encodeVal(resolved)
          }));
        }, function(err) {
          globalThis.rustCef.postMessage(JSON.stringify({
            __cefKind: 'cbResult',
            ctxId: ctxId,
            replyId: msg.replyId,
            ok: false,
            error: String(err && err.message ? err.message : err)
          }));
        });
      }
    } catch (err) {
      if (msg.replyId) {
        globalThis.rustCef.postMessage(JSON.stringify({
          __cefKind: 'cbResult',
          ctxId: ctxId,
          replyId: msg.replyId,
          ok: false,
          error: String(err && err.message ? err.message : err)
        }));
      }
    }
    return;
  }

  if (msg.__cefKind === 'postMessageInt') {
    try {
      (0, eval)(
        'if (typeof PostMessageIntTestCallback === "function") { PostMessageIntTestCallback(' +
        JSON.stringify(msg.value) +
        '); }'
      );
    } catch (_) {}
  }
});

var ASYNC_METHODS = [
  'error', 'div', 'divWithBlockingTaskCall', 'uIntAddModel', 'uIntAdd',
  'hello', 'doSomething', 'returnObject', 'returnClass', 'returnClassAsJsonString',
  'returnStructArray', 'returnClassesArray', 'echoDateTime', 'echoNullableDateTime',
  'echoArray', 'echoValueTypeArray', 'echoMultidimensionalArray', 'dynamiObjectList',
  'methodReturnsList', 'methodReturnsListOfLists', 'methodReturnsDictionary1',
  'methodReturnsDictionary2', 'methodReturnsDictionary3', 'passSimpleClassAsArgument',
  'returnTaskStringAsync', 'voidReturnAsync', 'javascriptCallbackEvalPromise',
  'waitBeforeReturnAsync', 'asyncWaitTwoSeconds', 'asyncDownloadFileAndSplitOnNewLines',
  'asyncThrowException', 'javascriptOptionalCallbackEvalPromise', 'setCallBack'
];

function makeBoundAsyncProxy(name) {
  var obj = {};
  for (var i = 0; i < ASYNC_METHODS.length; i++) {
    (function(method) {
      obj[method] = function() {
        return callHostRpc(name, method, Array.prototype.slice.call(arguments));
      };
    })(ASYNC_METHODS[i]);
  }
  return obj;
}

function makeBoundSyncObject() {
  var myPropertyVal = 42;
  var myReadOnlyVal = "I'm immutable!";
  var simplePropertyVal = "This is a very simple property.";

  var subObj = {};
  Object.defineProperty(subObj, 'simpleProperty', {
    enumerable: true,
    configurable: true,
    get: function() { return simplePropertyVal; },
    set: function(v) { simplePropertyVal = String(v); }
  });
  subObj.getMyType = function() {
    return 'My Type is cef-rs-gpui.Example.JavascriptBinding.SubBoundObject';
  };
  subObj.echoSimpleProperty = function() {
    return simplePropertyVal;
  };

  var excObj = {
    triggerNestedExceptions: function() {
      throw new Error('Nested Exception Canceled');
    },
    triggerParameterException: function(parameter) {
      if (arguments.length === 0 || typeof parameter !== 'number' || !isFinite(parameter) || Math.floor(parameter) !== parameter) {
        throw new Error('Invalid parameter');
      }
      return parameter | 0;
    },
    testCallbackException: function(errorCallback, errorCallbackResult) {
      void callHostRpc('bound', 'testCallbackException', [errorCallback, errorCallbackResult]);
    }
  };

  var obj = {};
  Object.defineProperty(obj, 'myProperty', {
    enumerable: true,
    configurable: true,
    get: function() { return myPropertyVal; },
    set: function(v) { myPropertyVal = Number(v) | 0; }
  });
  Object.defineProperty(obj, 'myReadOnlyProperty', {
    enumerable: true,
    configurable: true,
    get: function() { return myReadOnlyVal; }
  });
  obj.subObject = subObj;
  obj.exceptionTestObject = excObj;
  obj.myUintArray = [7, 8];
  obj.myIntArray = [1, 2, 3, 4, 5, 6, 7, 8];
  obj.myArray = [1, 2, 3];
  obj.myBytes = [3, 4, 5];

  obj.repeat = function(str, n) {
    var out = '';
    var s = String(str);
    var count = Number(n) | 0;
    for (var i = 0; i < count; i++) out += s;
    return out;
  };
  obj.echoMyProperty = function() {
    return myPropertyVal;
  };
  obj.getSubObject = function() {
    return subObj;
  };
  obj.echoDateTime = function(dt) {
    return dt instanceof Date ? new Date(dt.getTime()) : new Date(0);
  };
  obj.echoNullableDateTime = function(dt) {
    if (dt === null || dt === undefined) return null;
    return dt instanceof Date ? new Date(dt.getTime()) : null;
  };
  obj.returnJsonEmployeeList = function() {
    return '{"employees":[{"firstName":"John", "lastName":"Doe"},{"firstName":"Anna", "lastName":"Smith"},{"firstName":"Peter", "lastName":"Jones"}]}';
  };
  obj.methodWithParams = function(name) {
    var rest = [];
    for (var i = 1; i < arguments.length; i++) rest.push(String(arguments[i]));
    return 'Name:' + String(name) + ';Args:' + rest.join(', ');
  };
  obj.methodWithoutParams = function(name, arg2) {
    return String(name) + ', ' + String(arg2);
  };
  obj.methodWithoutAnything = function() {
    return 'Method without anything called and returned successfully.';
  };
  obj.methodWithThreeParamsOneOptionalOneArray = function(name, optionalParam) {
    var n = (name === null || name === undefined) ? 'No Name Specified' : String(name);
    var o = (optionalParam === null || optionalParam === undefined) ? 'No Optional Param Specified' : String(optionalParam);
    var rest = [];
    for (var i = 2; i < arguments.length; i++) rest.push(String(arguments[i]));
    return 'MethodWithThreeParamsOneOptionalOneArray:' + n + ' - ' + o + ';Args:' + rest.join(', ');
  };
  obj.echoParamOrDefault = function(param) {
    return param === undefined ? 'This is the default value' : String(param);
  };
  obj.echoVoid = function() {};
  obj.echoString = function(arg0) { return arg0; };
  obj.lowercaseMethod = function() { return 'lowercase'; };
  obj.testCallback = function(cb) {
    void callHostRpc('bound', 'testCallback', [cb]);
  };
  obj.testCallbackFromObject = function(arg) {
    void callHostRpc('bound', 'testCallbackFromObject', [arg]);
    return 'waiting for callback execution...';
  };
  obj.testCallbackWithDateTime = function(cb) {
    void callHostRpc('bound', 'testCallbackWithDateTime', [cb]);
  };
  obj.testCallbackWithDateTime1900 = function(cb) {
    void callHostRpc('bound', 'testCallbackWithDateTime1900', [cb]);
  };
  obj.testCallbackWithDateTime1970 = function(cb) {
    void callHostRpc('bound', 'testCallbackWithDateTime1970', [cb]);
  };
  obj.testCallbackWithDateTime1985 = function(cb) {
    void callHostRpc('bound', 'testCallbackWithDateTime1985', [cb]);
  };
  return obj;
}

function attachObject(name) {
  var instance = (name === 'bound') ? makeBoundSyncObject() : makeBoundAsyncProxy(name);
  globalThis[name] = instance;
  boundSet.add(name);
}

function postMessageFn(msg) {
  return globalThis.rustCef.postMessage(JSON.stringify({
    __cefKind: 'postMessage',
    ctxId: ctxId,
    payload: encodeVal(msg)
  }));
}

function bindObjectAsyncFn() {
  var args = Array.prototype.slice.call(arguments);
  if (args.length > 0 && args[0] !== null && typeof args[0] === 'object' && !Array.isArray(args[0])) {
    args.shift();
  }
  var names = args.length > 0 ? args.map(String) : ['boundAsync', 'boundAsync2', 'bound'];
  var toBind = [];
  for (var i = 0; i < names.length; i++) {
    if (!boundSet.has(names[i])) {
      toBind.push(names[i]);
    }
  }
  if (toBind.length === 0) {
    return Promise.resolve({ Count: 0, Message: 'Object(s) already bound', Result: [], Success: false });
  }
  for (var j = 0; j < toBind.length; j++) {
    attachObject(toBind[j]);
  }
  return Promise.resolve({ Count: toBind.length, Message: 'Object(s) bound', Result: toBind, Success: true });
}

function deleteBoundObjectFn(name) {
  var key = String(name);
  var existed = boundSet.delete(key) || Object.prototype.hasOwnProperty.call(globalThis, key);
  try {
    delete globalThis[key];
  } catch (_) {
    globalThis[key] = undefined;
  }
  return existed;
}

function isObjectCachedFn(name) {
  return boundSet.has(String(name));
}

function removeObjectFromCacheFn(name) {
  return boundSet.delete(String(name));
}

var repoApi = {
  PostMessage: postMessageFn,
  BindObjectAsync: bindObjectAsyncFn,
  DeleteBoundObject: deleteBoundObjectFn,
  IsObjectCached: isObjectCachedFn,
  RemoveObjectFromCache: removeObjectFromCacheFn,
  postMessage: postMessageFn,
  bindObjectAsync: bindObjectAsyncFn,
  deleteBoundObject: deleteBoundObjectFn,
  isObjectCached: isObjectCachedFn,
  removeObjectFromCache: removeObjectFromCacheFn
};

if (typeof globalThis['cef-rs-gpui'] === 'undefined') {
  globalThis['cef-rs-gpui'] = repoApi;
}
if (typeof globalThis.CefRsGpui === 'undefined') {
  globalThis.CefRsGpui = repoApi;
}
if (typeof globalThis.cefRsGpui === 'undefined') {
  globalThis.cefRsGpui = repoApi;
}

if (legacyBinding) {
  attachObject('bound');
  attachObject('boundAsync');
}
"##;
