//! The macOS menu bar app: a status item coloured by state, the same panel
//! as on Windows (popup.html, here in a WKWebView) on a click and a menu on a
//! right-click. AppKit runs on the main thread, the engine on its own.
//!
//! launchd keeps it alive (macinstall.rs). Stays awake for the work: App Nap
//! is held off for as long as it runs, and while plugged in `caffeinate`
//! stops idle sleep during work (the screen still sleeps, closing a laptop's
//! lid still sleeps).

use crate::config::{Config, Mode};
use crate::engine::{Engine, Phase, Shared};
use crate::macinstall as install;
use crate::paths::Paths;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSApplicationPresentationOptions,
    NSBackingStoreType, NSControlStateValueOff, NSControlStateValueOn, NSEventMask, NSEventType, NSImage, NSMenu,
    NSMenuItem, NSPanel, NSScreen, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWindowCollectionBehavior,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    ns_string, NSActivityOptions, NSData, NSNotification, NSPoint, NSProcessInfo, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKUserScript, WKUserScriptInjectionTime,
    WKWebView, WKWebViewConfiguration,
};
use std::cell::RefCell;
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const HTML: &str = include_str!("popup.html");
const LOGO: &str = include_str!("../../../assets/icon.svg");
const WIDTH: f64 = 620.0;
const HEIGHT: f64 = 404.0;

/// The page talks to window.chrome.webview, as in WebView2.
const BRIDGE: &str = r#"(function () {
  var heard = [];
  window.chrome = window.chrome || {};
  window.chrome.webview = {
    postMessage: function (s) { window.webkit.messageHandlers.mk.postMessage(s); },
    addEventListener: function (type, f) { if (type === "message") heard.push(f); }
  };
  window.__mkPush = function (d) { heard.forEach(function (f) { f({ data: d }); }); };
})();"#;

const ID_RUNNING: isize = 10;
const ID_MODE_AUTO: isize = 20;
const ID_MODE_LIGHT: isize = 21;
const ID_MODE_BALANCED: isize = 22;
const ID_MODE_MAX: isize = 23;
const ID_PAUSE_FULLSCREEN: isize = 30;
const ID_PAUSE_BATTERY: isize = 31;
const ID_STARTUP: isize = 32;
const ID_KEEP_AWAKE: isize = 33;
const ID_LOGS: isize = 41;
const ID_SITE: isize = 43;
const ID_PANEL: isize = 44;
const ID_QUIT: isize = 50;
const ID_UNINSTALL: isize = 51;

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

fn shared() -> &'static Arc<Shared> {
    SHARED.get().expect("engine state")
}

#[derive(Clone, Copy, PartialEq)]
enum Look {
    Green,
    Amber,
    Grey,
    Red,
}

fn look() -> Look {
    match &shared().st.lock().unwrap().phase {
        Phase::Running => Look::Green,
        Phase::Off => Look::Grey,
        Phase::TokenRejected(_) | Phase::Offline(_) => Look::Red,
        _ => Look::Amber,
    }
}

fn icon(look: Look) -> Option<Retained<NSImage>> {
    let png: &[u8] = match look {
        Look::Green => include_bytes!("../../../assets/tray-running-256.png"),
        Look::Amber => include_bytes!("../../../assets/tray-paused-256.png"),
        Look::Grey => include_bytes!("../../../assets/tray-off-256.png"),
        Look::Red => include_bytes!("../../../assets/tray-error-256.png"),
    };
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(png))?;
    image.setSize(NSSize::new(18.0, 18.0));
    Some(image)
}

struct Ui {
    item: Retained<NSStatusItem>,
    panel: Retained<KeyPanel>,
    web: Retained<WKWebView>,
    _controller: Retained<Controller>,
    engine: JoinHandle<()>,
    look: Option<Look>,
    ready: bool,
    visible: bool,
    hidden_at: Option<Instant>,
    caffeinate: Option<std::process::Child>,
    ticks: u64,
    uninstall: Option<bool>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    // AppKit can call back into us while a borrow is held (showing the
    // window, a menu): never panic here.
    UI.with(|u| u.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

define_class!(
    /// Borderless windows can't take the keyboard unless they say so, and
    /// losing the keyboard is how the panel knows a click went elsewhere.
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "MKPanel"]
    struct KeyPanel;

    impl KeyPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MKController"]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSApplicationDelegate for Controller {
        /// Opened again from Finder or Spotlight while running.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            show();
            true
        }

