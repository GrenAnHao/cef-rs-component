//! Renderer-side V8 bootstrap script implementing configurable JS Object
//! Repository (`BindObjectAsync`, `DeleteBoundObject`, `IsObjectCached`,
//! `RemoveObjectFromCache`, `PostMessage`, JS callbacks `__cefCbId`, and `Date`
//! marshaling) on top of origin-gated `window.rustCef` IPC.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn bootstrap_js(legacy_binding: bool) -> String {
    bootstrap_js_with_config(legacy_binding, None)
}

pub(crate) fn bootstrap_js_with_config(
    legacy_binding: bool,
    repo_config_json: Option<&str>,
) -> String {
    let legacy_flag = if legacy_binding { "true" } else { "false" };
    let fallback_cfg = if legacy_binding {
        r#"{"enabled":true,"compatDefault":true,"globalObjectName":null,"camelCaseMethods":false,"legacyBinding":true,"objects":[]}"#
    } else {
        r#"{"enabled":true,"compatDefault":true,"globalObjectName":null,"camelCaseMethods":false,"legacyBinding":false,"objects":[]}"#
    };
    let raw_json = repo_config_json.unwrap_or(fallback_cfg);
    let quoted_cfg = serde_json::to_string(raw_json).unwrap_or_else(|_| {
        serde_json::to_string(fallback_cfg).unwrap_or_else(|_| "\"{}\"".to_owned())
    });
    format!(
        "if (typeof globalThis !== 'undefined' && globalThis.rustCef) {{\n\
         if (Object.freeze) {{ Object.freeze(globalThis.rustCef); }}\n\
         (function(legacyBinding, repoConfigJson) {{\n\
         {CEF_RS_GPUI_BOOTSTRAP_BODY_JS}\n\
         }})({legacy_flag}, {quoted_cfg});\n\
         }}"
    )
}

const CEF_RS_GPUI_BOOTSTRAP_BODY_JS: &str = r##"
if (globalThis.__cefRsGpuiInitialized) { return; }
globalThis.__cefRsGpuiInitialized = true;

var repoConfig = {
  enabled: true,
  compatDefault: true,
  globalObjectName: null,
  camelCaseMethods: false,
  legacyBinding: Boolean(legacyBinding),
  objects: []
};
if (typeof repoConfigJson === 'string' && repoConfigJson.length > 0) {
  try {
    var parsedCfg = JSON.parse(repoConfigJson);
    if (parsedCfg && typeof parsedCfg === 'object') {
      repoConfig = parsedCfg;
    }
  } catch (_) {}
}
if (repoConfig.enabled === false) {
  return;
}

var MAX_JSON_DEPTH = 32;
var MAX_LIVE_CALLBACKS = 4096;
var ctxId = Math.random().toString(36).slice(2) + '_' + Date.now().toString(36);
var nextReqId = 1;
var nextCbId = 1;
var pending = new Map();
var pendingBinds = new Map();
var callbacks = new Map();
var boundSet = new Set();
var cachedDescriptors = new Map();
var preRegistered = new Map();

function isSafePropName(name) {
  return typeof name === 'string' &&
    name.length > 0 &&
    name.length <= 128 &&
    name !== '__proto__' &&
    name !== 'constructor' &&
    name !== 'prototype';
}

if (Array.isArray(repoConfig.objects)) {
  for (var oi = 0; oi < repoConfig.objects.length; oi++) {
    var objDesc = repoConfig.objects[oi];
    if (objDesc && isSafePropName(objDesc.name)) {
      preRegistered.set(objDesc.name, objDesc);
    }
  }
}

function encodeVal(v, depth) {
  var d = depth || 0;
  if (d > MAX_JSON_DEPTH) {
    throw new Error('JSON depth exceeded');
  }
  if (v === undefined || v === null) return null;
  if (typeof v === 'function') {
    if (callbacks.size >= MAX_LIVE_CALLBACKS) {
      var oldest = callbacks.keys().next();
      if (!oldest.done) callbacks.delete(oldest.value);
    }
    var cbId = 'cb_' + (nextCbId++);
    callbacks.set(cbId, v);
    return { __cefCbId: cbId };
  }
  if (v instanceof Date) {
    return { __cefDateMs: v.getTime() };
  }
  if (Array.isArray(v)) {
    var arrOut = [];
    for (var ai = 0; ai < v.length; ai++) {
      arrOut.push(encodeVal(v[ai], d + 1));
    }
    return arrOut;
  }
  if (typeof v === 'object') {
    var out = {};
    var keys = Object.keys(v);
    for (var i = 0; i < keys.length; i++) {
      var k = keys[i];
      out[k] = encodeVal(v[k], d + 1);
    }
    return out;
  }
  return v;
}

