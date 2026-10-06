//! OLE drag and drop for the file shelf: the notch as a **drop target** (files dragged onto it) and
//! the shelf's tiles as a **drag source** (files dragged out of it).
//!
//! Both are plain COM objects; nothing is hooked or injected. Dragging out offers *copy and link
//! only*, never move, so a drop onto Explorer can never relocate a file the shelf merely points at.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use notch_core::bus::BusSender;
use notch_core::events::{EventKind, Source};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, POINTL, S_OK,
};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_NONE, DoDragDrop,
    IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, DragQueryFileW, HDROP, ILFree, SHCreateShellItemArrayFromIDLists,
    SHParseDisplayName,
};
use windows::core::{BOOL, HRESULT, PCWSTR, Ref, Result, implement};

use super::util::wide;
use crate::services::shelf::Req as ShelfReq;

/// Where dropped paths go: the shelf service's inbox, if the shelf module is active right now.
pub type ShelfSlot = Arc<Mutex<Option<Sender<ShelfReq>>>>;

/// Most files accepted from a single drop (a whole drive's worth would only hang the UI).
const MAX_DROP: u32 = 200;

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// Does the data object carry a list of files?
fn has_files(d: &IDataObject) -> bool {
    unsafe { d.QueryGetData(&hdrop_format()) == S_OK }
}

/// The file paths in a data object's `CF_HDROP`.
pub fn paths_in(d: &IDataObject) -> Vec<PathBuf> {
    let mut out = Vec::new();
    unsafe {
        let fe = hdrop_format();
        let Ok(mut medium) = d.GetData(&fe) else {
            return out;
        };
        let hglobal = medium.u.hGlobal;
        let locked = GlobalLock(hglobal);
        if !locked.is_null() {
            let hdrop = HDROP(locked);
            let count = DragQueryFileW(hdrop, u32::MAX, None).min(MAX_DROP);
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, None) as usize;
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; len + 1];
                let n = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
                out.push(PathBuf::from(String::from_utf16_lossy(&buf[..n.min(len)])));
            }
            let _ = GlobalUnlock(hglobal);
        }
        ReleaseStgMedium(&mut medium);
    }
    out
}

// ----- the drop target -------------------------------------------------------------------------------

#[implement(IDropTarget)]
pub struct NotchDropTarget {
    bus: BusSender,
    slot: ShelfSlot,
    accepting: std::cell::Cell<bool>,
}

impl NotchDropTarget {
    fn shelf_active(&self) -> bool {
        self.slot.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    fn set_hover(&self, on: bool) {
        self.bus.send(Source::Local, EventKind::DragHover(on));
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for NotchDropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        let ok = self.shelf_active() && pdataobj.as_ref().is_some_and(has_files);
        self.accepting.set(ok);
        unsafe { *pdweffect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        if ok {
            self.set_hover(true);
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        unsafe {
            *pdweffect = if self.accepting.get() {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            }
        };
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        if self.accepting.replace(false) {
            self.set_hover(false);
        }
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> Result<()> {
        let was = self.accepting.replace(false);
        let paths = if was {
            pdataobj.as_ref().map(paths_in).unwrap_or_default()
        } else {
            Vec::new()
        };
        let delivered = !paths.is_empty()
            && self
                .slot
                .lock()
                .ok()
                .and_then(|g| {
                    g.as_ref()
                        .map(|tx| tx.send(ShelfReq::Dropped(paths)).is_ok())
                })
                .unwrap_or(false);
        unsafe {
            *pdweffect = if delivered {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            }
        };
        if was {
            self.set_hover(false);
        }
        Ok(())
    }
}

/// Create the notch's drop target (one per process; register it on every stage window).
pub fn new_target(bus: BusSender, slot: ShelfSlot) -> IDropTarget {
    NotchDropTarget {
        bus,
        slot,
        accepting: std::cell::Cell::new(false),
    }
    .into()
}

pub fn register(hwnd: HWND, target: &IDropTarget) {
    unsafe {
        if let Err(e) = RegisterDragDrop(hwnd, target) {
            crate::warn!("RegisterDragDrop failed (is OLE initialised?): {e}");
        }
    }
}

pub fn revoke(hwnd: HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

// ----- the drag source -------------------------------------------------------------------------------

#[implement(IDropSource)]
struct ShelfDragSource;

#[allow(non_snake_case)]
impl IDropSource_Impl for ShelfDragSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if grfkeystate.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// A shell data object for `paths` (so the target sees a normal file drag: `CF_HDROP`, shell ID
/// lists, drag image). Built from an `IShellItemArray`, which — unlike `SHCreateDataObject` with a
/// parent folder — accepts files from different folders. `None` if no path could be resolved.
pub fn data_object_for(paths: &[String]) -> Option<IDataObject> {
    unsafe {
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        for p in paths {
            let w = wide(p);
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(
                PCWSTR(w.as_ptr()),
                None::<&windows::Win32::System::Com::IBindCtx>,
                &mut pidl,
                0,
                None,
            )
            .is_ok()
                && !pidl.is_null()
            {
                pidls.push(pidl);
            }
        }
        if pidls.is_empty() {
            return None;
        }
        let consts: Vec<*const ITEMIDLIST> =
            pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
        let data: Option<IDataObject> = SHCreateShellItemArrayFromIDLists(&consts)
            .and_then(|items| {
                items.BindToHandler::<_, IDataObject>(
                    None::<&windows::Win32::System::Com::IBindCtx>,
                    &BHID_DataObject,
                )
            })
            .map_err(|e| crate::warn!("shelf: cannot build a shell data object: {e}"))
            .ok();
        for p in pidls {
            ILFree(Some(p));
        }
        data
    }
}

/// Run a drag-and-drop of `paths` out of the notch. **Blocks** (it is OLE's modal loop) until the
/// button is released or Escape is pressed. Returns whether something accepted the drop.
pub fn start_drag(paths: &[String]) -> bool {
    let Some(data) = data_object_for(paths) else {
        crate::warn!("shelf: none of the dragged files could be resolved");
        return false;
    };
    let source: IDropSource = ShelfDragSource.into();
    let mut effect = DROPEFFECT_NONE;
    let ok_effects = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_LINK.0);
    let hr = unsafe { DoDragDrop(&data, &source, ok_effects, &mut effect) };
    hr == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shark-notch-dd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, b"hello").unwrap();
        p
    }

    #[test]
    fn a_shell_data_object_round_trips_file_paths() {
        unsafe {
            let _ = OleInitialize(None);
        }
        let a = temp_file("one.txt");
        let b = temp_file("two with spaces.txt");
        let data = data_object_for(&[
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        ])
        .expect("the shell builds a data object");
        let hr = unsafe { data.QueryGetData(&hdrop_format()) };
        assert!(
            hr == S_OK,
            "it offers CF_HDROP (QueryGetData returned {hr:?})"
        );
        let mut got = paths_in(&data);
        got.sort();
        let mut want = vec![a.clone(), b.clone()];
        want.sort();
        assert_eq!(got, want);
        let _ = std::fs::remove_dir_all(a.parent().unwrap());
        unsafe { OleUninitialize() };
    }

    #[test]
    fn nothing_resolvable_gives_no_data_object() {
        unsafe {
            let _ = OleInitialize(None);
        }
        assert!(data_object_for(&[]).is_none());
        unsafe { OleUninitialize() };
    }
}
