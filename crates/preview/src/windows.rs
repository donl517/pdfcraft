//! The File Explorer preview handler: an in-process COM server exporting `DllGetClassObject`
//! and `DllCanUnloadNow`, whose one class implements `IInitializeWithStream` (Explorer hands it
//! the file as a stream), `IObjectWithSite`, and `IPreviewHandler` (it shows the first page in
//! the preview pane).
//!
//! The pane is a child window of the host window Explorer names in `SetWindow`, created on
//! `DoPreview` and painted from its own `WM_PAINT`, so the page survives every repaint of the
//! host (a resize, another window passing over the pane, a theme change). The first page is
//! rendered once per pane size and kept as the picture the window paints; `SetRect` moves the
//! window and re-renders when its size changed; `Unload` destroys it.
//!
//! Explorer hosts preview handlers in a separate surrogate process; this still never panics across
//! the COM boundary (`catch_unwind` on every entry, and in the window procedure). Failures return
//! `E_FAIL` and the pane stays blank.
//!
//! `unsafe` is confined to this module: exported functions dereference COM pointers, the window
//! procedure reads the picture its window was created with, and GDI paints it.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock};

use crate::{PreviewBitmap, render_page_fit};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, COLORREF, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, ERROR_ALREADY_INITIALIZED,
    ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, S_FALSE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, CreateSolidBrush, DIB_RGB_COLORS, DeleteObject, EndPaint, FillRect, HDC, InvalidateRect,
    PAINTSTRUCT, SRCCOPY, StretchDIBits, UpdateWindow,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl, IStream, STATFLAG_NONAME, STATSTG};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW,
};
use windows::Win32::System::Ole::{IObjectWithSite, IObjectWithSite_Impl};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithStream, IInitializeWithStream_Impl};
use windows::Win32::UI::Shell::{IPreviewHandler, IPreviewHandler_Impl};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, MSG,
    MoveWindow, RegisterClassExW, SetParent, SetWindowLongPtrW, WINDOW_EX_STYLE, WM_ERASEBKGND, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WNDCLASSEXW,
    WS_CHILD, WS_CLIPSIBLINGS, WS_VISIBLE,
};
use windows_core::{BOOL, GUID, HRESULT, IUnknown, Interface, PCWSTR, Ref, implement, w};

/// The preview handler's class id, registered under `PdfCraft.Document\ShellEx` and PreviewHandlers.
pub const CLSID: GUID = GUID::from_u128(0xc31a5f44_7e02_4d9a_a1b3_8f2e6d4c0a91);

/// Files larger than this aren't read for a preview (the whole file is parsed in memory).
pub const MAX_FILE_BYTES: u64 = 64 << 20;

/// The window class of the pane (a child of Explorer's host window).
pub const PANE_CLASS: PCWSTR = w!("PdfCraft.PreviewPane");

/// What the pane paints: the first page rendered for the pane's current size, or nothing yet.
type Picture = Mutex<Option<PreviewBitmap>>;

/// The document bytes, Explorer's host window and rectangle, and our pane inside it.
///
/// The pane's window procedure reads `picture` through a raw pointer stored on the window. That
/// is sound because the handler owns the window: `Unload` and `Drop` destroy it before `picture`
/// can go away, and the pane is only ever created, moved, painted and destroyed on the host's
/// own (apartment) thread.
#[implement(IInitializeWithStream, IObjectWithSite, IPreviewHandler)]
#[derive(Default)]
struct Handler {
    bytes: Mutex<Option<Arc<Vec<u8>>>>,
    host: Mutex<Option<HWND>>,
    rect: Mutex<RECT>,
    pane: Mutex<Option<HWND>>,
    /// The pane size the picture was rendered for.
    rendered_for: Mutex<Option<(u32, u32)>>,
    picture: Arc<Picture>,
}

