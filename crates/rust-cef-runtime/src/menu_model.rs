//! Callback-local CEF menu -> owned data. No native model/child escapes this
//! synchronous walk; no handler, continuation or host action is installed here.
//! CEF getters allocate their own userfree strings before we can inspect them.
//! These limits govern raw rows and Runtime-owned UTF-8 copies, NOT CEF/RSS.
use cef::{CefStringUserfreeUtf16, CefStringUtf16, ImplMenuModel, MenuItemType};
use rust_cef_core::{
    menu::MenuLimits,
    menu_model::{MenuChild, MenuCommand, MenuItem, MenuModel, MenuSubmenu, MenuTreeError},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyError {
    WrongThread,
    UnsupportedType(i32),
    TooDeep,
    MissingSubmenu,
    ChangedDuringCopy,
    RowBudget,
    LabelBudget,
    InvalidUtf16,
    InvalidTextView,
    Model(MenuTreeError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Command,
    Check,
    Radio,
    Separator,
    Submenu,
    Unsupported(i32),
}

/// Private seam for count/budget fault fixtures, not an alternate public port.
trait Reader: Sized {
    fn count(&self) -> usize;
    fn kind(&self, index: usize) -> Kind;
    fn visible(&self, index: usize) -> bool;
    fn enabled(&self, index: usize) -> bool;
    fn command_id(&self, index: usize) -> i32;
    fn checked(&self, index: usize) -> bool;
    fn group_id(&self, index: usize) -> i32;
    fn label(&self, index: usize, byte_limit: usize) -> Result<String, CopyError>;
    fn submenu(&self, index: usize) -> Option<Self>;
}

// Root is borrowed. Each returned child handle lives only on this CEF UI
// stack and is dropped before the owned result returns; none goes to the UI.
impl Reader for cef::MenuModel {
    fn count(&self) -> usize {
        ImplMenuModel::count(self)
    }
    fn kind(&self, i: usize) -> Kind {
        match ImplMenuModel::type_at(self, i) {
            MenuItemType::COMMAND => Kind::Command,
            MenuItemType::CHECK => Kind::Check,
            MenuItemType::RADIO => Kind::Radio,
            MenuItemType::SEPARATOR => Kind::Separator,
            MenuItemType::SUBMENU => Kind::Submenu,
            other => Kind::Unsupported(other.get_raw()),
        }
    }
    fn visible(&self, i: usize) -> bool {
        ImplMenuModel::is_visible_at(self, i) != 0
    }
    fn enabled(&self, i: usize) -> bool {
        ImplMenuModel::is_enabled_at(self, i) != 0
    }
    fn command_id(&self, i: usize) -> i32 {
        ImplMenuModel::command_id_at(self, i)
    }
    fn checked(&self, i: usize) -> bool {
        ImplMenuModel::is_checked_at(self, i) != 0
    }
    fn group_id(&self, i: usize) -> i32 {
        ImplMenuModel::group_id_at(self, i)
    }
    fn label(&self, i: usize, limit: usize) -> Result<String, CopyError> {
        read_userfree(&ImplMenuModel::label_at(self, i), limit)
    }
    fn submenu(&self, i: usize) -> Option<Self> {
        ImplMenuModel::sub_menu_at(self, i)
    }
}

/// CEF UI/callback only. Result remains raw (not normalized), so downstream
/// admission still charges every original row, including invisible entries.
pub(crate) fn copy_cef_menu_model(
    model: &cef::MenuModel,
    x: i32,
    y: i32,
    limits: MenuLimits,
) -> Result<MenuModel, CopyError> {
    if cef::currently_on(cef::ThreadId::UI) == 0 {
        return Err(CopyError::WrongThread);
    }
    copy_root(model, x, y, limits)
}

struct Budget {
    rows: usize,
    labels: usize,
    per_label: usize,
}
impl Budget {
    fn rows(&mut self, count: usize) -> Result<(), CopyError> {
        self.rows = self.rows.checked_sub(count).ok_or(CopyError::RowBudget)?;
        Ok(())
    }
    fn label(&mut self, reader: &impl Reader, index: usize) -> Result<String, CopyError> {
        let limit = self.per_label.min(self.labels);
        if limit == 0 {
            return Err(CopyError::LabelBudget);
        }
        let value = reader.label(index, limit)?;
        // Also defend the seam against a reader that ignores its ceiling.
        if value.len() > limit {
            return Err(CopyError::LabelBudget);
        }
        self.labels -= value.len();
        Ok(value)
    }
    fn command(&mut self, r: &impl Reader, i: usize, kind: Kind) -> Result<MenuCommand, CopyError> {
        Ok(MenuCommand {
            command_id: r.command_id(i),
            label: self.label(r, i)?,
            enabled: r.enabled(i),
            visible: r.visible(i),
            checked: match kind {
                Kind::Check | Kind::Radio => Some(r.checked(i)),
                _ => None,
            },
            radio_group: if kind == Kind::Radio {
                Some(r.group_id(i))
            } else {
                None
            },
        })
    }
}
fn copy_root(r: &impl Reader, x: i32, y: i32, limits: MenuLimits) -> Result<MenuModel, CopyError> {
    let mut budget = Budget {
        rows: limits.max_entries.get(),
        labels: limits.max_total_label_bytes.get(),
        per_label: limits.max_label_bytes.get(),
    };
    let count = r.count();
    budget.rows(count)?;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        entries.push(match r.kind(i) {
            k @ (Kind::Command | Kind::Check | Kind::Radio) => {
                MenuItem::Command(budget.command(r, i, k)?)
            }
            Kind::Separator => MenuItem::Separator {
                visible: r.visible(i),
            },
            Kind::Unsupported(raw) => return Err(CopyError::UnsupportedType(raw)),
            Kind::Submenu => {
                let label = budget.label(r, i)?;
                let child = r.submenu(i).ok_or(CopyError::MissingSubmenu)?;
                let count = child.count();
                budget.rows(count)?;
                let mut items = Vec::with_capacity(count);
                for j in 0..count {
                    items.push(match child.kind(j) {
                        k @ (Kind::Command | Kind::Check | Kind::Radio) => {
                            MenuChild::Command(budget.command(&child, j, k)?)
                        }
                        Kind::Separator => MenuChild::Separator {
                            visible: child.visible(j),
                        },
                        Kind::Submenu => return Err(CopyError::TooDeep),
                        Kind::Unsupported(raw) => return Err(CopyError::UnsupportedType(raw)),
                    });
                }
                if child.count() != count {
                    return Err(CopyError::ChangedDuringCopy);
                }
                MenuItem::Submenu(MenuSubmenu {
                    label,
                    enabled: r.enabled(i),
                    visible: r.visible(i),
                    entries: items,
                })
            }
        });
    }
    if r.count() != count {
        return Err(CopyError::ChangedDuringCopy);
    }
    let model = MenuModel { entries, x, y };
    model.validate(limits).map_err(CopyError::Model)?;
    Ok(model)
}

pub(super) fn read_userfree(
    value: &CefStringUserfreeUtf16,
    limit: usize,
) -> Result<String, CopyError> {
    let raw: Option<&cef::sys::_cef_string_utf16_t> = value.into();
    let Some(raw) = raw else {
        return Ok(String::new());
    };
    // SAFETY: the borrowed userfree owner remains live and immutable throughout
    // the synchronous copy. No raw buffer/view escapes this call.
    unsafe { read_native_utf16(raw, limit) }
}

/// Copies a CEF-owned UTF16 header without an intermediate deep-copy.
///
/// # Safety
/// Any non-null, aligned buffer accepted by the checks below must be valid for
/// raw.length UTF16 units and immutable until return. Its original native owner
/// must remain alive; this function borrows and never clears/frees that buffer.
pub(super) unsafe fn read_native_utf16(
    raw: &cef::sys::_cef_string_utf16_t,
    limit: usize,
) -> Result<String, CopyError> {
    if raw.length == 0 {
        return Ok(String::new());
    }
    if raw.length > limit {
        return Err(CopyError::LabelBudget);
    }
    if raw.str_.is_null()
        || (raw.str_ as usize) % std::mem::align_of::<u16>() != 0
        || raw.length > isize::MAX as usize / 2
    {
        return Err(CopyError::InvalidTextView);
    }
    // From<raw struct> is Borrowed in pinned cef-rs string.rs:547-550, unlike
    // From<&userfree> which deep-copies. The caller holds the original owner;
    // this view has no dtor and cannot escape the synchronous owned copy.
    let view = CefStringUtf16::from(cef::sys::_cef_string_utf16_t {
        str_: raw.str_,
        length: raw.length,
        dtor: None,
    });
    copy_utf16(view.as_slice().ok_or(CopyError::InvalidTextView)?, limit)
}
pub(super) fn copy_utf16(units: &[u16], limit: usize) -> Result<String, CopyError> {
    if units.len() > limit {
        return Err(CopyError::LabelBudget);
    }
    let mut bytes = 0usize;
    for c in char::decode_utf16(units.iter().copied()) {
        bytes = bytes
            .checked_add(c.map_err(|_| CopyError::InvalidUtf16)?.len_utf8())
            .ok_or(CopyError::LabelBudget)?;
        if bytes > limit {
            return Err(CopyError::LabelBudget);
        }
    }
    let mut result = String::with_capacity(bytes);
    for c in char::decode_utf16(units.iter().copied()) {
        result.push(c.map_err(|_| CopyError::InvalidUtf16)?);
    }
    Ok(result)
}

#[cfg(test)]
#[path = "menu_model/tests.rs"]
mod tests;
