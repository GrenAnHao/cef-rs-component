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
    DRAG_OP_COPY, DRAG_OP_LINK, DRAG_OP_MOVE, DragImage, DragOperationMask, DraggedPayload,
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
const DV_E_LINDEX: i32 = 0x8004_0068u32 as i32;
const DV_E_TYMED: i32 = 0x8004_0069u32 as i32;
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
const TYMED_ISTREAM: u32 = 4;
const TYMED_ISTORAGE: u32 = 8;
const TYMED_GDI: u32 = 16;
const TYMED_MFPICT: u32 = 32;
const TYMED_ENHMF: u32 = 64;
const GMEM_MOVEABLE: u32 = 0x0002;
const GMEM_ZEROINIT: u32 = 0x0040;
const CLSCTX_INPROC_SERVER: u32 = 1;
const DSH_ALLOWDROPDESCRIPTIONTEXT: u32 = 1;
const BI_RGB: u32 = 0;
const DIB_RGB_COLORS: u32 = 0;
const COLOR_KEY_NONE: u32 = 0xFFFF_FFFF;
const FD_ATTRIBUTES: u32 = 0x0000_0004;
const FD_FILESIZE: u32 = 0x0000_0040;
const FD_UNICODE: u32 = 0x8000_0000;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

const CF_TEXT: u16 = 1;
const CF_UNICODETEXT: u16 = 13;
const CF_HDROP: u16 = 15;

#[link(name = "ole32")]
unsafe extern "system" {
    fn OleInitialize(pv_reserved: *mut c_void) -> i32;
    fn OleUninitialize();
    fn CoCreateInstance(
        rclsid: *const Guid,
        p_unk_outer: *mut c_void,
        dw_cls_context: u32,
        riid: *const Guid,
        ppv: *mut *mut c_void,
    ) -> i32;
    fn CreateStreamOnHGlobal(
        hglobal: *mut c_void,
        f_delete_on_release: i32,
        ppstm: *mut *mut c_void,
    ) -> i32;
    fn OleDuplicateData(h_src: *mut c_void, cf_format: u16, ui_flags: u32) -> *mut c_void;
    fn ReleaseStgMedium(medium: *mut StgMedium);
    fn DoDragDrop(
        data_object: *mut c_void,
        drop_source: *mut c_void,
        ok_effects: u32,
        out_effect: *mut u32,
    ) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateDIBSection(
        hdc: *mut c_void,
        pbmi: *const BitmapInfo,
        usage: u32,
        ppv_bits: *mut *mut c_void,
        h_section: *mut c_void,
        offset: u32,
    ) -> *mut c_void;
    fn DeleteObject(ho: *mut c_void) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalLock(mem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(mem: *mut c_void) -> i32;
    fn GlobalSize(mem: *mut c_void) -> usize;
    fn GlobalFree(mem: *mut c_void) -> *mut c_void;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClipboardFormatW(format: *const u16) -> u32;
    fn GetCursorPos(point: *mut Point) -> i32;
    fn ScreenToClient(hwnd: *mut c_void, point: *mut Point) -> i32;
    fn GetAsyncKeyState(v_key: i32) -> i16;
    fn GetDC(hwnd: *mut c_void) -> *mut c_void;
    fn ReleaseDC(hwnd: *mut c_void, hdc: *mut c_void) -> i32;
    fn SetTimer(
        hwnd: *mut c_void,
        id_event: usize,
        elapse: u32,
        timer_func: Option<unsafe extern "system" fn(*mut c_void, u32, usize, u32)>,
    ) -> usize;
    fn KillTimer(hwnd: *mut c_void, id_event: usize) -> i32;
}

const VK_LBUTTON: i32 = 0x01;
const VK_RBUTTON: i32 = 0x02;
const VK_MBUTTON: i32 = 0x04;

/// Returns whether any physical OS mouse button capable of driving OLE `DoDragDrop`
/// is currently held down.
pub fn is_os_drag_button_down() -> bool {
    unsafe {
        ((GetAsyncKeyState(VK_LBUTTON) as u16 & 0x8000) != 0)
            || ((GetAsyncKeyState(VK_RBUTTON) as u16 & 0x8000) != 0)
            || ((GetAsyncKeyState(VK_MBUTTON) as u16 & 0x8000) != 0)
    }
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

const CLSID_DRAG_DROP_HELPER: Guid = Guid {
    data1: 0x4657_278A,
    data2: 0x411B,
    data3: 0x11D2,
    data4: [0x83, 0x9A, 0x00, 0xC0, 0x4F, 0xD9, 0x18, 0xD0],
};

const IID_IDRAG_SOURCE_HELPER: Guid = Guid {
    data1: 0xDE5B_F786,
    data2: 0x477A,
    data3: 0x11D2,
    data4: [0x83, 0x9D, 0x00, 0xC0, 0x4F, 0xD9, 0x18, 0xD0],
};

const IID_IDRAG_SOURCE_HELPER2: Guid = Guid {
    data1: 0x83E0_7D0D,
    data2: 0x0C5F,
    data3: 0x4163,
    data4: [0xBF, 0x1A, 0x60, 0xB2, 0x74, 0x05, 0x1E, 0x40],
};

#[repr(C)]
struct IUnknownVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct IStreamVtbl {
    base: IUnknownVtbl,
    read: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *mut u32) -> i32,
    write: unsafe extern "system" fn(*mut c_void, *const c_void, u32, *mut u32) -> i32,
    seek: unsafe extern "system" fn(*mut c_void, i64, u32, *mut u64) -> i32,
    set_size: unsafe extern "system" fn(*mut c_void, u64) -> i32,
}

#[repr(C)]
struct IDragSourceHelperVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    initialize_from_bitmap:
        unsafe extern "system" fn(*mut c_void, *const ShDragImage, *mut c_void) -> i32,
    initialize_from_window:
        unsafe extern "system" fn(*mut c_void, *mut c_void, *const Point, *mut c_void) -> i32,
}

