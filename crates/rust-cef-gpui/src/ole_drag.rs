//! Zero-new-dependency Win32 COM OLE `DoDragDrop` runner for OSR outbound drags.
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Wpf/ChromiumWebBrowser.cs:919-973` (non-blocking async `DoDragDrop`
//!   + `DragSourceEndedAt` / `DragSourceSystemDragEnded`).
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    mem::size_of,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    rc::Rc,
    sync::atomic::{AtomicU32, Ordering},
};

use rust_cef_core::outbound_drag::{
    DRAG_OP_COPY, DRAG_OP_LINK, DRAG_OP_MOVE, DragOperationMask, DraggedPayload,
    OutboundDragFinish, OutboundDragStart,
};

pub type IntegratedPumpCallback = Rc<dyn Fn()>;

const S_OK: i32 = 0;
const S_FALSE: i32 = 1;
const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const E_POINTER: i32 = 0x8000_4003u32 as i32;
const E_OUTOFMEMORY: i32 = 0x8007_000Eu32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
const DV_E_FORMATETC: i32 = 0x8004_0064u32 as i32;
const OLE_E_ADVISENOTSUPPORTED: i32 = 0x8004_0003u32 as i32;

const DRAGDROP_S_DROP: i32 = 0x0004_0100u32 as i32;
const DRAGDROP_S_CANCEL: i32 = 0x0004_0101u32 as i32;
const DRAGDROP_S_USEDEFAULTCURSORS: i32 = 0x0004_0102u32 as i32;

const DROPEFFECT_NONE: u32 = 0;
const DROPEFFECT_COPY: u32 = 1;
const DROPEFFECT_MOVE: u32 = 2;
const DROPEFFECT_LINK: u32 = 4;

const MK_LBUTTON: u32 = 0x0001;
const MK_RBUTTON: u32 = 0x0002;
const MK_MBUTTON: u32 = 0x0010;

const DATADIR_GET: u32 = 1;
const DVASPECT_CONTENT: u32 = 1;
const TYMED_HGLOBAL: u32 = 1;
const GMEM_MOVEABLE: u32 = 0x0002;
const GMEM_ZEROINIT: u32 = 0x0040;

const CF_TEXT: u16 = 1;
const CF_UNICODETEXT: u16 = 13;
const CF_HDROP: u16 = 15;

#[link(name = "ole32")]
unsafe extern "system" {
    fn OleInitialize(pv_reserved: *mut c_void) -> i32;
    fn OleUninitialize();
    fn DoDragDrop(
        data_object: *mut c_void,
        drop_source: *mut c_void,
        ok_effects: u32,
        out_effect: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(mem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(mem: *mut c_void) -> i32;
    fn GlobalFree(mem: *mut c_void) -> *mut c_void;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClipboardFormatW(format: *const u16) -> u32;
    fn GetCursorPos(point: *mut Point) -> i32;
    fn ScreenToClient(hwnd: *mut c_void, point: *mut Point) -> i32;
    fn SetTimer(
        hwnd: *mut c_void,
        id_event: usize,
        elapse: u32,
        timer_func: Option<unsafe extern "system" fn(*mut c_void, u32, usize, u32)>,
    ) -> usize;
    fn KillTimer(hwnd: *mut c_void, id_event: usize) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const IID_IUNKNOWN: Guid = Guid {
    data1: 0x0000_0000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

const IID_IDROPSOURCE: Guid = Guid {
    data1: 0x0000_0121,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

const IID_IDATAOBJECT: Guid = Guid {
    data1: 0x0000_010E,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

const IID_IENUMFORMATETC: Guid = Guid {
    data1: 0x0000_0103,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

#[repr(C)]
#[derive(Clone, Copy)]
struct FormatEtc {
    cf_format: u16,
    ptd: *mut c_void,
    dw_aspect: u32,
    lindex: i32,
    tymed: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StgMedium {
    tymed: u32,
    data: *mut c_void,
    p_unk_for_release: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DropFiles {
    p_files: u32,
    pt_x: i32,
    pt_y: i32,
    f_nc: i32,
    f_wide: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

pub fn core_mask_to_dropeffect(mask: DragOperationMask) -> u32 {
    let mut effects = DROPEFFECT_NONE;
    if mask.contains(DRAG_OP_COPY) {
        effects |= DROPEFFECT_COPY;
    }
    if mask.contains(DRAG_OP_MOVE) {
        effects |= DROPEFFECT_MOVE;
    }
    if mask.contains(DRAG_OP_LINK) {
        effects |= DROPEFFECT_LINK;
    }
    effects
}

pub fn dropeffect_to_core_mask(effect: u32) -> DragOperationMask {
    let mut bits = 0u32;
    if (effect & DROPEFFECT_COPY) != 0 {
        bits |= DRAG_OP_COPY;
    }
    if (effect & DROPEFFECT_MOVE) != 0 {
        bits |= DRAG_OP_MOVE;
    }
    if (effect & DROPEFFECT_LINK) != 0 {
        bits |= DRAG_OP_LINK;
    }
    DragOperationMask(bits)
}

/// Formats an HTML fragment as Windows `HTML Format` (`CF_HTML`).
pub fn format_cf_html(html: &str) -> Vec<u8> {
    let header_len = concat!(
        "Version:0.9\r\n",
        "StartHTML:0000000000\r\n",
        "EndHTML:0000000000\r\n",
        "StartFragment:0000000000\r\n",
        "EndFragment:0000000000\r\n",
    )
    .len();
    let prefix = "<html><body><!--StartFragment-->";
    let suffix = "<!--EndFragment--></body></html>";
    let start_html = header_len;
    let start_fragment = start_html + prefix.len();
    let end_fragment = start_fragment + html.len();
    let end_html = end_fragment + suffix.len();

    let mut out = String::with_capacity(end_html + 1);
    out.push_str(&format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    ));
    out.push_str(prefix);
    out.push_str(html);
    out.push_str(suffix);
    let mut bytes = out.into_bytes();
    bytes.push(0);
    bytes
}

/// Builds a Unicode `DROPFILES` + double-NUL-terminated UTF-16LE buffer for `CF_HDROP`.
pub fn build_hdrop_bytes(paths: &[String]) -> Vec<u8> {
    let header = DropFiles {
        p_files: size_of::<DropFiles>() as u32,
        pt_x: 0,
        pt_y: 0,
        f_nc: 0,
        f_wide: 1,
    };
    let mut out = Vec::with_capacity(size_of::<DropFiles>() + paths.len() * 32 + 2);
    // SAFETY: `DropFiles` is `#[repr(C)]` plain integers without padding.
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            (&header as *const DropFiles).cast::<u8>(),
            size_of::<DropFiles>(),
        )
    };
    out.extend_from_slice(header_bytes);
    for path in paths {
        for unit in path.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn format_utf16le_nul(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len().saturating_mul(2) + 2);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes
}

