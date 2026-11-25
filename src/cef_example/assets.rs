//! Embedded `CefSharp.Example` web fixtures, custom-scheme assets, host
//! resource interceptor, and streaming `ResponseBodyFilter`.
//!
//! Mirrors `CefSharp.Example/CefExample.cs`, `CefSharpSchemeHandlerFactory.cs`,
//! `Handlers/ExampleResourceRequestHandler.cs`, and `Resources/*.html`.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::OnceLock,
};

use rust_cef_core::{
    contracts::{ResourceResponseInfo, truncate_utf8_bytes},
    custom_schemes::{
        CustomSchemeConfig, CustomSchemeOptions, InterceptAction, InterceptRequest, MemoryAsset,
        ResourceInterceptor, ResponseBodyFilter, ResponseFilterStep, SchemeAssetSource,
    },
};

#[allow(dead_code)]
pub const EXAMPLE_DOMAIN: &str = "cefsharp.example";
#[allow(dead_code)]
pub const CUSTOM_POST_DOMAIN: &str = "cefsharp.com";
#[allow(dead_code)]
pub const BASE_URL: &str = "https://cefsharp.example";

pub const DEFAULT_HOME_URL: &str = "https://cefsharp.example/home.html";
pub const BINDING_TEST_URL: &str = "https://cefsharp.example/BindingTest.html";
pub const POST_MESSAGE_TEST_URL: &str = "https://cefsharp.example/PostMessageTest.html";
pub const TOOLTIP_TEST_URL: &str = "https://cefsharp.example/TooltipTest.html";
pub const DRAG_DROP_CURSORS_TEST_URL: &str = "https://cefsharp.example/DragDropCursorsTest.html";
pub const POPUP_TEST_URL: &str = "https://cefsharp.example/PopupTest.html";
pub const SCHEME_TEST_URL: &str = "custom://cefsharp/SchemeTest.html";
pub const RESPONSE_FILTER_TEST_URL: &str = "https://cefsharp.example/ResponseFilterTest.html";
pub const DRAGGABLE_REGION_TEST_URL: &str = "https://cefsharp.example/DraggableRegionTest.html";
pub const EXCEPTION_TEST_URL: &str = "https://cefsharp.example/ExceptionTest.html";
#[allow(dead_code)]
pub const HELLO_WORLD_URL: &str = "https://cefsharp.example/HelloWorld.html";
pub const DOWNLOAD_TEST_URL: &str = "https://cefsharp.example/download/sample-report.txt";
pub const CUSTOM_POST_URL: &str = "https://cefsharp.com/PostDataTest.html";
pub const DIAGNOSTICS_URL: &str = "custom://cefsharp/diagnostics.html";
pub const BENCHMARK_SUITE_URL: &str = "https://cefsharp.example/BenchmarkSuite.html";

pub const RESPONSE_FILTER_NEEDLE: &[u8] = b"REPLACE_THIS_STRING";
pub const RESPONSE_FILTER_REPLACEMENT: &[u8] = b"This is the replaced string!";

const DOCS_CSS: &str = include_str!("docs.css");

static RESPONSE_FILTER_HTML: OnceLock<Vec<u8>> = OnceLock::new();

pub fn response_filter_test_html() -> &'static [u8] {
    RESPONSE_FILTER_HTML.get_or_init(|| {
        let mut out = String::from(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
            <title>Response Filter Test</title><style>body{font-family:Segoe UI,Arial,sans-serif;padding:24px;line-height:1.55;color:#222;background:#fff;max-width:960px;margin:0 auto}.target{color:#d32f2f;font-weight:700}</style></head><body>\
            <h1>Response Filter Test</h1>\
            <p>The red text below has been replaced by <code>FindReplaceResponseFilter</code> via <code>ResponseBodyFilter</code>. Document size exceeds 32 KB so multiple streaming chunks and split boundaries are exercised:</p>\n",
        );
        let para = "It was the best of times, it was the worst of times, it was the age of wisdom, \
            it was the age of foolishness, it was the epoch of belief, it was the epoch of incredulity, \
            it was the season of Light, it was the season of Darkness, it was the spring of hope, \
            it was the winter of despair, we had everything before us, we had nothing before us.\n";
        for block in 0..12 {
            out.push_str("<p><span class=\"target\">REPLACE_THIS_STRING</span></p>\n");
            for i in 0..10 {
                out.push_str(&format!("<p>{block}.{i} {para}</p>\n"));
            }
        }
        out.push_str("<p><span class=\"target\">REPLACE_THIS_STRING</span></p><p>End.</p></body></html>");
        out.into_bytes()
    })
}

const HOME_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Getting started &middot; CefSharp</title>
  <link href="/assets/docs.css" rel="stylesheet" />