#[repr(C)]
struct IDragSourceHelper2Vtbl {
    base: IDragSourceHelperVtbl,
    set_flags: unsafe extern "system" fn(*mut c_void, u32) -> i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Size {
    cx: i32,
    cy: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ShDragImage {
    size_drag_image: Size,
    pt_offset: Point,
    hbmp_drag_image: *mut c_void,
    cr_color_key: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BitmapInfo {
    bmi_header: BitmapInfoHeader,
    bmi_colors: [u32; 1],
}

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

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    dw_low_date_time: u32,
    dw_high_date_time: u32,
}

#[repr(C, packed(1))]
#[derive(Clone, Copy)]
struct FileDescriptorW {
    dw_flags: u32,
    clsid: Guid,
    sizel: Size,
    pointl: Point,
    dw_file_attributes: u32,
    ft_creation_time: FileTime,
    ft_last_access_time: FileTime,
    ft_last_write_time: FileTime,
    n_file_size_high: u32,
    n_file_size_low: u32,
    c_file_name: [u16; 260],
}

#[repr(C, packed(1))]
#[derive(Clone, Copy)]
struct FileGroupDescriptorW {
    c_items: u32,
    fgd: [FileDescriptorW; 1],
}

#[derive(Clone, Copy)]
struct ClipboardFormats {
    html: u16,
    url_w: u16,
    url_a: u16,
    file_desc_w: u16,
    file_contents: u16,
}

impl ClipboardFormats {
    fn register_os() -> Self {
        Self {
            html: register_clip_format("HTML Format"),
            url_w: register_clip_format("UniformResourceLocatorW"),
            url_a: register_clip_format("UniformResourceLocator"),
            file_desc_w: register_clip_format("FileGroupDescriptorW"),
            file_contents: register_clip_format("FileContents"),
        }
    }
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

/// Builds a 1-item Unicode `FILEGROUPDESCRIPTORW` payload for `CFSTR_FILEDESCRIPTORW`
/// matching Chromium's `OSExchangeDataProviderWin::SetFileContents`.
pub fn build_file_group_descriptor_w(file_name: &str, byte_len: usize) -> Vec<u8> {
    let mut name_buf = [0u16; 260];
    for (dst, unit) in name_buf[..259].iter_mut().zip(file_name.encode_utf16()) {
        *dst = unit;
    }
    let len_u64 = byte_len as u64;
    let descriptor = FileGroupDescriptorW {
        c_items: 1,
        fgd: [FileDescriptorW {
            dw_flags: FD_ATTRIBUTES | FD_FILESIZE | FD_UNICODE,
            clsid: Guid {
                data1: 0,
                data2: 0,
                data3: 0,
                data4: [0; 8],
            },
            sizel: Size::default(),
            pointl: Point::default(),
            dw_file_attributes: FILE_ATTRIBUTE_NORMAL,
            ft_creation_time: FileTime::default(),
            ft_last_access_time: FileTime::default(),
            ft_last_write_time: FileTime::default(),
            n_file_size_high: (len_u64 >> 32) as u32,
            n_file_size_low: len_u64 as u32,
            c_file_name: name_buf,
        }],
    };
    // SAFETY: `FileGroupDescriptorW` is `#[repr(C, packed(1))]` of plain integers without padding.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&descriptor as *const FileGroupDescriptorW).cast::<u8>(),
            size_of::<FileGroupDescriptorW>(),
        )
    };
    bytes.to_vec()
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

struct DynamicFormatEntry {
    format: FormatEtc,
    medium: StgMedium,
}

impl Drop for DynamicFormatEntry {
    fn drop(&mut self) {
        if self.medium.tymed != 0 {
            unsafe {
                ReleaseStgMedium(&mut self.medium);
            }
        }
    }
}

fn clone_stg_medium(src: &StgMedium, cf_format: u16) -> Result<StgMedium, i32> {
    match src.tymed {
        TYMED_HGLOBAL => {
            if src.data.is_null() {
                return Err(E_INVALIDARG);
            }
            let size = unsafe { GlobalSize(src.data) };
            let ptr = unsafe { GlobalLock(src.data) };
            if ptr.is_null() {
                return Err(E_OUTOFMEMORY);
            }
            let slice = if size == 0 {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) }
            };
            let copy = alloc_hglobal(slice);
            unsafe {
                GlobalUnlock(src.data);
            }
            copy.map(|data| StgMedium {
                tymed: TYMED_HGLOBAL,
                data,
                p_unk_for_release: ptr::null_mut(),
            })
        }
        TYMED_GDI | TYMED_MFPICT | TYMED_ENHMF => {
            if src.data.is_null() {
                return Err(E_INVALIDARG);
            }
            let dup = unsafe { OleDuplicateData(src.data, cf_format, 0) };
            if dup.is_null() {
                return Err(E_OUTOFMEMORY);
            }
            Ok(StgMedium {
                tymed: src.tymed,
                data: dup,
                p_unk_for_release: ptr::null_mut(),
            })
        }
        TYMED_ISTREAM | TYMED_ISTORAGE => {
            if src.data.is_null() {
                return Err(E_INVALIDARG);
            }
            unsafe {
                let vtbl = *(src.data.cast::<*const IUnknownVtbl>());
                ((*vtbl).add_ref)(src.data);
            }
            Ok(StgMedium {
                tymed: src.tymed,
                data: src.data,
                p_unk_for_release: ptr::null_mut(),
            })
        }
        _ => Err(DV_E_TYMED),
    }
}

