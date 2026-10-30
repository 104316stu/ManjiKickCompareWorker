//! The Windows tray app: a notification-area icon coloured by state, a
//! popup panel on left-click (popup.rs) and a right-click menu. Plain Win32;
//! the engine runs on its own thread.

use crate::config::{Config, Mode};
use crate::engine::{Engine, Phase, Shared};
use crate::paths::Paths;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
/// Sent by a second launch: open the panel of the one already running.
const WM_OPEN_PANEL: u32 = WM_APP + 2;
/// `--hide-panel` from a second launch (scripts and timing checks).
const WM_HIDE_PANEL: u32 = WM_APP + 3;
/// The tray window's class (found by a second launch); per instance home.
fn class_name() -> String {
    format!("ManjiKickWorkerTray{}", crate::platform::instance_suffix())
}
const TIMER_ID: usize = 1;

const ID_RUNNING: u32 = 10;
const ID_MODE_AUTO: u32 = 20;
const ID_MODE_LIGHT: u32 = 21;
const ID_MODE_BALANCED: u32 = 22;
const ID_MODE_MAX: u32 = 23;
const ID_PAUSE_FULLSCREEN: u32 = 30;
const ID_PAUSE_BATTERY: u32 = 31;
const ID_STARTUP: u32 = 32;
const ID_TOKEN: u32 = 40;
const ID_LOGS: u32 = 41;
const ID_SITE: u32 = 43;
const ID_PANEL: u32 = 44;
const ID_QUIT: u32 = 50;
const ID_UNINSTALL: u32 = 51;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "ManjiKickWorker";

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn copy_into(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

fn shared() -> &'static Arc<Shared> {
    SHARED.get().expect("engine state")
}

// ------------------------------------------------------------------ icons
#[derive(Clone, Copy, PartialEq)]
enum Look {
    Green,
    Amber,
    Grey,
    Red,
    Blue,
}

fn look() -> Look {
    match &shared().st.lock().unwrap().phase {
        Phase::Running => Look::Green,
        Phase::Paused(_) | Phase::Cooldown => Look::Amber,
        Phase::Off => Look::Grey,
        Phase::NoToken => Look::Amber,
        Phase::TokenRejected(_) | Phase::Offline(_) => Look::Red,
        Phase::Starting | Phase::Setup(_) | Phase::Updating(_) => Look::Blue,
    }
}

/// The state icons from assets/ (16 and 32 px PNGs, picked by the system's
/// small-icon size). Windows reads PNG icon images directly.
fn make_icon(look: Look) -> HICON {
    let (small, large): (&[u8], &[u8]) = match look {
        Look::Green => (include_bytes!("../../../assets/tray-running-16.png"), include_bytes!("../../../assets/tray-running-32.png")),
        Look::Amber | Look::Blue => (include_bytes!("../../../assets/tray-paused-16.png"), include_bytes!("../../../assets/tray-paused-32.png")),
        Look::Grey => (include_bytes!("../../../assets/tray-off-16.png"), include_bytes!("../../../assets/tray-off-32.png")),
        Look::Red => (include_bytes!("../../../assets/tray-error-16.png"), include_bytes!("../../../assets/tray-error-32.png")),
    };
    unsafe {
        let want = GetSystemMetrics(SM_CXSMICON);
        let png = if want > 16 { large } else { small };
        CreateIconFromResourceEx(png.as_ptr(), png.len() as u32, 1, 0x0003_0000, want, want, LR_DEFAULTCOLOR)
    }
}

struct Icons {
    current: Option<Look>,
    handle: HICON,
}

static ICONS: std::sync::Mutex<Icons> = std::sync::Mutex::new(Icons { current: None, handle: 0 as HICON });
unsafe impl Send for Icons {}

fn notify(hwnd: HWND, action: u32, balloon: Option<(&str, &str)>) {
    let lk = look();
    let mut icons = ICONS.lock().unwrap();
    if icons.current != Some(lk) {
        let old = icons.handle;
        icons.handle = make_icon(lk);
        icons.current = Some(lk);
        if !old.is_null() {
            unsafe { DestroyIcon(old) };
        }
    }
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = icons.handle;
        copy_into(&mut nid.szTip, &format!("ManjiKick worker\n{}", shared().status_text()));
        if let Some((title, text)) = balloon {
            nid.uFlags |= NIF_INFO;
            copy_into(&mut nid.szInfoTitle, title);
            copy_into(&mut nid.szInfo, text);
            nid.dwInfoFlags = NIIF_INFO;
        }
        Shell_NotifyIconW(action, &nid);
    }
}