impl Drop for Handler {
    fn drop(&mut self) {
        // Explorer normally calls Unload first; a release without it must not leave a window
        // whose procedure points at a picture that is about to be freed.
        if let Some(pane) = self.pane.get_mut().unwrap_or_else(|p| p.into_inner()).take() {
            // SAFETY: the pane is a window this handler created and still owns.
            unsafe {
                let _ = DestroyWindow(pane);
            }
        }
    }
}

fn guarded<T>(f: impl FnOnce() -> windows_core::Result<T>) -> windows_core::Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(E_FAIL.into()),
    }
}

impl IInitializeWithStream_Impl for Handler_Impl {
    fn Initialize(&self, stream: Ref<IStream>, _mode: u32) -> windows_core::Result<()> {
        guarded(|| {
            let mut slot = self.bytes.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_some() {
                return Err(ERROR_ALREADY_INITIALIZED.to_hresult().into());
            }
            *slot = Some(Arc::new(read_all(stream.ok()?)?));
            Ok(())
        })
    }
}

impl IObjectWithSite_Impl for Handler_Impl {
    fn SetSite(&self, _punksite: Ref<IUnknown>) -> windows_core::Result<()> {
        Ok(())
    }

    fn GetSite(&self, _riid: *const GUID, ppvsite: *mut *mut c_void) -> windows_core::Result<()> {
        if ppvsite.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: non-null out-parameter; no site is exposed.
        unsafe {
            *ppvsite = std::ptr::null_mut();
        }
        Err(E_FAIL.into())
    }
}

impl Handler_Impl {
    fn host(&self) -> windows_core::Result<HWND> {
        self.host.lock().unwrap_or_else(|p| p.into_inner()).ok_or(windows_core::Error::from(E_FAIL))
    }

