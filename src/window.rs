//! The Win32 side: a per-pixel-alpha layered window, tray icon, menu and timers.
//! The window has no redraw loop of its own: a timer redraws only the character area, at a rate set
//! by the state, and stops entirely while the buddy is hidden. Fully transparent pixels of a layered
//! window don't receive mouse input, so clicks there reach whatever is underneath with no extra work.

use crate::i18n::t;
use crate::anim::{self, CharAnim, Player};
use crate::config::{Config, MIN_ZOOM, SIZE_FACTORS};
use crate::focus;
use crate::fx;
use crate::theme;
use crate::hooks_config;
use crate::library::{self, state_index, Character, Source};
use crate::motion::Motion;
use crate::picker;
use crate::render::{self, Bubble, Hit, HitRect, TextDrawer, View, CHAR_SIZE, MOTION_MARGIN_SIDE, MOTION_MARGIN_TOP, WIN_H, WIN_W};
use crate::server::{self, now_ms, Shared};
use crate::sound;
use crate::store::{self, name_of, status_of, Session, State};
use crate::usage;
use std::cell::RefCell;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::core::{w, GUID};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Power::{RegisterPowerSettingNotification, POWERBROADCAST_SETTING};
use windows_sys::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows_sys::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, ReleaseCapture, SetCapture, TrackMouseEvent, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_EXCLUDED_FROM_PEEK, DWMWA_TRANSITIONS_FORCEDISABLED};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_WAKE: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
const WM_DPI: u32 = WM_APP + 3;
const WM_FIRST_RUN: u32 = WM_APP + 4;
const WM_ANIM_READY: u32 = WM_APP + 5;
const WM_OPEN_PICKER: u32 = WM_APP + 6;
const WM_RESTORE_FOCUS: u32 = WM_APP + 7;
const WM_IMPORT_PACKS: u32 = WM_APP + 8;
/// Marks a WM_COPYDATA as a .buddy path from a second copy of the app.
const COPYDATA_PACK: usize = 0xB0DD_1E01;
// Not exposed by the windows-sys features this app enables.
const WM_MOUSELEAVE: u32 = 0x02A3;
const NIN_KEYSELECT: u32 = WM_USER + 1;

const TIMER_FRAME: usize = 1;
const TIMER_HOUSEKEEPING: usize = 2;
const TIMER_BUBBLE: usize = 3;
const TIMER_PANEL: usize = 4;
const TIMER_HOVER: usize = 5;
const TIMER_TRIM: usize = 6;
const TIMER_SCAN: usize = 7;
const TIMER_ZOOM: usize = 8;
const MAX_WINDOWS_WALKED: usize = 512;
const TRIM_DELAY_MS: u32 = 3_000;
/// Once the wheel rests this long, the size is saved and sharper frames prepared if they would help.
const ZOOM_SETTLE_MS: u32 = 500;
/// Where each wheel notch lands; past the last, and capped by it, is as big as fits on the screen.
const ZOOM_LEVELS: [f32; 12] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0];
/// Room kept over the character for the bubble and panel, as long as the screen has it.
const UI_ROOM: f32 = 340.0;
/// Built-in characters are drawn, so they stay sharp at any size; past this their frames get stretched
/// too, because a full-screen loop of them would be hundreds of MB.
const VECTOR_PREP_MAX: f32 = 2.0;
/// Full screen gives up at most this much of its size to show frames exactly as prepared, which costs
/// a fraction of scaling them every frame.
const FULL_SNAP: f32 = 0.9;
/// How much of the screen the character may fill. Its frames are small, so any bigger is mostly blur.
const MAX_SCREEN_SHARE: f32 = 0.6;
/// The buddy never has the keyboard, so its skills are hotkeys: Ctrl+Alt+key anywhere, and plain Ctrl+key
/// only while the pointer is on the buddy, so the rest of the time those stay with other apps (Ctrl+W
/// closes their tabs, Ctrl+R reloads). Global ids count from 1, the hover ones from HOVER_HOTKEY. The
/// keys themselves are the user's, in config.skill_keys, in this order.
const SKILLS: [Skill; 3] = [Skill::Blink, Skill::Clones, Skill::Ultimate];
const HOVER_HOTKEY: i32 = 11;
const TIMER_SKILL: usize = 9;
const TIMER_COMBO: usize = 10;
const TIMER_AUTO_CLONE: usize = 11;
/// After a skill the plain Ctrl keys stay with the buddy this long, wherever the pointer is, so the
/// next one can follow without chasing the buddy into its new corner first.
const COMBO_MS: u32 = 4_000;
const SKILL_FRAME_MS: u32 = 16;
/// Teleport: the buddy fades out over the mark, jumps, and fades in inside a flash.
const BLINK_OUT_MS: u64 = 160;
const BLINK_IN_MS: u64 = 160;
const FLASH_MS: u64 = 420;
const SMOKE_MS: u64 = 450;
/// Clones fade in a little after their smoke bursts, and out quicker than it clears.
const CLONE_FADE_IN_DELAY_MS: u64 = 80;
const CLONE_FADE_MS: u64 = 150;
const CLONE_LIFE_MS: u64 = 8_000;
const CLONE_STAGGER_MS: u64 = 120;
/// Ultimate: the flash goes off first, then the buddy fills the screen.
const ULTIMATE_DELAY_MS: u64 = 150;
const FULL_WAIT_MS: u64 = 1_500;
/// Bigger flashes would cost more to draw than they add.
const MAX_FLASH_PX: u32 = 1000;
const FX_CLASS: *const u16 = w!("ClaudeBuddyFx");
/// A finished turn is counted after this, so a burst of them costs one scan.
const SCAN_DELAY_MS: u32 = 20_000;
/// How long the bubble stays after a session's state changes, unless it needs you.
const BUBBLE_MS: u32 = 8_000;
/// Lets the pointer cross the transparent gap to the panel or bubble without them vanishing.
const HOVER_GRACE_MS: u32 = 500;
/// An open panel closes by itself soon after the pointer leaves, so it is never left covering work.
const PANEL_GRACE_MS: u32 = 2_500;
const BALLOON_ICON_SIZE: u32 = 64;
const HOUSEKEEPING_MS: u32 = 5_000;
const ACTIVE_FRAME_MS: u32 = 33;
const DRAG_THRESHOLD: i32 = 4;
const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
const WTS_SESSION_LOCK: u32 = 7;
const WTS_SESSION_UNLOCK: u32 = 8;
const PBT_POWERSETTINGCHANGE: u32 = 0x8013;
const DEVICE_NOTIFY_WINDOW_HANDLE: u32 = 0;
/// The display of this session. The console one stays off for as long as someone works over Remote Desktop.
const GUID_SESSION_DISPLAY_STATUS: GUID = GUID { data1: 0x2B84C20E, data2: 0xAD23, data3: 0x4DDF, data4: [0x93, 0xDB, 0x05, 0xFF, 0xBD, 0x7E, 0xFC, 0xA5] };
/// Same gap under the character as the drawing code leaves.
const CHAR_GAP: f32 = 4.0;
/// The vector characters' body and shadow, without the empty sides of their drawing square.
const VECTOR_BODY_W: f32 = 112.0;
/// Every 10 minutes, in housekeeping ticks: only for turns that never send Stop, like interrupted ones.
const USAGE_SCAN_TICKS: u32 = 120;

const CMD_TOGGLE: usize = 1;
const CMD_MUTE: usize = 2;
const CMD_TOASTS: usize = 3;
const CMD_HOOKS: usize = 4;
const CMD_AUTOSTART: usize = 5;
const CMD_RESET_POS: usize = 6;
const CMD_EXIT: usize = 7;
const CMD_PICKER: usize = 8;
const CMD_SETTINGS: usize = 11;
const CMD_USER_FOLDER: usize = 9;
const CMD_USAGE: usize = 10;
const CMD_FULL: usize = 12;
const CMD_BLINK: usize = 13;
const CMD_CLONES: usize = 14;
const CMD_AUTO_SKILLS: usize = 15;
const CMD_CHARACTER: usize = 100;

const RUN_KEY: *const u16 = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: *const u16 = w!("ClaudeBuddyRS");

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copies into a fixed UTF-16 field, cutting so the terminating zero always fits.
fn copy_wide(dst: &mut [u16], s: &str) {
    let mut n = 0;
    for u in s.encode_utf16().take(dst.len() - 1) {
        dst[n] = u;
        n += 1;
    }
    dst[n] = 0;
}

/// Wakes the UI thread; safe from any thread.
pub fn wake(hwnd: isize) {
    if hwnd != 0 {
        unsafe { PostMessageW(hwnd as HWND, WM_WAKE, 0, 0) };
    }
}

fn exe_path() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

fn message_box(text: &str, flags: u32) -> i32 {
    let t = wide(text);
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), w!("Claude Buddy"), flags | MB_SETFOREGROUND) }
}

fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) == ERROR_SUCCESS }
}

fn set_autostart(on: bool) {
    unsafe {
        if on {
            let cmd = wide(&format!("\"{}\"", exe_path()));
            RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ, cmd.as_ptr() as *const _, (cmd.len() * 2) as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}

/// Windows can leave a window marked topmost with ordinary windows above it, so the mark can't be trusted.
fn normal_window_above(hwnd: HWND) -> bool {
    unsafe {
        let mut w = GetWindow(hwnd, GW_HWNDPREV);
        for _ in 0..MAX_WINDOWS_WALKED {
            if w.is_null() {
                return false;
            }
            if IsWindowVisible(w) != 0 && GetWindowLongPtrW(w, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST == 0 {
                return true;
            }
            w = GetWindow(w, GW_HWNDPREV);
        }
        false
    }
}

/// The order of windows changed: put right at once, before the screen shows it if at all possible.
unsafe extern "system" fn windows_reordered(_hook: HWINEVENTHOOK, event: u32, window: HWND, _object: i32, _child: i32, _thread: u32, _time: u32) {
    // Reordering inside a program's own window says nothing about the buddy.
    if event == EVENT_OBJECT_REORDER && window != GetDesktopWindow() {
        return;
    }
    with_app(|a| {
        // Opening some programs, like Photos, costs it its place without anything to show for it until
        // the next window comes up over it. Claimed again here, it never gets that far.
        if event == EVENT_SYSTEM_FOREGROUND && !a.behind_picker {
            SetWindowPos(a.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        a.sync_z_order();
    });
}

fn process_alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            // Access denied still means it exists; anything else means it is gone.
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

/// The window's pixel buffer: a DIB section that tiny-skia draws into directly.
pub(crate) struct Canvas {
    pub dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    pub bits: *mut u8,
    pub w: u32,
    pub h: u32,
}

impl Canvas {
    pub fn new(w: u32, h: u32) -> Option<Canvas> {
        let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w as i32;
        info.bmiHeader.biHeight = -(h as i32); // top-down
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        unsafe {
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bmp.is_null() || bits.is_null() {
                DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bmp);
            Some(Canvas { dc, bmp, old, bits: bits as *mut u8, w, h })
        }
    }

}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bmp);
            DeleteDC(self.dc);
        }
    }
}

/// Pushes a canvas to its layered window; `dirty` limits what the compositor has to take in.
fn present_layered(hwnd: HWND, canvas: &Canvas, dirty: Option<(u32, u32, u32, u32)>, alpha: u8) {
    let size = SIZE { cx: canvas.w as i32, cy: canvas.h as i32 };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
    let rect = dirty.map(|(x, y, w, h)| RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 });
    let info = UPDATELAYEREDWINDOWINFO {
        cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
        hdcDst: std::ptr::null_mut(),
        pptDst: std::ptr::null(),
        psize: &size,
        hdcSrc: canvas.dc,
        pptSrc: &src,
        crKey: 0,
        pblend: &blend,
        dwFlags: ULW_ALPHA,
        prcDirty: rect.as_ref().map_or(std::ptr::null(), |r| r as *const RECT),
    };
    unsafe { UpdateLayeredWindowIndirect(hwnd, &info) };
}

/// A small window of its own, for an effect or a clone. Clicks go through it to whatever is underneath.
struct Sprite {
    hwnd: HWND,
    canvas: Canvas,
    shown: bool,
}

impl Sprite {
    fn new(x: i32, y: i32, w: u32, h: u32) -> Option<Sprite> {
        let canvas = Canvas::new(w.max(1), h.max(1))?;
        let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE;
        let hwnd = unsafe { CreateWindowExW(ex, FX_CLASS, w!(""), WS_POPUP, x, y, canvas.w as i32, canvas.h as i32, std::ptr::null_mut(), std::ptr::null_mut(), GetModuleHandleW(std::ptr::null()), std::ptr::null()) };
        (!hwnd.is_null()).then_some(Sprite { hwnd, canvas, shown: false })
    }

