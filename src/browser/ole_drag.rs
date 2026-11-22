//! OLE drag-out support for `RenderHandler::start_dragging`.
//!
//! When the user drags a link, an image or selected text inside the page,
//! Chromium asks the embedder to run the OS drag loop. CEF does not expose its
//! `IDataObject`, so we rebuild one from the plain data CEF does expose
//! (fragment text/html, link URL, file paths) and run `DoDragDrop` ourselves.
//!
//! Dropping *into* the browser is the other direction and lives in `ui.rs`
//! through `drag_target_*`; this module only covers dragging out.

use cef::ImplDragData;
use std::cell::RefCell;
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_USEDEFAULTCURSORS, E_INVALIDARG, E_NOTIMPL, HGLOBAL, S_OK,
};
use windows::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::core::{Result as WinResult, implement};
/// `DV_E_FORMATETC` is a plain HRESULT with no named binding.
const DV_E_FORMATETC: windows::core::HRESULT = windows::core::HRESULT(0x8004_0064_u32 as _);
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE, DoDragDrop,
    IDropSource, IDropSource_Impl,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;

/// What the page offered to drag.
#[derive(Clone, Debug, Default)]
pub struct DraggedPayload {
    pub text: Option<String>,
    pub html: Option<String>,
    pub url: Option<String>,
    pub file_paths: Vec<String>,
}

impl DraggedPayload {
    pub fn is_empty(&self) -> bool {
        self.text.is_none()
            && self.html.is_none()
            && self.url.is_none()
            && self.file_paths.is_empty()
    }

    /// Reads what CEF's drag data exposes. `DragData` is only valid during the
    /// `start_dragging` callback, so everything is copied out immediately.
    pub fn from_cef(data: &cef::DragData) -> Self {
        // Only touch the getters that match what CEF says it has: the others
        // return null userfree strings, and converting those logs
        // "Invalid UTF-16 string" from inside the binding.
        let mut file_paths = Vec::new();
        if data.is_file() == 1 {
            // `get_file_paths` fills a CEF string list; the binding takes it by
            // mutable reference and returns 1 on success.
            let mut list = cef::CefStringList::default();
            if data.file_paths(Some(&mut list)) == 1 {
                file_paths.extend(list);
            }
        }
        let is_fragment = data.is_fragment() == 1;
        let is_link = data.is_link() == 1;
        Self {
            text: if is_fragment || is_link {
                non_empty(cef_string(data.fragment_text()))
            } else {
                None
            },
            html: if is_fragment {
                non_empty(cef_string(data.fragment_html()))
            } else {
                None
            },
            url: if is_link {
                non_empty(cef_string(data.link_url()))
            } else {
                None
            },
            file_paths,
        }
    }

    /// `CF_UNICODETEXT` body: the text, or the URL when only that is available.
    fn plain_text(&self) -> Option<&str> {
        self.text.as_deref().or(self.url.as_deref())
    }

    /// Preferred operation for this payload. Kept for the reporting path and
    /// covered by tests; the drag loop uses the operations CEF allowed instead.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn default_operation(&self) -> u32 {
        if self.file_paths.is_empty() {
            super::dnd::DRAG_OPERATION_COPY
        } else {
            super::dnd::DRAG_OPERATION_COPY | super::dnd::DRAG_OPERATION_MOVE
        }
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.is_empty())
}

fn cef_string(value: cef::CefStringUserfree) -> Option<String> {
    let owned = cef::CefString::from(&value);
    Some(owned.to_string())
}

/// Runs the blocking OLE drag loop on the calling (UI) thread.
///
/// Returns the CEF drag operation that resulted, so the caller can report it
/// back with `drag_source_ended_at` / `drag_source_system_drag_ended`.
pub fn run_drag_loop(payload: &DraggedPayload, allowed: u32) -> u32 {
    if payload.is_empty() {
        return super::dnd::DRAG_OPERATION_NONE;
    }
    let mut effect = DROPEFFECT_NONE;
    let data: IDataObject = DataObject::new(payload.clone()).into();
    let source: IDropSource = DropSource.into();
    // SAFETY: gpui's Windows platform calls `OleInitialize` during startup, so
    // the apartment is ready. Both COM objects outlive the synchronous call.
    let result = unsafe { DoDragDrop(&data, &source, map_allowed(allowed), &mut effect) };
    if result == DRAGDROP_S_CANCEL {
        return super::dnd::DRAG_OPERATION_NONE;
    }
    map_effect(effect)
}

fn map_allowed(allowed: u32) -> DROPEFFECT {
    let mut effect = DROPEFFECT_NONE;
    if allowed & super::dnd::DRAG_OPERATION_COPY != 0 {
        effect |= DROPEFFECT_COPY;
    }
    if allowed & super::dnd::DRAG_OPERATION_MOVE != 0 {
        effect |= DROPEFFECT_MOVE;
    }
    if allowed & super::dnd::DRAG_OPERATION_LINK != 0 {
        effect |= DROPEFFECT_LINK;
    }
    effect
}

fn map_effect(effect: DROPEFFECT) -> u32 {
    let mut out = super::dnd::DRAG_OPERATION_NONE;
    if effect & DROPEFFECT_COPY != DROPEFFECT_NONE {
        out |= super::dnd::DRAG_OPERATION_COPY;
    }
    if effect & DROPEFFECT_MOVE != DROPEFFECT_NONE {
        out |= super::dnd::DRAG_OPERATION_MOVE;
    }
    if effect & DROPEFFECT_LINK != DROPEFFECT_NONE {
        out |= super::dnd::DRAG_OPERATION_LINK;
    }
    out
}