fn format_ansi_nul(text: &str) -> Vec<u8> {
    let mut bytes: Vec<u8> = text
        .bytes()
        .map(|b| if b == 0 { b' ' } else { b })
        .collect();
    bytes.push(0);
    bytes
}

fn alloc_hglobal(bytes: &[u8]) -> Result<*mut c_void, i32> {
    let len = bytes.len().max(1);
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, len) };
    if hglobal.is_null() {
        return Err(E_OUTOFMEMORY);
    }
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        unsafe {
            GlobalFree(hglobal);
        }
        return Err(E_OUTOFMEMORY);
    }
    if !bytes.is_empty() {
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        }
    }
    unsafe {
        GlobalUnlock(hglobal);
    }
    Ok(hglobal)
}

fn register_clip_format(name: &str) -> u16 {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let id = unsafe { RegisterClipboardFormatW(wide.as_ptr()) };
    u16::try_from(id).unwrap_or(0)
}

#[repr(C)]
struct DropSourceVtbl {
    query_interface:
        unsafe extern "system" fn(*mut DropSource, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut DropSource) -> u32,
    release: unsafe extern "system" fn(*mut DropSource) -> u32,
    query_continue_drag: unsafe extern "system" fn(*mut DropSource, i32, u32) -> i32,
    give_feedback: unsafe extern "system" fn(*mut DropSource, u32) -> i32,
}

#[repr(C)]
struct DropSource {
    vtbl: *const DropSourceVtbl,
    ref_count: AtomicU32,
}

