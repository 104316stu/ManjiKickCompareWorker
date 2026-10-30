//! The tray popup: a small borderless WebView2 panel above the tray icon,
//! showing the worker's numbers and settings. WebView2 ships with Windows
//! 10/11; without it the tray falls back to its right-click menu.
//!
//! The page (popup.html) gets the state as JSON every second while it is
//! open and sends commands back as {cmd, value} strings.

use std::cell::RefCell;
use std::time::{Duration, Instant};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    WebMessageReceivedEventHandler,
};
use windows::Win32::Foundation::{E_POINTER, HWND as WHWND, RECT as WRECT};
use windows_core::{HSTRING, PCWSTR, PWSTR};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const HTML: &str = include_str!("popup.html");
const LOGO: &str = include_str!("../../../assets/icon.svg");
/// Logical size (96 dpi); scaled by the monitor's DPI.
const WIDTH: i32 = 620;
const HEIGHT: i32 = 404;

/// What the page asks for: (cmd, value).
pub type Handler = fn(&str, &str);

struct Popup {
    hwnd: HWND,
    tray: HWND,
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
    visible: bool,
    ready: bool,
    hidden_at: Option<Instant>,
    /// Creating WebView2 failed (when): use the menu, retry a bit later.
    failed: Option<Instant>,
    handler: Handler,
    /// The panel's data as JSON (from the tray).
    state: fn() -> String,
    data_dir: std::path::PathBuf,
}

thread_local! {
    /// When the current open was asked for (click, second launch), for the
    /// timing lines in the log.
    static OPENED_AT: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static POPUP: RefCell<Option<Popup>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Sets up the (hidden) popup window. Call once on the tray's UI thread.
pub fn init(tray: HWND, data_dir: std::path::PathBuf, handler: Handler, state: fn() -> String) {
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("ManjiKickWorkerPopup");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class.as_ptr(),
            wide("ManjiKick worker").as_ptr(),
            WS_POPUP,
            0, 0, WIDTH, HEIGHT,
            std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null(),
        );
        // Windows 11: rounded corners and a quiet border.
        let round: u32 = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(hwnd, 33, &round as *const _ as *const _, 4);
        let border: u32 = 0x0038_302a; // #2a3038 as COLORREF
        DwmSetWindowAttribute(hwnd, 34, &border as *const _ as *const _, 4);
        POPUP.with(|p| {
            *p.borrow_mut() = Some(Popup {
                hwnd, tray, controller: None, webview: None, visible: false, ready: false,
                hidden_at: None, failed: hwnd.is_null().then(Instant::now), handler, state, data_dir,
            })
        });
    }
}

fn create_webview(hwnd: HWND, data_dir: &std::path::Path, handler: Handler) -> Result<(ICoreWebView2Controller, ICoreWebView2), String> {
    let _ = std::fs::create_dir_all(data_dir);
    let data = HSTRING::from(data_dir.as_os_str());
    let (tx, rx) = std::sync::mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |h| unsafe {
            CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &data, None, &h)
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |code, env| {
            code?;
            tx.send(env.ok_or_else(|| windows::core::Error::from(E_POINTER))).ok();
            Ok(())
        }),
    )
    .map_err(|e| format!("WebView2 environment: {e:?}"))?;
    let env = rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;

    let (tx, rx) = std::sync::mpsc::channel();
    let parent = WHWND(hwnd);
    CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |h| unsafe {
            env.CreateCoreWebView2Controller(parent, &h).map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |code, controller| {
            code?;
            tx.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER))).ok();
            Ok(())
        }),
    )
    .map_err(|e| format!("WebView2 controller: {e:?}"))?;
    let controller = rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;

    unsafe {
        let webview = controller.CoreWebView2().map_err(|e| e.to_string())?;
        if let Ok(settings) = webview.Settings() {
            let _ = settings.SetAreDefaultContextMenusEnabled(false);
            let _ = settings.SetAreDevToolsEnabled(false);
            let _ = settings.SetIsZoomControlEnabled(false);
            let _ = settings.SetIsStatusBarEnabled(false);
        }
        if let Ok(c2) = windows_core::Interface::cast::<ICoreWebView2Controller2>(&controller) {
            let _ = c2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: 0x1b, G: 0x1f, B: 0x24 });
        }
        let on_message = WebMessageReceivedEventHandler::create(Box::new(move |_wv, args| {
            if let Some(args) = args {
                let mut raw = PWSTR::null();
                if args.TryGetWebMessageAsString(&mut raw).is_ok() {
                    let text = webview2_com::take_pwstr(raw);
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        let cmd = v["cmd"].as_str().unwrap_or("").to_string();
                        let value = v["value"].as_str().unwrap_or("").to_string();
                        on_page(&cmd, &value, handler);
                    }
                }
            }
            Ok(())
        }));
        let mut token = windows::Win32::System::WinRT::EventRegistrationToken::default();
        webview.add_WebMessageReceived(&on_message, &mut token).map_err(|e| e.to_string())?;
        webview.NavigateToString(&HSTRING::from(HTML.replace("<!--logo-->", LOGO))).map_err(|e| e.to_string())?;
        Ok((controller, webview))
    }
}