fn remove_icon(hwnd: HWND) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

// ------------------------------------------------------------------ startup key
/// What "Start with Windows" runs: always the installed app.
fn exe_command() -> String {
    let exe = if crate::install::is_installed_copy() {
        std::env::current_exe().unwrap_or_else(|_| crate::install::installed_exe())
    } else {
        crate::install::installed_exe()
    };
    format!("\"{}\"", exe.display())
}

fn run_key(access: u32) -> Option<HKEY> {
    let mut key: HKEY = std::ptr::null_mut();
    let ok = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, access, &mut key) };
    (ok == 0).then_some(key)
}

fn startup_value() -> Option<String> {
    let key = run_key(KEY_QUERY_VALUE)?;
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let mut kind = 0u32;
    let ok = unsafe {
        RegQueryValueExW(key, wide(RUN_VALUE).as_ptr(), std::ptr::null(), &mut kind, buf.as_mut_ptr() as *mut u8, &mut size)
    };
    unsafe { RegCloseKey(key) };
    if ok != 0 {
        return None;
    }
    let len = (size as usize / 2).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]).trim_end_matches('\0').to_string())
}

fn set_startup(on: bool) {
    let Some(key) = run_key(KEY_SET_VALUE) else { return };
    unsafe {
        if on {
            let data = wide(&exe_command());
            RegSetValueExW(key, wide(RUN_VALUE).as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32);
        } else {
            RegDeleteValueW(key, wide(RUN_VALUE).as_ptr());
        }
        RegCloseKey(key);
    }
}

// ------------------------------------------------------------------ clipboard
fn clipboard_text(hwnd: HWND) -> Option<String> {
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return None;
        }
        let mut out = None;
        let h = GetClipboardData(13); // CF_UNICODETEXT
        if !h.is_null() {
            let p = GlobalLock(h as _) as *const u16;
            if !p.is_null() {
                let mut n = 0;
                while *p.add(n) != 0 && n < 4096 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                GlobalUnlock(h as _);
            }
        }
        CloseClipboard();
        out
    }
}

fn set_token_from_clipboard(hwnd: HWND) {
    let text = clipboard_text(hwnd).unwrap_or_default();
    let token = text.trim();
    if !token.starts_with("mkw_") || token.len() < 12 || token.contains(char::is_whitespace) {
        notify(hwnd, NIM_MODIFY, Some(("No token on the clipboard", "Copy the mkw_... token from the site first.")));
        return;
    }
    let sh = shared();
    match sh.paths.write_token(token) {
        Ok(()) => {
            crate::log!("token set from the clipboard");
            sh.token_changed();
            notify(hwnd, NIM_MODIFY, Some(("Token saved", "Connecting to the site...")));
        }
        Err(e) => notify(hwnd, NIM_MODIFY, Some(("Could not save the token", &e.to_string()))),
    }
}

// ------------------------------------------------------------------ menu
fn add(menu: HMENU, id: u32, text: &str, checked: bool, enabled: bool) {
    let mut flags = MF_STRING;
    if checked {
        flags |= MF_CHECKED;
    }
    if !enabled {
        flags |= MF_GRAYED;
    }
    unsafe { AppendMenuW(menu, flags, id as usize, wide(text).as_ptr()) };
}

fn separator(menu: HMENU) {
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null()) };
}