function decodeVal(v, depth) {
  var d = depth || 0;
  if (d > MAX_JSON_DEPTH || v === null || v === undefined || typeof v !== 'object') return v;
  if (Array.isArray(v)) {
    var arrOut = [];
    for (var ai = 0; ai < v.length; ai++) {
      arrOut.push(decodeVal(v[ai], d + 1));
    }
    return arrOut;
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
    if (!isSafePropName(k)) continue;
    out[k] = decodeVal(v[k], d + 1);
  }
  return out;
}

function callHostRpc(target, method, rawArgs) {
  var id = nextReqId++;
  var encodedArgs = [];
  for (var i = 0; i < rawArgs.length; i++) {
    encodedArgs.push(encodeVal(rawArgs[i], 0));
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

function notifyHostObjectsBound(names, alreadyBound) {
  if (!Array.isArray(names) || names.length === 0) return;
  try {
    globalThis.rustCef.postMessage(JSON.stringify({
      __cefKind: 'objectsBound',
      ctxId: ctxId,
      names: names,
      alreadyBound: Boolean(alreadyBound)
    }));
  } catch (_) {}
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
      entry.resolve(decodeVal(msg.result, 0));
    } else {
      entry.reject(new Error(String(msg.error || 'Host RPC error')));
    }
    return;
  }

  if (msg.__cefKind === 'bindResult') {
    var bindEntry = pendingBinds.get(msg.id);
    if (!bindEntry) return;
    pendingBinds.delete(msg.id);
    var descriptors = Array.isArray(msg.objects) ? msg.objects : [];
    var attachedNames = [];
    for (var bi = 0; bi < descriptors.length; bi++) {
      var desc = descriptors[bi];
      if (desc && isSafePropName(desc.name)) {
        attachDescriptor(desc);
        attachedNames.push(desc.name);
      }
    }
    if (attachedNames.length > 0) {
      notifyHostObjectsBound(attachedNames, false);
    } else if (bindEntry.notifyIfAlreadyBound && bindEntry.alreadyBoundNames.length > 0) {
      notifyHostObjectsBound(bindEntry.alreadyBoundNames, true);
    }
    var count = attachedNames.length;
    var ok = Boolean(msg.ok && count > 0);
    bindEntry.resolve({
      Count: count,
      Message: String(msg.message || (ok ? 'Object(s) bound' : 'Object(s) not bound')),
      Result: attachedNames,
      Success: ok
    });
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
    var args = Array.isArray(msg.args) ? msg.args.map(function(item) { return decodeVal(item, 0); }) : [];
    try {
      var ret = fn.apply(undefined, args);
      if (msg.replyId) {
        Promise.resolve(ret).then(function(resolved) {
          globalThis.rustCef.postMessage(JSON.stringify({
            __cefKind: 'cbResult',
            ctxId: ctxId,
            replyId: msg.replyId,
            ok: true,
            result: encodeVal(resolved, 0)
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

function buildDescriptorProxy(desc, targetPath) {
  if (repoConfig.compatDefault && desc.name === 'bound' && targetPath === 'bound') {
    var syncObj = makeBoundSyncObject();
    if (Array.isArray(desc.methods)) {
      for (var mi = 0; mi < desc.methods.length; mi++) {
        (function(method) {
          if (isSafePropName(method) && typeof syncObj[method] === 'undefined') {
            syncObj[method] = function() {
              return callHostRpc(targetPath, method, Array.prototype.slice.call(arguments));
            };
          }
        })(desc.methods[mi]);
      }
    }
    return syncObj;
  }
  var obj = {};
  if (Array.isArray(desc.properties)) {
    for (var pi = 0; pi < desc.properties.length; pi++) {
      (function(prop) {
        if (!prop || !isSafePropName(prop.name)) return;
        var initialVal = null;
        if (typeof prop.valueJson === 'string') {
          try {
            initialVal = decodeVal(JSON.parse(prop.valueJson), 0);
          } catch (_) {
            initialVal = null;
          }
        } else if (prop.value !== undefined) {
          initialVal = decodeVal(prop.value, 0);
        }
        var currentVal = initialVal;
        if (prop.readonly) {
          Object.defineProperty(obj, prop.name, {
            enumerable: true,
            configurable: true,
            get: function() { return currentVal; }
          });
        } else {
          Object.defineProperty(obj, prop.name, {
            enumerable: true,
            configurable: true,
            get: function() { return currentVal; },
            set: function(v) { currentVal = v; }
          });
        }
      })(desc.properties[pi]);
    }
  }
  if (Array.isArray(desc.methods)) {
    for (var mi = 0; mi < desc.methods.length; mi++) {
      (function(method) {
        if (!isSafePropName(method)) return;
        obj[method] = function() {
          return callHostRpc(targetPath, method, Array.prototype.slice.call(arguments));
        };
      })(desc.methods[mi]);
    }
  }
  if (Array.isArray(desc.subObjects)) {
    for (var si = 0; si < desc.subObjects.length; si++) {
      var sub = desc.subObjects[si];
      if (sub && isSafePropName(sub.name)) {
        obj[sub.name] = buildDescriptorProxy(sub, targetPath + '.' + sub.name);
      }
    }
  }
  return obj;
}

function attachDescriptor(desc) {
  if (!desc || !isSafePropName(desc.name)) return;
  cachedDescriptors.set(desc.name, desc);
  var instance = buildDescriptorProxy(desc, desc.name);
  Object.defineProperty(globalThis, desc.name, {
    value: instance,
    writable: true,
    enumerable: true,
    configurable: true
  });
  boundSet.add(desc.name);
}

function attachObject(name) {
  if (!isSafePropName(name)) return;
  var pre = cachedDescriptors.get(name) || preRegistered.get(name);
  if (pre) {
    attachDescriptor(pre);
    return;
  }
  var instance = (name === 'bound') ? makeBoundSyncObject() : makeBoundAsyncProxy(name);
  Object.defineProperty(globalThis, name, {
    value: instance,
    writable: true,
    enumerable: true,
    configurable: true
  });
  boundSet.add(name);
  cachedDescriptors.set(name, {
    name: name,
    isAsync: name !== 'bound',
    methods: name === 'bound' ? [] : ASYNC_METHODS.slice(),
    properties: [],
    subObjects: []
  });
}

function postMessageFn(msg) {
  return globalThis.rustCef.postMessage(JSON.stringify({
    __cefKind: 'postMessage',
    ctxId: ctxId,
    payload: encodeVal(msg, 0)
  }));
}

function bindObjectAsyncFn() {
  var args = Array.prototype.slice.call(arguments);
  var notifyIfAlreadyBound = false;
  var ignoreCache = false;
  if (args.length > 0 && args[0] !== null && typeof args[0] === 'object' && !Array.isArray(args[0])) {
    var cfg = args.shift();
    notifyIfAlreadyBound = Boolean(cfg.NotifyIfAlreadyBound || cfg.notifyIfAlreadyBound);
    ignoreCache = Boolean(cfg.IgnoreCache || cfg.ignoreCache);
  }
  var names = [];
  for (var ai = 0; ai < args.length; ai++) {
    if (typeof args[ai] === 'string' && isSafePropName(args[ai])) {
      names.push(args[ai]);
    }
  }

  var alreadyBoundNames = [];
  var missingNames = [];
  for (var i = 0; i < names.length; i++) {
    var name = names[i];
    var isBound = boundSet.has(name) && Object.prototype.hasOwnProperty.call(globalThis, name);
    if (isBound && !ignoreCache) {
      alreadyBoundNames.push(name);
    } else {
      missingNames.push(name);
    }
  }

  if (names.length > 0 && missingNames.length === 0 && !ignoreCache) {
    if (notifyIfAlreadyBound) {
      notifyHostObjectsBound(alreadyBoundNames, true);
    }
    return Promise.resolve({
      Count: 0,
      Message: 'Object(s) already bound',
      Result: [],
      Success: false
    });
  }

  if (!ignoreCache && missingNames.length > 0) {
    var allCached = true;
    for (var ci = 0; ci < missingNames.length; ci++) {
      var mn = missingNames[ci];
      if (!cachedDescriptors.has(mn) && !preRegistered.has(mn)) {
        allCached = false;
        break;
      }
    }
    if (allCached) {
      for (var bi = 0; bi < missingNames.length; bi++) {
        attachObject(missingNames[bi]);
      }
      notifyHostObjectsBound(missingNames, false);
      return Promise.resolve({
        Count: missingNames.length,
        Message: 'Object(s) bound',
        Result: missingNames,
        Success: true
      });
    }
  }

  var reqId = nextReqId++;
  var queryNames = ignoreCache ? names.slice() : missingNames.slice();
  return new Promise(function(resolve, reject) {
    pendingBinds.set(reqId, {
      resolve: resolve,
      reject: reject,
      notifyIfAlreadyBound: notifyIfAlreadyBound,
      alreadyBoundNames: alreadyBoundNames
    });
    try {
      globalThis.rustCef.postMessage(JSON.stringify({
        __cefKind: 'bindObject',
        ctxId: ctxId,
        id: reqId,
        names: queryNames,
        ignoreCache: ignoreCache,
        notifyIfAlreadyBound: notifyIfAlreadyBound
      }));
    } catch (e) {
      pendingBinds.delete(reqId);
      reject(e);
    }
  });
}

function deleteBoundObjectFn(name) {
  if (typeof name !== 'string') {
    throw new Error('Must specify the name of a bound object to unbind, one object at a time.');
  }
  if (!isSafePropName(name)) return false;
  var existed = boundSet.delete(name) || Object.prototype.hasOwnProperty.call(globalThis, name);
  try {
    delete globalThis[name];
  } catch (_) {
    globalThis[name] = undefined;
  }
  return existed;
}

function isObjectCachedFn(name) {
  if (typeof name !== 'string') {
    throw new Error('Must specify the name of a single bound object to check the cache for');
  }
  return cachedDescriptors.has(name);
}

function removeObjectFromCacheFn(name) {
  if (typeof name !== 'string') {
    throw new Error('Must specify the name of a single bound object to remove from cache');
  }
  return cachedDescriptors.delete(name);
}

if (typeof repoConfig.globalObjectName === 'string' && isSafePropName(repoConfig.globalObjectName)) {
  var customApi = repoConfig.camelCaseMethods
    ? {
        postMessage: postMessageFn,
        bindObjectAsync: bindObjectAsyncFn,
        deleteBoundObject: deleteBoundObjectFn,
        isObjectCached: isObjectCachedFn,
        removeObjectFromCache: removeObjectFromCacheFn
      }
    : {
        PostMessage: postMessageFn,
        BindObjectAsync: bindObjectAsyncFn,
        DeleteBoundObject: deleteBoundObjectFn,
        IsObjectCached: isObjectCachedFn,
        RemoveObjectFromCache: removeObjectFromCacheFn
      };
  if (Object.freeze) { Object.freeze(customApi); }
  if (typeof globalThis[repoConfig.globalObjectName] === 'undefined') {
    globalThis[repoConfig.globalObjectName] = customApi;
  }
} else {
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
  if (Object.freeze) { Object.freeze(repoApi); }
  if (typeof globalThis['cef-rs-gpui'] === 'undefined') {
    globalThis['cef-rs-gpui'] = repoApi;
  }
  if (typeof globalThis.CefRsGpui === 'undefined') {
    globalThis.CefRsGpui = repoApi;
  }
  if (typeof globalThis.cefRsGpui === 'undefined') {
    globalThis.cefRsGpui = repoApi;
  }
}

if (repoConfig.legacyBinding || legacyBinding) {
  if (preRegistered.size > 0) {
    preRegistered.forEach(function(desc) {
      attachDescriptor(desc);
    });
  }
  if (repoConfig.compatDefault) {
    if (!boundSet.has('bound')) attachObject('bound');
    if (!boundSet.has('boundAsync')) attachObject('boundAsync');
  }
}
"##;