</head>
<body>
  <header class="bs-docs-nav" role="banner">
    <div class="container nav-inner">
      <a href="/home.html" class="navbar-brand">CefSharp</a>
      <nav class="nav-links">
        <a class="active" href="#the-basics">The Basics</a>
        <a href="/BindingTest.html">JavaScript Binding Tests</a>
      </nav>
    </div>
  </header>

  <div class="bs-header" id="content">
    <div class="container">
      <h1>Getting started</h1>
      <p>An overview of CefSharp / GpuiCef, how to use it, and an overview of some of the included features.</p>
    </div>
  </div>

  <div class="container bs-docs-container">
    <div class="doc-grid">
      <aside class="bs-sidebar" role="complementary">
        <ul class="bs-sidenav">
          <li><a class="side-link active" href="#introduction">Introduction</a></li>
          <li>
            <a class="side-link" href="#the-basics">The Basics</a>
            <ul>
              <li><a class="sub-link" href="#features-custom-schemes">Custom Schemes</a></li>
              <li><a class="sub-link" href="#features-javascript-integration">JavaScript Integration</a></li>
            </ul>
          </li>
          <li><a class="side-link" href="#features">Features</a></li>
          <li><a class="side-link" href="#useful-links">Useful Links</a></li>
        </ul>
      </aside>

      <main class="doc-main" role="main">
        <section class="bs-docs-section">
          <div class="page-header">
            <h2 id="introduction">Introduction to CefSharp (GPUI Edition)</h2>
          </div>
          <p class="lead">Welcome to GpuiCef! A full-featured desktop browser example with explicit Rust ownership boundaries.</p>
          <p>
            So, what is <code>gpui-cef</code> really? Structured along the same decoupled host/control boundaries as CefSharp:
          </p>
          <ul>
            <li><strong>A framework</strong> for embedding Chromium-based browsing capabilities into a native Rust GPUI desktop application (OSR or Win32 child HWND).</li>
            <li><strong>A set of high-level custom views</strong> (<code>CefBrowser</code> &amp; <code>NativeCefBrowser</code>) exposing CefSharp-style navigation getters, commands, filters, and per-view event callbacks.</li>
            <li><strong>Built on top of CEF</strong>, the Chromium Embedded Framework via pinned <code>cef-rs 156.7.0+156.0.31</code>.</li>
            <li><strong>Strict capability &amp; origin confinement</strong>: popups, downloads, JS dialogs, custom schemes, and response filters are explicit opt-in host capabilities.</li>
          </ul>
        </section>

        <section class="bs-docs-section">
          <div class="page-header">
            <h2 id="the-basics">The Basics</h2>
          </div>
          <p class="lead">Initialize an explicit <code>Runtime</code>, create browsers with <code>BrowserOptions</code>, and attach views to GPUI windows.</p>
          <h3>Initialize / Shutdown CEF Runtime</h3>
          <pre><code>let rt = Runtime::start(config, services)?;
let handle = rt.create_browser(BrowserConfig {
    initial_url: "https://cefsharp.example/home.html".into(),
    render_preference: RenderPreference::D3d11Required,
    ..
})?;
let view = cx.new(|cx| CefBrowser::new(rt.attach_browser(&amp;handle)?, window, cx));</code></pre>
        </section>

        <section class="bs-docs-section">
          <div class="page-header">
            <h2 id="features">Features</h2>
          </div>
          <h3 id="features-custom-schemes">Custom Schemes &amp; Response Filtering</h3>
          <p>
            Serve local in-memory bundles on <code>custom://cefsharp/</code>, intercept HTTPS requests in-process via <code>ResourceInterceptor</code>,
            and stream-rewrite bodies using <code>ResponseBodyFilter</code>.
          </p>
          <h3 id="features-javascript-integration">JavaScript Integration &amp; Right-Hand Sidebar</h3>
          <p>
            Use the right sidebar to asynchronously run <code>ExecuteJavascript</code>, evaluate expressions with <code>EvaluateScript</code>,
            or inspect bidirectional <code>window.rustCef.postMessage</code> calls.
          </p>
        </section>

        <section class="bs-docs-section">
          <div class="page-header">
            <h2 id="useful-links">Useful Links &amp; Built-in Tests</h2>
          </div>
          <div class="quick-links">
            <a class="pill-btn" href="/BindingTest.html">Binding Test</a>
            <a class="pill-btn" href="/PostMessageTest.html">PostMessage Test</a>
            <a class="pill-btn" href="/ResponseFilterTest.html">Response Filter Test</a>
            <a class="pill-btn" href="/TooltipTest.html">Tooltip Test</a>
            <a class="pill-btn" href="/DragDropCursorsTest.html">Drag&amp;Drop Cursors</a>
            <a class="pill-btn" href="/DraggableRegionTest.html">Draggable Region</a>
            <a class="pill-btn" href="/PopupTest.html">Popup Test</a>
            <a class="pill-btn" href="custom://cefsharp/SchemeTest.html">Custom Scheme Test</a>
            <a class="pill-btn" href="/ExceptionTest.html">Uncaught Exception Test</a>
            <a class="pill-btn" href="/BenchmarkSuite.html">Benchmark Self-Test Suite</a>
            <a class="pill-btn" href="/download/sample-report.txt">Download Sample</a>
          </div>
        </section>
      </main>
    </div>
  </div>
</body>
</html>
"##;

const BINDING_TEST_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>JavaScript Binding Tests &middot; CefSharp</title>
  <link href="/assets/docs.css" rel="stylesheet" />
</head>
<body>
  <header class="bs-docs-nav">
    <div class="container nav-inner">
      <a href="/home.html" class="navbar-brand">CefSharp</a>
      <nav class="nav-links">
        <a href="/home.html">The Basics</a>
        <a class="active" href="/BindingTest.html">JavaScript Binding Tests</a>
      </nav>
    </div>
  </header>
  <div class="container bs-docs-container">
    <h2>JavaScript Binding &amp; Evaluation Test</h2>
    <p>Tests bidirectional <code>window.rustCef.postMessage</code>, <code>EvaluateScript</code>, and <code>OnFocusedNodeChanged</code>.</p>
    <div class="card-box">
      <label for="focus-probe">Focused Node Probe Input:</label><br />
      <input id="focus-probe" type="text" value="Focus here to update FocusedNodeChanged" style="width:360px;padding:6px;margin:8px 0" /><br />
      <button id="btn-ping" onclick="sendPing()">Send Ping to Host</button>
      <button id="btn-stress" onclick="window.__runBindingStressStep()">Run Stress Step</button>
    </div>
    <p>Messages Sent: <strong id="msg-count">0</strong></p>
    <pre id="binding-log">Waiting for messages...</pre>
  </div>
  <script>
    let count = 0;
    function appendLog(line) {
      const pre = document.getElementById('binding-log');
      pre.textContent = line + "\n" + pre.textContent;
    }
    function sendPing() {
      count += 1;
      document.getElementById('msg-count').textContent = String(count);
      if (window.rustCef && typeof window.rustCef.postMessage === 'function') {
        window.rustCef.postMessage({ type: 'ping', count: count });
        appendLog('Sent ping #' + count);
      } else {
        appendLog('window.rustCef unavailable');
      }
    }
    window.__runBindingStressStep = function() {
      sendPing();
      return count;
    };
    window.__onHostMessage = function(payload) {
      appendLog('Host echoed: ' + JSON.stringify(payload));
    };
  </script>