fn show_menu(hwnd: HWND) {
    let sh = shared();
    let cfg: Config = sh.cfg.lock().unwrap().clone();
    unsafe {
        let menu = CreatePopupMenu();
        add(menu, ID_PANEL, "Open panel", false, true);
        separator(menu);
        add(menu, 1, &sh.status_text(), false, false);
        add(menu, 2, &sh.today_text(), false, false);
        add(menu, 3, &sh.now_text(), false, false);
        separator(menu);
        add(menu, ID_RUNNING, "Running", cfg.running, true);
        separator(menu);
        add(menu, ID_MODE_AUTO, "Auto: full power while you're away", false, true);
        add(menu, ID_MODE_LIGHT, "Light: one compare, stays out of the way", false, true);
        add(menu, ID_MODE_BALANCED, "Balanced: half the cores", false, true);
        add(menu, ID_MODE_MAX, "Max: no holding back", false, true);
        let checked = match cfg.mode {
            Mode::Auto => ID_MODE_AUTO,
            Mode::Light => ID_MODE_LIGHT,
            Mode::Balanced => ID_MODE_BALANCED,
            Mode::Max => ID_MODE_MAX,
        };
        CheckMenuRadioItem(menu, ID_MODE_AUTO, ID_MODE_MAX, checked, MF_BYCOMMAND);
        separator(menu);
        add(menu, ID_PAUSE_FULLSCREEN, "Pause while a game or fullscreen app runs", cfg.pause_fullscreen, true);
        add(menu, ID_PAUSE_BATTERY, "Pause on battery", cfg.pause_battery, true);
        add(menu, ID_STARTUP, "Start with Windows", startup_value().is_some(), true);
        separator(menu);
        add(menu, ID_TOKEN, "Set token from clipboard", false, true);
        add(menu, ID_SITE, "Open the site", false, true);
        add(menu, ID_LOGS, "Open log folder", false, true);
        separator(menu);
        add(menu, ID_UNINSTALL, "Uninstall", false, true);
        add(menu, ID_QUIT, "Quit", false, true);

        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        if cmd > 0 {
            command(hwnd, cmd as u32);
        }
    }
}

fn set_mode(mode: Mode) {
    let (pf, pb) = mode.default_pauses();
    shared().update_config(|c| {
        c.mode = mode;
        c.pause_fullscreen = pf;
        c.pause_battery = pb;
    });
    crate::log!("mode set to {}", mode.label());
}

fn command(hwnd: HWND, id: u32) {
    let sh = shared();
    match id {
        ID_RUNNING => sh.update_config(|c| c.running = !c.running),
        ID_MODE_AUTO => set_mode(Mode::Auto),
        ID_MODE_LIGHT => set_mode(Mode::Light),
        ID_MODE_BALANCED => set_mode(Mode::Balanced),
        ID_MODE_MAX => set_mode(Mode::Max),
        ID_PAUSE_FULLSCREEN => sh.update_config(|c| c.pause_fullscreen = !c.pause_fullscreen),
        ID_PAUSE_BATTERY => sh.update_config(|c| c.pause_battery = !c.pause_battery),
        ID_STARTUP => set_startup(startup_value().is_none()),
        ID_TOKEN => set_token_from_clipboard(hwnd),
        ID_LOGS => {
            let _ = crate::proc::spawn_outside(std::process::Command::new("explorer.exe").arg(sh.paths.log_dir()), 0);
        }
        ID_SITE => open_url(&sh.cfg.lock().unwrap().server.clone()),
        ID_PANEL => {
            if !crate::popup::is_visible() && !crate::popup::toggle() {
                notify(hwnd, NIM_MODIFY, Some(("Panel unavailable", "It could not be opened; the log folder has the reason.")));
                return;
            }
        }

        ID_QUIT => unsafe { PostQuitMessage(0) },
        ID_UNINSTALL => uninstall(hwnd),
        _ => {}
    }
    notify(hwnd, NIM_MODIFY, None);
    crate::popup::refresh(popup_state());
}

/// Opens a web page in the default browser.
pub fn open_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    // Not ShellExecute: a browser started from here must outlive the worker.
    crate::platform::open_url(url);
}

/// Right-click > Uninstall: removes the shortcut and the Run entry, quits,
/// and deletes the app (and, if asked, the settings and token too).
fn uninstall(hwnd: HWND) {
    let answer = unsafe {
        MessageBoxW(hwnd,
            wide("Uninstall ManjiKick Worker.\n\nAlso remove settings and token?").as_ptr(),
            wide("Uninstall ManjiKick Worker").as_ptr(),
            MB_YESNOCANCEL | MB_ICONQUESTION | MB_DEFBUTTON2)
    };
    let everything = match answer {
        IDYES => true,
        IDNO => false,
        _ => return,
    };
    crate::log!("uninstalling{}", if everything { " with settings and token" } else { "" });
    set_startup(false);
    UNINSTALL.with(|u| u.set(Some(everything)));
    crate::popup::hide();
    unsafe { PostQuitMessage(0) };
}