    fn pane(&self) -> Option<HWND> {
        *self.pane.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn rect(&self) -> RECT {
        *self.rect.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The pane inside the host window, created the first time it is needed.
    fn ensure_pane(&self) -> windows_core::Result<HWND> {
        if let Some(pane) = self.pane() {
            return Ok(pane);
        }
        let host = self.host()?;
        let rect = self.rect();
        let (w, h) = rect_size(&rect)?;
        let pane = create_pane(host, &rect, w, h, &self.picture)?;
        *self.pane.lock().unwrap_or_else(|p| p.into_inner()) = Some(pane);
        Ok(pane)
    }

    /// Renders the first page for the current rectangle unless the picture already fits it.
    fn render_for_rect(&self) -> windows_core::Result<()> {
        let bytes = self.bytes.lock().unwrap_or_else(|p| p.into_inner()).clone().ok_or(windows_core::Error::from(E_INVALIDARG))?;
        let size = rect_size(&self.rect())?;
        if *self.rendered_for.lock().unwrap_or_else(|p| p.into_inner()) == Some(size) {
            return Ok(());
        }
        let picture = render_page_fit(bytes, size.0, size.1).map_err(|_| windows_core::Error::from(E_FAIL))?;
        *self.picture.lock().unwrap_or_else(|p| p.into_inner()) = Some(picture);
        *self.rendered_for.lock().unwrap_or_else(|p| p.into_inner()) = Some(size);
        Ok(())
    }

    /// Shows the loaded document in the pane: renders it for the pane's size and repaints.
    fn show(&self) -> windows_core::Result<()> {
        self.render_for_rect()?;
        let pane = self.ensure_pane()?;
        // SAFETY: `pane` is the window this handler created.
        unsafe {
            let _ = InvalidateRect(Some(pane), None, false);
            let _ = UpdateWindow(pane);
        }
        Ok(())
    }

    /// Moves the pane to the current rectangle.
    fn place_pane(&self) -> windows_core::Result<()> {
        if let Some(pane) = self.pane() {
            let rect = self.rect();
            let (w, h) = rect_size(&rect)?;
            // SAFETY: `pane` is the window this handler created.
            unsafe { MoveWindow(pane, rect.left, rect.top, w as i32, h as i32, true)? };
        }
        Ok(())
    }
}

impl IPreviewHandler_Impl for Handler_Impl {
    fn SetWindow(&self, hwnd: HWND, prc: *const RECT) -> windows_core::Result<()> {
        if prc.is_null() {
            return Err(E_POINTER.into());
        }
        guarded(|| {
            *self.host.lock().unwrap_or_else(|p| p.into_inner()) = Some(hwnd);
            // SAFETY: `prc` is non-null and, per the COM contract, points to the host's rectangle.
            *self.rect.lock().unwrap_or_else(|p| p.into_inner()) = unsafe { *prc };
            if let Some(pane) = self.pane() {
                // SAFETY: `pane` is the window this handler created; `hwnd` is the host's new window.
                unsafe { SetParent(pane, Some(hwnd))? };
                self.place_pane()?;
            }
            Ok(())
        })
    }

    fn SetRect(&self, prc: *const RECT) -> windows_core::Result<()> {
        if prc.is_null() {
            return Err(E_POINTER.into());
        }
        guarded(|| {
            // SAFETY: `prc` is non-null and, per the COM contract, points to the new rectangle.
            *self.rect.lock().unwrap_or_else(|p| p.into_inner()) = unsafe { *prc };
            self.place_pane()?;
            if self.pane().is_some() && self.bytes.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
                self.show()?;
            }
            Ok(())
        })
    }

    fn DoPreview(&self) -> windows_core::Result<()> {
        guarded(|| self.show())
    }

    fn Unload(&self) -> windows_core::Result<()> {
        guarded(|| {
            if let Some(pane) = self.pane.lock().unwrap_or_else(|p| p.into_inner()).take() {
                // SAFETY: `pane` is the window this handler created.
                unsafe { DestroyWindow(pane)? };
            }
            *self.bytes.lock().unwrap_or_else(|p| p.into_inner()) = None;
            *self.picture.lock().unwrap_or_else(|p| p.into_inner()) = None;
            *self.rendered_for.lock().unwrap_or_else(|p| p.into_inner()) = None;
            *self.host.lock().unwrap_or_else(|p| p.into_inner()) = None;
            *self.rect.lock().unwrap_or_else(|p| p.into_inner()) = RECT::default();
            Ok(())
        })
    }

    fn SetFocus(&self) -> windows_core::Result<()> {
        guarded(|| {
            if let Some(hwnd) = self.pane().or_else(|| self.host().ok()) {
                // SAFETY: the pane is ours, or the host window Explorer gave us in SetWindow.
                unsafe {
                    let _ = SetFocus(Some(hwnd));
                }
            }
            Ok(())
        })
    }

    fn QueryFocus(&self) -> windows_core::Result<HWND> {
        guarded(|| match self.pane() {
            Some(pane) => Ok(pane),
            None => self.host(),
        })
    }

    fn TranslateAccelerator(&self, _pmsg: *const MSG) -> windows_core::Result<()> {
        Err(S_FALSE.into())
    }
}

fn rect_size(rect: &RECT) -> windows_core::Result<(u32, u32)> {
    let w = i64::from(rect.right) - i64::from(rect.left);
    let h = i64::from(rect.bottom) - i64::from(rect.top);
    if w <= 0 || h <= 0 {
        return Err(E_INVALIDARG.into());
    }
    let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else {
        return Err(E_OUTOFMEMORY.into());
    };
    Ok((w, h))
}

/// Reads the whole stream (up to [`MAX_FILE_BYTES`]).
fn read_all(stream: &IStream) -> windows_core::Result<Vec<u8>> {
    let mut stat = STATSTG::default();
    // SAFETY: `stat` is a valid, writable STATSTG; with STATFLAG_NONAME no name is allocated.
    unsafe { stream.Stat(&mut stat, STATFLAG_NONAME)? };
    if stat.cbSize > MAX_FILE_BYTES {
        return Err(E_FAIL.into());
    }
    let size = usize::try_from(stat.cbSize).map_err(|_| windows_core::Error::from(E_OUTOFMEMORY))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| windows_core::Error::from(E_OUTOFMEMORY))?;
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let mut read = 0u32;
        // SAFETY: `chunk` is a writable buffer of exactly the length passed, and `read` a valid u32.
        let hr = unsafe { stream.Read(chunk.as_mut_ptr().cast(), chunk.len() as u32, Some(&mut read)) };
        hr.ok()?;
        let got = chunk.get(..read as usize).ok_or(windows_core::Error::from(E_FAIL))?;
        if bytes.len().saturating_add(got.len()) as u64 > MAX_FILE_BYTES {
            return Err(E_FAIL.into());
        }
        bytes.try_reserve(got.len()).map_err(|_| windows_core::Error::from(E_OUTOFMEMORY))?;
        bytes.extend_from_slice(got);
        if read == 0 || hr == S_FALSE {
            return Ok(bytes);
        }
    }
}

