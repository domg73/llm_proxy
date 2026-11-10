// Tray icon for llm_proxy, written in raw Win32 for full control:
// hidden message window + Shell_NotifyIcon + TrackPopupMenu.
// Every message and every menu action is logged (tracing).
// Runs on a dedicated thread with its own Win32 message pump.
//
// Icon: two pre-generated .ico status dots (assets/, see make_dots.py):
//   RED   = assets/icon_red.ico   (backend not ready)
//   GREEN = assets/icon_green.ico (backend ready)
// In-memory icon creation (CreateIconFromResource/Ex, CreateIcon,
// CreateIconIndirect) returns NULL on this system, so the dots are
// embedded in the exe, materialized to %LOCALAPPDATA%\llm_proxy, loaded
// with LoadImageW and swapped at runtime with NIM_MODIFY.
// The tooltip carries runtime info (proxy/stats addresses, backend state)
// and is refreshed together with the dot.

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// Set once the tray window + TrayData exist. The remote dot-probe and
/// profile-poller (bridge mode) can win the startup race and observe the
/// backend before this thread has created the window — they gate their
/// posts on this flag instead of losing the first transition.
static TRAY_READY: AtomicBool = AtomicBool::new(false);

/// True once the tray window exists (see TRAY_READY).
pub fn is_ready() -> bool {
    TRAY_READY.load(Ordering::SeqCst)
}
use std::sync::OnceLock;

use anyhow::Result;
use windows::core::PCWSTR;
use windows::Win32::{
    Foundation::{HWND, LRESULT, WPARAM, LPARAM, POINT},
    Graphics::Gdi::{HBRUSH, GetStockObject, NULL_BRUSH},
    System::LibraryLoader::GetModuleHandleW,
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
        NOTIFYICONDATAW, Shell_NotifyIconW,
    },
    UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, AppendMenuW, ASFW_ANY, CreatePopupMenu,
        CreateWindowExW, CS_HREDRAW, CS_VREDRAW, DefWindowProcW, DestroyIcon,
        DestroyMenu, DestroyWindow, DispatchMessageW, GetMessageW, HCURSOR,
        HICON, HMENU, IMAGE_ICON, LoadImageW, LR_DEFAULTSIZE, LR_LOADFROMFILE,
        MF_STRING, PostMessageW, PostQuitMessage, RegisterClassW,
        SetForegroundWindow, GetForegroundWindow, GetWindowThreadProcessId,
        TPM_RETURNCMD, TPM_RIGHTBUTTON, TranslateMessage,
        WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
        WM_APP, WM_COMMAND, WM_DESTROY, WM_NULL, GetCursorPos,
    },
};

use tracing::{debug, info};

const TRAY_CLASS: &str = "llm_proxy_tray";
const TRAY_TITLE: &str = "llm_proxy";
const TRAY_ICON_ID: u32 = 1;
const WM_TASKBARCREATED: u32 = 0x02E2; // not in the windows crate bindings
const MENU_BASE_ID: usize = 100;
const WM_APP_TRAY: u32 = 0x8001; // registered callback message
const WM_APP_STATE: u32 = 0x8002; // internal: backend state changed
const WM_APP_PROFILE: u32 = 0x8003; // internal: active profile changed
// Mouse event codes delivered in the tray callback lParam (WM_APP_TRAY):
const TRAY_RBUTTONUP: u32 = 0x0205;
const TRAY_LBUTTONUP: u32 = 0x0202;
const TRAY_LBUTTONDBLCLK: u32 = 0x0203;

pub enum Action {
    LoadProfile(String),
    Stop,
    RemoteLoadProfile(String),
    RemoteStop,
    OpenStats,
    Restart,
    Quit,
}

#[derive(Clone)]
enum TrayAction {
    LoadProfile(String),
    Stop,
    RemoteLoadProfile(String),
    RemoteStop,
    OpenStats,
    Restart,
    Quit,
}