        /// Logging out or shutting down.
        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _note: &NSNotification) {
            finish(Duration::from_secs(5));
        }
    }

    unsafe impl NSWindowDelegate for Controller {
        #[unsafe(method(windowDidResignKey:))]
        fn resign_key(&self, _note: &NSNotification) {
            hide();
        }
    }

    unsafe impl WKScriptMessageHandler for Controller {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn message(&self, _c: &WKUserContentController, message: &WKScriptMessage) {
            let body = unsafe { message.body() };
            let Ok(text) = body.downcast::<NSString>() else { return };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text.to_string()) {
                on_page(v["cmd"].as_str().unwrap_or(""), v["value"].as_str().unwrap_or(""));
            }
        }
    }

    impl Controller {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            tick();
        }

        #[unsafe(method(statusClicked:))]
        fn status_clicked(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();
            let right = NSApplication::sharedApplication(mtm).currentEvent().is_some_and(|e| {
                e.r#type() == NSEventType::RightMouseUp || e.modifierFlags().contains(objc2_app_kit::NSEventModifierFlags::Control)
            });
            if right {
                hide();
                show_menu(self, mtm);
            } else {
                toggle();
            }
        }

        #[unsafe(method(menuItem:))]
        fn menu_item(&self, item: &NSMenuItem) {
            command(item.tag());
        }
    }
);

impl Controller {
    fn create(mtm: MainThreadMarker) -> Retained<Controller> {
        unsafe { msg_send![Controller::alloc(mtm), init] }
    }
}

// ------------------------------------------------------------------ panel
fn on_page(cmd: &str, value: &str) {
    match cmd {
        "ready" => {
            with_ui(|ui| ui.ready = true);
            push();
        }
        "hide" => hide(),
        "painted" => {}
        _ => popup_command(cmd, value),
    }
}

fn state() -> String {
    shared().popup_json(install::start_at_login()).to_string()
}

fn push() {
    let js = NSString::from_str(&format!("window.__mkPush({})", state()));
    with_ui(|ui| {
        if ui.ready {
            unsafe { ui.web.evaluateJavaScript_completionHandler(&js, None) };
        }
    });
}

fn toggle() {
    let state = with_ui(|ui| (ui.visible, ui.hidden_at));
    let Some((visible, hidden_at)) = state else { return };
    if visible {
        hide();
    // A click on the icon first takes the keyboard from the panel, which
    // hides it: that click means "close", not "open again".
    } else if !hidden_at.is_some_and(|t| t.elapsed() < Duration::from_millis(350)) {
        show();
    }
}

fn place(ui: &Ui, mtm: MainThreadMarker) -> NSPoint {
    let icon = ui.item.button(mtm).and_then(|b| b.window());
    let frame = icon.as_ref().map(|w| w.frame());
    let screen = icon.as_ref().and_then(|w| w.screen()).or_else(|| NSScreen::mainScreen(mtm));
    let work = screen.map(|s| s.visibleFrame()).unwrap_or(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)));
    let gap = 6.0;
    let (mut x, mut y) = match frame {
        Some(f) => (f.origin.x + f.size.width / 2.0 - WIDTH / 2.0, f.origin.y - HEIGHT - gap),
        None => (work.origin.x + work.size.width - WIDTH - gap, work.origin.y + work.size.height - HEIGHT - gap),
    };
    x = x.min(work.origin.x + work.size.width - WIDTH - gap).max(work.origin.x + gap);
    y = y.min(work.origin.y + work.size.height - HEIGHT - gap).max(work.origin.y + gap);
    NSPoint::new(x, y)
}

fn show() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    with_ui(|ui| {
        ui.panel.setFrameOrigin(place(ui, mtm));
        ui.visible = true;
        ui.hidden_at = None;
    });
    let panel = with_ui(|ui| ui.panel.clone());
    if let Some(panel) = panel {
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        panel.makeKeyAndOrderFront(None);
    }
    push();
}

fn hide() {
    let panel = with_ui(|ui| {
        let was = ui.visible;
        ui.visible = false;
        if was {
            ui.hidden_at = Some(Instant::now());
        }
        was.then(|| ui.panel.clone())
    });
    if let Some(Some(panel)) = panel {
        panel.orderOut(None);
    }
}