#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(
        &self,
        escape_pressed: windows::core::BOOL,
        _key_state: MODIFIERKEYS_FLAGS,
    ) -> windows::core::HRESULT {
        if escape_pressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> windows::core::HRESULT {
        // Let OLE draw the standard drag cursors and the snap-back animation.
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Minimal read-only `IDataObject` built from a `DraggedPayload`.
#[implement(IDataObject)]
struct DataObject {
    payload: RefCell<DraggedPayload>,
}

impl DataObject {
    fn new(payload: DraggedPayload) -> Self {
        Self {
            payload: RefCell::new(payload),
        }
    }
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> WinResult<STGMEDIUM> {
        let format = unsafe { format.as_ref() }.ok_or(E_INVALIDARG)?;
        let payload = self.payload.borrow();
        build_medium(&payload, format.dwAspect, format.tymed)
    }

    fn GetDataHere(&self, _format: *const FORMATETC, _medium: *mut STGMEDIUM) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> windows::core::HRESULT {
        let Some(format) = (unsafe { format.as_ref() }) else {
            return E_INVALIDARG;
        };
        if format.dwAspect != DVASPECT_CONTENT.0 as u32
            || format.tymed & TYMED_HGLOBAL.0 as u32 == 0
        {
            return DV_E_FORMATETC;
        }
        let payload = self.payload.borrow();
        if matches_format(&payload, format.cfFormat) {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _input: *const FORMATETC,
        _output: *mut FORMATETC,
    ) -> windows::core::HRESULT {
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _format: *const FORMATETC,
        _medium: *const STGMEDIUM,
        _release: windows::core::BOOL,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, _direction: u32) -> WinResult<IEnumFORMATETC> {
        Err(E_NOTIMPL.into())
    }

    fn DAdvise(
        &self,
        _format: *const FORMATETC,
        _flags: u32,
        _sink: windows::core::Ref<IAdviseSink>,
    ) -> WinResult<u32> {
        Err(E_NOTIMPL.into())
    }

    fn DUnadvise(&self, _connection: u32) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
        Err(E_NOTIMPL.into())
    }
}

/// Clipboard format ids used by the payload. `CF_TEXT` = 1, `CF_UNICODETEXT` = 13.
const CF_TEXT: u16 = 1;
const CF_UNICODETEXT: u16 = 13;

fn matches_format(payload: &DraggedPayload, cf: u16) -> bool {
    match cf {
        CF_TEXT | CF_UNICODETEXT => payload.plain_text().is_some(),
        _ => false,
    }
}

fn build_medium(payload: &DraggedPayload, aspect: u32, tymed: u32) -> WinResult<STGMEDIUM> {
    if aspect != DVASPECT_CONTENT.0 as u32 || tymed & TYMED_HGLOBAL.0 as u32 == 0 {
        return Err(DV_E_FORMATETC.into());
    }
    let Some(text) = payload.plain_text() else {
        return Err(E_INVALIDARG.into());
    };
    // UTF-16LE with a trailing NUL, which is what CF_UNICODETEXT expects.
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();

    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len()) }
        .map_err(|_| windows::core::Error::from_win32())?;
    unsafe {
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return Err(windows::core::Error::from_win32());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        let _ = GlobalUnlock(hglobal);
    }
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: hglobal },
        pUnkForRelease: std::mem::ManuallyDrop::new(None),
    })
}

/// Keeps the raw handle type referenced so the intent of `build_medium` is clear.
#[allow(dead_code)]
fn _handle_type_is_hglobal(handle: HGLOBAL) -> HGLOBAL {
    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_reports_itself_empty() {
        assert!(DraggedPayload::default().is_empty());
        let text = DraggedPayload {
            text: Some("hi".into()),
            ..Default::default()
        };
        assert!(!text.is_empty());
    }

    #[test]
    fn plain_text_falls_back_to_the_url() {
        let payload = DraggedPayload {
            url: Some("https://example.com".into()),
            ..Default::default()
        };
        assert_eq!(payload.plain_text(), Some("https://example.com"));
    }

    #[test]
    fn default_operation_depends_on_files() {
        let text = DraggedPayload {
            text: Some("hi".into()),
            ..Default::default()
        };
        assert_eq!(
            text.default_operation(),
            super::super::dnd::DRAG_OPERATION_COPY
        );
        let files = DraggedPayload {
            file_paths: vec!["C:/a.txt".into()],
            ..Default::default()
        };
        assert_ne!(
            files.default_operation() & super::super::dnd::DRAG_OPERATION_MOVE,
            0
        );
    }

    #[test]
    fn effect_mapping_round_trips() {
        assert_eq!(
            map_effect(DROPEFFECT_NONE),
            super::super::dnd::DRAG_OPERATION_NONE
        );
        assert_eq!(
            map_effect(DROPEFFECT_COPY),
            super::super::dnd::DRAG_OPERATION_COPY
        );
        assert_eq!(
            map_effect(DROPEFFECT_COPY | DROPEFFECT_MOVE),
            super::super::dnd::DRAG_OPERATION_COPY | super::super::dnd::DRAG_OPERATION_MOVE
        );
    }

    #[test]
    fn only_text_formats_are_advertised() {
        let payload = DraggedPayload {
            text: Some("hi".into()),
            ..Default::default()
        };
        assert!(matches_format(&payload, CF_TEXT));
        assert!(matches_format(&payload, CF_UNICODETEXT));
        assert!(!matches_format(&payload, 15));
        assert!(!matches_format(&DraggedPayload::default(), CF_TEXT));
    }
}