fn popup_state() -> String {
    shared().popup_json(startup_value().is_some()).to_string()
}

/// A click in the popup.
fn popup_command(cmd: &str, value: &str) {
    let hwnd = TRAY_HWND.with(|h| h.get());
    let id = match (cmd, value) {
        ("mode", "auto") => ID_MODE_AUTO,
        ("mode", "light") => ID_MODE_LIGHT,
        ("mode", "balanced") => ID_MODE_BALANCED,
        ("mode", "max") => ID_MODE_MAX,
        ("toggle", "running") => ID_RUNNING,
        ("toggle", "pause_fullscreen") => ID_PAUSE_FULLSCREEN,
        ("toggle", "pause_battery") => ID_PAUSE_BATTERY,
        ("toggle", "startup") => ID_STARTUP,
        ("open", "logs") => ID_LOGS,
        ("open", "site") => ID_SITE,
        ("quit", _) => ID_QUIT,
        ("update", _) => {
            shared().st.lock().unwrap().update_check_now = true;
            0
        }
        _ => 0,
    };
    if id == 0 {
        crate::popup::refresh(popup_state());
        return;
    }
    if id == ID_LOGS || id == ID_SITE || id == ID_QUIT {
        crate::popup::hide();
    }
    command(hwnd, id);
}