</body>
</html>
"##;

const POST_MESSAGE_TEST_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Post Message Test &middot; CefSharp</title>
  <style>
    body{font-family:Segoe UI,Arial,sans-serif;padding:24px;background:#fafafa;color:#222}
    .row{padding:10px 14px;margin:8px 0;border-radius:4px;border:1px solid #ddd;background:#fff}
    .pass{border-color:#2e7d32;background:#e8f5e9;color:#1b5e20}
    .pending{border-color:#f9a825;background:#fffde7;color:#795548}
  </style>
</head>
<body>
  <h2>PostMessage Test (CefSharp.Example Style)</h2>
  <div id="test-obj" class="row pending">1. PostMessage Object Test: waiting...</div>
  <div id="test-int" class="row pending">2. PostMessage Int Test: waiting...</div>
  <div id="test-alias" class="row pending">3. CefSharp.PostMessage Compatibility Alias: waiting...</div>
  <script>
    window.__onHostPostMessageCallback = function(msg) {
      const el = document.getElementById('test-obj');
      if (msg === 'Update') {
        el.className = 'row pass';
        el.textContent = '1. PostMessage Object Test: PASS (returned "Update")';
      }
      const aliasEl = document.getElementById('test-alias');
      if (window.CefSharp && typeof window.CefSharp.PostMessage === 'function') {
        aliasEl.className = 'row pass';
        aliasEl.textContent = '3. CefSharp.PostMessage Compatibility Alias: PASS';
      }
    };
    window.PostMessageIntTestCallback = function(val) {
      const el = document.getElementById('test-int');
      if (val === 123456) {
        el.className = 'row pass';
        el.textContent = '2. PostMessage Int Test: PASS (returned 123456)';
      }
    };
    if (window.CefSharp && typeof window.CefSharp.PostMessage === 'function') {
      window.CefSharp.PostMessage({ Type: 'Update', Data: { Property: 123 } });
      window.CefSharp.PostMessage(123456);
    } else if (window.rustCef && typeof window.rustCef.postMessage === 'function') {
      window.rustCef.postMessage({ Type: 'Update', Data: { Property: 123 } });
      window.rustCef.postMessage(123456);
    }
  </script>
</body>
</html>
"##;

const TOOLTIP_TEST_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8" /><title>Tooltip Test</title></head>
<body style="font-family:Segoe UI,Arial,sans-serif;padding:20px">
  <div style="width:520px;">
    <input type="text" size="28" title="This is the first tooltip" value="Hover me (tooltip 1)" /><br /><br />
    <input type="text" size="28" title="This is the second tooltip" value="Hover me (tooltip 2)" /><br /><br />
    <div style="display:flex;justify-content:space-between">
      <select><option selected>test</option><option>one</option><option>two</option></select>
      <select><option selected>testRight</option><option>three</option><option>four</option></select>
    </div>
  </div>
  <div style="margin-top:24px;display:flex;gap:16px">
    <div>Hover over the box on the right to see a long tooltip propagated to the GPUI StatusBar.</div>
    <div style="padding:8px 12px;border:1px solid #7e57c2;background:#ede7f6" title="Credit check failed: Settlement risk overflow (PosFlows), group AllDeals, Counterparty VSMPOTIRUS, pillar Inf: over bound by 70,787.">Hover HERE</div>
  </div>
</body>
</html>
"##;

const DRAG_DROP_CURSORS_TEST_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Drag&amp;Drop Cursors Test</title>
  <style>
    body{font-family:Segoe UI,Arial,sans-serif;padding:20px}
    .dropzone{width:90px;height:70px;padding:10px;display:inline-block;border:1px solid #999;background:#fff;vertical-align:top;margin-right:6px}
    #drag{width:240px;height:50px;padding:12px;border:1px solid #999;background:wheat;cursor:grab}
  </style>
  <script>
    function allowDrop(ev, effect) {
      ev.dataTransfer.dropEffect = effect;
      ev.preventDefault();
    }
    function drag(ev) {
      ev.dataTransfer.effectAllowed = "all";
      ev.dataTransfer.setData("text/plain", ev.target.id);
    }
    function drop(ev) {
      ev.preventDefault();
      var data = ev.dataTransfer.getData("text/plain");
      var element = document.getElementById(data);
      if (element) { ev.target.appendChild(element); }
      alert(ev.dataTransfer.dropEffect + " | " + data);
    }
  </script>
</head>
<body>
  <div class="dropzone" ondrop="drop(event)" ondragover="allowDrop(event, 'none')">None</div>
  <div class="dropzone" ondrop="drop(event)" ondragover="allowDrop(event, 'copy')">Copy</div>
  <div class="dropzone" ondrop="drop(event)" ondragover="allowDrop(event, 'move')">Move</div>
  <div class="dropzone" ondrop="drop(event)" ondragover="allowDrop(event, 'link')">Link</div>
  <br /><br />
  <div id="drag" draggable="true" ondragstart="drag(event)">Drag me over the drop zones</div>
</body>
</html>
"##;

const POPUP_TEST_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8" /><title>Popup Test</title></head>
<body style="font-family:Segoe UI,Arial,sans-serif;padding:20px;line-height:2">
  <h2>Popup Test</h2>
  <a href="https://cefsharp.example/HelloWorld.html" target="_blank">Open HelloWorld (target=_blank)</a><br />
  <a href="#" onclick="window.open('custom://cefsharp/SchemeTest.html'); return false;">window.open('custom://cefsharp/SchemeTest.html')</a><br />
  <a href="#" onclick="window.open('https://cefsharp.example/BindingTest.html'); return false;">window.open('BindingTest.html')</a><br />
  <button id="openMyWindow">Open "myWindow"</button>
  <button id="closeMyWindow">Close "myWindow"</button>
  <script>
    var myWindow = null;
    document.getElementById('openMyWindow').addEventListener('click', function () {
      myWindow = window.open('/HelloWorld.html', 'Popup WindowName', 'menubar=0,resizable=1,width=640,height=480');
    });
    document.getElementById('closeMyWindow').addEventListener('click', function () {
      if (myWindow && !myWindow.closed) { myWindow.close(); }
    });
  </script>
</body>
</html>
"##;

const DRAGGABLE_REGION_TEST_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8" /><title>Draggable Region Test</title></head>
<body style="font-family:Segoe UI,Arial,sans-serif;padding:20px">
  <h1>Draggable Region Test</h1>
  <div style="-webkit-app-region:drag;width:220px;height:140px;border:2px solid #5e35b1;background:#ede7f6;padding:12px">
    A draggable area (-webkit-app-region: drag)
  </div>
  <br />
  <div style="-webkit-app-region:drag;width:420px;height:220px;border:2px solid #1565c0;background:#e3f2fd;padding:12px">
    Outer draggable area
    <div style="-webkit-app-region:no-drag;margin:16px;border:2px solid #c62828;background:#ffebee;width:160px;height:70px;padding:8px">
      Non-draggable area (no-drag)
    </div>
  </div>
</body>
</html>
"##;

const SCHEME_TEST_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8" /><title>Scheme Handler Test</title></head>
<body style="font-family:Segoe UI,Arial,sans-serif;padding:20px;line-height:1.5">
  <h1>Success</h1>
  <p>Custom Scheme Handler (<code>custom://cefsharp/SchemeTest.html</code>) functioned correctly.</p>
  <form id="postForm" action="https://cefsharp.com/PostDataTest.html" method="post">
    User: <input type="text" name="user" value="gpui" />
    Mode: <input type="text" name="mode" value="scheme-post" /><br /><br />
    <input type="submit" value="POST to cefsharp.com/PostDataTest.html" />
  </form>
  <br />
  <button id="ajaxBtn" onclick="runAjaxPost()">Ajax POST to PostDataAjaxTest.html</button>
  <pre id="ajax-result" style="background:#f5f5f5;padding:10px;border:1px solid #ddd;margin-top:10px">Ajax result will appear here...</pre>
  <script>
    async function runAjaxPost() {
      const out = document.getElementById('ajax-result');
      try {
        const resp = await fetch('https://cefsharp.com/PostDataAjaxTest.html', {
          method: 'POST',
          headers: { 'Content-Type': 'text/plain;charset=utf-8' },
          body: 'ajax_payload=hello_from_scheme_test'
        });
        out.textContent = 'HTTP ' + resp.status + ': ' + await resp.text();
      } catch (e) {
        out.textContent = 'Ajax Error: ' + String(e);
      }
    }
  </script>
</body>
</html>
"##;

const POST_DATA_TEST_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8" /><title>Post Data / Scheme Response</title></head>
<body style="font-family:Segoe UI,Arial,sans-serif;padding:20px">
  <h1>cefsharp.com Test Endpoint</h1>
  <p>Method: <strong>GET</strong> (no POST body)</p>
  <p>Intercepted cleanly via <code>ExampleResourceInterceptor</code>.</p>
</body>
</html>
"##;

const BENCHMARK_SUITE_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>Benchmark Self-Test &middot; GpuiCef</title>
  <style>
    body{font-family:Segoe UI,Arial,sans-serif;padding:24px;background:#fafafa;color:#212121;max-width:920px;margin:0 auto}
    h1{color:#4527a0;margin-bottom:6px}
    .row{padding:10px 14px;margin:8px 0;border-radius:4px;border:1px solid #ddd;background:#fff;display:flex;justify-content:space-between;gap:12px}
    .pass{border-color:#2e7d32;background:#e8f5e9;color:#1b5e20}
    .fail{border-color:#c62828;background:#ffebee;color:#b71c1c}
    #sandbox{position:absolute;left:-9999px;width:300px;height:120px;overflow:hidden}
  </style>
</head>
<body>
  <h1>GpuiCef Benchmark &amp; Capability Suite</h1>
  <p id="summary">Running checks...</p>
  <div id="rows"></div>
  <div id="sandbox"></div>
  <script>
    (async function runSuite() {
      const rowsEl = document.getElementById('rows');
      const results = [];
      function addResult(name, ok, detail) {
        results.push({ name, ok: !!ok, detail: String(detail) });
        const d = document.createElement('div');
        d.className = 'row ' + (ok ? 'pass' : 'fail');
        d.innerHTML = '<strong>' + name + '</strong><span>' + (ok ? 'PASS' : 'FAIL') + ' &mdash; ' + String(detail).replace(/[<>&]/g, '') + '</span>';
        rowsEl.appendChild(d);
      }

      // 1. DOM / Layout throughput (Speedometer-style work)
      try {
        const sb = document.getElementById('sandbox');
        const t0 = performance.now();
        let checksum = 0;
        for (let round = 0; round < 5; round++) {
          const ul = document.createElement('ul');
          for (let i = 0; i < 40; i++) {
            const li = document.createElement('li');
            li.textContent = 'Item ' + i;
            ul.appendChild(li);
          }
          sb.appendChild(ul);
          checksum += ul.offsetHeight;
          sb.removeChild(ul);
        }
        const elapsed = (performance.now() - t0).toFixed(2);
        addResult('DOM & Layout Cycle', checksum >= 0, elapsed + ' ms (200 nodes)');
      } catch (e) {
        addResult('DOM & Layout Cycle', false, e);
      }

      // 2. Canvas2D & WebGL rendering
      try {
        const c2 = document.createElement('canvas');
        c2.width = 64; c2.height = 64;
        const ctx = c2.getContext('2d');
        ctx.fillStyle = 'rgb(94, 53, 177)';
        ctx.fillRect(0, 0, 64, 64);
        const px = ctx.getImageData(8, 8, 1, 1).data;
        const c2Ok = px[0] === 94 && px[1] === 53 && px[2] === 177 && px[3] === 255;

        const cgl = document.createElement('canvas');
        const gl = cgl.getContext('webgl2') || cgl.getContext('webgl');
        let glInfo = 'unavailable';
        let glOk = false;
        if (gl) {
          gl.clearColor(0.2, 0.6, 0.4, 1.0);
          gl.clear(gl.COLOR_BUFFER_BIT);
          const gpx = new Uint8Array(4);
          gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, gpx);
          glInfo = String(gl.getParameter(gl.RENDERER) || 'webgl');
          glOk = gpx[1] > 120 && gpx[3] === 255;
        }
        addResult('Canvas2D & WebGL Shader', c2Ok && glOk, glInfo);
      } catch (e) {
        addResult('Canvas2D & WebGL Shader', false, e);
      }

      // 3. WebAssembly minimal module validation & instantiation
      try {
        const wasmBytes = new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);
        const valid = WebAssembly.validate(wasmBytes);
        const mod = await WebAssembly.instantiate(wasmBytes);
        addResult('WebAssembly Instantiate', valid && !!mod.instance, 'WASM v1 OK');
      } catch (e) {
        addResult('WebAssembly Instantiate', false, e);
      }

      // 4. blob: fetch + data: Worker + blob: Worker + data:/srcdoc iframes
      try {
        const bUrl = URL.createObjectURL(new Blob(['blob-ok'], { type: 'text/plain' }));
        const bText = await fetch(bUrl).then(r => r.text());
        URL.revokeObjectURL(bUrl);

        const wData = await new Promise((resolve, reject) => {
          const w = new Worker('data:text/javascript,onmessage=function(e){postMessage(e.data*2);};');
          const timer = setTimeout(() => { w.terminate(); reject('data worker timeout'); }, 2000);
          w.onmessage = (e) => { clearTimeout(timer); w.terminate(); resolve(e.data); };
          w.onerror = (e) => { clearTimeout(timer); w.terminate(); reject(e.message || 'data worker error'); };
          w.postMessage(21);
        });

        const wbUrl = URL.createObjectURL(new Blob(['onmessage=function(e){postMessage(e.data+"!");};'], { type: 'text/javascript' }));
        const wBlob = await new Promise((resolve, reject) => {
          const w = new Worker(wbUrl);
          const timer = setTimeout(() => { w.terminate(); URL.revokeObjectURL(wbUrl); reject('blob worker timeout'); }, 2000);
          w.onmessage = (e) => { clearTimeout(timer); w.terminate(); URL.revokeObjectURL(wbUrl); resolve(e.data); };
          w.onerror = (e) => { clearTimeout(timer); w.terminate(); URL.revokeObjectURL(wbUrl); reject(e.message || 'blob worker error'); };
          w.postMessage('cef');
        });

        const frameOk = await new Promise((resolve) => {
          const sb = document.getElementById('sandbox');
          const f1 = document.createElement('iframe');
          const f2 = document.createElement('iframe');
          let left = 2;
          const timer = setTimeout(() => resolve(false), 2000);
          function done() {
            left -= 1;
            if (left === 0) {
              clearTimeout(timer);
              const sd = f2.contentDocument && f2.contentDocument.getElementById('sd');
              resolve(Boolean(sd && sd.textContent === 'srcdoc-ok'));
            }
          }
          f1.onload = done;
          f2.onload = done;
          f1.src = 'data:text/html,<!doctype html><body><p>data-frame</p></body>';
          f2.srcdoc = '<!doctype html><p id="sd">srcdoc-ok</p>';
          sb.appendChild(f1);
          sb.appendChild(f2);
        });

        const ok = bText === 'blob-ok' && wData === 42 && wBlob === 'cef!' && frameOk;
        addResult('Workers, Blob, Data & Srcdoc Frames', ok, 'dataW=' + wData + ' blobW=' + wBlob + ' frames=' + frameOk);
      } catch (e) {
        addResult('Workers, Blob, Data & Srcdoc Frames', false, e);
      }

      // 5. Storage, CacheStorage & ServiceWorker APIs
      try {
        localStorage.setItem('gcef_bench', '1');
        const lsOk = localStorage.getItem('gcef_bench') === '1';
        const swOk = ('serviceWorker' in navigator) && ('caches' in window);
        const idbOk = await new Promise((resolve) => {
          const req = indexedDB.open('gcef_bench_db', 1);
          req.onsuccess = () => { req.result.close(); resolve(true); };
          req.onerror = () => resolve(false);
        });
        addResult('Storage & ServiceWorker API', lsOk && swOk && idbOk, 'localStorage/IndexedDB/CacheStorage OK');
      } catch (e) {
        addResult('Storage & ServiceWorker API', false, e);
      }

      // 6. JS Bridge & CefSharp Alias
      try {
        const hasBridge = !!(window.rustCef && typeof window.rustCef.postMessage === 'function');
        const hasAlias = !!(window.CefSharp && typeof window.CefSharp.PostMessage === 'function' && typeof window.CefSharp.BindObjectAsync === 'function');
        const bindRes = hasAlias ? await window.CefSharp.BindObjectAsync('test') : null;
        const ok = hasBridge && hasAlias && bindRes && bindRes.Success === true;
        addResult('JS Bridge & CefSharp Shim', ok, 'rustCef + CefSharp.PostMessage + BindObjectAsync');
      } catch (e) {
        addResult('JS Bridge & CefSharp Shim', false, e);
      }

      const passed = results.filter(r => r.ok).length;
      const total = results.length;
      const allOk = passed === total && total > 0;
      window.__benchmarkSummary = { ok: allOk, passed, total, results };
      document.getElementById('summary').textContent = (allOk ? 'ALL PASSED: ' : 'FAILED SOME CHECKS: ') + passed + ' / ' + total;
      document.title = allOk ? 'BenchmarkSuite:PASS' : ('BenchmarkSuite:FAIL:' + passed + '/' + total);
    })();
  </script>
</body>
</html>
"##;

const EXCEPTION_TEST_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8" /><title>Uncaught Exception Test</title></head>
<body style="font-family:Segoe UI,Arial,sans-serif;padding:20px">
  <h2>Uncaught Exception Test</h2>
  <p>Clicking below throws an uncaught JS error so <code>set_on_uncaught_exception</code> logs it in the StatusBar.</p>
  <button onclick="setTimeout(function(){ throw new Error('Simulated uncaught error from ExceptionTest.html'); }, 10);">Trigger Uncaught Exception</button>
</body>
</html>
"##;

const HELLO_WORLD_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8" /><title>Hello World</title></head>
<body style="font-family:Segoe UI,Arial,sans-serif;padding:20px">
  <p>Hello World</p>
  <script>document.write("<p>Testing</p>");</script>
</body>
</html>
"##;

const DIAGNOSTICS_HTML: &[u8] = br##"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8" /><title>Runtime Diagnostics &middot; GpuiCef</title></head>
<body style="font-family:Segoe UI,Arial,sans-serif;padding:24px;line-height:1.6">
  <h2>GpuiCef Runtime Diagnostics</h2>
  <ul id="info"></ul>
  <p>
    <button onclick="console.log('Diagnostic console message @ ' + new Date().toISOString())">Emit console.log</button>
    <button onclick="alert('Hello from GPUI-CEF JsDialogService!')">Test alert()</button>
    <button onclick="prompt('Enter value:', 'CefSharp')">Test prompt()</button>
  </p>
  <script>
    const rows = [
      ['Origin', location.origin],
      ['UserAgent', navigator.userAgent],
      ['DevicePixelRatio', window.devicePixelRatio],
      ['window.rustCef', typeof window.rustCef]
    ];
    const ul = document.getElementById('info');
    for (const [k, v] of rows) {
      const li = document.createElement('li');
      li.innerHTML = '<strong>' + k + ':</strong> ' + String(v);
      ul.appendChild(li);
    }
  </script>
</body>
</html>
"##;

const SAMPLE_DOWNLOAD_BYTES: &[u8] = b"GpuiCef.Example\r\nSample download artifact delivered via ResourceInterceptor.\r\n";

struct ResolvedAsset {
    status_code: u16,
    mime_type: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn lookup_asset(raw_path: &str) -> Option<ResolvedAsset> {
    let path = raw_path.split(['?', '#']).next().unwrap_or(raw_path);
    let normalized = if path.is_empty() || path == "/" {
        "/home.html"
    } else {
        path
    };
    let (status_code, mime_type, headers, body) = if normalized.eq_ignore_ascii_case("/home.html")
        || normalized.eq_ignore_ascii_case("/index.html")
    {
        (200, "text/html", Vec::new(), HOME_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/assets/docs.css") {
        (200, "text/css", Vec::new(), DOCS_CSS.as_bytes().to_vec())
    } else if normalized.eq_ignore_ascii_case("/BindingTest.html")
        || normalized.eq_ignore_ascii_case("/BindingTestNetCore.html")
    {
        (200, "text/html", Vec::new(), BINDING_TEST_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/PostMessageTest.html") {
        (200, "text/html", Vec::new(), POST_MESSAGE_TEST_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/TooltipTest.html") {
        (200, "text/html", Vec::new(), TOOLTIP_TEST_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/DragDropCursorsTest.html") {
        (
            200,
            "text/html",
            Vec::new(),
            DRAG_DROP_CURSORS_TEST_HTML.to_vec(),
        )
    } else if normalized.eq_ignore_ascii_case("/PopupTest.html") {
        (200, "text/html", Vec::new(), POPUP_TEST_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/DraggableRegionTest.html") {
        (
            200,
            "text/html",
            Vec::new(),
            DRAGGABLE_REGION_TEST_HTML.to_vec(),
        )
    } else if normalized.eq_ignore_ascii_case("/SchemeTest.html") {
        (200, "text/html", Vec::new(), SCHEME_TEST_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/PostDataTest.html") {
        (200, "text/html", Vec::new(), POST_DATA_TEST_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/ExceptionTest.html") {
        (200, "text/html", Vec::new(), EXCEPTION_TEST_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/HelloWorld.html") {
        (200, "text/html", Vec::new(), HELLO_WORLD_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/ResponseFilterTest.html") {
        (
            200,
            "text/html",
            Vec::new(),
            response_filter_test_html().to_vec(),
        )
    } else if normalized.eq_ignore_ascii_case("/BenchmarkSuite.html") {
        (200, "text/html", Vec::new(), BENCHMARK_SUITE_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/diagnostics.html") {
        (200, "text/html", Vec::new(), DIAGNOSTICS_HTML.to_vec())
    } else if normalized.eq_ignore_ascii_case("/download/sample-report.txt") {
        (
            200,
            "application/octet-stream",
            vec![(
                "Content-Disposition".into(),
                "attachment; filename=\"gpui-cef-example-report.txt\"".into(),
            )],
            SAMPLE_DOWNLOAD_BYTES.to_vec(),
        )
    } else {
        return None;
    };
    Some(ResolvedAsset {
        status_code,
        mime_type,
        headers,
        body,
    })
}

fn memory_assets_map() -> BTreeMap<String, MemoryAsset> {
    let entries = [
        "home.html",
        "index.html",
        "assets/docs.css",
        "BindingTest.html",
        "BindingTestNetCore.html",
        "PostMessageTest.html",
        "TooltipTest.html",
        "DragDropCursorsTest.html",
        "PopupTest.html",
        "DraggableRegionTest.html",
        "SchemeTest.html",
        "PostDataTest.html",
        "ExceptionTest.html",
        "HelloWorld.html",
        "ResponseFilterTest.html",
        "BenchmarkSuite.html",
        "diagnostics.html",
        "download/sample-report.txt",
    ];
    let mut map = BTreeMap::new();
    for rel in entries {
        let lookup = format!("/{rel}");
        if let Some(asset) = lookup_asset(&lookup) {
            map.insert(
                rel.to_owned(),
                MemoryAsset {
                    status_code: asset.status_code,
                    mime_type: asset.mime_type.to_owned(),
                    headers: asset.headers,
                    body: asset.body,
                },
            );
        }
    }
    map
}

pub fn build_custom_schemes() -> Vec<CustomSchemeConfig> {
    let assets = memory_assets_map();
    vec![
        CustomSchemeConfig {
            scheme: "custom".into(),
            options: CustomSchemeOptions::standard_app(),
            domain: Some("cefsharp".into()),
            asset_source: Some(SchemeAssetSource::Memory(assets.clone())),
        },
        CustomSchemeConfig {
            scheme: "test".into(),
            options: CustomSchemeOptions::standard_app(),
            domain: Some("cefsharp".into()),
            asset_source: Some(SchemeAssetSource::Memory(assets)),
        },
    ]
}

/// Streaming byte-level find/replace filter mirroring CefSharp's
/// `FindReplaceResponseFilter`. Handles split matches across chunk boundaries
/// and small output slices without losing bytes.
pub struct FindReplaceResponseFilter {
    find: Vec<u8>,
    replace: Vec<u8>,
    tail: Vec<u8>,
    ready: VecDeque<u8>,
    eof_flushed: bool,
}

impl FindReplaceResponseFilter {
    pub fn new(find: &[u8], replace: &[u8]) -> Self {
        Self {
            find: find.to_vec(),
            replace: replace.to_vec(),
            tail: Vec::new(),
            ready: VecDeque::new(),
            eof_flushed: false,
        }
    }

    fn process_tail(&mut self) {
        if self.find.is_empty() {
            self.ready.extend(self.tail.drain(..));
            return;
        }
        let needle_len = self.find.len();
        let mut cursor = 0usize;
        while cursor + needle_len <= self.tail.len() {
            if let Some(rel) = self.tail[cursor..]
                .windows(needle_len)
                .position(|w| w == self.find.as_slice())
            {
                let match_start = cursor + rel;
                self.ready.extend(&self.tail[cursor..match_start]);
                self.ready.extend(&self.replace);
                cursor = match_start + needle_len;
            } else {
                break;
            }
        }
        // Keep only the longest trailing slice (length < needle_len) that is a
        // prefix of `self.find` so split matches remain detectable across chunks.
        let remaining = &self.tail[cursor..];
        let max_keep = (needle_len - 1).min(remaining.len());
        let mut keep = 0usize;
        for cand in (1..=max_keep).rev() {
            let suffix = &remaining[remaining.len() - cand..];
            if self.find.starts_with(suffix) {
                keep = cand;
                break;
            }
        }
        let flush_end = remaining.len() - keep;
        self.ready.extend(&remaining[..flush_end]);
        let held = remaining[flush_end..].to_vec();
        self.tail = held;
    }
}

impl ResponseBodyFilter for FindReplaceResponseFilter {
    fn init(&mut self) -> bool {
        self.tail.clear();
        self.ready.clear();
        self.eof_flushed = false;
        true
    }

    fn filter(&mut self, input: &[u8], output: &mut [u8]) -> (usize, usize, ResponseFilterStep) {
        let mut bytes_read = 0usize;
        if input.is_empty() {
            if !self.eof_flushed {
                self.ready.extend(self.tail.drain(..));
                self.eof_flushed = true;
            }
        } else if self.ready.len() < 64 * 1024 {
            self.tail.extend_from_slice(input);
            bytes_read = input.len();
            self.process_tail();
        }

        let writable = output.len().min(self.ready.len());
        for dst in output.iter_mut().take(writable) {
            *dst = self.ready.pop_front().unwrap_or_default();
        }

        let step = if input.is_empty() && self.eof_flushed && self.ready.is_empty() {
            ResponseFilterStep::Done
        } else {
            ResponseFilterStep::NeedMoreData
        };
        (bytes_read, writable, step)
    }
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

pub struct ExampleResourceInterceptor;

impl ResourceInterceptor for ExampleResourceInterceptor {
    fn intercept(&self, request: &InterceptRequest) -> InterceptAction {
        let lower = request.url.to_ascii_lowercase();
        let path_opt = if let Some(rest) = lower.strip_prefix("https://cefsharp.example") {
            let offset = "https://cefsharp.example".len();
            let raw = if rest.is_empty() {
                "/home.html"
            } else {
                &request.url[offset..]
            };
            Some(raw)
        } else if let Some(rest) = lower.strip_prefix("https://cefsharp.com") {
            let offset = "https://cefsharp.com".len();
            let raw = if rest.is_empty() {
                "/PostDataTest.html"
            } else {
                &request.url[offset..]
            };
            Some(raw)
        } else {
            None
        };
        if let Some(raw_path) = path_opt {
            let clean = raw_path.split(['?', '#']).next().unwrap_or(raw_path);
            if clean.eq_ignore_ascii_case("/PostDataAjaxTest.html") {
                let body_str = request
                    .post_body
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                return InterceptAction::Respond {
                    status_code: 200,
                    mime_type: "text/plain".into(),
                    headers: vec![("Access-Control-Allow-Origin".into(), "*".into())],
                    body: format!("Method={} Body={}", request.method, body_str).into_bytes(),
                };
            }
            if clean.eq_ignore_ascii_case("/PostDataTest.html")
                && request.method.eq_ignore_ascii_case("POST")
            {
                let raw_body = request
                    .post_body
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                let html = format!(
                    "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\" />\
                    <title>Post Data Response</title></head>\
                    <body style=\"font-family:Segoe UI,Arial,sans-serif;padding:20px\">\
                    <h1>cefsharp.com POST Echo</h1>\
                    <p>Method: <strong>{}</strong> ({} bytes)</p>\
                    <pre style=\"background:#f5f5f5;padding:12px;border:1px solid #ddd\">{}</pre>\
                    </body></html>",
                    escape_html(&request.method),
                    request.post_body.as_ref().map_or(0, Vec::len),
                    escape_html(&raw_body)
                );
                return InterceptAction::Respond {
                    status_code: 200,
                    mime_type: "text/html".into(),
                    headers: Vec::new(),
                    body: html.into_bytes(),
                };
            }
            if let Some(asset) = lookup_asset(raw_path) {
                return InterceptAction::Respond {
                    status_code: asset.status_code,
                    mime_type: asset.mime_type.to_owned(),
                    headers: asset.headers,
                    body: asset.body,
                };
            }
        }
        InterceptAction::Continue
    }

    fn response_filter(&self, info: &ResourceResponseInfo) -> Option<Box<dyn ResponseBodyFilter>> {
        let clean = info.url.split(['?', '#']).next().unwrap_or(&info.url);
        if clean.to_ascii_lowercase().ends_with("/responsefiltertest.html") {
            return Some(Box::new(FindReplaceResponseFilter::new(
                RESPONSE_FILTER_NEEDLE,
                RESPONSE_FILTER_REPLACEMENT,
            )));
        }
        None
    }
}

pub fn format_runtime_summary(extra: &str) -> String {
    let base = "Chromium: 156.0.8078.80, CEF: 156.0.31, gpui-cef: 0.2.0";
    if extra.is_empty() {
        base.to_owned()
    } else {
        truncate_utf8_bytes(&format!("{base} | {extra}"), 320)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::custom_schemes::validate_intercept_header;

    fn run_filter_streaming(input: &[u8], chunk_in: usize, chunk_out: usize) -> Vec<u8> {
        let mut filter =
            FindReplaceResponseFilter::new(RESPONSE_FILTER_NEEDLE, RESPONSE_FILTER_REPLACEMENT);
        assert!(filter.init());
        let mut out = Vec::new();
        let mut offset = 0usize;
        let mut guard = 0usize;
        loop {
            guard += 1;
            assert!(guard < 100_000, "filter failed to terminate");
            let end = (offset + chunk_in).min(input.len());
            let slice = if offset < input.len() {
                &input[offset..end]
            } else {
                &[]
            };
            let mut buf = vec![0u8; chunk_out];
            let (read, written, step) = filter.filter(slice, &mut buf);
            offset += read;
            out.extend_from_slice(&buf[..written]);
            if slice.is_empty() && step == ResponseFilterStep::Done {
                break;
            }
            assert_ne!(step, ResponseFilterStep::Error);
        }
        out
    }

    #[test]
    fn find_replace_response_filter_handles_split_needles_and_large_document() {
        let split_sample = b"head REPLACE_THIS_STRING mid REPL\x41CE_THIS_STRING tail";
        // Chunk size 7 guarantees split across "REPLACE_THIS_STRING" and small 11-byte output buffers.
        let filtered = run_filter_streaming(split_sample, 7, 11);
        let text = String::from_utf8(filtered).expect("utf8");
        assert_eq!(
            text,
            "head This is the replaced string! mid This is the replaced string! tail"
        );

        let full_doc = response_filter_test_html();
        assert!(full_doc.len() > 32 * 1024);
        let filtered_doc = run_filter_streaming(full_doc, 509, 257);
        let filtered_str = String::from_utf8(filtered_doc).expect("utf8");
        assert!(!filtered_str.contains("REPLACE_THIS_STRING"));
        assert!(filtered_str.contains("This is the replaced string!"));
    }

    #[test]
    fn interceptor_serves_home_and_download_with_valid_headers() {
        assert!(validate_intercept_header(
            "Content-Disposition",
            "attachment; filename=\"gpui-cef-example-report.txt\""
        ));
        let interceptor = ExampleResourceInterceptor;
        let home = interceptor.intercept(&InterceptRequest {
            url: DEFAULT_HOME_URL.into(),
            method: "GET".into(),
            is_main_frame: true,
            post_body: None,
        });
        match home {
            InterceptAction::Respond {
                status_code,
                mime_type,
                body,
                ..
            } => {
                assert_eq!(status_code, 200);
                assert_eq!(mime_type, "text/html");
                assert!(String::from_utf8_lossy(&body).contains("Getting started"));
            }
            other => panic!("unexpected action: {other:?}"),
        }

        let post_resp = interceptor.intercept(&InterceptRequest {
            url: CUSTOM_POST_URL.into(),
            method: "POST".into(),
            is_main_frame: true,
            post_body: Some(b"user=gpui&mode=custom".to_vec()),
        });
        match post_resp {
            InterceptAction::Respond {
                status_code, body, ..
            } => {
                assert_eq!(status_code, 200);
                let text = String::from_utf8_lossy(&body);
                assert!(text.contains("POST"));
                assert!(text.contains("user=gpui&amp;mode=custom"));
            }
            other => panic!("unexpected post response: {other:?}"),
        }

        let ajax_resp = interceptor.intercept(&InterceptRequest {
            url: "https://cefsharp.com/PostDataAjaxTest.html".into(),
            method: "POST".into(),
            is_main_frame: false,
            post_body: Some(b"ajax=ok".to_vec()),
        });
        match ajax_resp {
            InterceptAction::Respond {
                status_code,
                mime_type,
                body,
                ..
            } => {
                assert_eq!(status_code, 200);
                assert_eq!(mime_type, "text/plain");
                assert_eq!(String::from_utf8_lossy(&body), "Method=POST Body=ajax=ok");
            }
            other => panic!("unexpected ajax response: {other:?}"),
        }

        let bench_resp = interceptor.intercept(&InterceptRequest {
            url: BENCHMARK_SUITE_URL.into(),
            method: "GET".into(),
            is_main_frame: true,
            post_body: None,
        });
        assert!(matches!(bench_resp, InterceptAction::Respond { status_code: 200, .. }));
        let rf = interceptor.response_filter(&ResourceResponseInfo {
            url: RESPONSE_FILTER_TEST_URL.into(),
            method: "GET".into(),
            is_main_frame: true,
            status_code: 200,
            mime_type: "text/html".into(),
        });
        assert!(rf.is_some());
    }
}