    /// `size` pixels square, centred on a point of the screen.
    fn around(c: (i32, i32), size: u32) -> Option<Sprite> {
        Sprite::new(c.0 - size as i32 / 2, c.1 - size as i32 / 2, size, size)
    }

    /// Redraws the whole sprite with `f` and puts it on screen.
    fn draw(&mut self, f: impl FnOnce(&mut [u8], u32, u32)) {
        let (w, h) = (self.canvas.w, self.canvas.h);
        let buf = unsafe { std::slice::from_raw_parts_mut(self.canvas.bits, (w * h * 4) as usize) };
        buf.fill(0);
        f(buf, w, h);
        present_layered(self.hwnd, &self.canvas, None, 255);
        if !self.shown {
            self.shown = true;
            unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
        }
    }
}

impl Drop for Sprite {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.hwnd) };
    }
}

unsafe extern "system" fn fx_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Skill {
    /// Q: Flying Thunder God, a teleport to another corner of the screen.
    Blink,
    /// W: shadow clones in the corners.
    Clones,
    /// R: full screen.
    Ultimate,
}

fn skill_of_hotkey(id: i32) -> Option<Skill> {
    let i = if id >= HOVER_HOTKEY { id - HOVER_HOTKEY } else { id - 1 };
    usize::try_from(i).ok().and_then(|i| SKILLS.get(i)).copied()
}

/// Taken by another app, a shortcut is simply missing; the menu still has every skill. A letter or digit's
/// virtual-key code is its uppercase ASCII, so the configured keys go to Windows as they are.
fn register_skill_keys(hwnd: HWND, hover: bool, keys: &[u8; 3]) {
    for (i, key) in keys.iter().enumerate().filter(|(_, k)| **k != 0) {
        let (id, mods) = if hover { (HOVER_HOTKEY + i as i32, MOD_CONTROL) } else { (1 + i as i32, MOD_CONTROL | MOD_ALT) };
        unsafe { RegisterHotKey(hwnd, id, mods | MOD_NOREPEAT, *key as u32) };
    }
}

fn unregister_skill_keys(hwnd: HWND, hover: bool) {
    let base = if hover { HOVER_HOTKEY } else { 1 };
    for i in 0..SKILLS.len() as i32 {
        unsafe { UnregisterHotKey(hwnd, base + i) };
    }
}

/// "\tCtrl+Q" for a menu item, or nothing when the skill has no key.
fn key_hint(key: u8) -> String {
    if key == 0 { String::new() } else { format!("\tCtrl+{}", key as char) }
}

/// A teleport under way.
struct Blink {
    start: Instant,
    from: Option<Sprite>,
    to: Option<Sprite>,
    dest: DragStep,
    jumped: bool,
}

/// What a clone shows: frames of its own, or a built-in character drawn afresh.
enum CloneBody {
    Frames(Player),
    Drawn(usize),
}

/// A shadow clone: the character again, playing on its own in a corner.
struct Bunshin {
    sprite: Sprite,
    body: CloneBody,
    /// How much its frames are stretched (for a drawn one, its pixel density), where its feet are on
    /// the sprite, and how big it is there.
    k: f32,
    feet: (f32, f32),
    size: (f32, f32),
    born: Instant,
    leave_at: Instant,
    drawn: bool,
}

/// What the character looks like right now.
enum Visual {
    Vector(usize),
    /// `room`: keeps the margins that movement on the stage needs. `prep`: the pixel density the frames
    /// were prepared at; showing them bigger than that stretches them.
    Media { player: Player, states: Box<[(Motion, bool); 5]>, room: bool, prep: f32 },
}

/// A character finished loading on a worker thread.
struct Loaded {
    token: u64,
    id: String,
    result: Result<Arc<CharAnim>, String>,
    prep: f32,
    native: Option<f32>,
}

/// Density to prepare frames at. Up to the large size, exactly what is shown, so frames are only
/// copied. Bigger, all that the source holds (for drawn characters, a fixed cap): one build then
/// serves every zoom up to full screen, and the wheel never waits on another. Rounded so the same
/// size always finds the same cache file. Never past `full`, the density of full screen when known:
/// more would only be scaled down again on every frame. Rounded down, so full screen can show the
/// frames exactly as they are.
fn prep_density(char_scale: f32, scale: f32, native: Option<f32>, full: Option<f32>) -> f32 {
    let usual = scale * SIZE_FACTORS[2];
    let d = if char_scale <= usual + 0.001 { char_scale } else { native.map_or(scale * VECTOR_PREP_MAX, |n| n.max(usual)).min(full.unwrap_or(f32::MAX)).max(usual) };
    (d * 100.0 + 0.001).floor() / 100.0
}

/// Where the window goes when its size changes.
enum Anchor {
    /// Grows and shrinks around the bottom middle, where the character stands.
    Keep,
    At((i32, i32)),
    /// Bottom middle of the screen.
    ScreenBottom,
}

/// What full screen returns to.
struct FullRestore {
    zoom: f32,
    pos: (i32, i32),
    shift: f32,
    lift: f32,
}

static LOADED: Mutex<Option<Loaded>> = Mutex::new(None);

pub use crate::config::{user_root, Setting, SettingsView};

struct Drag {
    start_cursor: POINT,
    last_cursor: POINT,
    /// Where the window would be with the character in its usual spot.
    virtual_pos: (i32, i32),
    moved: bool,
    hit: Option<Hit>,
}

#[derive(Debug, PartialEq)]
struct DragStep {
    window: (i32, i32),
    shift: i32,
    lift: i32,
    virtual_pos: (i32, i32),
}

/// One step of a drag. The window (with room for bubble and panel) stays on the screen; past an edge
/// the character slides on inside it, so it can reach all four edges. Sizes are pixels.
fn drag_step(wanted: (i32, i32), win: (i32, i32), character: (i32, i32), gap: i32, area: &RECT) -> DragStep {
    let window = clamp_to(area, wanted.0, wanted.1, win.0, win.1);
    let x_limit = ((win.0 - character.0) / 2).max(0);
    let y_limit = (win.1 - gap - character.1).max(0);
    let shift = (wanted.0 - window.0).clamp(-x_limit, x_limit);
    let lift = (window.1 - wanted.1).clamp(0, y_limit);
    // Measured from here on, so turning back moves the character at once instead of working off an overshoot.
    DragStep { window, shift, lift, virtual_pos: (window.0 + shift, window.1 - lift) }
}

struct App {
    hwnd: HWND,
    shared: Arc<Shared>,
    config: Config,
    canvas: Canvas,
    text: TextDrawer,
    scale: f32,
    hits: Vec<HitRect>,
    state: State,
    library: Vec<Character>,
    housekeeping_ticks: u32,
    behind_picker: bool,
    hovering: bool,
    bubble_until: u64,
    known_states: Vec<(String, State)>,
    /// A full-screen app or presentation is in front: stay out of sight until it is gone.
    suppressed: bool,
    tray_state: State,
    session_locked: bool,
    display_off: bool,
    visual: Visual,
    current_sounds: [Option<PathBuf>; 2],
    load_token: u64,
    /// The current character's source resolution, from its last load; None for drawn ones.
    native: Option<f32>,
    /// The media character the frames on screen are of.
    shown_id: String,
    /// Density of sharper frames already being prepared, so settling twice doesn't start a second build.
    pending_prep: Option<f32>,
    /// Set while full screen is on.
    full_restore: Option<FullRestore>,
    /// Wheel movement not yet worth a whole notch, as touchpads send it.
    wheel_acc: i32,
    /// The whole window's opacity, which the teleport fades.
    alpha: u8,
    blink: Option<Blink>,
    bunshin: Vec<Bunshin>,
    /// Ultimate flashes going off, and when the full screen they announce comes.
    bursts: Vec<(Sprite, Instant)>,
    full_at: Option<Instant>,
    /// Full screen waits until this for its sharp frames, so it doesn't open blurred: loading them
    /// from the cache takes a moment, building them takes far too long to wait for.
    full_wait: Option<Instant>,
    /// A skill was just used: the plain Ctrl keys stay registered until TIMER_COMBO.
    combo: bool,
    state_since: Instant,
    sessions: Vec<Session>,
    panel_open: bool,
    hover: Option<String>,
    tracking_leave: bool,
    visible: bool,
    drag: Option<Drag>,
    started: Instant,
    frame_ms: u32,
    note_until: u64,
    tray: NOTIFYICONDATAW,
    tray_icon: HICON,
    taskbar_created: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app, unless it is already in use further up the stack: Win32 calls made while
/// handling one message can deliver another synchronously.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

fn work_area(pt: POINT) -> RECT {
    unsafe {
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        mi.rcWork
    }
}

fn clamp_to(a: &RECT, x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    (x.min(a.right - w).max(a.left), y.min(a.bottom - h).max(a.top))
}

impl App {
    fn size(&self) -> (i32, i32) {
        (self.canvas.w as i32, self.canvas.h as i32)
    }

    fn default_position(&self) -> (i32, i32) {
        let a = work_area(POINT { x: 0, y: 0 });
        let (w, h) = self.size();
        (a.right - w - (24.0 * self.scale) as i32, a.bottom - h - (8.0 * self.scale) as i32)
    }

    fn window_pos(&self) -> (i32, i32) {
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        unsafe { GetWindowRect(self.hwnd, &mut r) };
        (r.left, r.top)
    }

    fn move_to(&self, x: i32, y: i32) {
        unsafe { SetWindowPos(self.hwnd, std::ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) };
    }

    /// Pushes the buffer to the screen; `dirty` limits what the compositor has to take in.
    fn present(&mut self, dirty: Option<(u32, u32, u32, u32)>) {
        present_layered(self.hwnd, &self.canvas, dirty, self.alpha);
    }

    fn time(&self) -> f32 {
        // Wrapped so float precision never degrades in a long-running app; the wrap is a whole
        // number of the slowest cycles closely enough that nobody can catch the jump.
        (self.started.elapsed().as_secs_f64() % 3960.0) as f32
    }

    /// Draws the character as it should look now. None means nothing changed since the last call.
    fn draw_character(&mut self, force: bool) -> Option<(u32, u32, u32, u32)> {
        let (w, h, scale, state, t) = (self.canvas.w, self.canvas.h, self.char_scale(), self.state, self.time());
        let off = self.offset_px();
        let buf = unsafe { std::slice::from_raw_parts_mut(self.canvas.bits, (w * h * 4) as usize) };
        match &mut self.visual {
            Visual::Vector(kind) => Some(render::paint_character(buf, w, h, scale, *kind, state, t, off)),
            Visual::Media { player, states, room, prep } => {
                let (motion, badge) = states[state_index(state)];
                let changed = player.advance(now_ms());
                let (fw, fh) = (player.anim.w, player.anim.h);
                let stretch = scale / *prep;
                if force || motion != Motion::None || badge {
                    let moving = self.state_since.elapsed().as_secs_f32();
                    let badge = badge.then_some(state);
                    return Some(render::paint_stage_zoomed(buf, w, h, scale, &player.pixels, fw, fh, motion.at(moving), badge, t, off, *room, stretch));
                }
                if (stretch - 1.0).abs() > 0.001 {
                    return changed.map(|r| render::paint_change_zoomed(buf, w, h, scale, &player.pixels, fw, fh, r, off, stretch)).filter(|r| r.2 > 0);
                }
                // The usual case: only what differs from the last frame, or nothing at all.
                changed.map(|r| render::paint_change(buf, w, h, scale, &player.pixels, fw, fh, r, off)).filter(|r| r.2 > 0)
            }
        }
    }

    /// Actually on screen: wanted by the user, and nothing full-screen in front.
    fn shown(&self) -> bool {
        self.visible && !self.suppressed
    }

    /// Pixel density the character is drawn at: the display's, times the chosen size.
    fn char_scale(&self) -> f32 {
        self.scale * self.config.zoom
    }

    /// How much the prepared frames are stretched to reach the size shown.
    fn stretch(&self) -> f32 {
        match &self.visual {
            Visual::Media { prep, .. } => self.char_scale() / prep,
            Visual::Vector(_) => 1.0,
        }
    }