fn action_label(a: &TrayAction) -> String {
    match a {
        TrayAction::LoadProfile(n) => format!("LoadProfile({n})"),
        TrayAction::Stop => "Stop".into(),
        TrayAction::RemoteLoadProfile(n) => format!("RemoteLoadProfile({n})"),
        TrayAction::RemoteStop => "RemoteStop".into(),
        TrayAction::OpenStats => "OpenStats".into(),
        TrayAction::Restart => "Restart".into(),
        TrayAction::Quit => "Quit".into(),
    }
}

fn action_to(a: &TrayAction) -> Action {
    match a {
        TrayAction::LoadProfile(n) => Action::LoadProfile(n.clone()),
        TrayAction::Stop => Action::Stop,
        TrayAction::RemoteLoadProfile(n) => Action::RemoteLoadProfile(n.clone()),
        TrayAction::RemoteStop => Action::RemoteStop,
        TrayAction::OpenStats => Action::OpenStats,
        TrayAction::Restart => Action::Restart,
        TrayAction::Quit => Action::Quit,
    }
}

/// Dots live in a stable per-user dir (no build-machine path is baked into
/// the binary): the embedded .ico bytes are materialized there on demand.
fn load_dot(name: &str) -> HICON {
    let data: &[u8] = match name {
        "icon_red.ico" => include_bytes!("../assets/icon_red.ico"),
        "icon_green.ico" => include_bytes!("../assets/icon_green.ico"),
        other => panic!("unknown tray dot {other}"),
    };
    let local = std::env::var("LOCALAPPDATA")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    let dir = std::path::Path::new(&local).join("llm_proxy");
    let _ = std::fs::create_dir_all(&dir);
    let full = dir.join(name);
    if std::fs::read(&full).ok().as_deref() != Some(data) {
        std::fs::write(&full, data).unwrap_or_else(|e| panic!("write tray dot {full:?}: {e}"));
    }
    let w = w(&full.to_string_lossy());
    unsafe {
        let h = LoadImageW(
            None,
            PCWSTR(w.as_ptr()),
            IMAGE_ICON,
            0,
            0,
            LR_LOADFROMFILE | LR_DEFAULTSIZE,
        )
        .unwrap_or_else(|e| panic!("load tray dot {full:?}: {e}"));
        HICON(h.0)
    }
}

struct TrayData {
    tx: mpsc::Sender<Action>,
    actions: Mutex<Vec<TrayAction>>,
    menu: AtomicPtr<core::ffi::c_void>,
    icon_red: AtomicPtr<core::ffi::c_void>,
    icon_green: AtomicPtr<core::ffi::c_void>,
    /// Backend ready? Drives the dot color and the tooltip last line.
    state: AtomicBool,
    proxy_addr: String,
    stats_addr: String,
    /// Active profile name (runtime-switchable via set_active_profile).
    /// Bridge mode: the remote's active profile (set_remote_profiles).
    profile: Mutex<String>,
    profile_names: Vec<String>,
    /// Bridge mode: the remote server's profile list as (id, name) pairs,
    /// mirrored from the remote /api/stats feed (set_remote_profiles).
    remote_profiles: Mutex<Vec<(String, String)>>,
    has_stats: bool,
    router: bool,
    hwnd: AtomicPtr<core::ffi::c_void>,
}

impl TrayData {
    fn action_for(&self, id: u32) -> Option<TrayAction> {
        self.actions.lock().unwrap().get((id - MENU_BASE_ID as u32) as usize).cloned()
    }

    fn state(&self) -> bool {
        self.state.load(Ordering::SeqCst)
    }

    /// Tooltip: runtime info, refreshed with the state.
    fn tip(&self) -> [u16; 128] {
        let last = if self.state() {
            format!("backend: ready ({})", self.profile.lock().unwrap())
        } else {
            "backend: not active".to_string()
        };
        let text = format!(
            "llm_proxy\nproxy {}\nstats http://{}\n{}",
            self.proxy_addr, self.stats_addr, last
        );
        let mut sz = [0u16; 128];
        let u: Vec<u16> = text.encode_utf16().collect();
        let n = u.len().min(127);
        sz[..n].copy_from_slice(&u[..n]);
        sz
    }