thread_local! {
    static UNINSTALL: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static TRAY_HWND: std::cell::Cell<HWND> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let event = (lparam & 0xffff) as u32;
            if event == WM_LBUTTONUP {
                if !crate::popup::toggle() {
                    show_menu(hwnd);
                }
            } else if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                crate::popup::hide();
                show_menu(hwnd);
            }
            0
        }
        WM_HIDE_PANEL => {
            crate::popup::hide();
            0
        }
        WM_OPEN_PANEL => {
            if !crate::popup::is_visible() && !crate::popup::toggle() {
                show_menu(hwnd);
            }
            0
        }
        WM_TIMER => {
            // A file named crash-test in the data folder makes the worker
            // panic once: checks the crash log and the automatic restart.
            let trigger = shared().paths.state_dir.join("crash-test");
            if trigger.exists() {
                let _ = std::fs::remove_file(&trigger);
                panic!("crash test requested");
            }
            notify(hwnd, NIM_MODIFY, None);
            crate::popup::tick(popup_state);
            if shared().quitting() {
                PostQuitMessage(0);
            }
            0
        }
        _ if Some(&msg) == TASKBAR_CREATED.get() => {
            // Explorer restarted: put the icon back.
            notify(hwnd, NIM_ADD, None);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn main() {
    // `--version`: the self-update tries a new build this way before using it.
    if std::env::args().any(|a| a == "--version" || a == "version") {
        println!("mkworker-tray {}", crate::VERSION);
        return;
    }
    crate::update::set_asset("mkworker-tray.exe");
    unsafe {
        // WebView2 (the popup) is COM and needs a single-threaded apartment
        // on this, the UI thread.
        windows_sys::Win32::System::Com::CoInitializeEx(std::ptr::null(), 2); // COINIT_APARTMENTTHREADED
        // Sharp text and a correctly sized popup on scaled displays.
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(-4isize as _);
    }
    let paths = Paths::get();
    paths.create_all();
    crate::logging::init(&paths.log_file, false);
    crate::logging::install_panic_hook();
    if !crate::install::is_installed_copy() {
        run_downloaded_copy();
        return;
    }
    // The program started from the shortcut or at login supervises a child
    // copy that does the work (and owns the tray icon), and restarts it if
    // it ever dies.
    if std::env::var_os("MKW_CHILD").is_none() && std::env::var_os("MKW_NO_SUPERVISOR").is_none() {
        supervise();
        return;
    }
    let Some(_instance) = crate::platform::single_instance_after_update() else {
        // Already running: open its panel instead (like a second click on the icon).
        unsafe {
            let other = FindWindowW(wide(&class_name()).as_ptr(), std::ptr::null());
            if !other.is_null() {
                AllowSetForegroundWindow(u32::MAX); // ASFW_ANY
                let msg = if std::env::args().any(|a| a == "--hide-panel") { WM_HIDE_PANEL } else { WM_OPEN_PANEL };
                PostMessageW(other, msg, 0, 0);
            } else {
                MessageBoxW(std::ptr::null_mut(), wide("The ManjiKick worker is already running (see the tray).").as_ptr(),
                    wide("ManjiKick worker").as_ptr(), MB_OK | MB_ICONINFORMATION);
            }
        }
        return;
    };
    crate::platform::set_priority(true);
    crate::platform::init_family();
    let cfg_path = paths.config_file();
    let cfg = Config::load(&cfg_path);
    if !cfg_path.exists() {
        let _ = cfg.save(&cfg_path);
    }
    // The installed app always has its Start menu entry.
    if !crate::install::shortcut().exists() && std::env::var_os("MKW_NO_INSTALL").is_none()
        && std::env::var_os("MKW_HOME").is_none()
    {
        if let Ok(exe) = std::env::current_exe() {
            if let Err(e) = crate::install::create_shortcut(&exe) {
                crate::log!("Start menu shortcut not created: {e}");
            }
        }
    }
    // The exe moved since "Start with Windows" was set: follow it.
    if let Some(v) = startup_value() {
        if v != exe_command() {
            set_startup(true);
        }
    }
    crate::log!("mkworker-tray {} starting", crate::VERSION);
    let dry_run = std::env::var_os("MKW_DRY_RUN").is_some_and(|v| v == "1");
    let sh = Shared::new(paths, cfg, None, dry_run, true);
    let _ = SHARED.set(sh.clone());
    // Lets `mkworker status` / `mkworker stop` see the tray's worker too.
    let pid_file = sh.paths.pid_file();
    let _ = std::fs::write(&pid_file, format!("{}
", std::process::id()));
    let engine = {
        let sh = sh.clone();
        std::thread::spawn(move || Engine::new(sh).run(None))
    };

    unsafe {
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide(&class_name());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(0, class.as_ptr(), wide("ManjiKick worker").as_ptr(), 0, 0, 0, 0, 0,
            std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null());
        TRAY_HWND.with(|h| h.set(hwnd));
        notify(hwnd, NIM_ADD, None);
        crate::popup::init(hwnd, sh.paths.state_dir.join("webview"), popup_command, popup_state);
        // Build the panel now, hidden, so the first click shows it at once.
        if !crate::popup::prewarm() {
            crate::log!("panel not prepared; it is created on the first click instead");
        }
        if sh.paths.read_token().is_none() {
            notify(hwnd, NIM_MODIFY, Some(("ManjiKick worker", "Connecting this PC to the site...")));
        }
        SetTimer(hwnd, TIMER_ID, 1000, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        KillTimer(hwnd, TIMER_ID);
        crate::log!("quitting");
        sh.quit.store(true, std::sync::atomic::Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(25);
        while !engine.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        remove_icon(hwnd);
    }
    let _ = std::fs::remove_file(pid_file);
    if let Some(everything) = UNINSTALL.with(|u| u.get()) {
        crate::install::uninstall(everything);
        return;
    }
    if sh.st.lock().unwrap().restart {
        if let Ok(exe) = std::env::current_exe() {
            if let Err(e) = crate::update::relaunch(&exe) {
                crate::log!("{e}");
            }
        }
    }
}

/// A copy started from anywhere but the app folder (e.g. Downloads):
/// install or update the app if this one is newer, then hand over to it.
fn run_downloaded_copy() {
    let installed = crate::install::installed_exe();
    let mine = crate::update::current();
    let theirs = if installed.exists() { crate::install::version_of(&installed) } else { None };
    let newer = theirs.map_or(true, |v| mine > v);
    let running = unsafe { FindWindowW(wide(&class_name()).as_ptr(), std::ptr::null()) };
    if !newer {
        // Same or newer version installed: open it (its panel if it runs).
        unsafe {
            if !running.is_null() {
                AllowSetForegroundWindow(u32::MAX);
                PostMessageW(running, WM_OPEN_PANEL, 0, 0);
                return;
            }
        }
        crate::install::launch(&installed);
        return;
    }
    crate::log!("installing v{}.{}.{} to {}", mine.0, mine.1, mine.2, installed.display());
    crate::install::stop_running();
    match crate::install::install_self() {
        Ok(target) => {
            // Keep "Start with Windows" on, now pointing at the installed app.
            if startup_value().is_some() {
                set_startup(true);
            }
            if !crate::install::launch(&target) {
                crate::log!("installed, but it did not start");
            }
        }
        Err(e) => {
            crate::log!("install failed: {e}");
            unsafe {
                MessageBoxW(std::ptr::null_mut(), wide(&format!("ManjiKick Worker could not be installed:\n{e}")).as_ptr(),
                    wide("ManjiKick Worker").as_ptr(), MB_OK | MB_ICONWARNING);
            }
        }
    }
}

/// Exit code the child uses for a deliberate quit (menu, uninstall, update):
/// anything else is a crash and gets restarted.
pub fn supervisor_lock() -> String {
    format!("Local\\ManjiKickWorker.supervisor{}", crate::platform::instance_suffix())
}

fn open_running_panel() -> bool {
    unsafe {
        let other = FindWindowW(wide(&class_name()).as_ptr(), std::ptr::null());
        if other.is_null() {
            return false;
        }
        AllowSetForegroundWindow(u32::MAX); // ASFW_ANY
        let msg = if std::env::args().any(|a| a == "--hide-panel") { WM_HIDE_PANEL } else { WM_OPEN_PANEL };
        PostMessageW(other, msg, 0, 0);
        true
    }
}

/// Runs the worker as a child process and starts it again after a crash:
/// 5 s, doubling up to 5 min; after 5 crashes within 10 min it gives up and
/// says so. A normal exit (Quit, Uninstall, an update handing over) ends it.
fn supervise() {
    let updated = std::env::var_os("MKW_UPDATED").is_some();
    let mut lock = crate::platform::named_lock(&supervisor_lock());
    // Right after an update the previous supervisor may still be exiting.
    let mut tries = 0;
    while lock.is_none() && updated && tries < 60 {
        std::thread::sleep(Duration::from_millis(500));
        lock = crate::platform::named_lock(&supervisor_lock());
        tries += 1;
    }
    let Some(_lock) = lock else {
        // Already running: a second launch opens the panel.
        if !open_running_panel() {
            unsafe {
                MessageBoxW(std::ptr::null_mut(), wide("The ManjiKick worker is already running (see the tray).").as_ptr(),
                    wide("ManjiKick worker").as_ptr(), MB_OK | MB_ICONINFORMATION);
            }
        }
        return;
    };
    let Ok(exe) = std::env::current_exe() else { return };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut crashes: std::collections::VecDeque<Instant> = std::collections::VecDeque::new();
    let mut delay = 5u64;
    let mut first = true;
    loop {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&args).env("MKW_CHILD", "1");
        if !first {
            cmd.env_remove("MKW_UPDATED");
        }
        first = false;
        let started = Instant::now();
        let status = match crate::proc::spawn_outside(&mut cmd, 0).and_then(|mut c| c.wait()) {
            Ok(s) => s,
            Err(e) => {
                crate::log!("supervisor: could not start the worker: {e}");
                return;
            }
        };
        if status.success() {
            return; // Quit, Uninstall, or an update that started the new version
        }
        let code = status.code().map(|c| format!("0x{:08X}", c as u32)).unwrap_or_else(|| "?".into());
        // A child that ran a while resets the backoff.
        if started.elapsed() > Duration::from_secs(600) {
            delay = 5;
        }
        crashes.push_back(Instant::now());
        while crashes.front().is_some_and(|t| t.elapsed() > Duration::from_secs(600)) {
            crashes.pop_front();
        }
        if crashes.len() >= 5 {
            crate::log!("supervisor: the worker stopped unexpectedly 5 times in 10 minutes (last exit {code}); giving up");
            unsafe {
                MessageBoxW(std::ptr::null_mut(),
                    wide("ManjiKick Worker keeps stopping, so it was not restarted.\n\nThe log folder (%LOCALAPPDATA%\\ManjiKickWorker\\log) has the details.").as_ptr(),
                    wide("ManjiKick Worker").as_ptr(), MB_OK | MB_ICONWARNING);
            }
            return;
        }
        crate::log!("supervisor: the worker stopped unexpectedly (exit {code}); restarting in {delay} s");
        std::thread::sleep(Duration::from_secs(delay));
        delay = (delay * 2).min(300);
    }
}