    /// The character at zoom 1 in logical pixels, and the room motion adds around it; both grow with
    /// the zoom: (body w, body h, margin w, margin h).
    fn char_dims(&self) -> (f32, f32, f32, f32) {
        let room = (MOTION_MARGIN_SIDE * 2.0, MOTION_MARGIN_TOP);
        match &self.visual {
            Visual::Media { player, prep, room: true, .. } => (player.anim.w as f32 / prep, player.anim.h as f32 / prep, room.0, room.1),
            Visual::Media { .. } => (CHAR_SIZE, CHAR_SIZE, 0.0, 0.0),
            // A media character still loading: the largest its stage can be.
            Visual::Vector(_) => match self.current() {
                Some(c) if !c.is_builtin() => (anim::MAX_WIDTH, (anim::BASE_HEIGHT * c.scale).min(anim::MAX_STAGE_HEIGHT), room.0, room.1),
                _ => (CHAR_SIZE, CHAR_SIZE, 0.0, 0.0),
            },
        }
    }

    fn area(&self) -> RECT {
        let (x, y) = self.window_pos();
        let (w, h) = self.size();
        work_area(POINT { x: x + w / 2, y: y + h / 2 })
    }

    fn area_logical(&self) -> (f32, f32) {
        let a = self.area();
        ((a.right - a.left) as f32 / self.scale, (a.bottom - a.top) as f32 / self.scale)
    }

    /// The biggest the character may be: what R goes to and where the wheel stops. A small screen still
    /// gets the three sizes, as far as they fit.
    fn max_zoom(&self) -> f32 {
        let (bw, bh, mw, mh) = self.char_dims();
        let (aw, ah) = self.area_logical();
        // The gap under the feet grows with the character too.
        let fits = (ah / (bh + mh + CHAR_GAP)).min(aw / (bw + mw));
        (fits * MAX_SCREEN_SHARE).max(SIZE_FACTORS[2]).min(fits).max(MIN_ZOOM)
    }

    /// Pixel density of character `id` at full screen, once its size is known: it is the one on screen.
    fn full_density(&self, id: &str) -> Option<f32> {
        (matches!(self.visual, Visual::Media { .. }) && self.shown_id == id).then(|| self.scale * self.max_zoom())
    }

    /// Full screen: as big as fits, or a touch smaller when that is the size the frames were prepared
    /// at, so they are copied as they are instead of scaled on every frame.
    fn full_zoom(&self) -> f32 {
        let max = self.max_zoom();
        match &self.visual {
            Visual::Media { prep, .. } if (max * FULL_SNAP..=max).contains(&(prep / self.scale)) => prep / self.scale,
            _ => max,
        }
    }

    /// Window size in pixels for the current zoom: the usual one, or bigger to hold a bigger
    /// character with the bubble's room over it, never past the screen.
    fn wanted_size(&self) -> (u32, u32) {
        let (bw, bh, mw, mh) = self.char_dims();
        let f = self.config.zoom;
        let (aw, ah) = self.area_logical();
        let w = WIN_W.max((bw + mw) * f).min(aw);
        let h = WIN_H.max((bh + mh + CHAR_GAP) * f + UI_ROOM).min(ah);
        ((w * self.scale).ceil() as u32, (h * self.scale).ceil() as u32)
    }

    /// Resizes the window to suit the zoom. The caller repaints.
    fn fit_window(&mut self, anchor: Anchor) {
        let (w, h) = self.wanted_size();
        let (ox, oy) = self.window_pos();
        let (ow, oh) = self.size();
        let a = self.area();
        if (w, h) != (self.canvas.w, self.canvas.h) {
            let Some(canvas) = Canvas::new(w, h) else { return };
            self.canvas = canvas;
        }
        let (w, h) = (w as i32, h as i32);
        let (x, y) = match anchor {
            Anchor::Keep => (ox + (ow - w) / 2, oy + oh - h),
            Anchor::At(p) => p,
            Anchor::ScreenBottom => ((a.left + a.right - w) / 2, a.bottom - h),
        };
        let (x, y) = clamp_to(&a, x, y, w, h);
        unsafe { SetWindowPos(self.hwnd, std::ptr::null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) };
        self.clamp_offset();
    }

    /// Shows the character at `z` times its usual size right away, stretching the frames it has;
    /// sharper ones follow once the wheel rests.
    fn set_zoom(&mut self, z: f32) {
        let z = z.clamp(MIN_ZOOM, self.max_zoom());
        if (z - self.config.zoom).abs() < 0.0005 {
            return;
        }
        self.config.zoom = z;
        if let Some(i) = SIZE_FACTORS.iter().position(|f| (f - z).abs() < 0.0005) {
            self.config.size = i as u8;
        }
        self.fit_window(Anchor::Keep);
        self.repaint_if_shown();
        unsafe { SetTimer(self.hwnd, TIMER_ZOOM, ZOOM_SETTLE_MS, None) };
    }

    /// One wheel notch up or down the ladder of sizes.
    fn zoom_step(&mut self, up: bool) {
        let (cur, max) = (self.config.zoom, self.max_zoom());
        let next = if up {
            ZOOM_LEVELS.iter().copied().find(|z| *z > cur + 0.001).unwrap_or(max).min(max)
        } else {
            ZOOM_LEVELS.iter().rev().copied().find(|z| *z < cur - 0.001).unwrap_or(MIN_ZOOM)
        };
        self.set_zoom(next);
    }

    fn zoom_settled(&mut self) {
        unsafe { KillTimer(self.hwnd, TIMER_ZOOM) };
        self.config.position = Some(self.window_pos());
        self.save_config();
        let Visual::Media { prep, .. } = &self.visual else { return };
        let want = prep_density(self.char_scale(), self.scale, self.native, self.full_density(&self.shown_id));
        let differs = |p: f32| (want - p).abs() > 0.005;
        if differs(*prep) && self.pending_prep.is_none_or(differs) {
            self.pending_prep = Some(want);
            self.apply_character();
        }
    }

    /// Full screen isn't kept across a restart, so while it is on, what is saved is the size and
    /// spot it goes back to.
    fn save_config(&self) {
        match &self.full_restore {
            Some(r) => Config { zoom: r.zoom, position: Some(r.pos), shift: r.shift, lift: r.lift, ..self.config.clone() }.save(),
            None => self.config.save(),
        }
    }

    /// Full screen and back to the size and spot it had before.
    fn toggle_full(&mut self) {
        if let Some(r) = self.full_restore.take() {
            self.config.zoom = r.zoom;
            self.config.shift = r.shift;
            self.config.lift = r.lift;
            self.fit_window(Anchor::At(r.pos));
        } else {
            self.full_restore = Some(FullRestore { zoom: self.config.zoom, pos: self.window_pos(), shift: self.config.shift, lift: self.config.lift });
            self.config.shift = 0.0;
            self.config.lift = 0.0;
            self.config.zoom = self.full_zoom();
            self.fit_window(Anchor::ScreenBottom);
        }
        self.repaint_if_shown();
        unsafe { SetTimer(self.hwnd, TIMER_ZOOM, ZOOM_SETTLE_MS, None) };
    }

    // ---- skills ----

    fn use_skill(&mut self, s: Skill) {
        // Registering again is harmless: Windows just refuses the ones already there.
        register_skill_keys(self.hwnd, true, &self.config.skill_keys);
        self.combo = true;
        unsafe { SetTimer(self.hwnd, TIMER_COMBO, COMBO_MS, None) };
        match s {
            Skill::Blink => self.blink(),
            Skill::Clones => self.clones(),
            Skill::Ultimate => self.ultimate(),
        }
    }

    fn gap_px(&self) -> i32 {
        (CHAR_GAP * self.scale) as i32
    }

    /// Centre of the character's body on screen, with the window at `window` and the character moved
    /// inside it by `shift` and `lift` pixels.
    fn body_center_at(&self, window: (i32, i32), shift: i32, lift: i32) -> (i32, i32) {
        let (w, h) = self.size();
        let (_, bh) = self.char_size_px();
        (window.0 + w / 2 + shift, window.1 + h - self.gap_px() - lift - bh / 2)
    }

    fn body_center(&self) -> (i32, i32) {
        let (shift, up) = self.offset_px();
        self.body_center_at(self.window_pos(), shift, -up)
    }

    /// Where the window goes, and the character inside it, to stand in each corner of work area `a`.
    fn corners(&self, a: &RECT) -> Vec<DragStep> {
        let (w, h) = self.size();
        let (bw, bh) = self.char_size_px();
        let gap = self.gap_px();
        let xs = [a.left - w / 2 + bw / 2, a.right - w / 2 - bw / 2];
        let ys = [a.top - (h - gap - bh), a.bottom - h];
        ys.iter().flat_map(|&y| xs.iter().map(move |&x| drag_step((x, y), (w, h), (bw, bh), gap, a))).collect()
    }

    /// Each corner of `a`, with where the character's body would be in it.
    fn corner_spots(&self, a: &RECT) -> Vec<(DragStep, (i32, i32))> {
        self.corners(a)
            .into_iter()
            .map(|s| {
                let c = self.body_center_at(s.window, s.shift, s.lift);
                (s, c)
            })
            .collect()
    }

    /// Whether a body centre is about where the character stands now.
    fn is_here(&self, c: (i32, i32)) -> bool {
        let here = self.body_center();
        let (bw, bh) = self.char_size_px();
        (c.0 - here.0).abs().max((c.1 - here.1).abs()) <= bw.max(bh) / 2
    }

    /// The corners of its screen the character isn't already standing in.
    fn other_corners(&self) -> Vec<(DragStep, (i32, i32))> {
        self.corner_spots(&self.area()).into_iter().filter(|(_, c)| !self.is_here(*c)).collect()
    }

    /// A flash `k` times the character's size.
    fn flash_size(&self, k: f32) -> u32 {
        let (bw, bh) = self.char_size_px();
        ((bw.max(bh) as f32 * k) as u32).min(MAX_FLASH_PX)
    }

    fn skill_timer(&self) {
        unsafe { SetTimer(self.hwnd, TIMER_SKILL, SKILL_FRAME_MS, None) };
    }

    fn can_blink(&self) -> bool {
        self.blink.is_none() && self.full_restore.is_none() && self.full_at.is_none() && self.shown()
    }

    /// Q: gone in a flash, and back over a kunai mark in another corner.
    fn blink(&mut self) {
        if !self.can_blink() {
            return;
        }
        let mut far = self.other_corners();
        if far.is_empty() {
            return;
        }
        let (dest, there) = far.swap_remove(now_ms() as usize % far.len());
        self.start_blink(dest, there);
    }

    /// Q towards the pointer: to the corner nearest it, on whichever screen it is, so the buddy turns
    /// up where you are looking. Nothing if it is already there.
    fn blink_to_pointer(&mut self) {
        if !self.can_blink() {
            return;
        }
        let mut p = POINT { x: 0, y: 0 };
        unsafe { GetCursorPos(&mut p) };
        let dist = |c: (i32, i32)| (c.0 - p.x).pow(2) as i64 + (c.1 - p.y).pow(2) as i64;
        let Some((dest, there)) = self.corner_spots(&work_area(p)).into_iter().min_by_key(|(_, c)| dist(*c)) else { return };
        if !self.is_here(there) {
            self.start_blink(dest, there);
        }
    }

    fn start_blink(&mut self, dest: DragStep, there: (i32, i32)) {
        let size = self.flash_size(1.6);
        let (from, to) = (Sprite::around(self.body_center(), size), Sprite::around(there, size));
        self.panel_open = false;
        self.blink = Some(Blink { start: Instant::now(), from, to, dest, jumped: false });
        self.skill_timer();
    }

    /// W: a clone bursts out of smoke in every other corner. Again, and they go.
    fn clones(&mut self) {
        if !self.bunshin.is_empty() {
            self.dismiss_clones();
            return;
        }
        if self.full_restore.is_some() || self.full_at.is_some() || !self.shown() {
            return;
        }
        let (k, dw, dh) = match &self.visual {
            Visual::Media { player, .. } => {
                let k = self.stretch();
                (k, (player.anim.w as f32 * k).ceil() as i32, (player.anim.h as f32 * k).ceil() as i32)
            }
            Visual::Vector(_) => {
                let k = self.char_scale();
                let s = (CHAR_SIZE * k).ceil() as i32;
                (k, s, s)
            }
        };
        // Room around the character for the smoke.
        let (m, gap) = (dw.max(dh) / 4, self.gap_px());
        let (sw, sh) = (dw + 2 * m, dh + m + gap);
        let (_, bh) = self.char_size_px();
        let (now, state) = (Instant::now(), state_index(self.state));
        for (i, (_, c)) in self.other_corners().into_iter().enumerate() {
            let Some(sprite) = Sprite::new(c.0 - sw / 2, c.1 + bh / 2 - (sh - gap), sw as u32, sh as u32) else { continue };
            let body = match &self.visual {
                // Each a little ahead of the next, so they don't all move as one.
                Visual::Media { player, .. } => CloneBody::Frames(Player::new(player.anim.clone(), state, now_ms().saturating_sub(137 * (i as u64 + 1)))),
                Visual::Vector(kind) => CloneBody::Drawn(*kind),
            };
            let born = now + Duration::from_millis(CLONE_STAGGER_MS * i as u64);
            let leave_at = born + Duration::from_millis(CLONE_LIFE_MS);
            let (feet, size) = ((sw as f32 / 2.0, (sh - gap) as f32), (dw as f32, dh as f32));
            self.bunshin.push(Bunshin { sprite, body, k, feet, size, born, leave_at, drawn: false });
        }
        self.skill_timer();
    }