fn make_panel(controller: &Controller, mtm: MainThreadMarker) -> (Retained<KeyPanel>, Retained<WKWebView>) {
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT));
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let panel: Retained<KeyPanel> = unsafe {
        msg_send![KeyPanel::alloc(mtm), initWithContentRect: rect, styleMask: style,
                  backing: NSBackingStoreType::Buffered, defer: false]
    };
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setLevel(25); // NSStatusWindowLevel: over everything, like the menu bar
    panel.setHasShadow(true);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&objc2_app_kit::NSColor::clearColor()));
    panel.setCollectionBehavior(NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary);
    panel.setDelegate(Some(ProtocolObject::from_ref(controller)));

    let web = unsafe {
        let config = WKWebViewConfiguration::new(mtm);
        let scripts = config.userContentController();
        scripts.addScriptMessageHandler_name(ProtocolObject::from_ref(controller), ns_string!("mk"));
        let bridge = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
            WKUserScript::alloc(mtm),
            &NSString::from_str(BRIDGE),
            WKUserScriptInjectionTime::AtDocumentStart,
            true,
        );
        scripts.addUserScript(&bridge);
        WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), rect, &config)
    };
    if let Some(content) = panel.contentView() {
        // Rounded corners, as on Windows 11 and every macOS panel.
        content.setWantsLayer(true);
        unsafe {
            let layer: *mut AnyObject = msg_send![&*content, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: 10.0f64];
                let _: () = msg_send![layer, setMasksToBounds: true];
            }
        }
        content.addSubview(&web);
    }
    let html = HTML.replace("<!--logo-->", LOGO).replace("Start with Windows", "Start at login")
        .replace("this PC", "this Mac").replace("use the PC", "use the Mac");
    let _ = unsafe { web.loadHTMLString_baseURL(&NSString::from_str(&html), None) };
    (panel, web)
}

// ------------------------------------------------------------------ menu
fn add(menu: &NSMenu, target: &Controller, mtm: MainThreadMarker, id: isize, text: &str, checked: bool, enabled: bool) {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(text),
            enabled.then_some(sel!(menuItem:)),
            ns_string!(""),
        )
    };
    item.setTag(id);
    item.setEnabled(enabled);
    item.setState(if checked { NSControlStateValueOn } else { NSControlStateValueOff });
    if enabled {
        unsafe { item.setTarget(Some(target)) };
    }
    menu.addItem(&item);
}

fn separator(menu: &NSMenu, mtm: MainThreadMarker) {
    menu.addItem(&NSMenuItem::separatorItem(mtm));
}