fn create_drag_dib(image: &DragImage) -> Option<*mut c_void> {
    if image.validate().is_err() {
        return None;
    }
    let bmi = BitmapInfo {
        bmi_header: BitmapInfoHeader {
            bi_size: size_of::<BitmapInfoHeader>() as u32,
            bi_width: image.width as i32,
            // Negative height creates a top-down 32-bit BGRA DIB matching `image.bgra`.
            bi_height: -(image.height as i32),
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: BI_RGB,
            ..BitmapInfoHeader::default()
        },
        bmi_colors: [0],
    };
    let screen_dc = unsafe { GetDC(ptr::null_mut()) };
    let mut bits: *mut c_void = ptr::null_mut();
    let hbmp = unsafe {
        CreateDIBSection(
            screen_dc,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            ptr::null_mut(),
            0,
        )
    };
    if !screen_dc.is_null() {
        unsafe {
            ReleaseDC(ptr::null_mut(), screen_dc);
        }
    }
    if hbmp.is_null() || bits.is_null() {
        if !hbmp.is_null() {
            unsafe {
                DeleteObject(hbmp);
            }
        }
        return None;
    }
    unsafe {
        ptr::copy_nonoverlapping(image.bgra.as_ptr(), bits.cast::<u8>(), image.bgra.len());
    }
    Some(hbmp)
}