    fn dismiss_clones(&mut self) {
        let now = Instant::now();
        for (i, b) in self.bunshin.iter_mut().enumerate() {
            b.leave_at = b.leave_at.min(now.max(b.born) + Duration::from_millis(CLONE_STAGGER_MS * i as u64));
        }
    }

    /// R: a flash, then full screen. Again, and back.
    fn ultimate(&mut self) {
        if self.full_at.is_some() || self.blink.is_some() {
            return;
        }
        if self.full_restore.is_some() {
            self.toggle_full();
            self.burst(1.8);
        } else {
            self.burst(1.8);
            self.full_at = Some(Instant::now() + Duration::from_millis(ULTIMATE_DELAY_MS));
            // Sharp frames first, while the flash plays.
            if let Visual::Media { prep, .. } = &self.visual {
                let full = self.scale * self.max_zoom();
                let want = prep_density(full, self.scale, self.native, self.full_density(&self.shown_id));
                if (want - prep).abs() > 0.005 {
                    self.full_wait = Some(Instant::now() + Duration::from_millis(FULL_WAIT_MS));
                    self.pending_prep = Some(want);
                    self.apply_character_at(full);
                }
            }
        }
        self.skill_timer();
    }

    fn burst(&mut self, k: f32) {
        if !self.shown() {
            return;
        }
        if let Some(s) = Sprite::around(self.body_center(), self.flash_size(k)) {
            self.bursts.push((s, Instant::now()));
        }
    }

    fn skill_tick(&mut self) {
        let now = Instant::now();
        let ms = |since: Instant| now.saturating_duration_since(since).as_millis() as u64;
        if self.full_at.is_some_and(|t| now >= t) && self.full_wait.is_none_or(|t| now >= t) {
            self.full_at = None;
            self.full_wait = None;
            self.dismiss_clones();
            self.toggle_full();
            self.burst(1.2);
        }
        if let Some(b) = &mut self.blink {
            let t = ms(b.start);
            if let Some(s) = &mut b.from {
                s.draw(|buf, w, h| fx::paint_flash(buf, w, h, t as f32 / FLASH_MS as f32));
            }
            if let Some(s) = &mut b.to {
                s.draw(|buf, w, h| {
                    if t < BLINK_OUT_MS {
                        fx::paint_mark(buf, w, h, t as f32 / BLINK_OUT_MS as f32)
                    } else {
                        fx::paint_flash(buf, w, h, (t - BLINK_OUT_MS) as f32 / FLASH_MS as f32)
                    }
                });
            }
            let jump = (t >= BLINK_OUT_MS && !b.jumped).then(|| {
                b.jumped = true;
                (b.dest.window, b.dest.shift, b.dest.lift)
            });
            let alpha = if t < BLINK_OUT_MS { 1.0 - t as f32 / BLINK_OUT_MS as f32 } else { (t - BLINK_OUT_MS) as f32 / BLINK_IN_MS as f32 };
            let done = t >= BLINK_OUT_MS + FLASH_MS;
            self.alpha = (alpha.clamp(0.0, 1.0) * 255.0) as u8;
            if let Some((pos, shift, lift)) = jump {
                self.move_to(pos.0, pos.1);
                self.config.shift = shift as f32 / self.scale;
                self.config.lift = lift as f32 / self.scale;
                self.repaint();
            }
            if done {
                self.blink = None;
                self.alpha = 255;
                self.config.position = Some(self.window_pos());
                self.save_config();
            }
            self.present(None);
        }
        let (clock, state, t) = (now_ms(), self.state, self.time());
        for b in &mut self.bunshin {
            if now < b.born {
                continue;
            }
            let (age, leaving, gone) = (ms(b.born), now >= b.leave_at, ms(b.leave_at));
            let smoke = if leaving {
                Some(gone as f32 / SMOKE_MS as f32)
            } else {
                (age < SMOKE_MS).then(|| age as f32 / SMOKE_MS as f32)
            };
            let opacity = if leaving { 1.0 - gone as f32 / CLONE_FADE_MS as f32 } else { age.saturating_sub(CLONE_FADE_IN_DELAY_MS) as f32 / CLONE_FADE_MS as f32 }.clamp(0.0, 1.0);
            // A drawn character moves all the time.
            let changed = match &mut b.body {
                CloneBody::Frames(player) => player.advance(clock).is_some(),
                CloneBody::Drawn(_) => true,
            };
            if smoke.is_none() && !changed && b.drawn && opacity >= 1.0 {
                continue;
            }
            b.drawn = true;
            let Bunshin { sprite, body, k, feet, size, .. } = b;
            let (k, feet, size) = (*k, *feet, *size);
            let r = size.0.max(size.1) * 0.42;
            sprite.draw(|buf, w, h| {
                if opacity > 0.0 {
                    match body {
                        CloneBody::Frames(player) => fx::paint_frame(buf, w, h, &player.pixels, player.anim.w, player.anim.h, k, feet, opacity),
                        CloneBody::Drawn(kind) => {
                            render::paint_character_at(buf, w, h, k, *kind, state, t, feet);
                            fx::fade(buf, opacity);
                        }
                    }
                }
                if let Some(p) = smoke {
                    fx::paint_smoke(buf, w, h, feet.0, feet.1 - size.1 * 0.45, r, p);
                }
            });
        }
        self.bunshin.retain(|b| now < b.leave_at || ms(b.leave_at) < SMOKE_MS);
        for (s, start) in &mut self.bursts {
            let p = ms(*start) as f32 / FLASH_MS as f32;
            s.draw(|buf, w, h| fx::paint_flash(buf, w, h, p));
        }
        self.bursts.retain(|(_, start)| ms(*start) < FLASH_MS);
        if self.blink.is_none() && self.bunshin.is_empty() && self.bursts.is_empty() && self.full_at.is_none() {
            unsafe { KillTimer(self.hwnd, TIMER_SKILL) };
        }
    }

    fn auto_skills_on(&self) -> bool {
        self.config.auto_skills.unwrap_or_else(|| self.current().is_some_and(|c| c.auto_skills))
    }

    /// Skills that go off by themselves: clones come to help once the sessions have worked a while,
    /// and vanish when the work stops; finishing is a teleport, and needing you one to the pointer.
    fn auto_skill(&mut self, from: State, to: State) {
        unsafe { KillTimer(self.hwnd, TIMER_AUTO_CLONE) };
        if !self.auto_skills_on() || self.nobody_watching() || self.drag.is_some() {
            return;
        }
        if from == State::Working && !self.bunshin.is_empty() {
            self.dismiss_clones();
        }
        match to {
            State::Working => unsafe {
                SetTimer(self.hwnd, TIMER_AUTO_CLONE, self.config.auto_clone_secs.saturating_mul(1000), None);
            },
            State::Done => self.blink(),
            State::Waiting => self.blink_to_pointer(),
            _ => {}
        }
    }

    fn auto_clones_due(&mut self) {
        unsafe { KillTimer(self.hwnd, TIMER_AUTO_CLONE) };
        if self.state == State::Working && self.bunshin.is_empty() && self.auto_skills_on() && !self.nobody_watching() {
            self.clones();
        }
    }

    /// Hidden: effects and clones go at once, and the buddy is whole again when it is back.
    fn end_skills(&mut self) {
        self.blink = None;
        self.bunshin.clear();
        self.bursts.clear();
        self.full_at = None;
        self.full_wait = None;
        self.alpha = 255;
        unsafe { KillTimer(self.hwnd, TIMER_SKILL) };
    }

    fn bubble_mode(&self) -> Bubble {
        if self.panel_open || self.hovering || now_ms() < self.bubble_until {
            Bubble::All
        } else if self.sessions.iter().any(|s| matches!(s.state, State::Waiting | State::Error)) {
            Bubble::Attention
        } else {
            Bubble::Hidden
        }
    }

    /// New states earn the bubble a few seconds. Steps within a state don't: a long task would keep it up.
    fn note_state_changes(&mut self) {
        let mut states: Vec<(String, State)> = self.sessions.iter().map(|s| (s.id.clone(), s.state)).collect();
        states.sort_by(|a, b| a.0.cmp(&b.0));
        let woke = states.iter().any(|x| x.1 != State::Idle && !self.known_states.contains(x));
        self.known_states = states;
        if woke {
            self.bubble_until = now_ms() + BUBBLE_MS as u64;
            unsafe { SetTimer(self.hwnd, TIMER_BUBBLE, BUBBLE_MS + 50, None) };
        }
    }

    /// Windows' verdict on whether interrupting is OK right now.
    fn notification_state() -> i32 {
        let mut state = 0;
        if unsafe { SHQueryUserNotificationState(&mut state) } != 0 {
            return QUNS_ACCEPTS_NOTIFICATIONS;
        }
        state
    }

    fn check_fullscreen(&mut self) {
        let state = Self::notification_state();
        let busy = matches!(state, QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE);
        let want = busy && self.config.auto_hide;
        if want != self.suppressed {
            self.suppressed = want;
            self.apply_shown();
        }
    }

    fn trim_soon(&self) {
        unsafe { SetTimer(self.hwnd, TIMER_TRIM, TRIM_DELAY_MS, None) };
    }

    /// True when nobody can see it: locked, or the screen is off.
    fn nobody_watching(&self) -> bool {
        // CLAUDE_BUDDY_RS_KEEP_ANIMATING is for measuring animation cost on a machine that is locked.
        (self.session_locked || self.display_off) && std::env::var_os("CLAUDE_BUDDY_RS_KEEP_ANIMATING").is_none()
    }

    fn set_watching(&mut self, session_locked: bool, display_off: bool) {
        let was = self.nobody_watching();
        self.session_locked = session_locked;
        self.display_off = display_off;
        if was != self.nobody_watching() {
            self.update_timer();
            if was {
                self.repaint_if_shown(); // back in the present, not where it stopped
            }
        }
    }

    fn repaint_if_shown(&mut self) {
        if self.shown() {
            self.repaint();
        }
    }

    fn apply_shown(&mut self) {
        if !self.shown() {
            self.end_skills();
        }
        unsafe { ShowWindow(self.hwnd, if self.shown() { SW_SHOWNOACTIVATE } else { SW_HIDE }) };
        self.update_timer();
        if self.shown() {
            self.refresh();
        }
    }