    /// Rebuild the popup menu (labels AND actions vector): profiles with
    /// the active marker, then Stop, (Open stats), Exit. Bridge mode: the
    /// entries are the remote server's profiles (mirrored feed) — the
    /// list changes at runtime, so labels and actions are rebuilt together
    /// to keep the item ids consistent.
    fn rebuild_menu(&self) {
        unsafe {
            let _ = DestroyMenu(HMENU(self.menu.load(Ordering::SeqCst) as _));
            let menu = CreatePopupMenu().expect("CreatePopupMenu");
            let active = self.profile.lock().unwrap().clone();
            let is_remote = self.profile_names.is_empty();
            let profiles: Vec<(String, String)> = if is_remote {
                self.remote_profiles.lock().unwrap().clone()
            } else {
                self.profile_names.iter().map(|n| (n.clone(), n.clone())).collect()
            };
            // Router mode (local): no per-profile load entries (the client
            // drives load/unload via the model field / POST /models/load), but
            // "Stop backend" is kept — it frees VRAM while the backend is loaded.
            let entries: Vec<(String, TrayAction)> = if self.router && !is_remote {
                vec![("Stop backend".to_string(), TrayAction::Stop)]
            } else {
                profiles
                    .into_iter()
                    .map(|(id, name)| {
                        let label = if is_remote {
                            if id == active {
                                format!("● {name} (remote)")
                            } else {
                                format!("Remote profile: {name}")
                            }
                        } else if name == active {
                            format!("● {name} (active)")
                        } else {
                            format!("Load profile: {name}")
                        };
                        let action = if is_remote {
                            TrayAction::RemoteLoadProfile(id)
                        } else {
                            TrayAction::LoadProfile(id)
                        };
                        (label, action)
                    })
                    .chain(std::iter::once((
                        "Stop backend".to_string(),
                        if is_remote {
                            TrayAction::RemoteStop
                        } else {
                            TrayAction::Stop
                        },
                    )))
                    .collect()
            };
            // "Open stats" goes between Stop and Exit when enabled.
            let mut ordered: Vec<(String, TrayAction)> = Vec::new();
            for (label, action) in entries {
                let is_stop = label == "Stop backend";
                ordered.push((label, action));
                if is_stop && self.has_stats {
                    ordered.push(("Open stats".to_string(), TrayAction::OpenStats));
                }
            }
            ordered.push(("Restart proxy".to_string(), TrayAction::Restart));
            ordered.push(("Exit".to_string(), TrayAction::Quit));
            let mut actions: Vec<TrayAction> = Vec::with_capacity(ordered.len());
            let mut next_id = MENU_BASE_ID;
            for (label, action) in ordered {
                let lw = w(&label);
                AppendMenuW(
                    menu,
                    MF_STRING,
                    next_id,
                    PCWSTR(lw.as_ptr()),
                )
                .expect("AppendMenuW");
                actions.push(action);
                next_id += 1;
            }
            *self.actions.lock().unwrap() = actions;
            self.menu.store(menu.0 as *mut _, Ordering::SeqCst);
        }
    }

    fn nid(&self) -> NOTIFYICONDATAW {
        let icon_ptr = if self.state() {
            self.icon_green.load(Ordering::SeqCst)
        } else {
            self.icon_red.load(Ordering::SeqCst)
        };
        NOTIFYICONDATAW {
            cbSize: core::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: HWND(self.hwnd.load(Ordering::SeqCst) as _),
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: HICON(icon_ptr as _),
            szTip: self.tip(),
            ..Default::default()
        }
    }
}

static DATA: OnceLock<TrayData> = OnceLock::new();

/// Called from the proxy task whenever the backend readiness changes:
/// posts the new state to the tray thread, which swaps dot + tooltip.
/// No-op before the tray window exists.
pub fn set_backend_state(active: bool) {
    unsafe {
        if let Some(d) = DATA.get() {
            let hwnd = HWND(d.hwnd.load(Ordering::SeqCst) as _);
            if !hwnd.is_invalid() {
                let _ = PostMessageW(Some(hwnd), WM_APP_STATE, WPARAM(active as usize), LPARAM(0));
            }
        }
    }
}