/// Registers Chromium's drag ghost image onto our `IDataObject` via Windows
/// Shell's `CLSID_DragDropHelper` (`IDragSourceHelper::InitializeFromBitmap`),
/// matching Chromium's `ui/base/dragdrop/os_exchange_data_provider_win.cc`.
fn attach_drag_image(data_object: *mut DataObject, image: &DragImage) -> bool {
    let Some(hbmp) = create_drag_dib(image) else {
        return false;
    };
    let mut helper_ptr: *mut c_void = ptr::null_mut();
    let hr = unsafe {
        CoCreateInstance(
            &CLSID_DRAG_DROP_HELPER,
            ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IDRAG_SOURCE_HELPER,
            &mut helper_ptr,
        )
    };
    if hr < 0 || helper_ptr.is_null() {
        unsafe {
            DeleteObject(hbmp);
        }
        return false;
    }

    // Opt into Shell drop description tooltips (e.g. "Copy to ...") when available.
    let mut helper2_ptr: *mut c_void = ptr::null_mut();
    unsafe {
        let vtbl = *(helper_ptr.cast::<*const IDragSourceHelperVtbl>());
        if ((*vtbl).query_interface)(helper_ptr, &IID_IDRAG_SOURCE_HELPER2, &mut helper2_ptr) >= 0
            && !helper2_ptr.is_null()
        {
            let vtbl2 = *(helper2_ptr.cast::<*const IDragSourceHelper2Vtbl>());
            let _ = ((*vtbl2).set_flags)(helper2_ptr, DSH_ALLOWDROPDESCRIPTIONTEXT);
            ((*vtbl2).base.release)(helper2_ptr);
        }
    }

    let shdi = ShDragImage {
        size_drag_image: Size {
            cx: image.width as i32,
            cy: image.height as i32,
        },
        pt_offset: Point {
            x: image.hotspot_x,
            y: image.hotspot_y,
        },
        hbmp_drag_image: hbmp,
        cr_color_key: COLOR_KEY_NONE,
    };
    let init_hr = unsafe {
        let vtbl = *(helper_ptr.cast::<*const IDragSourceHelperVtbl>());
        let res = ((*vtbl).initialize_from_bitmap)(helper_ptr, &shdi, data_object.cast::<c_void>());
        ((*vtbl).release)(helper_ptr);
        res
    };
    if init_hr < 0 {
        // InitializeFromBitmap only takes ownership of `hbmp` on success.
        unsafe {
            DeleteObject(hbmp);
        }
        false
    } else {
        true
    }
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
    dynamic: RefCell<Vec<DynamicFormatEntry>>,
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
    fn new_raw(payload: &DraggedPayload, formats: ClipboardFormats) -> *mut Self {
        let mut entries = Vec::new();
        let make_fmt = |cf_format: u16| FormatEtc {
            cf_format,
            ptd: ptr::null_mut(),
            dw_aspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL,
        };
        // Order matches Chromium `PrepareDragData` so virtual/disk file formats take
        // precedence over URL/text fallbacks in Windows Explorer, chat apps, and editors.
        if !payload.file_paths.is_empty() {
            entries.push(FormatEntry {
                format: make_fmt(CF_HDROP),
                bytes: build_hdrop_bytes(&payload.file_paths),
            });
        }
        if formats.file_desc_w != 0
            && formats.file_contents != 0
            && let Some(fc) = &payload.file_contents
            && fc.validate().is_ok()
        {
            entries.push(FormatEntry {
                format: make_fmt(formats.file_desc_w),
                bytes: build_file_group_descriptor_w(&fc.file_name, fc.bytes.len()),
            });
            entries.push(FormatEntry {
                format: FormatEtc {
                    cf_format: formats.file_contents,
                    ptd: ptr::null_mut(),
                    dw_aspect: DVASPECT_CONTENT,
                    lindex: 0,
                    tymed: TYMED_HGLOBAL | TYMED_ISTREAM,
                },
                bytes: fc.bytes.clone(),
            });
        }
        if formats.html != 0
            && let Some(html) = payload.html.as_deref()
        {
            entries.push(FormatEntry {
                format: make_fmt(formats.html),
                bytes: format_cf_html(html),
            });
        }
        if formats.url_w != 0
            && let Some(url) = payload.url.as_deref()
        {
            entries.push(FormatEntry {
                format: make_fmt(formats.url_w),
                bytes: format_utf16le_nul(url),
            });
        }
        if formats.url_a != 0
            && let Some(url) = payload.url.as_deref()
        {
            entries.push(FormatEntry {
                format: make_fmt(formats.url_a),
                bytes: format_ansi_nul(url),
            });
        }
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
        Box::into_raw(Box::new(Self {
            vtbl: &DATA_OBJECT_VTBL,
            ref_count: AtomicU32::new(1),
            entries,
            dynamic: RefCell::new(Vec::new()),
        }))
    }

    fn find_entry(&self, req: &FormatEtc) -> Result<&FormatEntry, i32> {
        if req.dw_aspect != DVASPECT_CONTENT {
            return Err(DV_E_FORMATETC);
        }
        let mut saw_format = false;
        let mut saw_lindex = false;
        for entry in &self.entries {
            if entry.format.cf_format != req.cf_format {
                continue;
            }
            saw_format = true;
            let lindex_ok = if entry.format.lindex == -1 {
                req.lindex == -1 || req.lindex == 0
            } else {
                req.lindex == entry.format.lindex || req.lindex == -1
            };
            if !lindex_ok {
                continue;
            }
            saw_lindex = true;
            if (req.tymed & entry.format.tymed) != 0 {
                return Ok(entry);
            }
        }
        if !saw_format {
            Err(DV_E_FORMATETC)
        } else if !saw_lindex {
            Err(DV_E_LINDEX)
        } else {
            Err(DV_E_TYMED)
        }
    }

    fn matches_dynamic(entry: &DynamicFormatEntry, req: &FormatEtc) -> bool {
        entry.format.cf_format == req.cf_format
            && (req.dw_aspect == 0 || entry.format.dw_aspect == req.dw_aspect)
            && (req.lindex == -1 || entry.format.lindex == -1 || entry.format.lindex == req.lindex)
            && (req.tymed & entry.medium.tymed) != 0
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
    {
        let dynamic = obj.dynamic.borrow();
        if let Some(entry) = dynamic
            .iter()
            .find(|entry| DataObject::matches_dynamic(entry, req))
        {
            return match clone_stg_medium(&entry.medium, entry.format.cf_format) {
                Ok(cloned) => {
                    unsafe { *pmedium = cloned };
                    S_OK
                }
                Err(hr) => hr,
            };
        }
    }
    let entry = match obj.find_entry(req) {
        Ok(entry) => entry,
        Err(hr) => return hr,
    };
    let hglobal = match alloc_hglobal(&entry.bytes) {
        Ok(hglobal) => hglobal,
        Err(hr) => return hr,
    };
    if (req.tymed & TYMED_HGLOBAL) != 0 && (entry.format.tymed & TYMED_HGLOBAL) != 0 {
        unsafe {
            *pmedium = StgMedium {
                tymed: TYMED_HGLOBAL,
                data: hglobal,
                p_unk_for_release: ptr::null_mut(),
            };
        }
        S_OK
    } else if (req.tymed & TYMED_ISTREAM) != 0 && (entry.format.tymed & TYMED_ISTREAM) != 0 {
        let mut stream: *mut c_void = ptr::null_mut();
        let hr = unsafe { CreateStreamOnHGlobal(hglobal, 1, &mut stream) };
        if hr < 0 || stream.is_null() {
            unsafe {
                GlobalFree(hglobal);
            }
            return if hr < 0 { hr } else { E_OUTOFMEMORY };
        }
        // Clamp IStream size to exact byte length so GlobalAlloc heap rounding never leaks padding.
        unsafe {
            let vtbl = *(stream.cast::<*const IStreamVtbl>());
            let _ = ((*vtbl).set_size)(stream, entry.bytes.len() as u64);
            *pmedium = StgMedium {
                tymed: TYMED_ISTREAM,
                data: stream,
                p_unk_for_release: ptr::null_mut(),
            };
        }
        S_OK
    } else {
        unsafe {
            GlobalFree(hglobal);
        }
        DV_E_TYMED
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
    let obj = unsafe { &*this };
    let has_dynamic = obj
        .dynamic
        .borrow()
        .iter()
        .any(|entry| DataObject::matches_dynamic(entry, req));
    if has_dynamic {
        S_OK
    } else {
        match obj.find_entry(req) {
            Ok(_) => S_OK,
            Err(hr) => hr,
        }
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
    this: *mut DataObject,
    pformatetc: *const FormatEtc,
    pmedium: *const StgMedium,
    f_release: i32,
) -> i32 {
    if this.is_null() || pformatetc.is_null() || pmedium.is_null() {
        return E_INVALIDARG;
    }
    let mut format = unsafe { *pformatetc };
    format.ptd = ptr::null_mut();
    let incoming = unsafe { *pmedium };
    let owned_medium = if f_release != 0 {
        incoming
    } else {
        match clone_stg_medium(&incoming, format.cf_format) {
            Ok(cloned) => cloned,
            Err(hr) => return hr,
        }
    };
    let obj = unsafe { &*this };
    let mut dynamic = obj.dynamic.borrow_mut();
    if let Some(pos) = dynamic.iter().position(|entry| {
        entry.format.cf_format == format.cf_format
            && entry.format.dw_aspect == format.dw_aspect
            && entry.format.lindex == format.lindex
    }) {
        dynamic.remove(pos);
    }
    dynamic.push(DynamicFormatEntry {
        format,
        medium: owned_medium,
    });
    S_OK
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
    let obj = unsafe { &*this };
    let mut formats: Vec<FormatEtc> = obj.entries.iter().map(|entry| entry.format).collect();
    for entry in obj.dynamic.borrow().iter() {
        if !formats
            .iter()
            .any(|existing| existing.cf_format == entry.format.cf_format)
        {
            formats.push(entry.format);
        }
    }
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

/// Creates a real COM `IDataObject` (including `SHDRAGIMAGE` when present) for integration testing.
pub fn new_ole_data_object(payload: &DraggedPayload) -> *mut c_void {
    let formats = ClipboardFormats::register_os();
    let data_object = DataObject::new_raw(payload, formats);
    if let Some(image) = &payload.image {
        let _ = attach_drag_image(data_object, image);
    }
    data_object.cast::<c_void>()
}

/// Releases a COM `IDataObject` previously returned by [`new_ole_data_object`].
///
/// # Safety
/// `obj` must be a valid pointer previously returned by [`new_ole_data_object`].
pub unsafe fn release_ole_data_object(obj: *mut c_void) {
    if !obj.is_null() {
        unsafe {
            data_object_release(obj.cast::<DataObject>());
        }
    }
}

/// Runs a modal OLE drag-and-drop loop on the UI/STA thread after `App` borrows end.
pub fn run_ole_drag(
    start: &OutboundDragStart,
    hwnd: Option<*mut c_void>,
    integrated_pump: Option<IntegratedPumpCallback>,
) -> OutboundDragFinish {
    run_ole_drag_in_surface(start, hwnd, None, integrated_pump)
}

/// Runs a modal OLE drag-and-drop loop on the UI/STA thread and maps the ending cursor
/// position back into browser-view DIP coordinates via `surface_geometry`.
pub fn run_ole_drag_in_surface(
    start: &OutboundDragStart,
    hwnd: Option<*mut c_void>,
    surface_geometry: Option<(f32, f32, f32, f32, f32)>,
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

    let formats = ClipboardFormats::register_os();
    let data_object = DataObject::new_raw(&start.payload, formats);
    if let Some(image) = &start.payload.image {
        let _ = attach_drag_image(data_object, image);
    }
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
                match surface_geometry {
                    Some((sx, sy, _sw, _sh, scale)) if scale.is_finite() && scale > 0.0 => (
                        ((pt.x as f32 / scale) - sx).round() as i32,
                        ((pt.y as f32 / scale) - sy).round() as i32,
                    ),
                    _ => (pt.x, pt.y),
                }
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
        dropeffect_to_core_mask(effect).preferred_single_op()
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
        assert_eq!(size_of::<ShDragImage>(), 32);
        assert_eq!(size_of::<BitmapInfoHeader>(), 40);
        assert_eq!(size_of::<BitmapInfo>(), 44);
        assert_eq!(size_of::<FileDescriptorW>(), 592);
        assert_eq!(size_of::<FileGroupDescriptorW>(), 596);
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
        let formats = ClipboardFormats {
            html: 0xC100,
            url_w: 0xC101,
            url_a: 0xC102,
            file_desc_w: 0xC103,
            file_contents: 0xC104,
        };
        let obj = DataObject::new_raw(&payload, formats);
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
            assert_eq!(fetched, 6);
            assert_eq!(enum_fmt_reset(enumerator), S_OK);

            let mut cloned_ptr: *mut c_void = ptr::null_mut();
            assert_eq!(enum_fmt_clone(enumerator, &mut cloned_ptr), S_OK);
            assert_eq!(enum_fmt_release(cloned_ptr.cast::<EnumFormatEtc>()), 0);
            assert_eq!(enum_fmt_release(enumerator), 0);
            assert_eq!(data_object_release(obj), 0);
        }
    }

    #[test]
    fn drag_source_helper_attaches_shdragimage_via_set_data() {
        let ole_hr = unsafe { OleInitialize(ptr::null_mut()) };
        assert!(ole_hr >= 0, "OleInitialize failed: 0x{:08x}", ole_hr as u32);
        let payload = DraggedPayload::bounded(Some("link"), None, Some("https://cn.bing.com/"), []);
        // 2x2 BGRA (straight/unpremultiplied alpha):
        // top row = 50% alpha pure blue [0xFF, 0x00, 0x00, 0x80]
        // bottom row = 100% alpha pure red [0x00, 0x00, 0xFF, 0xFF]
        let pixels = vec![
            0xFF, 0x00, 0x00, 0x80, 0xFF, 0x00, 0x00, 0x80, // top row
            0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, // bottom row
        ];
        let image = DragImage::bounded(2, 2, 1, 0, pixels).unwrap();
        let formats = ClipboardFormats::register_os();
        let obj = DataObject::new_raw(&payload, formats);

        assert!(attach_drag_image(obj, &image));
        let cf_drag_image_bits = register_clip_format("DragImageBits");
        assert_ne!(cf_drag_image_bits, 0);
        let fmt = FormatEtc {
            cf_format: cf_drag_image_bits,
            ptd: ptr::null_mut(),
            dw_aspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL,
        };
        unsafe {
            assert_eq!(data_object_query_get_data(obj, &fmt), S_OK);
            let mut medium = StgMedium {
                tymed: 0,
                data: ptr::null_mut(),
                p_unk_for_release: ptr::null_mut(),
            };
            assert_eq!(data_object_get_data(obj, &fmt, &mut medium), S_OK);
            assert_eq!(medium.tymed, TYMED_HGLOBAL);
            assert!(!medium.data.is_null());
            let size = GlobalSize(medium.data);
            let ptr = GlobalLock(medium.data);
            assert!(!ptr.is_null());
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), size);
            assert!(size >= 24 + 16, "unexpected DragImageBits size: {size}");
            // Windows DragImageBits starts with (width, height, hotspot_x, hotspot_y, flags, color_key)
            // followed by bottom-up premultiplied BGRA pixels. Verify top-down input did NOT flip
            // upside down (row 0 in DragImageBits is our bottom red row, row 1 is our top 50%-blue row)
            // and straight-alpha input was premultiplied once (B == A on the 50%-blue pixel).
            let width = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let height = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
            let hot_x = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
            let hot_y = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
            assert_eq!((width, height, hot_x, hot_y), (2, 2, 1, 0));
            let bottom_px = &bytes[24..28];
            let top_px = &bytes[32..36];
            assert!(
                bottom_px[2] > 0 && bottom_px[0] == 0,
                "bottom row should be red: {bottom_px:?}"
            );
            assert!(
                top_px[0] > 0 && top_px[2] == 0,
                "top row should be blue: {top_px:?}"
            );
            assert_eq!(
                top_px[0], top_px[3],
                "straight-alpha blue should premultiply once: {top_px:?}"
            );
            GlobalUnlock(medium.data);
            ReleaseStgMedium(&mut medium);
            assert_eq!(data_object_release(obj), 0);
            if ole_hr == S_OK || ole_hr == S_FALSE {
                OleUninitialize();
            }
        }
    }

    #[test]
    fn virtual_image_file_contents_serves_descriptor_hglobal_and_istream() {
        use rust_cef_core::outbound_drag::DraggedFileContents;

        let png = b"\x89PNG\r\n\x1A\nTESTIMAGEBYTES".to_vec();
        let fc = DraggedFileContents::bounded(Some("bing_photo.png"), png.clone()).unwrap();
        let payload = DraggedPayload::bounded(
            None,
            Some("<img src=\"https://cn.bing.com/p.png\">"),
            Some("https://cn.bing.com/p.png"),
            [],
        )
        .with_file_contents(Some(fc));
        let formats = ClipboardFormats::register_os();
        assert_ne!(formats.file_desc_w, 0);
        assert_ne!(formats.file_contents, 0);
        let obj = DataObject::new_raw(&payload, formats);

        unsafe {
            // 1. EnumFormatEtc lists FileGroupDescriptorW + FileContents ahead of URL/text.
            let mut enum_ptr: *mut c_void = ptr::null_mut();
            assert_eq!(
                data_object_enum_format_etc(obj, DATADIR_GET, &mut enum_ptr),
                S_OK
            );
            let enumerator = enum_ptr.cast::<EnumFormatEtc>();
            let mut listed = [FormatEtc {
                cf_format: 0,
                ptd: ptr::null_mut(),
                dw_aspect: 0,
                lindex: 0,
                tymed: 0,
            }; 10];
            let mut fetched = 0u32;
            assert_eq!(
                enum_fmt_next(enumerator, 10, listed.as_mut_ptr(), &mut fetched),
                S_FALSE
            );
            assert!(fetched >= 7);
            assert_eq!(listed[0].cf_format, formats.file_desc_w);
            assert_eq!(listed[1].cf_format, formats.file_contents);
            assert_eq!(listed[1].lindex, 0);
            assert_eq!(listed[1].tymed, TYMED_HGLOBAL | TYMED_ISTREAM);
            assert_eq!(enum_fmt_release(enumerator), 0);

            // 2. FileGroupDescriptorW encodes cItems=1, size, and UTF-16 filename.
            let fmt_desc = FormatEtc {
                cf_format: formats.file_desc_w,
                ptd: ptr::null_mut(),
                dw_aspect: DVASPECT_CONTENT,
                lindex: -1,
                tymed: TYMED_HGLOBAL,
            };
            let mut med = StgMedium {
                tymed: 0,
                data: ptr::null_mut(),
                p_unk_for_release: ptr::null_mut(),
            };
            assert_eq!(data_object_get_data(obj, &fmt_desc, &mut med), S_OK);
            assert_eq!(med.tymed, TYMED_HGLOBAL);
            let desc_ptr = GlobalLock(med.data).cast::<FileGroupDescriptorW>();
            assert!(!desc_ptr.is_null());
            let desc = ptr::read_unaligned(desc_ptr);
            let c_items = desc.c_items;
            let dw_flags = desc.fgd[0].dw_flags;
            let file_size = desc.fgd[0].n_file_size_low;
            let name_buf = desc.fgd[0].c_file_name;
            let nul_pos = name_buf.iter().position(|&u| u == 0).unwrap();
            let decoded_name = String::from_utf16(&name_buf[..nul_pos]).unwrap();
            assert_eq!(c_items, 1);
            assert_eq!(dw_flags, FD_ATTRIBUTES | FD_FILESIZE | FD_UNICODE);
            assert_eq!(file_size as usize, png.len());
            assert_eq!(decoded_name, "bing_photo.png");
            GlobalUnlock(med.data);
            ReleaseStgMedium(&mut med);

            // 3. FileContents lindex=0 over TYMED_HGLOBAL returns exact bytes.
            let fmt_hglobal = FormatEtc {
                cf_format: formats.file_contents,
                ptd: ptr::null_mut(),
                dw_aspect: DVASPECT_CONTENT,
                lindex: 0,
                tymed: TYMED_HGLOBAL,
            };
            assert_eq!(data_object_get_data(obj, &fmt_hglobal, &mut med), S_OK);
            assert_eq!(med.tymed, TYMED_HGLOBAL);
            let p = GlobalLock(med.data).cast::<u8>();
            let got = std::slice::from_raw_parts(p, png.len()).to_vec();
            assert_eq!(got, png);
            GlobalUnlock(med.data);
            ReleaseStgMedium(&mut med);

            // 4. FileContents lindex=0 over TYMED_ISTREAM returns readable COM IStream with exact EOF.
            let fmt_stream = FormatEtc {
                cf_format: formats.file_contents,
                ptd: ptr::null_mut(),
                dw_aspect: DVASPECT_CONTENT,
                lindex: 0,
                tymed: TYMED_ISTREAM,
            };
            assert_eq!(data_object_query_get_data(obj, &fmt_stream), S_OK);
            assert_eq!(data_object_get_data(obj, &fmt_stream, &mut med), S_OK);
            assert_eq!(med.tymed, TYMED_ISTREAM);
            assert!(!med.data.is_null());
            let mut stream_buf = vec![0u8; png.len() + 32];
            let mut read_bytes = 0u32;
            let vtbl = *(med.data.cast::<*const IStreamVtbl>());
            let hr = ((*vtbl).read)(
                med.data,
                stream_buf.as_mut_ptr().cast::<c_void>(),
                stream_buf.len() as u32,
                &mut read_bytes,
            );
            assert!(hr >= 0);
            assert_eq!(read_bytes as usize, png.len());
            assert_eq!(&stream_buf[..read_bytes as usize], png.as_slice());
            ReleaseStgMedium(&mut med);

            // 5. Out-of-range FileContents lindex=1 returns DV_E_LINDEX.
            let fmt_bad_index = FormatEtc {
                cf_format: formats.file_contents,
                ptd: ptr::null_mut(),
                dw_aspect: DVASPECT_CONTENT,
                lindex: 1,
                tymed: TYMED_HGLOBAL | TYMED_ISTREAM,
            };
            assert_eq!(data_object_query_get_data(obj, &fmt_bad_index), DV_E_LINDEX);
            assert_eq!(data_object_release(obj), 0);
        }
    }
}