static DROP_SOURCE_VTBL: DropSourceVtbl = DropSourceVtbl {
    query_interface: drop_source_qi,
    add_ref: drop_source_add_ref,
    release: drop_source_release,
    query_continue_drag: drop_source_query_continue,
    give_feedback: drop_source_give_feedback,
};

impl DropSource {
    fn new_raw() -> *mut Self {
        Box::into_raw(Box::new(Self {
            vtbl: &DROP_SOURCE_VTBL,
            ref_count: AtomicU32::new(1),
        }))
    }
}

unsafe extern "system" fn drop_source_qi(
    this: *mut DropSource,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> i32 {
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe { *ppv = ptr::null_mut() };
    if this.is_null() || riid.is_null() {
        return E_INVALIDARG;
    }
    let iid = unsafe { *riid };
    if iid == IID_IUNKNOWN || iid == IID_IDROPSOURCE {
        unsafe { *ppv = this.cast::<c_void>() };
        unsafe { drop_source_add_ref(this) };
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "system" fn drop_source_add_ref(this: *mut DropSource) -> u32 {
    if this.is_null() {
        return 0;
    }
    unsafe { (*this).ref_count.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn drop_source_release(this: *mut DropSource) -> u32 {
    if this.is_null() {
        return 0;
    }
    let prev = unsafe { (*this).ref_count.fetch_sub(1, Ordering::Release) };
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(unsafe { Box::from_raw(this) });
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn drop_source_query_continue(
    _this: *mut DropSource,
    escape_pressed: i32,
    key_state: u32,
) -> i32 {
    if escape_pressed != 0 {
        DRAGDROP_S_CANCEL
    } else if (key_state & (MK_LBUTTON | MK_RBUTTON | MK_MBUTTON)) == 0 {
        DRAGDROP_S_DROP
    } else {
        S_OK
    }
}

unsafe extern "system" fn drop_source_give_feedback(_this: *mut DropSource, _effect: u32) -> i32 {
    DRAGDROP_S_USEDEFAULTCURSORS
}

#[repr(C)]
struct EnumFormatEtcVtbl {
    query_interface:
        unsafe extern "system" fn(*mut EnumFormatEtc, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut EnumFormatEtc) -> u32,
    release: unsafe extern "system" fn(*mut EnumFormatEtc) -> u32,
    next: unsafe extern "system" fn(*mut EnumFormatEtc, u32, *mut FormatEtc, *mut u32) -> i32,
    skip: unsafe extern "system" fn(*mut EnumFormatEtc, u32) -> i32,
    reset: unsafe extern "system" fn(*mut EnumFormatEtc) -> i32,
    clone: unsafe extern "system" fn(*mut EnumFormatEtc, *mut *mut c_void) -> i32,
}

#[repr(C)]
struct EnumFormatEtc {
    vtbl: *const EnumFormatEtcVtbl,
    ref_count: AtomicU32,
    formats: Vec<FormatEtc>,
    index: Cell<usize>,
}

static ENUM_FORMAT_ETC_VTBL: EnumFormatEtcVtbl = EnumFormatEtcVtbl {
    query_interface: enum_fmt_qi,
    add_ref: enum_fmt_add_ref,
    release: enum_fmt_release,
    next: enum_fmt_next,
    skip: enum_fmt_skip,
    reset: enum_fmt_reset,
    clone: enum_fmt_clone,
};

impl EnumFormatEtc {
    fn new_raw(formats: Vec<FormatEtc>, index: usize) -> *mut Self {
        Box::into_raw(Box::new(Self {
            vtbl: &ENUM_FORMAT_ETC_VTBL,
            ref_count: AtomicU32::new(1),
            formats,
            index: Cell::new(index),
        }))
    }
}

unsafe extern "system" fn enum_fmt_qi(
    this: *mut EnumFormatEtc,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> i32 {
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe { *ppv = ptr::null_mut() };
    if this.is_null() || riid.is_null() {
        return E_INVALIDARG;
    }
    let iid = unsafe { *riid };
    if iid == IID_IUNKNOWN || iid == IID_IENUMFORMATETC {
        unsafe { *ppv = this.cast::<c_void>() };
        unsafe { enum_fmt_add_ref(this) };
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "system" fn enum_fmt_add_ref(this: *mut EnumFormatEtc) -> u32 {
    if this.is_null() {
        return 0;
    }
    unsafe { (*this).ref_count.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn enum_fmt_release(this: *mut EnumFormatEtc) -> u32 {
    if this.is_null() {
        return 0;
    }
    let prev = unsafe { (*this).ref_count.fetch_sub(1, Ordering::Release) };
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(unsafe { Box::from_raw(this) });
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn enum_fmt_next(
    this: *mut EnumFormatEtc,
    celt: u32,
    rgelt: *mut FormatEtc,
    pcelt_fetched: *mut u32,
) -> i32 {
    if this.is_null() || (celt > 0 && rgelt.is_null()) {
        return E_INVALIDARG;
    }
    if celt > 1 && pcelt_fetched.is_null() {
        return E_INVALIDARG;
    }
    let obj = unsafe { &*this };
    let start = obj.index.get();
    let remaining = obj.formats.len().saturating_sub(start);
    let count = (celt as usize).min(remaining);
    for offset in 0..count {
        unsafe {
            ptr::write(rgelt.add(offset), obj.formats[start + offset]);
        }
    }
    obj.index.set(start + count);
    if !pcelt_fetched.is_null() {
        unsafe { *pcelt_fetched = count as u32 };
    }
    if count == celt as usize {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe extern "system" fn enum_fmt_skip(this: *mut EnumFormatEtc, celt: u32) -> i32 {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let obj = unsafe { &*this };
    let next = obj.index.get().saturating_add(celt as usize);
    if next <= obj.formats.len() {
        obj.index.set(next);
        S_OK
    } else {
        obj.index.set(obj.formats.len());
        S_FALSE
    }
}

unsafe extern "system" fn enum_fmt_reset(this: *mut EnumFormatEtc) -> i32 {
    if this.is_null() {
        return E_INVALIDARG;
    }
    unsafe { (*this).index.set(0) };
    S_OK
}

unsafe extern "system" fn enum_fmt_clone(
    this: *mut EnumFormatEtc,
    ppenum: *mut *mut c_void,
) -> i32 {
    if ppenum.is_null() {
        return E_POINTER;
    }
    unsafe { *ppenum = ptr::null_mut() };
    if this.is_null() {
        return E_INVALIDARG;
    }
    let obj = unsafe { &*this };
    let cloned = EnumFormatEtc::new_raw(obj.formats.clone(), obj.index.get());
    unsafe { *ppenum = cloned.cast::<c_void>() };
    S_OK
}

struct FormatEntry {
    format: FormatEtc,
    bytes: Vec<u8>,
}

#[repr(C)]
struct DataObjectVtbl {
    query_interface:
        unsafe extern "system" fn(*mut DataObject, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut DataObject) -> u32,
    release: unsafe extern "system" fn(*mut DataObject) -> u32,
    get_data: unsafe extern "system" fn(*mut DataObject, *const FormatEtc, *mut StgMedium) -> i32,
    get_data_here:
        unsafe extern "system" fn(*mut DataObject, *const FormatEtc, *mut StgMedium) -> i32,
    query_get_data: unsafe extern "system" fn(*mut DataObject, *const FormatEtc) -> i32,
    get_canonical_format_etc:
        unsafe extern "system" fn(*mut DataObject, *const FormatEtc, *mut FormatEtc) -> i32,
    set_data:
        unsafe extern "system" fn(*mut DataObject, *const FormatEtc, *const StgMedium, i32) -> i32,
    enum_format_etc: unsafe extern "system" fn(*mut DataObject, u32, *mut *mut c_void) -> i32,
    d_advise: unsafe extern "system" fn(
        *mut DataObject,
        *const FormatEtc,
        u32,
        *mut c_void,
        *mut u32,
    ) -> i32,
    d_unadvise: unsafe extern "system" fn(*mut DataObject, u32) -> i32,
    enum_d_advise: unsafe extern "system" fn(*mut DataObject, *mut *mut c_void) -> i32,
}

#[repr(C)]
struct DataObject {
    vtbl: *const DataObjectVtbl,
    ref_count: AtomicU32,
    entries: Vec<FormatEntry>,
}

static DATA_OBJECT_VTBL: DataObjectVtbl = DataObjectVtbl {
    query_interface: data_object_qi,
    add_ref: data_object_add_ref,
    release: data_object_release,
    get_data: data_object_get_data,
    get_data_here: data_object_get_data_here,
    query_get_data: data_object_query_get_data,
    get_canonical_format_etc: data_object_get_canonical,
    set_data: data_object_set_data,
    enum_format_etc: data_object_enum_format_etc,
    d_advise: data_object_d_advise,
    d_unadvise: data_object_d_unadvise,
    enum_d_advise: data_object_enum_d_advise,
};

impl DataObject {
    fn new_raw(payload: &DraggedPayload, cf_html: u16, cf_url_w: u16) -> *mut Self {
        let mut entries = Vec::new();
        let make_fmt = |cf_format: u16| FormatEtc {
            cf_format,
            ptd: ptr::null_mut(),
            dw_aspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL,
        };
        if let Some(plain) = payload.text.as_deref().or(payload.url.as_deref()) {
            entries.push(FormatEntry {
                format: make_fmt(CF_UNICODETEXT),
                bytes: format_utf16le_nul(plain),
            });
            entries.push(FormatEntry {
                format: make_fmt(CF_TEXT),
                bytes: format_ansi_nul(plain),
            });
        }
        if !payload.file_paths.is_empty() {
            entries.push(FormatEntry {
                format: make_fmt(CF_HDROP),
                bytes: build_hdrop_bytes(&payload.file_paths),
            });
        }
        if cf_html != 0
            && let Some(html) = payload.html.as_deref()
        {
            entries.push(FormatEntry {
                format: make_fmt(cf_html),
                bytes: format_cf_html(html),
            });
        }
        if cf_url_w != 0
            && let Some(url) = payload.url.as_deref()
        {
            entries.push(FormatEntry {
                format: make_fmt(cf_url_w),
                bytes: format_utf16le_nul(url),
            });
        }
        Box::into_raw(Box::new(Self {
            vtbl: &DATA_OBJECT_VTBL,
            ref_count: AtomicU32::new(1),
            entries,
        }))
    }

    fn find_entry(&self, req: &FormatEtc) -> Option<&FormatEntry> {
        if req.dw_aspect != DVASPECT_CONTENT || (req.tymed & TYMED_HGLOBAL) == 0 {
            return None;
        }
        self.entries
            .iter()
            .find(|entry| entry.format.cf_format == req.cf_format)
    }
}

unsafe extern "system" fn data_object_qi(
    this: *mut DataObject,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> i32 {
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe { *ppv = ptr::null_mut() };
    if this.is_null() || riid.is_null() {
        return E_INVALIDARG;
    }
    let iid = unsafe { *riid };
    if iid == IID_IUNKNOWN || iid == IID_IDATAOBJECT {
        unsafe { *ppv = this.cast::<c_void>() };
        unsafe { data_object_add_ref(this) };
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "system" fn data_object_add_ref(this: *mut DataObject) -> u32 {
    if this.is_null() {
        return 0;
    }
    unsafe { (*this).ref_count.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn data_object_release(this: *mut DataObject) -> u32 {
    if this.is_null() {
        return 0;
    }
    let prev = unsafe { (*this).ref_count.fetch_sub(1, Ordering::Release) };
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(unsafe { Box::from_raw(this) });
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn data_object_get_data(
    this: *mut DataObject,
    pformatetc_in: *const FormatEtc,
    pmedium: *mut StgMedium,
) -> i32 {
    if this.is_null() || pformatetc_in.is_null() || pmedium.is_null() {
        return E_INVALIDARG;
    }
    unsafe {
        *pmedium = StgMedium {
            tymed: 0,
            data: ptr::null_mut(),
            p_unk_for_release: ptr::null_mut(),
        };
    }
    let req = unsafe { &*pformatetc_in };
    let obj = unsafe { &*this };
    let Some(entry) = obj.find_entry(req) else {
        return DV_E_FORMATETC;
    };
    match alloc_hglobal(&entry.bytes) {
        Ok(hglobal) => {
            unsafe {
                *pmedium = StgMedium {
                    tymed: TYMED_HGLOBAL,
                    data: hglobal,
                    p_unk_for_release: ptr::null_mut(),
                };
            }
            S_OK
        }
        Err(hr) => hr,
    }
}

unsafe extern "system" fn data_object_get_data_here(
    _this: *mut DataObject,
    _pformatetc: *const FormatEtc,
    _pmedium: *mut StgMedium,
) -> i32 {
    E_NOTIMPL
}

unsafe extern "system" fn data_object_query_get_data(
    this: *mut DataObject,
    pformatetc: *const FormatEtc,
) -> i32 {
    if this.is_null() || pformatetc.is_null() {
        return E_INVALIDARG;
    }
    let req = unsafe { &*pformatetc };
    if unsafe { &*this }.find_entry(req).is_some() {
        S_OK
    } else {
        DV_E_FORMATETC
    }
}

unsafe extern "system" fn data_object_get_canonical(
    _this: *mut DataObject,
    _pformatetc_in: *const FormatEtc,
    pformatetc_out: *mut FormatEtc,
) -> i32 {
    if !pformatetc_out.is_null() {
        unsafe {
            *pformatetc_out = FormatEtc {
                cf_format: 0,
                ptd: ptr::null_mut(),
                dw_aspect: 0,
                lindex: -1,
                tymed: 0,
            };
        }
    }
    E_NOTIMPL
}

unsafe extern "system" fn data_object_set_data(
    _this: *mut DataObject,
    _pformatetc: *const FormatEtc,
    _pmedium: *const StgMedium,
    _f_release: i32,
) -> i32 {
    E_NOTIMPL
}

unsafe extern "system" fn data_object_enum_format_etc(
    this: *mut DataObject,
    dw_direction: u32,
    ppenum_formatetc: *mut *mut c_void,
) -> i32 {
    if ppenum_formatetc.is_null() {
        return E_POINTER;
    }
    unsafe { *ppenum_formatetc = ptr::null_mut() };
    if this.is_null() {
        return E_INVALIDARG;
    }
    if dw_direction != DATADIR_GET {
        return E_NOTIMPL;
    }
    let formats = unsafe { &*this }
        .entries
        .iter()
        .map(|entry| entry.format)
        .collect();
    let enumerator = EnumFormatEtc::new_raw(formats, 0);
    unsafe { *ppenum_formatetc = enumerator.cast::<c_void>() };
    S_OK
}

unsafe extern "system" fn data_object_d_advise(
    _this: *mut DataObject,
    _pformatetc: *const FormatEtc,
    _advf: u32,
    _p_adv_sink: *mut c_void,
    _pdw_connection: *mut u32,
) -> i32 {
    OLE_E_ADVISENOTSUPPORTED
}

unsafe extern "system" fn data_object_d_unadvise(
    _this: *mut DataObject,
    _dw_connection: u32,
) -> i32 {
    OLE_E_ADVISENOTSUPPORTED
}

unsafe extern "system" fn data_object_enum_d_advise(
    _this: *mut DataObject,
    ppenum_advise: *mut *mut c_void,
) -> i32 {
    if !ppenum_advise.is_null() {
        unsafe { *ppenum_advise = ptr::null_mut() };
    }
    OLE_E_ADVISENOTSUPPORTED
}

thread_local! {
    static DRAG_PUMP_CALLBACK: RefCell<Option<IntegratedPumpCallback>> = RefCell::new(None);
    static DRAG_PUMP_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "system" fn drag_pump_timer_proc(
    _hwnd: *mut c_void,
    _msg: u32,
    _timer_id: usize,
    _time: u32,
) {
    let entered = DRAG_PUMP_ACTIVE.with(|flag| {
        if flag.get() {
            false
        } else {
            flag.set(true);
            true
        }
    });
    if !entered {
        return;
    }
    let cb = DRAG_PUMP_CALLBACK.with(|slot| slot.borrow().clone());
    if let Some(cb) = cb {
        let _ = catch_unwind(AssertUnwindSafe(|| cb()));
    }
    DRAG_PUMP_ACTIVE.with(|flag| flag.set(false));
}

struct DragPumpGuard {
    timer_id: usize,
}

impl DragPumpGuard {
    fn arm(pump: IntegratedPumpCallback) -> Self {
        DRAG_PUMP_CALLBACK.with(|slot| *slot.borrow_mut() = Some(pump));
        let timer_id = unsafe { SetTimer(ptr::null_mut(), 0, 16, Some(drag_pump_timer_proc)) };
        Self { timer_id }
    }
}

impl Drop for DragPumpGuard {
    fn drop(&mut self) {
        if self.timer_id != 0 {
            unsafe {
                KillTimer(ptr::null_mut(), self.timer_id);
            }
        }
        DRAG_PUMP_CALLBACK.with(|slot| *slot.borrow_mut() = None);
        DRAG_PUMP_ACTIVE.with(|flag| flag.set(false));
    }
}

/// Runs a modal OLE drag-and-drop loop on the UI/STA thread after `App` borrows end.
pub fn run_ole_drag(
    start: &OutboundDragStart,
    hwnd: Option<*mut c_void>,
    integrated_pump: Option<IntegratedPumpCallback>,
) -> OutboundDragFinish {
    if start.payload.is_empty() {
        return OutboundDragFinish {
            drag_id: start.drag_id,
            x: start.x,
            y: start.y,
            operation: DragOperationMask::NONE,
        };
    }

    let ole_hr = unsafe { OleInitialize(ptr::null_mut()) };
    let ole_initialized = ole_hr == S_OK || ole_hr == S_FALSE;
    if ole_hr < 0 {
        return OutboundDragFinish {
            drag_id: start.drag_id,
            x: start.x,
            y: start.y,
            operation: DragOperationMask::NONE,
        };
    }

    let cf_html = register_clip_format("HTML Format");
    let cf_url_w = register_clip_format("UniformResourceLocatorW");
    let data_object = DataObject::new_raw(&start.payload, cf_html, cf_url_w);
    let drop_source = DropSource::new_raw();

    let mut effect = DROPEFFECT_NONE;
    let ok_effects = core_mask_to_dropeffect(start.allowed_ops);
    let _pump_guard = integrated_pump.map(DragPumpGuard::arm);
    let hr = unsafe {
        DoDragDrop(
            data_object.cast::<c_void>(),
            drop_source.cast::<c_void>(),
            ok_effects,
            &mut effect,
        )
    };
    drop(_pump_guard);

    unsafe {
        drop_source_release(drop_source);
        data_object_release(data_object);
    }

    let (x, y) = match hwnd.filter(|ptr| !ptr.is_null()) {
        Some(hwnd) => {
            let mut pt = Point::default();
            if unsafe { GetCursorPos(&mut pt) } != 0
                && unsafe { ScreenToClient(hwnd, &mut pt) } != 0
            {
                (pt.x, pt.y)
            } else {
                (start.x, start.y)
            }
        }
        None => (start.x, start.y),
    };

    if ole_initialized {
        unsafe {
            OleUninitialize();
        }
    }

    let operation = if hr == DRAGDROP_S_DROP {
        dropeffect_to_core_mask(effect)
    } else {
        DragOperationMask::NONE
    };

    OutboundDragFinish {
        drag_id: start.drag_id,
        x,
        y,
        operation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win64_com_abi_struct_sizes_match() {
        assert_eq!(size_of::<Guid>(), 16);
        assert_eq!(size_of::<FormatEtc>(), 32);
        assert_eq!(size_of::<StgMedium>(), 24);
        assert_eq!(size_of::<DropFiles>(), 20);
    }

    #[test]
    fn dropeffect_and_core_mask_round_trip() {
        let mask = DragOperationMask(DRAG_OP_COPY | DRAG_OP_MOVE | DRAG_OP_LINK);
        let eff = core_mask_to_dropeffect(mask);
        assert_eq!(eff, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK);
        assert_eq!(dropeffect_to_core_mask(eff), mask);
        assert_eq!(
            dropeffect_to_core_mask(DROPEFFECT_NONE),
            DragOperationMask::NONE
        );
    }

    #[test]
    fn cf_html_offsets_match_header() {
        let bytes = format_cf_html("<b>hi</b>");
        assert_eq!(bytes.last().copied(), Some(0));
        let text = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
        let start_html: usize = text
            .lines()
            .find_map(|l| l.strip_prefix("StartHTML:"))
            .unwrap()
            .parse()
            .unwrap();
        let end_html: usize = text
            .lines()
            .find_map(|l| l.strip_prefix("EndHTML:"))
            .unwrap()
            .parse()
            .unwrap();
        let start_frag: usize = text
            .lines()
            .find_map(|l| l.strip_prefix("StartFragment:"))
            .unwrap()
            .parse()
            .unwrap();
        let end_frag: usize = text
            .lines()
            .find_map(|l| l.strip_prefix("EndFragment:"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(&text[start_frag..end_frag], "<b>hi</b>");
        assert_eq!(
            &text[start_html..end_html],
            "<html><body><!--StartFragment--><b>hi</b><!--EndFragment--></body></html>"
        );
    }

    #[test]
    fn hdrop_encodes_dropfiles_and_double_nul_utf16_paths() {
        let bytes = build_hdrop_bytes(&["C:\\a.txt".into(), "C:\\b.txt".into()]);
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            size_of::<DropFiles>() as u32
        );
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 1);
        let units: Vec<u16> = bytes[size_of::<DropFiles>()..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let mut expected: Vec<u16> = "C:\\a.txt".encode_utf16().chain(Some(0)).collect();
        expected.extend("C:\\b.txt".encode_utf16());
        expected.push(0);
        expected.push(0);
        assert_eq!(units, expected);
    }

    #[test]
    fn drop_source_transitions_follow_ole_contract() {
        let src = DropSource::new_raw();
        unsafe {
            assert_eq!(
                drop_source_query_continue(src, 1, MK_LBUTTON),
                DRAGDROP_S_CANCEL
            );
            assert_eq!(drop_source_query_continue(src, 0, 0), DRAGDROP_S_DROP);
            assert_eq!(drop_source_query_continue(src, 0, MK_LBUTTON), S_OK);
            assert_eq!(
                drop_source_give_feedback(src, DROPEFFECT_COPY),
                DRAGDROP_S_USEDEFAULTCURSORS
            );
            assert_eq!(drop_source_release(src), 0);
        }
    }

    #[test]
    fn data_object_and_enum_format_etc_com_lifecycle() {
        let payload = DraggedPayload::bounded(
            Some("hello"),
            Some("<i>hi</i>"),
            Some("https://example.com"),
            vec!["C:\\file.txt".into()],
        );
        let cf_html = 0xC100;
        let cf_url_w = 0xC101;
        let obj = DataObject::new_raw(&payload, cf_html, cf_url_w);
        unsafe {
            let mut queried: *mut c_void = ptr::null_mut();
            assert_eq!(data_object_qi(obj, &IID_IDATAOBJECT, &mut queried), S_OK);
            assert_eq!(queried, obj.cast::<c_void>());
            assert_eq!(data_object_release(obj), 1);

            let fmt_unicode = FormatEtc {
                cf_format: CF_UNICODETEXT,
                ptd: ptr::null_mut(),
                dw_aspect: DVASPECT_CONTENT,
                lindex: -1,
                tymed: TYMED_HGLOBAL,
            };
            assert_eq!(data_object_query_get_data(obj, &fmt_unicode), S_OK);

            let mut medium = StgMedium {
                tymed: 0,
                data: ptr::null_mut(),
                p_unk_for_release: ptr::null_mut(),
            };
            assert_eq!(data_object_get_data(obj, &fmt_unicode, &mut medium), S_OK);
            assert_eq!(medium.tymed, TYMED_HGLOBAL);
            assert!(!medium.data.is_null());
            GlobalFree(medium.data);

            let mut enum_ptr: *mut c_void = ptr::null_mut();
            assert_eq!(
                data_object_enum_format_etc(obj, DATADIR_GET, &mut enum_ptr),
                S_OK
            );
            let enumerator = enum_ptr.cast::<EnumFormatEtc>();
            let mut out = [FormatEtc {
                cf_format: 0,
                ptd: ptr::null_mut(),
                dw_aspect: 0,
                lindex: 0,
                tymed: 0,
            }; 8];
            let mut fetched = 0u32;
            assert_eq!(
                enum_fmt_next(enumerator, 8, out.as_mut_ptr(), &mut fetched),
                S_FALSE
            );
            assert_eq!(fetched, 5);
            assert_eq!(enum_fmt_reset(enumerator), S_OK);

            let mut cloned_ptr: *mut c_void = ptr::null_mut();
            assert_eq!(enum_fmt_clone(enumerator, &mut cloned_ptr), S_OK);
            assert_eq!(enum_fmt_release(cloned_ptr.cast::<EnumFormatEtc>()), 0);
            assert_eq!(enum_fmt_release(enumerator), 0);
            assert_eq!(data_object_release(obj), 0);
        }
    }
}