fn show_menu(target: &Controller, mtm: MainThreadMarker) {
    let sh = shared();
    let cfg: Config = sh.cfg.lock().unwrap().clone();
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let line = |id, text: &str, on, enabled| add(&menu, target, mtm, id, text, on, enabled);
    line(ID_PANEL, "Open panel", false, true);
    separator(&menu, mtm);
    line(1, &sh.status_text(), false, false);
    line(2, &sh.today_text(), false, false);
    line(3, &sh.now_text(), false, false);
    separator(&menu, mtm);
    line(ID_RUNNING, "Running", cfg.running, true);
    separator(&menu, mtm);
    line(ID_MODE_AUTO, "Auto: full power while you're away", cfg.mode == Mode::Auto, true);
    line(ID_MODE_LIGHT, "Light: one compare, stays out of the way", cfg.mode == Mode::Light, true);
    line(ID_MODE_BALANCED, "Balanced: half the cores", cfg.mode == Mode::Balanced, true);
    line(ID_MODE_MAX, "Max: no holding back", cfg.mode == Mode::Max, true);
    separator(&menu, mtm);
    line(ID_PAUSE_FULLSCREEN, "Pause while a game or fullscreen app runs", cfg.pause_fullscreen, true);
    line(ID_PAUSE_BATTERY, "Pause on battery", cfg.pause_battery, true);
    line(ID_KEEP_AWAKE, "Keep the Mac awake while plugged in", cfg.keep_awake, true);
    line(ID_STARTUP, "Start at login", install::start_at_login(), true);
    separator(&menu, mtm);
    line(ID_SITE, "Open the site", false, true);
    line(ID_LOGS, "Open log folder", false, true);
    separator(&menu, mtm);
    line(ID_UNINSTALL, "Uninstall", false, true);
    line(ID_QUIT, "Quit", false, true);

    // The way to get a menu on one click only: attach it for this click.
    let item = with_ui(|ui| ui.item.clone());
    if let Some(item) = item {
        unsafe {
            item.setMenu(Some(&menu));
            if let Some(button) = item.button(mtm) {
                button.performClick(None);
            }
            item.setMenu(None);
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

fn command(id: isize) {
    let sh = shared();
    match id {
        ID_RUNNING => sh.update_config(|c| c.running = !c.running),
        ID_MODE_AUTO => set_mode(Mode::Auto),
        ID_MODE_LIGHT => set_mode(Mode::Light),
        ID_MODE_BALANCED => set_mode(Mode::Balanced),
        ID_MODE_MAX => set_mode(Mode::Max),
        ID_PAUSE_FULLSCREEN => sh.update_config(|c| c.pause_fullscreen = !c.pause_fullscreen),
        ID_PAUSE_BATTERY => sh.update_config(|c| c.pause_battery = !c.pause_battery),
        ID_KEEP_AWAKE => sh.update_config(|c| c.keep_awake = !c.keep_awake),
        ID_STARTUP => install::set_start_at_login(!install::start_at_login()),
        ID_LOGS => {
            let _ = std::process::Command::new("/usr/bin/open").arg(sh.paths.log_dir()).spawn();
        }
        ID_SITE => crate::platform::open_url(&sh.cfg.lock().unwrap().server.clone()),
        ID_PANEL => show(),
        ID_QUIT => finish(Duration::from_secs(25)),
        ID_UNINSTALL => uninstall(),
        _ => {}
    }
    update_icon();
    push();
}

/// A click in the panel.
fn popup_command(cmd: &str, value: &str) {
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
        push();
        return;
    }
    if id == ID_LOGS || id == ID_SITE || id == ID_QUIT {
        hide();
    }
    command(id);
}

fn uninstall() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    hide();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(ns_string!("Uninstall ManjiKick Worker?"));
    alert.setInformativeText(ns_string!("Remove the settings and token too, or keep them for later?"));
    alert.addButtonWithTitle(ns_string!("Keep Settings"));
    alert.addButtonWithTitle(ns_string!("Remove Everything"));
    alert.addButtonWithTitle(ns_string!("Cancel"));
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    let everything = match alert.runModal() {
        1000 => false,
        1001 => true,
        _ => return,
    };
    crate::log!("uninstalling{}", if everything { " with settings and token" } else { "" });
    with_ui(|ui| ui.uninstall = Some(everything));
    finish(Duration::from_secs(25));
}

// ------------------------------------------------------------------ running
fn update_icon() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let lk = look();
    let tip = NSString::from_str(&format!("ManjiKick worker\n{}", shared().status_text()));
    with_ui(|ui| {
        let Some(button) = ui.item.button(mtm) else { return };
        button.setToolTip(Some(&tip));
        if ui.look != Some(lk) {
            ui.look = Some(lk);
            button.setImage(icon(lk).as_deref());
        }
    });
}