fn on_page(cmd: &str, value: &str, handler: Handler) {
    match cmd {
        "ready" => {
            POPUP.with(|p| {
                if let Ok(mut b) = p.try_borrow_mut() {
                    if let Some(p) = b.as_mut() {
                        p.ready = true;
                    }
                }
            });
            handler("refresh", "");
            POPUP.with(|p| {
                if let Ok(b) = p.try_borrow() {
                    if let Some(p) = b.as_ref() {
                        if let Some(w) = &p.webview {
                            let _ = unsafe { w.PostWebMessageAsJson(&HSTRING::from((p.state)())) };
                        }
                    }
                }
            });
            crate::log!("panel: page loaded {} ms after start or click", since_click());
            POPUP.with(|p| {
                if let Ok(b) = p.try_borrow() {
                    if let Some(w) = b.as_ref().and_then(|p| p.webview.as_ref()) {
                        ping(w);
                    }
                }
            });
        }
        "hide" => hide(),
        "painted" => {
            if let Some(t) = OPENED_AT.with(|o| o.take()) {
                crate::log!("panel: painted {} ms after the click", t.elapsed().as_millis());
            }
        }
        _ => handler(cmd, value),
    }
}

/// True when the popup is (now) shown; false means use the menu instead.
pub fn toggle() -> bool {
    let state = POPUP.with(|p| p.borrow().as_ref().map(|p| (p.visible, p.failed, p.hidden_at)));
    let Some((visible, failed, hidden_at)) = state else { return false };
    if failed.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
        return false;
    }
    if visible {
        hide();
        return true;
    }
    // A click on the icon first takes focus from the popup (which hides it):
    // that click means "close", not "open again".
    if hidden_at.is_some_and(|t| t.elapsed() < Duration::from_millis(350)) {
        return true;
    }
    OPENED_AT.with(|o| o.set(Some(Instant::now())));
    crate::log!("panel: click");
    show()
}

/// LOW while hidden keeps the kept-alive panel small; NORMAL while shown.
fn memory_level(w: &ICoreWebView2, low: bool) {
    if let Ok(w19) = windows_core::Interface::cast::<ICoreWebView2_19>(w) {
        let level = if low { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW } else { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL };
        let _ = unsafe { w19.SetMemoryUsageTargetLevel(level) };
    }
}

fn since_click() -> u128 {
    OPENED_AT.with(|o| o.get()).map(|t| t.elapsed().as_millis()).unwrap_or(0)
}

/// Asks the page to report its next painted frame ("painted").
fn ping(w: &ICoreWebView2) {
    let _ = unsafe { w.PostWebMessageAsJson(&HSTRING::from("{\"ping\":1}")) };
}

/// Creates the panel hidden at startup, so a click only has to show it.
pub fn prewarm() -> bool {
    let ok = ensure_webview();
    POPUP.with(|p| {
        if let Ok(b) = p.try_borrow() {
            if let Some(w) = b.as_ref().and_then(|p| p.webview.as_ref()) {
                memory_level(w, true);
            }
        }
    });
    ok
}

fn show() -> bool {
    if !ensure_webview() {
        return false;
    }
    show_window()
}

fn ensure_webview() -> bool {
    // Create on first use (and again after it was closed to save memory).
    let need = POPUP.with(|p| p.borrow().as_ref().map(|p| (p.controller.is_none(), p.hwnd, p.data_dir.clone(), p.handler)));
    let Some((need, hwnd, data_dir, handler)) = need else { return false };
    if need {
        let t = Instant::now();
        let made = create_webview(hwnd, &data_dir, handler);
        if made.is_ok() {
            crate::log!("panel: WebView2 created in {} ms", t.elapsed().as_millis());
        }
        match made {
            Ok((c, w)) => POPUP.with(|p| {
                if let Some(p) = p.borrow_mut().as_mut() {
                    if p.failed.take().is_some() {
                        crate::log!("popup ready");
                    }
                    p.controller = Some(c);
                    p.webview = Some(w);
                    p.ready = false;
                }
            }),
            Err(e) => {
                crate::log!("popup unavailable, using the menu: {e}");
                POPUP.with(|p| {
                    if let Some(p) = p.borrow_mut().as_mut() {
                        p.failed = Some(Instant::now());
                    }
                });
                return false;
            }
        }
    }
    true
}