    fn update_tray_state(&mut self) {
        if self.state == self.tray_state {
            return;
        }
        self.tray_state = self.state;
        let icon = make_tray_icon(self.state);
        if icon.is_null() {
            return;
        }
        let old = std::mem::replace(&mut self.tray_icon, icon);
        self.tray.hIcon = icon;
        let mut n = self.tray;
        n.uFlags = NIF_ICON;
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &n);
            DestroyIcon(old);
        }
    }

    fn offset_px(&self) -> (i32, i32) {
        ((self.config.shift * self.scale) as i32, -(self.config.lift * self.scale) as i32)
    }

    /// Pixel size of what counts as the character for reaching screen edges: its visible body, not the
    /// transparent room around it.
    fn char_size_px(&self) -> (i32, i32) {
        match &self.visual {
            Visual::Vector(_) => ((VECTOR_BODY_W * self.char_scale()) as i32, (CHAR_SIZE * self.char_scale()) as i32),
            Visual::Media { room: false, .. } => ((VECTOR_BODY_W * self.char_scale()) as i32, (CHAR_SIZE * self.char_scale()) as i32),
            Visual::Media { player, .. } => {
                let k = self.stretch();
                ((player.anim.w as f32 * k) as i32, (player.anim.h as f32 * k) as i32)
            }
        }
    }

    /// A smaller or larger character may have less travel inside the window than the last one.
    fn clamp_offset(&mut self) {
        let (w, h) = self.size();
        let (cw, ch) = self.char_size_px();
        let gap = (CHAR_GAP * self.scale) as i32;
        let x_limit = ((w - cw) / 2).max(0) as f32 / self.scale;
        let y_limit = (h - gap - ch).max(0) as f32 / self.scale;
        self.config.shift = self.config.shift.clamp(-x_limit, x_limit);
        self.config.lift = self.config.lift.clamp(0.0, y_limit);
    }

    fn drag_to(&mut self, wanted: (i32, i32)) -> (i32, i32) {
        let gap = (CHAR_GAP * self.scale) as i32;
        let cursor = {
            let mut p = POINT { x: 0, y: 0 };
            unsafe { GetCursorPos(&mut p) };
            p
        };
        let step = drag_step(wanted, self.size(), self.char_size_px(), gap, &work_area(cursor));
        let (shift, lift) = (step.shift as f32 / self.scale, step.lift as f32 / self.scale);
        if self.window_pos() != step.window {
            self.move_to(step.window.0, step.window.1);
        }
        if shift != self.config.shift || lift != self.config.lift {
            self.config.shift = shift;
            self.config.lift = lift;
            self.repaint();
        }
        step.virtual_pos
    }

    fn char_region(&self) -> (f32, f32) {
        let f = self.config.zoom;
        match &self.visual {
            Visual::Vector(_) => (CHAR_SIZE * f, CHAR_SIZE * f),
            Visual::Media { room: false, .. } => (CHAR_SIZE * f, CHAR_SIZE * f),
            Visual::Media { player, .. } => {
                let k = self.stretch();
                let win_w = self.canvas.w as f32 / self.scale;
                let (w, h) = render::media_region_in(player.anim.w as f32 * k, player.anim.h as f32 * k, self.char_scale(), win_w / f);
                (w * f, h * f)
            }
        }
    }

    fn current(&self) -> Option<&Character> {
        self.library.iter().find(|c| c.id == self.config.character)
    }

    /// Shows the configured character. Vector ones are instant; media ones load on a worker thread
    /// while the old one keeps animating, then swap in at once.
    fn apply_character(&mut self) {
        self.apply_character_at(self.char_scale());
    }

    /// Same, with frames for `char_scale`, which needn't be the size shown now.
    fn apply_character_at(&mut self, char_scale: f32) {
        self.load_token += 1;
        let Some(c) = self.current().cloned() else {
            self.config.character = "mochi".into();
            self.visual = Visual::Vector(0);
            return;
        };
        self.current_sounds = c.sounds.clone();
        // A built-in shows a still picture right away, and comes alive once its loops are ready. One
        // already alive keeps moving while sharper frames load, and full_density below stays known.
        let alive = matches!(self.visual, Visual::Media { .. }) && self.shown_id == c.id;
        if let (Source::Vector(i), false) = (&c.source, alive) {
            self.visual = Visual::Vector(*i);
            self.after_visual_change();
        }
        let (token, hwnd, scale, full) = (self.load_token, self.hwnd as isize, self.scale, self.full_density(&c.id));
        // Until the character has been seen, nobody knows how big full screen makes it, and sharp
        // frames could come out bigger than any size it is shown at. The usual size it is, then; the
        // settle check after loading fetches the sharp ones.
        let char_scale = if full.is_none() { char_scale.min(scale * SIZE_FACTORS[2]) } else { char_scale };
        let _ = std::thread::Builder::new().name("load-character".into()).spawn(move || {
            let max_h = (anim::BASE_HEIGHT * c.scale).min(anim::MAX_STAGE_HEIGHT);
            let native = anim::native_dpi(&c, anim::MAX_WIDTH, max_h);
            let prep = prep_density(char_scale, scale, native, full);
            let result = anim::load(&c, anim::MAX_WIDTH, max_h, prep);
            *LOADED.lock().unwrap() = Some(Loaded { token, id: c.id, result, prep, native });
            unsafe { PostMessageW(hwnd as HWND, WM_ANIM_READY, 0, 0) };
        });
    }

    fn on_loaded(&mut self) {
        let Some(l) = LOADED.lock().unwrap().take() else { return };
        if l.token != self.load_token {
            return; // a newer choice won
        }
        self.pending_prep = None;
        self.full_wait = None;
        match l.result {
            Ok(a) => {
                let Some(c) = self.library.iter().find(|c| c.id == l.id) else { return };
                let states = Box::new(std::array::from_fn(|i| (c.states[i].motion, (c.states[i].fallback || a.failed[i]) && i != 0)));
                let room = !c.is_builtin();
                self.visual = Visual::Media { player: Player::new(a, state_index(self.state), now_ms()), states, room, prep: l.prep };
                self.native = l.native;
                self.shown_id = l.id;
                self.after_visual_change();
                // Now that its size is known, the settle check fetches sharper frames if the ones just
                // loaded are not the best for the size shown.
                if self.char_scale() > self.scale * SIZE_FACTORS[2] + 0.001 {
                    unsafe { SetTimer(self.hwnd, TIMER_ZOOM, ZOOM_SETTLE_MS, None) };
                }
            }
            // A built-in that couldn't prepare (no disk space, say) simply stays a still picture.
            Err(_) if matches!(self.visual, Visual::Vector(_)) && self.current().is_some_and(Character::is_builtin) => {}
            Err(e) => {
                let name = self.current().map_or(l.id.clone(), |c| c.name.clone());
                self.balloon(&crate::tf!("Không tải được {name}", "Could not load {name}"), &e);
                // What is on screen stays, so the choice on record must match it.
                if let Visual::Vector(i) = self.visual {
                    self.config.character = self.library.iter().find(|c| c.source == Source::Vector(i)).map_or("mochi".into(), |c| c.id.clone());
                }
            }
        }
    }

    fn after_visual_change(&mut self) {
        // Clones of the last character, or of another size, would no longer match.
        self.bunshin.clear();
        // Another character fills the screen at another zoom, and may not fit at this one.
        self.config.zoom = if self.full_restore.is_some() { self.full_zoom() } else { self.config.zoom.min(self.max_zoom()) };
        self.fit_window(Anchor::Keep);
        self.clamp_offset();
        self.trim_soon();
        self.frame_ms = 0;
        self.update_timer();
        if self.shown() {
            self.repaint();
        }
    }

    fn select_character(&mut self, id: &str) -> bool {
        if !self.library.iter().any(|c| c.id == id) {
            return false;
        }
        self.config.character = id.to_string();
        self.save_config();
        self.apply_character();
        true
    }

    fn reload_library(&mut self) {
        self.library = library::load_all(&user_root());
    }

    /// Full repaint, for when sessions or the panel changed.
    fn repaint(&mut self) {
        let (w, h, scale) = (self.canvas.w, self.canvas.h, self.scale);
        let note = self.shared.note.lock().unwrap().clone();
        let usage_line = self.shared.scanned.load(Ordering::Acquire).then(|| usage::summary_line(&self.shared.usage.lock().unwrap(), usage::today()));
        let view = View {
            sessions: &self.sessions,
            panel_open: self.panel_open,
            hover: self.hover.as_deref(),
            note: note.as_deref(),
            now: now_ms(),
            char_region: self.char_region(),
            char_off: (self.config.shift, -self.config.lift),
            usage: usage_line.as_deref(),
            bubble: self.bubble_mode(),
        };
        // Raw parts because `view` borrows other fields than the canvas while it is being written.
        let buf = unsafe { std::slice::from_raw_parts_mut(self.canvas.bits, (w * h * 4) as usize) };
        self.hits = render::paint_ui(buf, w, h, scale, &self.text, &view);
        self.draw_character(true);
        self.present(None);
    }

    fn frame(&mut self) {
        if self.shown() {
            if let Some(dirty) = self.draw_character(false) {
                self.present(Some(dirty));
            }
        }
    }

    fn update_timer(&mut self) {
        let want = match &self.visual {
            Visual::Vector(_) => 0,
            Visual::Media { player, states, .. } => {
                let (motion, badge) = states[state_index(self.state)];
                if motion != Motion::None || badge {
                    ACTIVE_FRAME_MS
                } else if player.clip().is_still() {
                    0 // a still picture costs nothing at all until something changes
                } else {
                    player.clip().min_delay().max(ACTIVE_FRAME_MS)
                }
            }
        };
        if !self.shown() || want == 0 || self.nobody_watching() {
            unsafe { KillTimer(self.hwnd, TIMER_FRAME) };
            self.frame_ms = 0;
        } else if self.frame_ms != want {
            unsafe { SetTimer(self.hwnd, TIMER_FRAME, want, None) };
            self.frame_ms = want;
        }
    }

    fn refresh(&mut self) {
        self.sessions = self.shared.store.lock().unwrap().list();
        let state = store::overall(&self.sessions);
        if state != self.state {
            self.auto_skill(self.state, state);
            self.state = state;
            self.state_since = Instant::now();
            if let Visual::Media { player, .. } = &mut self.visual {
                player.set_state(state_index(state), now_ms());
            }
            for b in &mut self.bunshin {
                if let CloneBody::Frames(player) = &mut b.body {
                    player.set_state(state_index(state), now_ms());
                }
            }
        }
        self.note_state_changes();
        self.update_timer();
        self.update_tip();
        self.update_tray_state();
        if self.shown() {
            self.repaint();
        }
    }

    fn announce(&mut self) {
        let pending = std::mem::take(&mut *self.shared.pending.lock().unwrap());
        // A finished turn is when new tokens have landed in the transcripts.
        if pending.iter().any(|t| t.to == State::Done) {
            unsafe { SetTimer(self.hwnd, TIMER_SCAN, SCAN_DELAY_MS, None) };
        }
        // Full screen, presenting or Focus Assist: a sound or popup now would interrupt. The state still
        // shows in the tray, and on the buddy once it is back.
        if Self::notification_state() != QUNS_ACCEPTS_NOTIFICATIONS {
            return;
        }
        for t in pending.iter().filter(|t| t.notify) {
            if focus::looking_at(&t.session) {
                continue; // already in front of you, in that session's own window
            }
            if !self.config.muted {
                let own = match t.to {
                    State::Done => self.current_sounds[0].as_ref(),
                    State::Waiting => self.current_sounds[1].as_ref(),
                    _ => None,
                };
                match own {
                    Some(path) => sound::play_file(path),
                    None => sound::play(t.to),
                }
            }
            if self.config.toasts {
                let (title, body) = crate::store::toast_text(&t.session, t.to);
                let icon = make_icon(t.to, BALLOON_ICON_SIZE);
                self.balloon_with_icon(&title, &body, icon);
            }
        }
    }

    fn balloon(&mut self, title: &str, body: &str) {
        let icon = make_icon(State::Idle, BALLOON_ICON_SIZE);
        self.balloon_with_icon(title, body, icon);
    }

    fn balloon_with_icon(&mut self, title: &str, body: &str, icon: HICON) {
        let mut n = self.tray;
        n.uFlags = NIF_INFO;
        // Windows drops a balloon with empty text.
        copy_wide(&mut n.szInfo, if body.is_empty() { " " } else { body });
        copy_wide(&mut n.szInfoTitle, title);
        n.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON | NIIF_NOSOUND;
        n.hBalloonIcon = icon;
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &n);
            // Windows has made its own copy by now.
            if !icon.is_null() {
                DestroyIcon(icon);
            }
        }
    }

    fn update_tip(&mut self) {
        let tip = match self.sessions.first() {
            Some(s) if s.state != State::Idle => format!("Claude Buddy · {}: {}", name_of(s), status_of(s)),
            _ => crate::tf!("Claude Buddy · {} phiên", "Claude Buddy · {} sessions", self.sessions.len()),
        };
        copy_wide(&mut self.tray.szTip, &tip);
        let mut n = self.tray;
        n.uFlags = NIF_TIP;
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
    }

    fn add_tray(&mut self) {
        let mut n = self.tray;
        n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &n);
            n.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &n);
        }
    }

    fn set_visible(&mut self, on: bool) {
        self.visible = on;
        self.apply_shown();
    }

    fn hit_at(&self, x: i32, y: i32) -> Option<Hit> {
        self.hits.iter().find(|r| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h).map(|r| r.hit.clone())
    }

    fn jump_to(&mut self, id: &str) {
        let session = {
            let mut st = self.shared.store.lock().unwrap();
            let s = st.get(id).cloned();
            if s.as_ref().is_some_and(|s| s.state.needs_attention()) {
                st.acknowledge(id);
            }
            s
        };
        self.refresh();
        let Some(s) = session else { return };
        if let Some(url) = &s.url {
            let u = wide(url);
            unsafe { ShellExecuteW(std::ptr::null_mut(), w!("open"), u.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
            return;
        }
        // Enumerating processes and windows can take a moment: never on the UI thread.
        let shared = self.shared.clone();
        let hwnd = self.hwnd as isize;
        let _ = std::thread::Builder::new().stack_size(256 * 1024).spawn(move || {
            if !focus::focus_session(&s) {
                *shared.note.lock().unwrap() = Some(crate::tf!("Không tìm thấy cửa sổ của {}", "Could not find the window of {}", name_of(&s)));
                wake(hwnd);
            }
        });
        self.note_until = now_ms() + 5_000;
    }

    fn on_click(&mut self, hit: Option<Hit>) {
        match hit {
            Some(Hit::Character) => {
                self.panel_open = !self.panel_open;
                self.repaint();
            }
            Some(Hit::Bubble) => {
                if let Some(id) = self.sessions.iter().find(|s| s.state != State::Idle).map(|s| s.id.clone()) {
                    self.jump_to(&id);
                }
            }
            Some(Hit::Row(id)) => self.jump_to(&id),
            None => {}
        }
    }

    /// Going by the real foreground window: a window can be told it's active while another app stays
    /// in front, and the buddy must not stay buried behind that app. Checked again on housekeeping
    /// because a switch isn't always announced.
    fn sync_z_order(&mut self) {
        unsafe {
            let picker = picker::window();
            let in_use = !picker.is_null() && GetForegroundWindow() == picker;
            if in_use != self.behind_picker {
                self.behind_picker = in_use;
                let after = if in_use { picker } else { HWND_TOPMOST };
                SetWindowPos(self.hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            } else if !in_use && normal_window_above(self.hwnd) {
                SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }

    fn housekeeping(&mut self) {
        let now = now_ms();
        self.shared.store.lock().unwrap().persist();
        if crate::update::ready() && picker::window().is_null() && self.drag.is_none() && crate::update::apply() {
            unsafe { DestroyWindow(self.hwnd) };
            return;
        }
        self.check_fullscreen();
        self.sync_z_order();
        // Also catches turns that never send Stop, like interrupted ones.
        self.housekeeping_ticks = self.housekeeping_ticks.wrapping_add(1);
        if self.housekeeping_ticks % USAGE_SCAN_TICKS == 0 {
            usage::request_scan(self.shared.clone());
        }
        let running = self.shared.store.lock().unwrap().running_pids();
        let dead: Vec<String> = running.into_iter().filter(|(_, p)| !process_alive(*p)).map(|(id, _)| id).collect();
        let mut changed = {
            let mut st = self.shared.store.lock().unwrap();
            let mut pending = self.shared.pending.lock().unwrap();
            for id in &dead {
                if let Some(t) = st.fail(id, t("Tiến trình Claude đã dừng", "The Claude process stopped"), now) {
                    pending.push(t);
                }
            }
            let ticked = st.tick(now) | st.forget_closed(process_alive);
            !dead.is_empty() || ticked
        };
        if self.note_until != 0 && now >= self.note_until {
            self.note_until = 0;
            *self.shared.note.lock().unwrap() = None;
            changed = true;
        }
        if changed {
            self.announce();
            self.refresh();
        } else if self.panel_open && self.shown() {
            // "3 phút" labels age on their own.
            self.repaint();
        }
    }

    fn apply_dpi(&mut self, dpi: u32) {
        let scale = dpi as f32 / 96.0;
        if (scale - self.scale).abs() < 0.001 {
            return;
        }
        self.scale = scale;
        self.text = TextDrawer::new(scale);
        // Media frames are prepared for one pixel density, so sharp ones are fetched for the new one.
        if matches!(self.visual, Visual::Media { .. }) {
            self.apply_character();
        }
        let pos = self.window_pos();
        self.fit_window(Anchor::At(pos));
        self.repaint();
    }

    fn set_hooks(&mut self, install: bool) {
        match hooks_config::apply(install, &exe_path(), self.config.port) {
            Ok(_) if install => self.balloon(t("Đã kết nối Claude Code", "Connected to Claude Code"), t("Các phiên Claude Code mới sẽ hiện ở đây.", "New Claude Code sessions will show up here.")),
            Ok(_) => self.balloon(t("Đã ngắt kết nối Claude Code", "Disconnected from Claude Code"), t("Đã gỡ hook của app khỏi settings.json.", "The app's hooks were removed from settings.json.")),
            Err(e) => {
                message_box(&crate::tf!("Không ghi được settings.json của Claude Code:\n{e}", "Could not write Claude Code's settings.json:\n{e}"), MB_OK | MB_ICONERROR);
            }
        }
    }

    fn on_command(&mut self, cmd: usize) {
        match cmd {
            CMD_TOGGLE => self.set_visible(!self.visible),
            CMD_FULL => self.use_skill(Skill::Ultimate),
            CMD_BLINK => self.use_skill(Skill::Blink),
            CMD_CLONES => self.use_skill(Skill::Clones),
            CMD_AUTO_SKILLS => {
                self.config.auto_skills = Some(!self.auto_skills_on());
                self.save_config();
                // Switched on mid-task, the clones count from now.
                unsafe { KillTimer(self.hwnd, TIMER_AUTO_CLONE) };
                if self.auto_skills_on() && self.state == State::Working {
                    unsafe { SetTimer(self.hwnd, TIMER_AUTO_CLONE, self.config.auto_clone_secs.saturating_mul(1000), None) };
                }
            }
            CMD_MUTE => {
                self.config.muted = !self.config.muted;
                self.save_config();
            }
            CMD_TOASTS => {
                self.config.toasts = !self.config.toasts;
                self.save_config();
            }
            CMD_HOOKS => {
                let installed = hooks_config::status(&exe_path(), self.config.port);
                self.set_hooks(!installed);
            }
            CMD_AUTOSTART => set_autostart(!autostart_enabled()),
            CMD_RESET_POS => {
                let (x, y) = self.default_position();
                self.move_to(x, y);
                self.config.position = Some((x, y));
                self.config.shift = 0.0;
                self.config.lift = 0.0;
                self.save_config();
                self.repaint();
            }
            CMD_EXIT => {
                unsafe { DestroyWindow(self.hwnd) };
            }
            CMD_PICKER => {
                // Opened once this borrow has ended: creating a window delivers messages right away.
                unsafe { PostMessageW(self.hwnd, WM_OPEN_PICKER, 0, 0) };
            }
            CMD_USAGE => {
                unsafe { PostMessageW(self.hwnd, WM_OPEN_PICKER, 1, 0) };
            }
            CMD_SETTINGS => {
                unsafe { PostMessageW(self.hwnd, WM_OPEN_PICKER, 2, 0) };
            }
            CMD_USER_FOLDER => {
                let dir = user_root();
                let _ = std::fs::create_dir_all(&dir);
                picker::open_folder(&dir);
            }
            c if c >= CMD_CHARACTER => {
                if let Some(id) = self.library.get(c - CMD_CHARACTER).map(|x| x.id.clone()) {
                    self.select_character(&id);
                    picker::notify_changed();
                }
            }
            _ => {}
        }
    }

    fn build_menu(&self) -> HMENU {
        unsafe {
            let m = CreatePopupMenu();
            let add = |menu: HMENU, id: usize, label: &str, checked: bool| {
                let l = wide(label);
                AppendMenuW(menu, MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED }, id, l.as_ptr());
            };
            add(m, CMD_TOGGLE, if self.visible { t("Ẩn Buddy", "Hide Buddy") } else { t("Hiện Buddy", "Show Buddy") }, false);
            // The keys as they work with the pointer on the buddy; anywhere else, with Alt too.
            let [kq, kw, kr] = self.config.skill_keys.map(key_hint);
            add(m, CMD_BLINK, &(t("Phi Lôi Thần", "Flying Thunder God").to_string() + &kq), false);
            add(m, CMD_CLONES, &(t("Ảnh phân thân", "Shadow clones").to_string() + &kw), !self.bunshin.is_empty());
            add(m, CMD_FULL, &(t("Phóng to", "Make it big").to_string() + &kr), self.full_restore.is_some());
            add(m, CMD_AUTO_SKILLS, t("Tự dùng skill theo phiên", "Skills follow the sessions"), self.auto_skills_on());
            AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
            let chars = CreatePopupMenu();
            for (i, c) in self.library.iter().enumerate() {
                add(chars, CMD_CHARACTER + i, &c.name, c.id == self.config.character);
            }
            let change = wide(t("Đổi nhân vật", "Change character"));
            AppendMenuW(m, MF_POPUP, chars as usize, change.as_ptr());
            add(m, CMD_PICKER, t("Quản lý nhân vật…", "Manage characters…"), false);
            add(m, CMD_USAGE, t("Xem mức dùng…", "View usage…"), false);
            add(m, CMD_SETTINGS, t("Cài đặt…", "Settings…"), false);
            AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
            add(m, CMD_MUTE, t("Tắt tiếng", "Mute"), self.config.muted);
            AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
            add(m, CMD_EXIT, t("Thoát", "Quit"), false);
            m
        }
    }
}

/// Runs outside any app borrow: the menu's modal loop keeps delivering timer messages meanwhile.
fn show_menu(hwnd: HWND) {
    let Some(menu) = with_app(|a| a.build_menu()) else { return };
    let cmd = unsafe {
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        // A menu only closes on clicking elsewhere if its owner is the foreground window, which a
        // no-activate window can't be. So that rule is lifted for just as long as the menu is up...
        let was_working_in = GetForegroundWindow();
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !(WS_EX_NOACTIVATE as isize));
        SetForegroundWindow(hwnd);
        let c = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
        // ...and the keyboard goes back to whatever you were typing in, unless the choice opens a
        // window, which is where you want to be next. Posted, because the menu is still closing at
        // this point and would take the focus straight back.
        let opens_window = matches!(c as usize, CMD_PICKER | CMD_USAGE | CMD_SETTINGS | CMD_USER_FOLDER);
        if !opens_window && !was_working_in.is_null() && was_working_in != hwnd {
            PostMessageW(hwnd, WM_RESTORE_FOCUS, was_working_in as WPARAM, 0);
        }
        c
    };
    if cmd != 0 {
        with_app(|a| a.on_command(cmd as usize));
    }
}

fn first_run_question() {
    let answer = message_box(
        t("Kết nối với Claude Code để Buddy hiện trạng thái các phiên?\n\nApp sẽ thêm hook của riêng nó vào settings.json của Claude Code (có sao lưu trước, hook khác giữ nguyên). Có thể gỡ bất cứ lúc nào từ menu khay.", "Connect to Claude Code so Buddy can show your sessions?\n\nThe app adds its own hooks to Claude Code's settings.json (after a backup; other hooks stay as they are). You can remove them at any time from the tray menu."),
        MB_YESNO | MB_ICONQUESTION,
    );
    with_app(|a| {
        a.config.asked_hooks = true;
        a.save_config();
        if answer == IDYES {
            a.set_hooks(true);
        }
    });
}

fn lparam_point(l: LPARAM) -> (i32, i32) {
    ((l & 0xffff) as i16 as i32, ((l >> 16) & 0xffff) as i16 as i32)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            match wp {
                TIMER_FRAME => with_app(|a| a.frame()),
                TIMER_HOUSEKEEPING => with_app(|a| a.housekeeping()),
                TIMER_TRIM => with_app(|_| {
                    // Startup and one-off work leave pages behind that the app won't touch again.
                    KillTimer(hwnd, TIMER_TRIM);
                    K32EmptyWorkingSet(GetCurrentProcess());
                }),
                TIMER_SCAN => with_app(|a| {
                    KillTimer(hwnd, TIMER_SCAN);
                    usage::request_scan(a.shared.clone());
                }),
                TIMER_ZOOM => with_app(|a| a.zoom_settled()),
                TIMER_SKILL => with_app(|a| a.skill_tick()),
                TIMER_AUTO_CLONE => with_app(|a| a.auto_clones_due()),
                TIMER_COMBO => with_app(|a| {
                    KillTimer(hwnd, TIMER_COMBO);
                    a.combo = false;
                    if !a.tracking_leave {
                        unregister_skill_keys(hwnd, true);
                    }
                }),
                TIMER_BUBBLE => with_app(|a| {
                    KillTimer(hwnd, TIMER_BUBBLE);
                    a.repaint_if_shown();
                }),
                TIMER_HOVER => with_app(|a| {
                    KillTimer(hwnd, TIMER_HOVER);
                    a.hovering = false;
                    a.repaint_if_shown();
                }),
                TIMER_PANEL => with_app(|a| {
                    KillTimer(hwnd, TIMER_PANEL);
                    if a.panel_open {
                        a.panel_open = false;
                        a.repaint_if_shown();
                    }
                }),
                _ => None,
            };
            0
        }
        WM_WAKE => {
            with_app(|a| {
                a.announce();
                a.refresh();
            });
            0
        }
        WM_LBUTTONDOWN => {
            let (x, y) = lparam_point(lp);
            with_app(|a| {
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                let (wx, wy) = a.window_pos();
                let (ox, oy) = a.offset_px();
                a.drag = Some(Drag { start_cursor: p, last_cursor: p, virtual_pos: (wx + ox, wy + oy), moved: false, hit: a.hit_at(x, y) });
            });
            SetCapture(hwnd);
            0
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lp);
            with_app(|a| {
                if !a.tracking_leave {
                    let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    TrackMouseEvent(&mut t);
                    a.tracking_leave = true;
                    register_skill_keys(hwnd, true, &a.config.skill_keys);
                }
                if let Some(d) = &mut a.drag {
                    let mut p = POINT { x: 0, y: 0 };
                    GetCursorPos(&mut p);
                    let (dx, dy) = (p.x - d.start_cursor.x, p.y - d.start_cursor.y);
                    if d.moved || dx.abs() > DRAG_THRESHOLD || dy.abs() > DRAG_THRESHOLD {
                        d.moved = true;
                        let wanted = (d.virtual_pos.0 + p.x - d.last_cursor.x, d.virtual_pos.1 + p.y - d.last_cursor.y);
                        d.last_cursor = p;
                        let next = a.drag_to(wanted);
                        if let Some(d) = &mut a.drag {
                            d.virtual_pos = next;
                        }
                    }
                } else {
                    KillTimer(hwnd, TIMER_HOVER);
                    KillTimer(hwnd, TIMER_PANEL);
                    let hit = a.hit_at(x, y);
                    let was_hovering = std::mem::replace(&mut a.hovering, hit.is_some());
                    let hover = match hit {
                        Some(Hit::Row(id)) => Some(id),
                        _ => None,
                    };
                    if hover != a.hover || was_hovering != a.hovering {
                        a.hover = hover;
                        a.repaint();
                    }
                }
            });
            0
        }
        WM_MOUSELEAVE => {
            with_app(|a| {
                if !a.combo {
                    unregister_skill_keys(hwnd, true);
                }
                a.tracking_leave = false;
                SetTimer(hwnd, TIMER_HOVER, HOVER_GRACE_MS, None);
                if a.panel_open {
                    SetTimer(hwnd, TIMER_PANEL, PANEL_GRACE_MS, None);
                }
                if a.hover.take().is_some() {
                    a.repaint();
                }
            });
            0
        }
        WM_LBUTTONUP => {
            // Taken first: ReleaseCapture delivers WM_CAPTURECHANGED right away, which cancels a drag.
            let drag = with_app(|a| a.drag.take()).flatten();
            ReleaseCapture();
            with_app(|a| {
                let Some(d) = drag else { return };
                if d.moved {
                    a.config.position = Some(a.window_pos());
                    a.save_config();
                } else {
                    a.on_click(d.hit);
                }
            });
            0
        }
        WM_CAPTURECHANGED => {
            with_app(|a| a.drag = None);
            0
        }
        WM_RBUTTONUP => {
            show_menu(hwnd);
            0
        }
        WM_MOUSEWHEEL => {
            // Windows sends the wheel to the window under the pointer, focused or not. Only over the
            // character itself, so scrolling through the bubble or panel never resizes anything.
            let delta = ((wp >> 16) & 0xffff) as i16 as i32;
            let (sx, sy) = lparam_point(lp);
            with_app(|a| {
                let mut p = POINT { x: sx, y: sy };
                ScreenToClient(hwnd, &mut p);
                if a.hit_at(p.x, p.y) != Some(Hit::Character) {
                    a.wheel_acc = 0;
                    return;
                }
                a.wheel_acc += delta;
                while a.wheel_acc.abs() >= WHEEL_DELTA as i32 {
                    let up = a.wheel_acc > 0;
                    a.wheel_acc -= if up { WHEEL_DELTA as i32 } else { -(WHEEL_DELTA as i32) };
                    a.zoom_step(up);
                }
            });
            0
        }
        WM_HOTKEY => {
            if let Some(s) = skill_of_hotkey(wp as i32) {
                with_app(|a| a.use_skill(s));
            }
            0
        }
        WM_TRAY => {
            // With NOTIFYICON_VERSION_4 the low word of lParam carries the event.
            match (lp & 0xffff) as u32 {
                WM_CONTEXTMENU | WM_RBUTTONUP => show_menu(hwnd),
                // As with any Windows tray app, a click opens the app; hiding the buddy is in the menu.
                NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP => picker::open(0),
                _ => {}
            }
            0
        }
        WM_DPICHANGED => {
            // Posted: this can arrive in the middle of a drag's SetWindowPos, while the app is in use.
            PostMessageW(hwnd, WM_DPI, (wp & 0xffff) as WPARAM, 0);
            0
        }
        WM_DPI => {
            with_app(|a| a.apply_dpi(wp as u32));
            0
        }
        WM_FIRST_RUN => {
            first_run_question();
            0
        }
        WM_WTSSESSION_CHANGE => {
            match wp as u32 {
                WTS_SESSION_LOCK => {
                    with_app(|a| a.set_watching(true, a.display_off));
                }
                WTS_SESSION_UNLOCK => {
                    with_app(|a| a.set_watching(false, a.display_off));
                }
                _ => {}
            }
            0
        }
        WM_POWERBROADCAST => {
            if wp as u32 == PBT_POWERSETTINGCHANGE {
                let s = &*(lp as *const POWERBROADCAST_SETTING);
                if s.PowerSetting.data1 == GUID_SESSION_DISPLAY_STATUS.data1 {
                    let off = s.Data[0] == 0;
                    with_app(|a| a.set_watching(a.session_locked, off));
                }
            }
            1
        }
        WM_SETTINGCHANGE => {
            // Light/dark or accent changed, among others: cheap enough to just look again.
            crate::theme::reload();
            with_app(|a| {
                if a.shown() {
                    a.repaint();
                }
            });
            0
        }
        WM_ANIM_READY => {
            with_app(|a| a.on_loaded());
            0
        }
        WM_RESTORE_FOCUS => {
            SetForegroundWindow(wp as HWND);
            0
        }
        WM_OPEN_PICKER => {
            picker::open(wp as u8);
            0
        }
        WM_COPYDATA => {
            let data = &*(lp as *const COPYDATASTRUCT);
            if data.dwData != COPYDATA_PACK || data.lpData.is_null() || data.cbData % 2 != 0 {
                return 0;
            }
            let text = std::slice::from_raw_parts(data.lpData as *const u16, data.cbData as usize / 2);
            let path = PathBuf::from(OsString::from_wide(text));
            if !picker::is_pack(&path) {
                return 0;
            }
            // Posted: the other copy is waiting for this to return, and importing can show a dialog.
            PENDING_PACKS.lock().unwrap().push(path);
            PostMessageW(hwnd, WM_IMPORT_PACKS, 0, 0);
            1
        }
        WM_IMPORT_PACKS => {
            let packs = std::mem::take(&mut *PENDING_PACKS.lock().unwrap());
            for pack in &packs {
                picker::import_pack(pack);
            }
            0
        }
        WM_DESTROY => {
            unregister_skill_keys(hwnd, false);
            unregister_skill_keys(hwnd, true);
            with_app(|a| {
                a.end_skills();
                let mut n = a.tray;
                n.uFlags = 0;
                Shell_NotifyIconW(NIM_DELETE, &n);
                DestroyIcon(a.tray_icon);
            });
            PostQuitMessage(0);
            0
        }
        _ => {
            if with_app(|a| msg == a.taskbar_created).unwrap_or(false) {
                // Explorer restarted: tray icons have to be added again.
                with_app(|a| a.add_tray());
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

/// Says what happened and where, in words, for a Windows notification.
/// The logo at tray size, with a dot saying what the sessions are doing. Lets you check on Claude with
/// a glance at the taskbar, with nothing covering your work.
fn make_tray_icon(state: State) -> HICON {
    make_icon(state, unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32)
}

fn make_icon(state: State, size: u32) -> HICON {
    let status = (state != State::Idle).then(|| render::state_color(state));
    unsafe {
        let pixels = render::paint_icon(size, status);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = size as i32;
        info.bmiHeader.biHeight = -(size as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(std::ptr::null_mut(), &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if color.is_null() || bits.is_null() {
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, std::ptr::null());
        let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

/// A .buddy file opened from Explorer arrives as the first argument.
fn pack_argument() -> Option<PathBuf> {
    std::env::args_os().nth(1).map(PathBuf::from).filter(|p| picker::is_pack(p))
}

static PENDING_PACKS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// A copy with its own data folder is a separate install, so its window has to be told apart: a file must
/// reach the copy it was meant for, and never add a character to another one's data.
fn instance_title() -> Vec<u16> {
    match std::env::var("CLAUDE_BUDDY_RS_HOME") {
        Ok(home) => wide(&format!("Claude Buddy {home}")),
        Err(_) => wide("Claude Buddy"),
    }
}

/// Hands a file to the copy that's already running, and lets it come to the front to show the result.
unsafe fn forward_pack(path: &Path) {
    let target = FindWindowW(w!("ClaudeBuddyRS"), instance_title().as_ptr());
    if target.is_null() {
        return;
    }
    let text: Vec<u16> = path.as_os_str().encode_wide().collect();
    let mut owner = 0;
    GetWindowThreadProcessId(target, &mut owner);
    AllowSetForegroundWindow(owner);
    let data = COPYDATASTRUCT { dwData: COPYDATA_PACK, cbData: (text.len() * 2) as u32, lpData: text.as_ptr() as *mut _ };
    let mut result = 0;
    SendMessageTimeoutW(target, WM_COPYDATA, 0, &data as *const _ as LPARAM, SMTO_ABORTIFHUNG, 5000, &mut result);
}

pub fn run() {
    unsafe {
        // The file and folder pickers are COM dialogs.
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        crate::theme::reload();
        // One instance per user: a second copy would only fail to bind the port. A copy with its own
        // data folder is a separate install with its own port, and may run alongside.
        let mutex_name = match std::env::var("CLAUDE_BUDDY_RS_HOME") {
            Ok(home) => wide(&format!("Local\\ClaudeBuddyRS-{}", home.replace(['\\', '/', ':'], "_"))),
            Err(_) => wide("Local\\ClaudeBuddyRS"),
        };
        CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Some(pack) = pack_argument() {
                forward_pack(&pack);
            }
            return;
        }
    }
    let config = Config::load();
    theme::set_translucent(config.translucent);
    crate::update::start(config.auto_update);
    let shared = Arc::new(Shared::default());
    shared.store.lock().unwrap().restore(now_ms(), process_alive);
    if let Err(e) = server::start(shared.clone(), config.port) {
        message_box(&crate::tf!("Không mở được cổng {}: {e}\nĐổi \"port\" trong config.json của app.", "Could not open port {}: {e}\nChange \"port\" in the app's config.json.", config.port), MB_OK | MB_ICONERROR);
        return;
    }

    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = w!("ClaudeBuddyRS");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class;
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        wc.hIcon = LoadIconW(instance, 1 as *const u16);
        RegisterClassW(&wc);
        let mut fx: WNDCLASSW = std::mem::zeroed();
        fx.lpfnWndProc = Some(fx_proc);
        fx.hInstance = instance;
        fx.lpszClassName = FX_CLASS;
        RegisterClassW(&fx);

        // WS_EX_NOACTIVATE: clicking the buddy must not steal focus from what you are typing in.
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class,
            instance_title().as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            message_box(t("Không tạo được cửa sổ.", "Could not create the window."), MB_OK | MB_ICONERROR);
            return;
        }

        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        let (w, h) = ((WIN_W * scale).ceil() as u32, (WIN_H * scale).ceil() as u32);
        let Some(canvas) = Canvas::new(w, h) else {
            message_box(t("Không đủ bộ nhớ đồ hoạ.", "Not enough graphics memory."), MB_OK | MB_ICONERROR);
            return;
        };

        let tray_icon = make_tray_icon(State::Idle);
        let mut tray: NOTIFYICONDATAW = std::mem::zeroed();
        tray.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        tray.hWnd = hwnd;
        tray.uID = 1;
        tray.uCallbackMessage = WM_TRAY;
        tray.hIcon = tray_icon;
        copy_wide(&mut tray.szTip, "Claude Buddy");

        // Whoever ran the install command already said yes.
        let ask_hooks = !config.asked_hooks && !hooks_config::status(&exe_path(), config.port);
        let app = App {
            hwnd,
            shared: shared.clone(),
            config,
            canvas,
            text: TextDrawer::new(scale),
            scale,
            hits: Vec::new(),
            state: State::Idle,
            library: library::load_all(&user_root()),
            housekeeping_ticks: 0,
            behind_picker: false,
            hovering: false,
            bubble_until: 0,
            known_states: Vec::new(),
            suppressed: false,
            tray_state: State::Idle,
            session_locked: false,
            display_off: false,
            visual: Visual::Vector(0),
            current_sounds: [None, None],
            load_token: 0,
            native: None,
            shown_id: String::new(),
            pending_prep: None,
            full_restore: None,
            wheel_acc: 0,
            alpha: 255,
            blink: None,
            bunshin: Vec::new(),
            bursts: Vec::new(),
            full_at: None,
            full_wait: None,
            combo: false,
            state_since: Instant::now(),
            sessions: Vec::new(),
            panel_open: false,
            hover: None,
            tracking_leave: false,
            visible: true,
            drag: None,
            started: Instant::now(),
            frame_ms: 0,
            note_until: 0,
            tray,
            tray_icon,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        };
        APP.with(|a| *a.borrow_mut() = Some(app));

        with_app(|a| {
            // The saved spot is that of a window the zoomed size, so the window has that size first.
            let (w, h) = a.wanted_size();
            if (w, h) != (a.canvas.w, a.canvas.h) {
                if let Some(c) = Canvas::new(w, h) {
                    a.canvas = c;
                }
            }
            let (w, h) = a.size();
            let (x, y) = match a.config.position {
                Some((x, y)) => clamp_to(&work_area(POINT { x: x + w / 2, y: y + h / 2 }), x, y, w, h),
                None => a.default_position(),
            };
            SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
            a.add_tray();
            a.apply_character();
            a.refresh();
            register_skill_keys(hwnd, false, &a.config.skill_keys);
        });
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(hwnd, TIMER_HOUSEKEEPING, HOUSEKEEPING_MS, None);
        // Alt+Tab and the taskbar's previews hide every other window for a moment, which would make it blink.
        let excluded: i32 = 1;
        DwmSetWindowAttribute(hwnd, DWMWA_EXCLUDED_FROM_PEEK as u32, &excluded as *const i32 as *const _, 4);
        // Nor should it fade or animate when the windows around it change.
        DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED as u32, &excluded as *const i32 as *const _, 4);
        // These call back on this thread, and cost nothing while no window moves.
        for event in [EVENT_SYSTEM_FOREGROUND, EVENT_OBJECT_REORDER] {
            SetWinEventHook(event, event, std::ptr::null_mut(), Some(windows_reordered), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        }
        // Nobody sees animation on a locked or dark screen, so it stops completely until they can.
        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        RegisterPowerSettingNotification(hwnd as *mut _, &GUID_SESSION_DISPLAY_STATUS, DEVICE_NOTIFY_WINDOW_HANDLE);
        shared.hwnd.store(hwnd as isize, Ordering::Relaxed);
        usage::request_scan(shared.clone());
        if ask_hooks {
            PostMessageW(hwnd, WM_FIRST_RUN, 0, 0);
        }
        if let Some(pack) = pack_argument() {
            PENDING_PACKS.lock().unwrap().push(pack);
            PostMessageW(hwnd, WM_IMPORT_PACKS, 0, 0);
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.message == WM_KEYDOWN && msg.wParam == 0x1B && picker::forward_escape(msg.hwnd) {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ---- what the picker needs from the app ----

/// Keeps the buddy under the main window while you use it, and on top of everything otherwise.
pub fn sync_buddy_with_picker() {
    with_app(|a| a.sync_z_order());
}

/// After something big went away, like the main window.
pub fn trim_memory_soon() {
    with_app(|a| a.trim_soon());
}

pub fn settings_view() -> Option<SettingsView> {
    with_app(|a| SettingsView {
        hooks: hooks_config::status(&exe_path(), a.config.port),
        sound: !a.config.muted,
        toasts: a.config.toasts,
        autostart: autostart_enabled(),
        auto_hide: a.config.auto_hide,
        translucent: a.config.translucent,
        auto_update: a.config.auto_update,
        // A wheel size between the three marks none of them.
        size: SIZE_FACTORS.iter().position(|f| (f - a.config.zoom).abs() < 0.0005).map_or(u8::MAX, |i| i as u8),
        language: match a.config.language.as_str() {
            "vi" => 1,
            "en" => 2,
            _ => 0,
        },
        auto_skills: a.auto_skills_on(),
        skill_keys: a.config.skill_keys,
    })
}

pub fn apply_setting(s: Setting) {
    with_app(|a| match s {
        Setting::Hooks => a.on_command(CMD_HOOKS),
        Setting::AutoSkills => a.on_command(CMD_AUTO_SKILLS),
        Setting::Sound => a.on_command(CMD_MUTE),
        Setting::Toasts => a.on_command(CMD_TOASTS),
        Setting::Autostart => a.on_command(CMD_AUTOSTART),
        Setting::ResetPosition => a.on_command(CMD_RESET_POS),
        Setting::Folder => a.on_command(CMD_USER_FOLDER),
        Setting::AutoUpdate => {
            a.config.auto_update = !a.config.auto_update;
            a.save_config();
            crate::update::set_enabled(a.config.auto_update);
        }
        Setting::AutoHide => {
            a.config.auto_hide = !a.config.auto_hide;
            a.save_config();
            a.check_fullscreen();
        }
        Setting::Translucent => {
            a.config.translucent = !a.config.translucent;
            a.save_config();
            theme::set_translucent(a.config.translucent);
            a.repaint_if_shown();
        }
        Setting::Language(n) => {
            a.config.language = ["auto", "vi", "en"][n.min(2) as usize].into();
            a.save_config();
            crate::i18n::apply(&a.config.language);
            a.update_tip();
            a.repaint_if_shown();
        }
        Setting::Size(n) => {
            a.full_restore = None;
            a.set_zoom(SIZE_FACTORS[n.min(2) as usize]);
            a.config.size = n.min(2);
            a.zoom_settled();
        }
    });
}

pub struct LibrarySnapshot {
    pub characters: Vec<Character>,
    pub current: String,
    pub usage: usage::Days,
    pub usage_ready: bool,
}

pub fn library_snapshot() -> Option<LibrarySnapshot> {
    with_app(|a| LibrarySnapshot {
        characters: a.library.clone(),
        current: a.config.character.clone(),
        usage: a.shared.usage.lock().unwrap().clone(),
        usage_ready: a.shared.scanned.load(Ordering::Acquire),
    })
}

pub fn choose_character(id: &str) -> bool {
    with_app(|a| a.select_character(id)).unwrap_or(false)
}

/// After characters were added, edited or removed on disk.
pub fn library_changed(edited: Option<&str>) {
    with_app(|a| {
        a.reload_library();
        if edited == Some(a.config.character.as_str()) {
            a.apply_character(); // the buddy shows the edit right away
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Event, Store};

    #[test]
    fn sharp_frames_are_never_bigger_than_full_screen() {
        // Up to the large size, exactly what is shown.
        assert_eq!(prep_density(0.75, 1.0, Some(4.89), Some(4.64)), 0.75);
        // Past it, the source's own resolution...
        assert_eq!(prep_density(2.0, 1.0, Some(2.44), Some(4.64)), 2.44);
        // ...but no more than full screen shows, rounded down so that fits exactly.
        assert_eq!(prep_density(2.0, 1.0, Some(4.89), Some(4.6428)), 4.64);
        assert_eq!(prep_density(2.0, 1.0, Some(4.89), None), 4.89);
    }

    #[test]
    fn notifications_say_what_happened_and_where() {
        let mut st = Store::default();
        let e = Event { session_id: "a".into(), event: "Notification".into(), cwd: Some("C:\\work\\shop-api".into()), message: Some("Cần quyền chạy lệnh".into()), ..Default::default() };
        st.apply(&e, 0).unwrap();
        let (title, body) = crate::store::toast_text(st.get("a").unwrap(), State::Waiting);
        assert_eq!(title, "Claude cần bạn");
        assert_eq!(body, "shop-api\nCần quyền chạy lệnh");
        assert!(!title.chars().any(|c| c as u32 > 0x2000), "no emoji in titles");
    }

    const SCREEN: RECT = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };
    const WIN: (i32, i32) = (300, 620);
    const CHARACTER: (i32, i32) = (112, 160);

    /// Runs a whole drag, the way the mouse would, from the usual bottom right spot.
    fn drag(from: (i32, i32), moves: &[(i32, i32)]) -> DragStep {
        let mut step = drag_step(from, WIN, CHARACTER, 4, &SCREEN);
        for m in moves {
            step = drag_step((step.virtual_pos.0 + m.0, step.virtual_pos.1 + m.1), WIN, CHARACTER, 4, &SCREEN);
        }
        step
    }

    /// Where the character's own rectangle ends up on the screen: (left, top, right, bottom).
    fn on_screen(s: &DragStep) -> (i32, i32, i32, i32) {
        let cx = s.window.0 + WIN.0 / 2 + s.shift;
        let bottom = s.window.1 + WIN.1 - 4 - s.lift;
        (cx - CHARACTER.0 / 2, bottom - CHARACTER.1, cx + CHARACTER.0 / 2, bottom)
    }

    #[test]
    fn the_character_reaches_all_four_edges_of_the_screen() {
        let start = (1500, 400);
        let (l, _, _, _) = on_screen(&drag(start, &[(-5000, 0)]));
        assert_eq!(l, 0, "left edge");
        let (_, _, r, _) = on_screen(&drag(start, &[(5000, 0)]));
        assert_eq!(r, 1920, "right edge");
        let (_, t, _, _) = on_screen(&drag(start, &[(0, -5000)]));
        assert_eq!(t, 0, "top edge");
        let (_, _, _, b) = on_screen(&drag(start, &[(0, 5000)]));
        assert_eq!(b, 1040 - 4, "bottom edge");
        let corner = on_screen(&drag(start, &[(-5000, -5000)]));
        assert_eq!((corner.0, corner.1), (0, 0), "corner");
    }

    #[test]
    fn the_window_itself_never_leaves_the_screen() {
        for m in [(-5000, -5000), (5000, 5000), (-5000, 5000), (5000, -5000)] {
            let s = drag((1000, 300), &[m]);
            assert!(s.window.0 >= 0 && s.window.0 + WIN.0 <= 1920 && s.window.1 >= 0 && s.window.1 + WIN.1 <= 1040, "{s:?}");
        }
    }

    #[test]
    fn turning_back_moves_at_once_instead_of_working_off_an_overshoot() {
        let at_edge = drag((1000, 300), &[(-5000, 0)]);
        let back = drag_step((at_edge.virtual_pos.0 + 10, at_edge.virtual_pos.1), WIN, CHARACTER, 4, &SCREEN);
        assert_eq!(on_screen(&back).0, on_screen(&at_edge).0 + 10);
    }

    #[test]
    fn ordinary_moves_only_move_the_window() {
        let s = drag((500, 200), &[(30, -20)]);
        assert_eq!((s.window, s.shift, s.lift), ((530, 180), 0, 0));
    }

    #[test]
    fn a_second_monitor_left_of_the_main_one_works_too() {
        let left = RECT { left: -1600, top: -100, right: 0, bottom: 800 };
        let s = drag_step((-5000, -5000), WIN, CHARACTER, 4, &left);
        assert_eq!((s.window.0, s.window.1), (-1600, -100));
        assert_eq!(s.shift, -(WIN.0 - CHARACTER.0) / 2);
        assert_eq!(s.lift, WIN.1 - 4 - CHARACTER.1);
    }

    #[test]
    fn a_character_wider_than_the_window_simply_has_no_side_travel() {
        let s = drag_step((-5000, 100), WIN, (400, 160), 4, &SCREEN);
        assert_eq!((s.window.0, s.shift), (0, 0));
    }
}