/// This DLL's module handle, so the pane's window class belongs to it, not to the host process.
fn module() -> windows_core::Result<HINSTANCE> {
    let mut module = HMODULE::default();
    // SAFETY: the address is this module's own window procedure; the handle is written to `module`
    // and, with UNCHANGED_REFCOUNT, the DLL's reference count is left alone.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(pane_proc as *const u16),
            &mut module,
        )?;
    }
    Ok(HINSTANCE(module.0))
}

/// Registers the pane's window class once per process; returns the instance it was registered for.
fn pane_class() -> windows_core::Result<HINSTANCE> {
    // The handle is kept as an integer: a raw pointer would make the lock unshareable.
    static CLASS: OnceLock<Result<isize, windows_core::Error>> = OnceLock::new();
    CLASS
        .get_or_init(|| -> Result<isize, windows_core::Error> {
            let instance = module()?;
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(pane_proc),
                hInstance: instance,
                lpszClassName: PANE_CLASS,
                ..Default::default()
            };
            // SAFETY: `class` is a fully initialized WNDCLASSEXW whose name is a static string.
            if unsafe { RegisterClassExW(&class) } == 0 {
                let error = windows_core::Error::from_thread();
                if error.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
                    return Err(error);
                }
            }
            Ok(instance.0 as isize)
        })
        .clone()
        .map(|instance| HINSTANCE(instance as *mut c_void))
}

/// Creates the pane as a visible child of `host` at `rect`, whose procedure paints `picture`.
fn create_pane(host: HWND, rect: &RECT, width: u32, height: u32, picture: &Arc<Picture>) -> windows_core::Result<HWND> {
    let instance = pane_class()?;
    // SAFETY: `host` is the window Explorer gave us; the class is registered; the create
    // parameter is the handler's picture, which outlives the window (see `Handler`).
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PANE_CLASS,
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            rect.left,
            rect.top,
            width as i32,
            height as i32,
            Some(host),
            None,
            Some(instance),
            Some(Arc::as_ptr(picture).cast()),
        )
    }
}

/// The picture a pane window was created with, or `None` once it is gone.
fn pane_picture(pane: HWND) -> Option<*const Picture> {
    // SAFETY: GWLP_USERDATA of our pane holds the pointer WM_NCCREATE stored, or 0.
    let p = unsafe { GetWindowLongPtrW(pane, GWLP_USERDATA) } as *const Picture;
    (!p.is_null()).then_some(p)
}