fn show_window() -> bool {
    POPUP.with(|p| {
        let mut b = p.borrow_mut();
        let Some(p) = b.as_mut() else { return false };
        unsafe {
            let dpi = GetDpiForWindow(p.hwnd).max(96) as i32;
            let (w, h) = (WIDTH * dpi / 96, HEIGHT * dpi / 96);
            let (x, y) = place(p.tray, w, h, 8 * dpi / 96);
            // Bounds first while hidden, then show: no layout work on screen.
            if let Some(c) = &p.controller {
                let _ = c.SetBounds(WRECT { left: 0, top: 0, right: w, bottom: h });
            }
            SetWindowPos(p.hwnd, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
            SetForegroundWindow(p.hwnd);
            if let Some(c) = &p.controller {
                let _ = c.SetIsVisible(true);
                let _ = c.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
            }
            if let Some(wv) = &p.webview {
                memory_level(wv, false);
                if p.ready {
                    // Fresh numbers right away (the page already shows the last ones).
                    let _ = wv.PostWebMessageAsJson(&HSTRING::from((p.state)()));
                }
            }
        }
        p.visible = true;
        p.hidden_at = None;
        crate::log!("panel: window shown {} ms after the click", since_click());
        if p.ready {
            if let Some(w) = &p.webview {
                ping(w);
            }
        }
        true
    })
}

/// Above the tray icon, inside the work area of its monitor.
unsafe fn place(tray: HWND, w: i32, h: i32, gap: i32) -> (i32, i32) {
    let id = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: tray,
        uID: 1,
        guidItem: std::mem::zeroed(),
    };
    let mut icon: RECT = std::mem::zeroed();
    if Shell_NotifyIconGetRect(&id, &mut icon) != 0 {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        icon = RECT { left: pt.x, top: pt.y, right: pt.x, bottom: pt.y };
    }
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(MonitorFromRect(&icon, MONITOR_DEFAULTTONEAREST), &mut mi);
    let work = mi.rcWork;
    let cx = (icon.left + icon.right) / 2;
    let cy = (icon.top + icon.bottom) / 2;
    let mut x = cx - w / 2;
    let mut y = work.bottom - h - gap; // taskbar at the bottom (usual)
    if cy < work.top {
        y = work.top + gap; // at the top
    }
    if cx >= work.right {
        x = work.right - w - gap; // on the right
        y = cy - h / 2;
    } else if cx < work.left {
        x = work.left + gap; // on the left
        y = cy - h / 2;
    }
    x = x.clamp(work.left + gap, (work.right - w - gap).max(work.left));
    y = y.clamp(work.top + gap, (work.bottom - h - gap).max(work.top));
    (x, y)
}

pub fn hide() {
    POPUP.with(|p| {
        // Messages can arrive while show() holds the state; never panic here.
        let Ok(mut b) = p.try_borrow_mut() else { return };
        if let Some(p) = b.as_mut() {
            if p.visible {
                unsafe { ShowWindow(p.hwnd, SW_HIDE) };
                if let Some(c) = &p.controller {
                    let _ = unsafe { c.SetIsVisible(false) };
                }
                if let Some(w) = &p.webview {
                    memory_level(w, true);
                }
                p.visible = false;
                p.hidden_at = Some(Instant::now());
            }
        }
    });
}

pub fn is_visible() -> bool {
    POPUP.with(|p| p.try_borrow().map(|b| b.as_ref().is_some_and(|p| p.visible)).unwrap_or(true))
}

/// Every second from the tray's timer: fresh data while open, nothing
/// while hidden (the panel is kept, at low memory, so it opens instantly).
pub fn tick(state: impl FnOnce() -> String) {
    POPUP.with(|p| {
        let Ok(mut b) = p.try_borrow_mut() else { return };
        let Some(p) = b.as_mut() else { return };
        if p.visible && p.ready {
            if let Some(w) = &p.webview {
                let _ = unsafe { w.PostWebMessageAsJson(&HSTRING::from(state())) };
            }
        }
    });
}

/// Pushes the state now (after a click), not at the next tick.
pub fn refresh(state: String) {
    POPUP.with(|p| {
        let Ok(b) = p.try_borrow() else { return };
        if let Some(p) = b.as_ref() {
            if p.visible && p.ready {
                if let Some(w) = &p.webview {
                    let _ = unsafe { w.PostWebMessageAsJson(&HSTRING::from(state)) };
                }
            }
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ACTIVATE if (wparam & 0xffff) as u32 == WA_INACTIVE => {
            hide();
            0
        }
        WM_CLOSE => {
            hide();
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