fn sense(mtm: MainThreadMarker) {
    let options = NSApplication::sharedApplication(mtm).currentSystemPresentationOptions();
    let full = options.intersects(NSApplicationPresentationOptions::FullScreen | NSApplicationPresentationOptions::HideMenuBar);
    crate::platform::FULLSCREEN.store(full, std::sync::atomic::Ordering::Relaxed);

    let sh = shared();
    let keep = sh.cfg.lock().unwrap().keep_awake;
    let working = matches!(sh.st.lock().unwrap().phase, Phase::Running);
    let want = keep && working && !crate::platform::on_battery();
    with_ui(|ui| {
        // Gone by itself (or killed): start it again if still wanted.
        if ui.caffeinate.as_mut().is_some_and(|c| !matches!(c.try_wait(), Ok(None))) {
            ui.caffeinate = None;
        }
        if want && ui.caffeinate.is_none() {
            // -w: it ends with us, however we end.
            let mut cmd = std::process::Command::new("/usr/bin/caffeinate");
            cmd.args(["-i", "-w", &std::process::id().to_string()]);
            ui.caffeinate = crate::proc::quiet(&mut cmd).spawn().ok();
        } else if !want {
            if let Some(mut c) = ui.caffeinate.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    });
}

fn tick() {
    let sh = shared();
    // A file named crash-test in the data folder makes the app panic once:
    // checks the crash log and that launchd starts it again.
    let trigger = sh.paths.state_dir.join("crash-test");
    if trigger.exists() {
        let _ = std::fs::remove_file(&trigger);
        panic!("crash test requested");
    }
    // A second launch asks for the panel.
    let open = sh.paths.state_dir.join("open-panel");
    if open.exists() {
        let _ = std::fs::remove_file(&open);
        show();
    }
    update_icon();
    let (visible, ticks) = with_ui(|ui| {
        ui.ticks += 1;
        (ui.visible, ui.ticks)
    })
    .unwrap_or((false, 0));
    if visible {
        push();
    }
    if ticks % 5 == 1 {
        if let Some(mtm) = MainThreadMarker::new() {
            sense(mtm);
        }
    }
    let restart = sh.st.lock().unwrap().restart;
    if sh.quitting() || restart {
        finish(Duration::from_secs(25));
    }
}

/// Stops the engine and ends the process: for Quit, Uninstall, an update
/// (which restarts into the new program) or the system's own stop.
fn finish(limit: Duration) -> ! {
    crate::log!("quitting");
    let sh = shared();
    sh.quit.store(true, std::sync::atomic::Ordering::Relaxed);
    let deadline = Instant::now() + limit;
    while with_ui(|ui| ui.engine.is_finished()) == Some(false) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let uninstall = with_ui(|ui| {
        if let Some(mut c) = ui.caffeinate.take() {
            let _ = c.kill();
        }
        ui.uninstall
    })
    .flatten();
    let _ = std::fs::remove_file(sh.paths.pid_file());
    if let Some(everything) = uninstall {
        install::uninstall(everything);
        std::process::exit(0);
    }
    if sh.st.lock().unwrap().restart {
        // A new process, not exec: an exec'd app keeps running but the window
        // server has lost it, so the menu bar icon never comes back.
        if std::env::var_os(install::AGENT_ENV).is_some() {
            std::process::exit(75); // not a clean stop: launchd starts the new program
        }
        install::open_later();
    }
    std::process::exit(0);
}

pub fn main() {
    let paths = Paths::get();
    paths.create_all();
    crate::logging::init(&paths.log_file, false);
    let open_panel = paths.state_dir.join("open-panel");
    let testing = std::env::var_os("MKW_HOME").is_some();
    if std::env::var_os(install::AGENT_ENV).is_none() && !testing && install::hand_to_launchd() {
        let _ = std::fs::write(&open_panel, "");
        return;
    }
    let Some(lock) = crate::platform::lock_pid_file(&paths.pid_file()) else {
        let _ = std::fs::write(&open_panel, "");
        return;
    };
    {
        use std::io::{Seek, Write};
        let mut f = &lock;
        let _ = f.set_len(0);
        let _ = f.seek(std::io::SeekFrom::Start(0));
        let _ = writeln!(f, "{}", std::process::id());
    }
    let _ = std::fs::remove_file(&open_panel);
    let _ = std::fs::remove_file(paths.stop_file());
    if !testing {
        install::refresh();
    }
    crate::platform::install_stop_handler();
    let cfg_path = paths.config_file();
    let cfg = Config::load(&cfg_path);
    if !cfg_path.exists() {
        let _ = cfg.save(&cfg_path);
    }
    crate::log!("mkworker {} starting as the menu bar app", crate::VERSION);
    let dry_run = std::env::var_os("MKW_DRY_RUN").is_some_and(|v| v == "1");
    let sh = Shared::new(paths, cfg, None, dry_run, true);
    let _ = SHARED.set(sh.clone());

    let Some(mtm) = MainThreadMarker::new() else {
        crate::log!("not on the main thread");
        return;
    };
    // The app has no window most of the time: without this macOS naps it.
    let activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        ns_string!("Comparing audio"),
    );
    std::mem::forget(activity);
    let engine = {
        let sh = sh.clone();
        std::thread::spawn(move || Engine::new(sh).run(None))
    };

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let controller = Controller::create(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        unsafe {
            button.setTarget(Some(&controller));
            button.setAction(Some(sel!(statusClicked:)));
        }
        button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
    }
    let (panel, web) = make_panel(&controller, mtm);
    if let Some(content) = panel.contentView() {
        web.setFrame(content.bounds());
    }
    UI.with(|u| {
        *u.borrow_mut() = Some(Ui {
            item, panel, web, _controller: controller.clone(), engine, look: None, ready: false, visible: false,
            hidden_at: None, caffeinate: None, ticks: 0, uninstall: None,
        })
    });
    update_icon();
    unsafe {
        // Common modes: it also ticks while the menu is open.
        let timer = NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
            1.0, &controller, sel!(tick:), None, true,
        );
        NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
    }
    std::mem::forget(lock); // held until the process ends
    app.run();
}