/// The pane's window procedure: paints the picture, and never lets a panic reach the host.
extern "system" fn pane_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| pane_message(hwnd, msg, wparam, lparam))) {
        Ok(Some(result)) => result,
        // SAFETY: the arguments are the ones Windows passed to this procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Handles the messages the pane cares about; `None` leaves the rest to `DefWindowProcW`.
fn pane_message(hwnd: HWND, msg: u32, _wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    match msg {
        WM_NCCREATE => {
            // SAFETY: for WM_NCCREATE, lParam points to the CREATESTRUCTW of the CreateWindowExW
            // call, whose lpCreateParams is the picture pointer `create_pane` passed.
            let picture = unsafe { (*(lparam.0 as *const CREATESTRUCTW)).lpCreateParams };
            // SAFETY: `hwnd` is the window being created.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, picture as isize) };
            None
        }
        WM_NCDESTROY => {
            // SAFETY: `hwnd` is the window being destroyed; the picture is the handler's to free.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            None
        }
        // The paint covers the whole client area, so no background erase (it would flicker).
        WM_ERASEBKGND => Some(LRESULT(1)),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let mut client = RECT::default();
            // SAFETY: `hwnd` is our pane handling its own WM_PAINT; `ps` and `client` are valid
            // out-parameters; BeginPaint and EndPaint are paired; the picture pointer, when set,
            // is the handler's, alive while the window is (see `Handler`).
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                if !hdc.is_invalid() {
                    if GetClientRect(hwnd, &mut client).is_ok() {
                        match pane_picture(hwnd) {
                            Some(picture) => paint(hdc, &client, (*picture).lock().unwrap_or_else(|p| p.into_inner()).as_ref()),
                            None => paint(hdc, &client, None),
                        }
                    }
                    let _ = EndPaint(hwnd, &ps);
                }
            }
            Some(LRESULT(0))
        }
        _ => None,
    }
}

/// Paints white paper over `dest` and, when there is one, the picture letterboxed on it.
fn paint(hdc: HDC, dest: &RECT, picture: Option<&PreviewBitmap>) {
    // SAFETY: `hdc` is a device context the caller owns for the duration of this call; the brush
    // is created and deleted here.
    unsafe {
        let brush = CreateSolidBrush(COLORREF(0x00FF_FFFF));
        if !brush.is_invalid() {
            let _ = FillRect(hdc, dest, brush);
            let _ = DeleteObject(brush.into());
        }
    }
    let Some(thumb) = picture else { return };
    let Ok((dest_w, dest_h)) = rect_size(dest) else { return };
    let (Ok(tw), Ok(th)) = (i32::try_from(thumb.width), i32::try_from(thumb.height)) else { return };
    if tw <= 0 || th <= 0 || thumb.bgra.len() != (tw as usize).saturating_mul(th as usize).saturating_mul(4) {
        return;
    }
    let scale = (dest_w as f32 / tw as f32).min(dest_h as f32 / th as f32);
    let draw_w = (tw as f32 * scale).round().clamp(1.0, dest_w as f32) as i32;
    let draw_h = (th as f32 * scale).round().clamp(1.0, dest_h as f32) as i32;
    let x = dest.left + (i32::try_from(dest_w).unwrap_or(0) - draw_w) / 2;
    let y = dest.top + (i32::try_from(dest_h).unwrap_or(0) - draw_h) / 2;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: tw,
            biHeight: -th,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `info` describes `thumb.bgra` exactly (checked above: width × height × 4 bytes,
    // top-down 32-bit rows), and `hdc` is valid for this call.
    unsafe {
        let _ = StretchDIBits(hdc, x, y, draw_w, draw_h, 0, 0, tw, th, Some(thumb.bgra.as_ptr().cast()), &info, DIB_RGB_COLORS, SRCCOPY);
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> windows_core::Result<()> {
        if ppv.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: `ppv` is non-null and points to the caller's out-parameter.
        unsafe { *ppv = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        guarded(|| {
            let handler: IUnknown = Handler::default().into();
            // SAFETY: `riid` and `ppv` are non-null and valid for the call (checked above).
            unsafe { handler.query(riid, ppv) }.ok()
        })
    }

    fn LockServer(&self, _lock: BOOL) -> windows_core::Result<()> {
        Ok(())
    }
}

/// COM entry point: the class factory for [`CLSID`].
///
/// # Safety
/// Called by COM with valid pointers (each is still checked for null).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    match guarded(|| {
        // SAFETY: all three pointers are non-null and, per the COM contract, valid for this call.
        unsafe {
            *ppv = std::ptr::null_mut();
            if *rclsid != CLSID {
                return Err(CLASS_E_CLASSNOTAVAILABLE.into());
            }
            let factory: IClassFactory = Factory.into();
            factory.query(riid, ppv).ok()
        }
    }) {
        Ok(()) => HRESULT(0),
        Err(error) => error.code(),
    }
}