/// Called from the action dispatcher when the active profile changes:
/// the tray thread rebuilds the menu (active marker) and refreshes the
/// tooltip. No-op before the tray window exists.
pub fn set_active_profile(name: &str) {
    unsafe {
        if let Some(d) = DATA.get() {
            *d.profile.lock().unwrap() = name.to_string();
            let hwnd = HWND(d.hwnd.load(Ordering::SeqCst) as _);
            if !hwnd.is_invalid() {
                let _ = PostMessageW(Some(hwnd), WM_APP_PROFILE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Bridge mode: the remote server's profile list + its active profile
/// (from the mirrored /api/stats feed, polled in app.rs). The tray thread
/// rebuilds the menu (the entries are remote now) and refreshes the
/// tooltip, which carries the active profile name. No-op before the tray
/// window exists. Call only when the feed actually changed.
pub fn set_remote_profiles(
    profiles: Vec<(String, String)>,
    active: String,
) {
    unsafe {
        if let Some(d) = DATA.get() {
            *d.remote_profiles.lock().unwrap() = profiles;
            *d.profile.lock().unwrap() = active;
            let hwnd = HWND(d.hwnd.load(Ordering::SeqCst) as _);
            if !hwnd.is_invalid() {
                let _ = PostMessageW(Some(hwnd), WM_APP_PROFILE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

// The crate binding declares TrackPopupMenu as returning BOOL, but with
// TPM_RETURNCMD the real API returns the selected command id. Call it raw
// so we can dispatch the menu action directly, without depending on
// WM_COMMAND delivery to this hidden window.
extern "system" {
    #[link_name = "TrackPopupMenu"]
    fn TrackPopupMenuCmdId(
        hmenu: *mut core::ffi::c_void,
        uflags: u32,
        x: i32,
        y: i32,
        nreserved: i32,
        hwnd: *mut core::ffi::c_void,
        prcrect: *const core::ffi::c_void,
    ) -> i32;
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if let Some(d) = DATA.get() {
        match msg {
            WM_APP_TRAY => {
                // The callback message registered in NOTIFYICONDATAW.
                let event = lp.0 as u32;
                match event {
                    TRAY_RBUTTONUP => {
                        info!("[tray] right-click on tray icon -> popup menu");
                        // The popup owner must be the foreground window,
                        // otherwise the first click just dismisses the menu
                        // without delivering the selection.
                        let sfw = SetForegroundWindow(hwnd);
                        // Fallback for the Windows foreground lock: if we did
                        // not get the foreground, share the input queue with
                        // the current foreground thread so the modal menu
                        // loop still receives keyboard/mouse. Detached after
                        // the menu closes.
                        let mut attach: (u32, u32) = (0, 0);
                        let fg = GetForegroundWindow();
                        if sfw.0 == 0 && !fg.0.is_null() && fg != hwnd {
                            let mut _pid = 0u32;
                            let fg_tid = GetWindowThreadProcessId(fg, Some(&mut _pid));
                            let cur_tid = GetCurrentThreadId();
                            if fg_tid != 0
                                && fg_tid != cur_tid
                                && AttachThreadInput(cur_tid, fg_tid, true).0 != 0
                            {
                                attach = (cur_tid, fg_tid);
                            }
                        }
                        let mut pos = POINT { x: 0, y: 0 };
                        let _ = GetCursorPos(&mut pos);
                        let cmd = TrackPopupMenuCmdId(
                            d.menu.load(Ordering::SeqCst) as *mut _,
                            TPM_RIGHTBUTTON.0 | TPM_RETURNCMD.0,
                            pos.x,
                            pos.y,
                            0,
                            hwnd.0 as *mut _,
                            core::ptr::null(),
                        );
                        if attach.0 != 0 {
                            let _ = AttachThreadInput(attach.0, attach.1, false);
                        }
                        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                        info!(sfw_ok = sfw.0, attached = attach.0 != 0, cmd, "[tray] menu closed");
                        // TPM_RETURNCMD: cmd = selected item id, 0 = dismissed.
                        // Dispatch here directly: the hidden window may never
                        // receive the matching WM_COMMAND.
                        if cmd > 0 {
                            match d.action_for(cmd as u32) {
                                Some(a) => {
                                    info!(id = cmd, action = %action_label(&a), "[tray] menu action");
                                    let _ = d.tx.send(action_to(&a));
                                }
                                None => info!(id = cmd, "[tray] unknown menu id"),
                            }
                        }
                    }
                    TRAY_LBUTTONDBLCLK => {
                        info!("[tray] icon double-click -> open stats");
                        let _ = d.tx.send(Action::OpenStats);
                    }
                    TRAY_LBUTTONUP => debug!("[tray] icon single-click (ignored)"),
                    _ => debug!(event, "[tray] icon callback message"),
                }
                return LRESULT(0);
            }
            WM_APP_STATE => {
                let active = wp.0 != 0;
                if d.state() != active {
                    d.state.store(active, Ordering::SeqCst);
                    info!(
                        active,
                        "[tray] backend state changed -> dot/tooltip updated"
                    );
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &d.nid());
                }
                return LRESULT(0);
            }
            WM_APP_PROFILE => {
                info!("[tray] active profile changed -> menu/tooltip rebuilt");
                d.rebuild_menu();
                let _ = Shell_NotifyIconW(NIM_MODIFY, &d.nid());
                return LRESULT(0);
            }
            WM_APP..=0x80FF => {
                debug!(msg, "[tray] WM_APP-range message (not tray callback/state)");
            }
            WM_COMMAND => {
                // Windows may also post WM_COMMAND for the selected item;
                // the action is already dispatched from the TrackPopupMenu
                // return value, so log only (no double dispatch).
                info!(id = (wp.0 & 0xFFFF), "[tray] WM_COMMAND (already dispatched)");
                return LRESULT(0);
            }
            WM_TASKBARCREATED => {
                info!("[tray] taskbar recreated, re-adding icon");
                let _ = Shell_NotifyIconW(NIM_ADD, &d.nid());
                return LRESULT(0);
            }
            WM_DESTROY => {
                info!("[tray] hidden window destroyed");
                let _ = Shell_NotifyIconW(NIM_DELETE, &d.nid());
                let _ = PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct Tray {
    profile_names: Vec<String>,
    active: String,
    has_stats: bool,
    proxy_addr: String,
    stats_addr: String,
    router: bool,
}

impl Tray {
    pub fn new(
        profile_names: Vec<String>,
        active: &str,
        has_stats: bool,
        proxy_addr: &str,
        stats_addr: &str,
        router: bool,
    ) -> Self {
        Self {
            profile_names,
            active: active.to_string(),
            has_stats,
            proxy_addr: proxy_addr.to_string(),
            stats_addr: stats_addr.to_string(),
            router,
        }
    }

    /// Creates the hidden window + tray icon and blocks on the message pump
    /// until Quit is dispatched or the pump dies. Must run on its own thread.
    pub fn run(self, tx: mpsc::Sender<Action>) -> Result<()> {
        unsafe {
            // ---- hidden message window ----
            // Let this process set the foreground window (needed for
            // SetForegroundWindow before TrackPopupMenu from this thread).
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            let hinst = GetModuleHandleW(None)?;
            let class_name = w(TRAY_CLASS);
            let wnd_class = WNDCLASSW {
                style: CS_VREDRAW | CS_HREDRAW,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst.into(),
                hIcon: HICON::default(),
                hCursor: HCURSOR::default(),
                hbrBackground: HBRUSH(GetStockObject(NULL_BRUSH).0),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
            };
            let atom = RegisterClassW(&wnd_class);
            if atom == 0 {
                panic!("RegisterClassW failed");
            }
            // ---- hidden message window ----
            // LLM_PROXY_TITLE env override: lets a second (test) instance
            // own a distinguishable window title.
            let title = match std::env::var("LLM_PROXY_TITLE") {
                Ok(t) if !t.is_empty() => w(&t),
                _ => w(TRAY_TITLE),
            };
            // The window is invisible (0x0, far off-screen) but MUST carry
            // WS_VISIBLE: a popup menu owned by a window that cannot be made
            // foreground is dismissed by the first click without delivering
            // the selection. WS_EX_TOOLWINDOW keeps it out of the taskbar
            // and Alt+Tab.
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP | WS_VISIBLE,
                -4000,
                -4000,
                0,
                0,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .expect("CreateWindowExW");

            // ---- status dots (pre-generated .ico, loaded from assets/) ----
            let icon_red = load_dot("icon_red.ico");
            let icon_green = load_dot("icon_green.ico");

            // ---- menu ----
            let menu = CreatePopupMenu().expect("CreatePopupMenu");
            let mut actions: Vec<TrayAction> = Vec::new();
            // Router mode: no per-profile load entries — the client picks the
            // model via the field / POST /models/load.
            if !self.router {
                for name in &self.profile_names {
                    let label = if *name == self.active {
                        format!("● {name} (active)")
                    } else {
                        format!("Load profile: {name}")
                    };
                    let lw = w(&label);
                    AppendMenuW(
                        menu,
                        MF_STRING,
                        MENU_BASE_ID + actions.len(),
                        PCWSTR(lw.as_ptr()),
                    )
                    .expect("AppendMenuW");
                    actions.push(TrayAction::LoadProfile(name.clone()));
                }
            }
            for (label, action) in [
                ("Stop backend", TrayAction::Stop),
                ("Open stats", TrayAction::OpenStats),
                ("Restart proxy", TrayAction::Restart),
                ("Exit", TrayAction::Quit),
            ] {
                if label == "Open stats" && !self.has_stats {
                    continue;
                }
                // Router mode keeps "Stop backend" (frees VRAM); only the
                // per-profile load entries are removed (see the profile loop).
                // Bridge mode (no local profiles): Stop acts on the remote.
                let action = if label == "Stop backend" && self.profile_names.is_empty() {
                    TrayAction::RemoteStop
                } else {
                    action
                };
                let lw = w(label);
                AppendMenuW(
                    menu,
                    MF_STRING,
                    MENU_BASE_ID + actions.len(),
                    PCWSTR(lw.as_ptr()),
                )
                .expect("AppendMenuW");
                actions.push(action);
            }

            DATA.get_or_init(|| TrayData {
                tx,
                actions: Mutex::new(actions),
                menu: AtomicPtr::new(menu.0 as *mut _),
                icon_red: AtomicPtr::new(icon_red.0 as *mut _),
                icon_green: AtomicPtr::new(icon_green.0 as *mut _),
                state: AtomicBool::new(false),
                proxy_addr: self.proxy_addr,
                stats_addr: self.stats_addr,
                profile: Mutex::new(self.active),
                profile_names: self.profile_names.clone(),
                remote_profiles: Mutex::new(Vec::new()),
                has_stats: self.has_stats,
                router: self.router,
                hwnd: AtomicPtr::new(hwnd.0 as *mut _),
            });
            let data = DATA.get().unwrap();
            TRAY_READY.store(true, Ordering::SeqCst);

            // ---- tray icon registration (starts RED: backend not ready) ----
            Shell_NotifyIconW(NIM_ADD, &data.nid()).expect("Shell_NotifyIconW(NIM_ADD)");
            info!("[tray] icon registered (dot: red)");

            // ---- message pump ----
            let mut msg = core::mem::zeroed();
            loop {
                let r = GetMessageW(&mut msg, None, 0, 0).0;
                if r <= 0 {
                    break; // 0 = error, -1 = WM_QUIT
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            info!("[tray] message pump exited");
            let _ = Shell_NotifyIconW(NIM_DELETE, &data.nid());
            let _ = DestroyWindow(hwnd);
            let _ = DestroyMenu(HMENU(data.menu.load(Ordering::SeqCst) as _));
            let _ = DestroyIcon(HICON(data.icon_red.load(Ordering::SeqCst) as _));
            let _ = DestroyIcon(HICON(data.icon_green.load(Ordering::SeqCst) as _));
        }
        Ok(())
    }
}