/// COM entry point. The DLL stays loaded for the life of the preview host process.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{CreateCompatibleDC, CreateDIBSection, DeleteDC, GetPixel, SelectObject};
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::Shell::SHCreateMemStream;
    use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, FindWindowExW, HWND_MESSAGE, WINDOW_STYLE};

    /// A child window under the message-only root: valid without an interactive desktop (CI).
    fn preview_host_window(width: i32, height: i32) -> windows_core::Result<HWND> {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::w!("STATIC"),
                windows::core::w!(""),
                WINDOW_STYLE(WS_CHILD.0),
                0,
                0,
                width,
                height,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
    }

    /// The pane inside `host`, if there is one.
    fn pane_of(host: HWND) -> Option<HWND> {
        unsafe { FindWindowExW(Some(host), None, PANE_CLASS, None).ok() }
    }

    fn client_size(hwnd: HWND) -> (i32, i32) {
        let mut r = RECT::default();
        unsafe { GetClientRect(hwnd, &mut r).unwrap() };
        (r.right - r.left, r.bottom - r.top)
    }

    fn pdf(w: u32, h: u32, content: &str) -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents 4 0 R >>"),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len() + 1),
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
        out
    }

    #[test]
    fn explorer_gets_a_pane_that_follows_the_rectangle_and_goes_with_unload() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
            let mut factory: *mut c_void = std::ptr::null_mut();
            DllGetClassObject(&CLSID, &IClassFactory::IID, &mut factory).ok().unwrap();
            let factory = IClassFactory::from_raw(factory);
            let init: IInitializeWithStream = factory.CreateInstance(None).unwrap();
            let bytes = pdf(200, 100, "1 0 0 rg 0 0 100 100 re f");
            let stream = SHCreateMemStream(Some(&bytes)).unwrap();
            init.Initialize(&stream, 0).unwrap();
            let preview: IPreviewHandler = init.cast().unwrap();
            let host = preview_host_window(320, 240).expect("a message-only host window for the preview pane");
            let rect = RECT { left: 0, top: 0, right: 320, bottom: 240 };
            preview.SetWindow(host, &rect).unwrap();
            assert!(pane_of(host).is_none(), "SetWindow alone creates no pane");
            preview.DoPreview().unwrap();
            let pane = pane_of(host).expect("DoPreview creates the pane inside the host");
            assert_eq!(client_size(pane), (320, 240));
            assert_eq!(preview.QueryFocus().unwrap(), pane);
            let smaller = RECT { left: 10, top: 20, right: 170, bottom: 140 };
            preview.SetRect(&smaller).unwrap();
            assert_eq!(pane_of(host), Some(pane), "SetRect moves the pane, it does not recreate it");
            assert_eq!(client_size(pane), (160, 120));
            preview.Unload().unwrap();
            assert!(pane_of(host).is_none(), "Unload destroys the pane");
            DestroyWindow(host).unwrap();
        }
    }

    #[test]
    fn a_second_host_window_takes_the_pane_over() {
        unsafe {
            let init: IInitializeWithStream = Handler::default().into();
            let preview: IPreviewHandler = init.cast().unwrap();
            let bytes = pdf(100, 100, "0 0 1 rg 0 0 50 50 re f");
            let stream = SHCreateMemStream(Some(&bytes)).unwrap();
            init.Initialize(&stream, 0).unwrap();
            let first = preview_host_window(100, 100).unwrap();
            let second = preview_host_window(200, 200).unwrap();
            preview.SetWindow(first, &RECT { left: 0, top: 0, right: 100, bottom: 100 }).unwrap();
            preview.DoPreview().unwrap();
            let pane = pane_of(first).unwrap();
            preview.SetWindow(second, &RECT { left: 0, top: 0, right: 200, bottom: 200 }).unwrap();
            assert!(pane_of(first).is_none());
            assert_eq!(pane_of(second), Some(pane));
            assert_eq!(client_size(pane), (200, 200));
            preview.Unload().unwrap();
            let _ = DestroyWindow(first);
            let _ = DestroyWindow(second);
        }
    }

    #[test]
    fn a_released_handler_takes_its_pane_with_it() {
        unsafe {
            let init: IInitializeWithStream = Handler::default().into();
            let preview: IPreviewHandler = init.cast().unwrap();
            let bytes = pdf(100, 100, "0 0 1 rg 0 0 50 50 re f");
            let stream = SHCreateMemStream(Some(&bytes)).unwrap();
            init.Initialize(&stream, 0).unwrap();
            let host = preview_host_window(100, 100).unwrap();
            preview.SetWindow(host, &RECT { left: 0, top: 0, right: 100, bottom: 100 }).unwrap();
            preview.DoPreview().unwrap();
            assert!(pane_of(host).is_some());
            drop(preview);
            drop(init);
            assert!(pane_of(host).is_none(), "no Unload, but the last release destroys the pane");
            let _ = DestroyWindow(host);
        }
    }

    #[test]
    fn the_pane_paints_the_page_on_white_paper() {
        let picture = PreviewBitmap { width: 2, height: 1, bgra: vec![0, 0, 255, 255, 0, 0, 255, 255] };
        let (red, white) = unsafe {
            let hdc = CreateCompatibleDC(None);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 40,
                    biHeight: -40,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let old = SelectObject(hdc, bitmap.into());
            paint(hdc, &RECT { left: 0, top: 0, right: 40, bottom: 40 }, Some(&picture));
            // A 2×1 picture in a 40×40 pane: 40×20, centred, rows 10..30.
            let red = GetPixel(hdc, 20, 20);
            let white = GetPixel(hdc, 20, 2);
            SelectObject(hdc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(hdc);
            (red, white)
        };
        assert_eq!(red.0 & 0x00FF_FFFF, 0x0000_00FF, "COLORREF is 0x00BBGGRR: red");
        assert_eq!(white.0 & 0x00FF_FFFF, 0x00FF_FFFF);
    }

    #[test]
    fn unreadable_files_fail_without_panicking() {
        unsafe {
            let init: IInitializeWithStream = Handler::default().into();
            let preview: IPreviewHandler = init.cast().unwrap();
            let stream = SHCreateMemStream(Some(b"not a PDF")).unwrap();
            init.Initialize(&stream, 0).unwrap();
            let hwnd = preview_host_window(64, 64).expect("a message-only host window for the preview pane");
            let rect = RECT { left: 0, top: 0, right: 64, bottom: 64 };
            preview.SetWindow(hwnd, &rect).unwrap();
            assert!(preview.DoPreview().is_err());
            preview.Unload().unwrap();
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn other_class_ids_are_refused() {
        unsafe {
            let mut out: *mut c_void = std::ptr::null_mut();
            let other = GUID::from_u128(1);
            assert_eq!(DllGetClassObject(&other, &IClassFactory::IID, &mut out), CLASS_E_CLASSNOTAVAILABLE);
            assert!(out.is_null());
        }
        assert_eq!(DllCanUnloadNow(), S_FALSE);
    }

    #[test]
    fn com_guard_turns_an_escaped_panic_into_a_failure() {
        assert!(guarded(|| -> windows_core::Result<()> { panic!("synthetic COM failure") }).is_err());
    }
}
