//! The buddy on macOS: the same picture as on Windows, drawn into a buffer and shown in a floating
//! panel that never takes the keyboard. A menu bar item stands in for the tray.

use crate::config::{self, Config};
pub use crate::config::{user_root, Setting, SettingsView};
use crate::focus;
use crate::host;
use crate::hooks_config;
use crate::i18n::t;
use crate::library::{self, state_index, Character, Source};
use crate::motion::Motion;
use crate::picker;
use crate::render::{self, Bubble, Hit, HitRect, TextDrawer, View, CHAR_SIZE, WIN_H, WIN_W};
use crate::server::{self, now_ms, Shared};
use crate::sound;
use crate::store::{self, name_of, Session, State};
use crate::theme;
use crate::usage;
use crate::anim::{self, CharAnim, Player};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSEvent, NSFloatingWindowLevel,
    NSImage, NSMenu, NSMenuItem, NSPanel, NSScreen, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSView, NSWindowCollectionBehavior, NSWindowOcclusionState, NSWindowOrderingMode,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGFloat};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSTimer};
use objc2_quartz_core::CATransaction;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const SIZE_FACTORS: [f32; 3] = [0.75, 1.0, 1.25];
const ACTIVE_FRAME_MS: u32 = 33;
/// How often the pointer is looked at, because the window only gets mouse events over its visible parts.
const WATCH_MS: f64 = 0.04;
const HOUSEKEEPING_MS: u64 = 5_000;
const USAGE_SCAN_MS: u64 = 600_000;
const SCAN_DELAY_MS: u64 = 20_000;
const BUBBLE_MS: u64 = 8_000;
const HOVER_GRACE_MS: u64 = 500;
const PANEL_GRACE_MS: u64 = 2_500;
const DRAG_THRESHOLD: f64 = 4.0;
const CHAR_GAP: f32 = 4.0;
const VECTOR_BODY_W: f32 = 112.0;
const MENU_ICON_SIZE: u32 = 36;
/// A pixel this transparent lets clicks through to what is behind it.
const CLICK_ALPHA: u8 = 16;
const AGENT_LABEL: &str = "com.taivt0109.claude-buddy";

static WOKEN: AtomicBool = AtomicBool::new(false);
static PENDING_FILES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Asks the UI thread to look again; safe from any thread.
pub fn wake(_hwnd: isize) {
    WOKEN.store(true, Ordering::Release);
}

fn exe_path() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

fn agent_file() -> PathBuf {
    config::home().join("Library").join("LaunchAgents").join(format!("{AGENT_LABEL}.plist"))
}

fn autostart_enabled() -> bool {
    agent_file().exists()
}

fn set_autostart(on: bool) {
    let file = agent_file();
    if !on {
        let _ = std::fs::remove_file(file);
        return;
    }
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let exe = exe_path().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let _ = std::fs::write(file, format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{AGENT_LABEL}</string><key>ProgramArguments</key><array><string>{exe}</string></array><key>RunAtLoad</key><true/></dict></plist>\n"));
}

fn process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) }
}

/// A pop-up from the system, with arguments passed separately so no text can become a command.
fn notify(title: &str, body: &str) {
    let script = ["-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)", "-e", "end run"];
    let _ = std::process::Command::new("osascript").args(script).arg(title).arg(if body.is_empty() { " " } else { body }).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

/// The window's pixel buffer, in the same premultiplied BGRA as everywhere else.
pub(crate) struct Canvas {
    pixels: Vec<u8>,
    pub bits: *mut u8,
    pub w: u32,
    pub h: u32,
}

impl Canvas {
    pub fn new(w: u32, h: u32) -> Option<Canvas> {
        let mut pixels = vec![0u8; (w as usize) * (h as usize) * 4];
        let bits = pixels.as_mut_ptr();
        Some(Canvas { pixels, bits, w, h })
    }

    pub fn image(&self) -> Option<CFRetained<CGImage>> {
        host::image_of(&self.pixels, 0, 0, self.w as usize, self.h as usize, self.w as usize)
    }

    pub fn image_of_part(&self, x: i32, y: i32, w: i32, h: i32) -> Option<CFRetained<CGImage>> {
        host::image_of(&self.pixels, x as usize, y as usize, w as usize, h as usize, self.w as usize)
    }

    fn alpha_at(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return 0;
        }
        self.pixels[(y as usize * self.w as usize + x as usize) * 4 + 3]
    }
}

enum Visual {
    Vector(usize),
    Media { player: Player, states: Box<[(Motion, bool); 5]>, room: bool },
}

struct Loaded {
    token: u64,
    id: String,
    result: Result<Arc<CharAnim>, String>,
}

static LOADED: Mutex<Option<Loaded>> = Mutex::new(None);

struct Drag {
    start_mouse: NSPoint,
    last_mouse: NSPoint,
    /// Top left of where the window would be with the character in its usual spot, y counted down.
    virtual_pos: (f64, f64),
    moved: bool,
    hit: Option<Hit>,
}

struct App {
    shared: Arc<Shared>,
    config: Config,
    canvas: Canvas,
    text: TextDrawer,
    scale: f32,
    hits: Vec<HitRect>,
    state: State,
    library: Vec<Character>,
    hovering: bool,
    hover_until: u64,
    panel_until: u64,
    bubble_until: u64,
    scan_at: u64,
    next_scan: u64,
    known_states: Vec<(String, State)>,
    tray_state: State,
    visual: Visual,
    current_sounds: [Option<PathBuf>; 2],
    load_token: u64,
    state_since: Instant,
    sessions: Vec<Session>,
    panel_open: bool,
    hover: Option<String>,
    visible: bool,
    drag: Option<Drag>,
    started: Instant,
    frame_ms: u32,
    note_until: u64,
    ignoring: bool,
    window: Retained<NSPanel>,
    view: Retained<BuddyView>,
    status: Retained<NSStatusItem>,
    frame_timer: Option<Retained<NSTimer>>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "ClaudeBuddyView"]
    struct BuddyView;

    impl BuddyView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(frameTick:))]
        fn frame_tick(&self, _timer: &NSTimer) {
            with_app(App::frame);
        }

        #[unsafe(method(watchTick:))]
        fn watch_tick(&self, _timer: &NSTimer) {
            with_app(App::watch);
            let files = std::mem::take(&mut *PENDING_FILES.lock().unwrap());
            open_files(files);
        }

        #[unsafe(method(housekeepingTick:))]
        fn housekeeping_tick(&self, _timer: &NSTimer) {
            with_app(App::housekeeping);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let from_top = self.bounds().size.height - p.y;
            with_app(|a| {
                let mouse = NSEvent::mouseLocation();
                let (x, y) = a.top_left();
                let (dx, dy) = (a.config.shift as f64, -a.config.lift as f64);
                let k = a.scale as f64;
                a.drag = Some(Drag { start_mouse: mouse, last_mouse: mouse, virtual_pos: (x + dx, y + dy), moved: false, hit: a.hit_at((p.x * k) as i32, (from_top * k) as i32) });
            });
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, _event: &NSEvent) {
            with_app(App::dragged);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            with_app(|a| {
                let Some(d) = a.drag.take() else { return };
                if d.moved {
                    let o = a.window.frame().origin;
                    a.config.position = Some((o.x as i32, o.y as i32));
                    a.config.save();
                } else {
                    a.on_click(d.hit);
                }
            });
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            // Outside the borrow: the menu runs a loop of its own, and choosing from it needs the app.
            let Some(menu) = with_app(|a| a.build_menu()) else { return };
            NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
        }

        #[unsafe(method(menuCommand:))]
        fn menu_command(&self, item: &NSMenuItem) {
            let cmd = item.tag() as usize;
            match cmd {
                CMD_PICKER => picker::open(0),
                CMD_USAGE => picker::open(1),
                CMD_SETTINGS => picker::open(2),
                CMD_EXIT => quit(),
                _ => {
                    with_app(|a| a.on_command(cmd));
                }
            }
        }
    }
);

const CMD_TOGGLE: usize = 1;
const CMD_MUTE: usize = 2;
const CMD_EXIT: usize = 7;
const CMD_PICKER: usize = 8;
const CMD_USAGE: usize = 10;
const CMD_SETTINGS: usize = 11;
const CMD_CHARACTER: usize = 100;

/// Codex must not keep running, and keep spending the user's quota, once the app is gone.
fn quit() -> ! {
    crate::codex::stop_running();
    std::process::exit(0);
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ClaudeBuddyDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        /// Double-clicking a .buddy file, or one dropped on the app.
        #[unsafe(method(application:openFiles:))]
        fn open_files_event(&self, _app: &NSApplication, files: &NSArray<NSString>) {
            let mut pending = PENDING_FILES.lock().unwrap();
            pending.extend(files.iter().map(|f| PathBuf::from(f.to_string())));
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn stays_open(&self, _app: &NSApplication) -> bool {
            false
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _note: &objc2_foundation::NSNotification) {
            crate::codex::stop_running();
        }
    }
);

fn open_files(files: Vec<PathBuf>) {
    for f in files.iter().filter(|f| picker::is_pack(f)) {
        picker::import_pack(f);
    }
    let others: Vec<PathBuf> = files.into_iter().filter(|f| !picker::is_pack(f)).collect();
    if !others.is_empty() {
        picker::open(0);
        picker::files_dropped(others, 0.0, 0.0);
    }
}

/// One step of a drag, in points with y counting down. The window (with room for bubble and panel)
/// stays on the screen; past an edge the character slides on inside it, so it can reach all four edges.
fn drag_step(wanted: (f64, f64), win: (f64, f64), character: (f64, f64), gap: f64, area: (f64, f64, f64, f64)) -> ((f64, f64), f64, f64, (f64, f64)) {
    let (left, top, right, bottom) = area;
    let window = (wanted.0.min(right - win.0).max(left), wanted.1.min(bottom - win.1).max(top));
    let x_limit = ((win.0 - character.0) / 2.0).max(0.0);
    let y_limit = (win.1 - gap - character.1).max(0.0);
    let shift = (wanted.0 - window.0).clamp(-x_limit, x_limit);
    let lift = (window.1 - wanted.1).clamp(0.0, y_limit);
    (window, shift, lift, (window.0 + shift, window.1 - lift))
}

impl App {
    fn mtm(&self) -> MainThreadMarker {
        self.view.mtm()
    }

    /// Height of the screen everything is measured from, for turning Cocoa's y into one that counts down.
    fn flip_height(&self) -> f64 {
        NSScreen::screens(self.mtm()).iter().next().map_or(0.0, |s| s.frame().size.height)
    }

    fn top_left(&self) -> (f64, f64) {
        let f = self.window.frame();
        (f.origin.x, self.flip_height() - f.origin.y - f.size.height)
    }

    fn move_to(&self, x: f64, y: f64) {
        let h = self.window.frame().size.height;
        self.window.setFrameOrigin(NSPoint::new(x, self.flip_height() - y - h));
    }

    /// The usable part of the screen under the pointer: left, top, right, bottom, y counting down.
    fn work_area(&self) -> (f64, f64, f64, f64) {
        let m = NSEvent::mouseLocation();
        let flip = self.flip_height();
        let screens = NSScreen::screens(self.mtm());
        let found = screens.iter().find(|s| {
            let f = s.frame();
            m.x >= f.origin.x && m.x <= f.origin.x + f.size.width && m.y >= f.origin.y && m.y <= f.origin.y + f.size.height
        });
        let v = found.or_else(|| screens.iter().next()).map_or(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1440.0, 900.0)), |s| s.visibleFrame());
        (v.origin.x, flip - v.origin.y - v.size.height, v.origin.x + v.size.width, flip - v.origin.y)
    }

    fn default_position(&self) -> (f64, f64) {
        let (_, _, right, bottom) = self.work_area();
        (right - WIN_W as f64 - 24.0, bottom - WIN_H as f64 - 8.0)
    }

    fn dragged(&mut self) {
        let Some(d) = &mut self.drag else { return };
        let mouse = NSEvent::mouseLocation();
        if !d.moved && (mouse.x - d.start_mouse.x).abs() <= DRAG_THRESHOLD && (mouse.y - d.start_mouse.y).abs() <= DRAG_THRESHOLD {
            return;
        }
        d.moved = true;
        // Cocoa's y counts up.
        let wanted = (d.virtual_pos.0 + mouse.x - d.last_mouse.x, d.virtual_pos.1 - (mouse.y - d.last_mouse.y));
        d.last_mouse = mouse;
        let (cw, ch) = self.char_size();
        let (window, shift, lift, virtual_pos) = drag_step(wanted, (WIN_W as f64, WIN_H as f64), (cw as f64, ch as f64), CHAR_GAP as f64, self.work_area());
        if let Some(d) = &mut self.drag {
            d.virtual_pos = virtual_pos;
        }
        self.move_to(window.0, window.1);
        if shift as f32 != self.config.shift || lift as f32 != self.config.lift {
            self.config.shift = shift as f32;
            self.config.lift = lift as f32;
            self.repaint();
        }
    }

    fn offset_px(&self) -> (i32, i32) {
        ((self.config.shift * self.scale) as i32, -(self.config.lift * self.scale) as i32)
    }

    /// Logical size of what counts as the character for reaching screen edges.
    fn char_size(&self) -> (f32, f32) {
        let f = SIZE_FACTORS[self.config.size.min(2) as usize];
        match &self.visual {
            Visual::Media { player, room: true, .. } => (player.anim.w as f32 / self.scale, player.anim.h as f32 / self.scale),
            _ => (VECTOR_BODY_W * f, CHAR_SIZE * f),
        }
    }

    fn clamp_offset(&mut self) {
        let (cw, ch) = self.char_size();
        let x_limit = ((WIN_W - cw) / 2.0).max(0.0);
        let y_limit = (WIN_H - CHAR_GAP - ch).max(0.0);
        self.config.shift = self.config.shift.clamp(-x_limit, x_limit);
        self.config.lift = self.config.lift.clamp(0.0, y_limit);
    }

    fn char_scale(&self) -> f32 {
        self.scale * SIZE_FACTORS[self.config.size.min(2) as usize]
    }

    fn char_region(&self) -> (f32, f32) {
        let f = SIZE_FACTORS[self.config.size.min(2) as usize];
        match &self.visual {
            Visual::Media { player, room: true, .. } => {
                let (w, h) = render::media_region(player.anim.w, player.anim.h, self.char_scale());
                (w * f, h * f)
            }
            _ => (CHAR_SIZE * f, CHAR_SIZE * f),
        }
    }

    fn time(&self) -> f32 {
        (self.started.elapsed().as_secs_f64() % 3960.0) as f32
    }

    fn present(&self) {
        let Some(image) = self.canvas.image() else { return };
        let Some(layer) = self.view.layer() else { return };
        let image: *const CGImage = &*image;
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // A layer takes a CGImage as its contents, through a property typed as any object.
        unsafe { layer.setContents(Some(&*(image as *const AnyObject))) };
        CATransaction::commit();
    }

    fn draw_character(&mut self, force: bool) -> bool {
        let (w, h, scale, state, t) = (self.canvas.w, self.canvas.h, self.char_scale(), self.state, self.time());
        let off = self.offset_px();
        let buf = unsafe { std::slice::from_raw_parts_mut(self.canvas.bits, (w * h * 4) as usize) };
        match &mut self.visual {
            Visual::Vector(kind) => {
                render::paint_character(buf, w, h, scale, *kind, state, t, off);
                true
            }
            Visual::Media { player, states, room } => {
                let (motion, badge) = states[state_index(state)];
                let changed = player.advance(now_ms());
                let (fw, fh) = (player.anim.w, player.anim.h);
                if force || motion != Motion::None || badge {
                    let moving = self.state_since.elapsed().as_secs_f32();
                    render::paint_stage(buf, w, h, scale, &player.pixels, fw, fh, motion.at(moving), badge.then_some(state), t, off, *room);
                    return true;
                }
                changed.map(|r| render::paint_change(buf, w, h, scale, &player.pixels, fw, fh, r, off)).is_some_and(|r| r.2 > 0)
            }
        }
    }

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
        let buf = unsafe { std::slice::from_raw_parts_mut(self.canvas.bits, (w * h * 4) as usize) };
        self.hits = render::paint_ui(buf, w, h, scale, &self.text, &view);
        self.draw_character(true);
        self.present();
    }

    fn repaint_if_shown(&mut self) {
        if self.visible {
            self.repaint();
        }
    }

    fn frame(&mut self) {
        // Nobody sees it on a locked or dark screen. Asked anew each time, so it can't get stuck paused.
        if !self.visible || !self.window.occlusionState().contains(NSWindowOcclusionState::Visible) {
            return;
        }
        if self.draw_character(false) {
            self.present();
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
                    0
                } else {
                    player.clip().min_delay().max(ACTIVE_FRAME_MS)
                }
            }
        };
        let want = if self.visible { want } else { 0 };
        if want == self.frame_ms {
            return;
        }
        self.frame_ms = want;
        if let Some(old) = self.frame_timer.take() {
            old.invalidate();
        }
        if want != 0 {
            let target: &AnyObject = &self.view;
            self.frame_timer = Some(unsafe { NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(want as f64 / 1000.0, target, sel!(frameTick:), None, true) });
        }
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

    fn note_state_changes(&mut self) {
        let mut states: Vec<(String, State)> = self.sessions.iter().map(|s| (s.id.clone(), s.state)).collect();
        states.sort_by(|a, b| a.0.cmp(&b.0));
        let woke = states.iter().any(|x| x.1 != State::Idle && !self.known_states.contains(x));
        self.known_states = states;
        if woke {
            self.bubble_until = now_ms() + BUBBLE_MS;
        }
    }

    fn hit_at(&self, x: i32, y: i32) -> Option<Hit> {
        self.hits.iter().find(|r| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h).map(|r| r.hit.clone())
    }

    /// Clicks go through wherever nothing is drawn, and hovering is noticed even with no event to say so.
    fn watch(&mut self) {
        if LOAD_READY.swap(false, Ordering::AcqRel) {
            self.on_loaded();
        }
        if WOKEN.swap(false, Ordering::AcqRel) {
            self.announce();
            self.refresh();
        }
        let now = now_ms();
        if self.scale != self.window.backingScaleFactor() as f32 {
            self.apply_scale();
        }
        if self.scan_at != 0 && now >= self.scan_at {
            self.scan_at = 0;
            usage::request_scan(self.shared.clone());
        }
        if !self.visible {
            return;
        }
        let m = NSEvent::mouseLocation();
        let f = self.window.frame();
        let k = self.scale as f64;
        let (px, py) = (((m.x - f.origin.x) * k) as i32, ((f.origin.y + f.size.height - m.y) * k) as i32);
        let hit = self.hit_at(px, py);
        let over = self.drag.is_some() || hit.is_some() || self.canvas.alpha_at(px, py) >= CLICK_ALPHA;
        if self.ignoring == over {
            self.ignoring = !over;
            self.window.setIgnoresMouseEvents(!over);
        }
        if self.drag.is_some() {
            return;
        }
        if over {
            self.hover_until = now + HOVER_GRACE_MS;
            self.panel_until = now + PANEL_GRACE_MS;
        }
        let hovering = over || now < self.hover_until;
        let hover = match hit {
            Some(Hit::Row(id)) => Some(id),
            _ => None,
        };
        let panel_expired = self.panel_open && !over && now >= self.panel_until;
        let bubble_expired = self.bubble_until != 0 && now >= self.bubble_until;
        if hovering != self.hovering || hover != self.hover || panel_expired || bubble_expired {
            self.hovering = hovering;
            self.hover = hover;
            if panel_expired {
                self.panel_open = false;
            }
            if bubble_expired {
                self.bubble_until = 0;
            }
            self.repaint();
        }
    }

    fn refresh(&mut self) {
        self.sessions = self.shared.store.lock().unwrap().list();
        let state = store::overall(&self.sessions);
        if state != self.state {
            self.state = state;
            self.state_since = Instant::now();
            if let Visual::Media { player, .. } = &mut self.visual {
                player.set_state(state_index(state), now_ms());
            }
        }
        self.note_state_changes();
        self.update_timer();
        self.update_status();
        self.repaint_if_shown();
    }

    fn announce(&mut self) {
        let pending = std::mem::take(&mut *self.shared.pending.lock().unwrap());
        if pending.iter().any(|t| t.to == State::Done) {
            self.scan_at = now_ms() + SCAN_DELAY_MS;
        }
        for t in pending.iter().filter(|t| t.notify) {
            if focus::looking_at(&t.session) {
                continue; // already in front of you
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
                let (title, body) = store::toast_text(&t.session, t.to);
                notify(&title, &body);
            }
        }
    }

    fn update_status(&mut self) {
        let tip = match self.sessions.first() {
            Some(s) if s.state != State::Idle => format!("Claude Buddy · {}: {}", name_of(s), store::status_of(s)),
            _ => crate::tf!("Claude Buddy · {} phiên", "Claude Buddy · {} sessions", self.sessions.len()),
        };
        let mtm = self.mtm();
        if let Some(button) = self.status.button(mtm) {
            button.setToolTip(Some(&NSString::from_str(&tip)));
            if self.state != self.tray_state || button.image().is_none() {
                self.tray_state = self.state;
                if let Some(image) = menu_icon(self.state) {
                    button.setImage(Some(&image));
                }
            }
        }
    }

    fn current(&self) -> Option<&Character> {
        self.library.iter().find(|c| c.id == self.config.character)
    }

    fn apply_character(&mut self) {
        self.load_token += 1;
        let Some(c) = self.current().cloned() else {
            self.config.character = "mochi".into();
            self.visual = Visual::Vector(0);
            return;
        };
        self.current_sounds = c.sounds.clone();
        if let Source::Vector(i) = c.source {
            self.visual = Visual::Vector(i);
            self.after_visual_change();
        }
        let (token, dpi) = (self.load_token, self.char_scale());
        let _ = std::thread::Builder::new().name("load-character".into()).spawn(move || {
            let max_h = (anim::BASE_HEIGHT * c.scale).min(anim::MAX_STAGE_HEIGHT);
            let result = anim::load(&c, anim::MAX_WIDTH, max_h, dpi);
            *LOADED.lock().unwrap() = Some(Loaded { token, id: c.id, result });
            LOAD_READY.store(true, Ordering::Release);
        });
    }

    fn on_loaded(&mut self) {
        let Some(l) = LOADED.lock().unwrap().take() else { return };
        if l.token != self.load_token {
            return; // a newer choice won
        }
        match l.result {
            Ok(a) => {
                let Some(c) = self.library.iter().find(|c| c.id == l.id) else { return };
                let states = Box::new(std::array::from_fn(|i| (c.states[i].motion, (c.states[i].fallback || a.failed[i]) && i != 0)));
                let room = !c.is_builtin();
                self.visual = Visual::Media { player: Player::new(a, state_index(self.state), now_ms()), states, room };
                self.after_visual_change();
            }
            Err(_) if matches!(self.visual, Visual::Vector(_)) && self.current().is_some_and(Character::is_builtin) => {}
            Err(e) => {
                let name = self.current().map_or(l.id.clone(), |c| c.name.clone());
                notify(&crate::tf!("Không tải được {name}", "Could not load {name}"), &e);
                if let Visual::Vector(i) = self.visual {
                    self.config.character = self.library.iter().find(|c| c.source == Source::Vector(i)).map_or("mochi".into(), |c| c.id.clone());
                }
            }
        }
    }

    fn after_visual_change(&mut self) {
        self.clamp_offset();
        self.frame_ms = u32::MAX; // whatever the new character wants is news
        self.update_timer();
        self.repaint_if_shown();
    }

    fn select_character(&mut self, id: &str) -> bool {
        if !self.library.iter().any(|c| c.id == id) {
            return false;
        }
        self.config.character = id.to_string();
        self.config.save();
        self.apply_character();
        self.rebuild_status_menu();
        true
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
            host::open_path(url);
            return;
        }
        let shared = self.shared.clone();
        let _ = std::thread::Builder::new().stack_size(256 * 1024).spawn(move || {
            if !focus::focus_session(&s) {
                *shared.note.lock().unwrap() = Some(crate::tf!("Không tìm thấy cửa sổ của {}", "Could not find the window of {}", name_of(&s)));
                wake(0);
            }
        });
        self.note_until = now_ms() + 5_000;
    }

    fn on_click(&mut self, hit: Option<Hit>) {
        match hit {
            Some(Hit::Character) => {
                self.panel_open = !self.panel_open;
                self.panel_until = now_ms() + PANEL_GRACE_MS;
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

    fn housekeeping(&mut self) {
        let now = now_ms();
        self.shared.store.lock().unwrap().persist();
        if crate::update::ready() && picker::window().is_null() && crate::update::apply() {
            quit();
        }
        // The system says nothing to a program like this one when light and dark are switched.
        let was_dark = theme::is_dark();
        theme::reload();
        if theme::is_dark() != was_dark {
            picker::theme_changed();
            self.repaint_if_shown();
        }
        if now >= self.next_scan {
            self.next_scan = now + USAGE_SCAN_MS;
            usage::request_scan(self.shared.clone());
        }
        let running = self.shared.store.lock().unwrap().running_pids();
        let dead: Vec<String> = running.into_iter().filter(|(_, p)| !process_alive(*p)).map(|(id, _)| id).collect();
        let mut changed = {
            let mut st = self.shared.store.lock().unwrap();
            let mut pending = self.shared.pending.lock().unwrap();
            for id in &dead {
                if let Some(x) = st.fail(id, t("Tiến trình Claude đã dừng", "The Claude process stopped"), now) {
                    pending.push(x);
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
        } else if self.panel_open && self.visible {
            self.repaint();
        }
    }

    fn apply_scale(&mut self) {
        let scale = self.window.backingScaleFactor() as f32;
        let (w, h) = ((WIN_W * scale).ceil() as u32, (WIN_H * scale).ceil() as u32);
        let Some(canvas) = Canvas::new(w, h) else { return };
        self.canvas = canvas;
        self.scale = scale;
        self.text = TextDrawer::new(scale);
        if matches!(self.visual, Visual::Media { .. }) {
            self.apply_character();
        }
        self.repaint_if_shown();
    }

    fn set_hooks(&mut self, install: bool) {
        match hooks_config::apply(install, &exe_path(), self.config.port) {
            Ok(_) if install => notify(t("Đã kết nối Claude Code", "Connected to Claude Code"), t("Các phiên Claude Code mới sẽ hiện ở đây.", "New Claude Code sessions will show up here.")),
            Ok(_) => notify(t("Đã ngắt kết nối Claude Code", "Disconnected from Claude Code"), t("Đã gỡ hook của app khỏi settings.json.", "The app's hooks were removed from settings.json.")),
            Err(e) => host::show_error(std::ptr::null_mut(), "Claude Buddy", &crate::tf!("Không ghi được settings.json của Claude Code:\n{e}", "Could not write Claude Code's settings.json:\n{e}")),
        }
    }

    fn on_command(&mut self, cmd: usize) {
        match cmd {
            CMD_TOGGLE => {
                self.visible = !self.visible;
                if self.visible {
                    self.window.orderFrontRegardless();
                    self.update_timer();
                    self.refresh();
                } else {
                    self.window.orderOut(None);
                    self.update_timer();
                }
                self.rebuild_status_menu();
            }
            CMD_MUTE => {
                self.config.muted = !self.config.muted;
                self.config.save();
                self.rebuild_status_menu();
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

    fn item(&self, label: &str, cmd: usize, checked: bool) -> Retained<NSMenuItem> {
        let mtm = self.mtm();
        let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(label), Some(sel!(menuCommand:)), &NSString::new()) };
        let target: &AnyObject = &self.view;
        unsafe { item.setTarget(Some(target)) };
        item.setTag(cmd as isize);
        item.setState(if checked { NSControlStateValueOn } else { NSControlStateValueOff });
        item
    }

    fn build_menu(&self) -> Retained<NSMenu> {
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        menu.addItem(&self.item(if self.visible { t("Ẩn Buddy", "Hide Buddy") } else { t("Hiện Buddy", "Show Buddy") }, CMD_TOGGLE, false));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let chars = NSMenu::new(mtm);
        for (i, c) in self.library.iter().enumerate() {
            chars.addItem(&self.item(&c.name, CMD_CHARACTER + i, c.id == self.config.character));
        }
        let change = self.item(t("Đổi nhân vật", "Change character"), 0, false);
        change.setSubmenu(Some(&chars));
        menu.addItem(&change);
        menu.addItem(&self.item(t("Quản lý nhân vật…", "Manage characters…"), CMD_PICKER, false));
        menu.addItem(&self.item(t("Xem mức dùng…", "View usage…"), CMD_USAGE, false));
        menu.addItem(&self.item(t("Cài đặt…", "Settings…"), CMD_SETTINGS, false));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&self.item(t("Tắt tiếng", "Mute"), CMD_MUTE, self.config.muted));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&self.item(t("Thoát", "Quit"), CMD_EXIT, false));
        menu
    }

    fn rebuild_status_menu(&self) {
        self.status.setMenu(Some(&self.build_menu()));
    }

    fn apply_collection_behavior(&self) {
        let mut b = NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::Stationary | NSWindowCollectionBehavior::IgnoresCycle;
        // Left out, the system keeps it away from full-screen apps by itself.
        if !self.config.auto_hide {
            b |= NSWindowCollectionBehavior::FullScreenAuxiliary;
        }
        self.window.setCollectionBehavior(b);
    }
}

static LOAD_READY: AtomicBool = AtomicBool::new(false);

fn menu_icon(state: State) -> Option<Retained<NSImage>> {
    let status = (state != State::Idle).then(|| render::state_color(state));
    let pixels = render::paint_icon(MENU_ICON_SIZE, status);
    let size = MENU_ICON_SIZE as usize;
    let image = host::image_of(&pixels, 0, 0, size, size, size)?;
    Some(NSImage::initWithCGImage_size(NSImage::alloc(), &image, NSSize::new(18.0, 18.0)))
}

/// Keeps the buddy out of the way of the main window while you use it.
pub fn sync_buddy_with_picker() {
    with_app(|a| {
        let in_use = picker::window_is_key();
        a.window.setLevel(if in_use { 0 } else { NSFloatingWindowLevel });
        // Changing level can bring it to the front of that level, which would be in front of the window.
        if let Some(n) = host::window_number(picker::window()).filter(|_| in_use && a.visible) {
            a.window.orderWindow_relativeTo(NSWindowOrderingMode::Below, n);
        }
    });
}

pub fn trim_memory_soon() {}

pub fn settings_view() -> Option<SettingsView> {
    with_app(|a| SettingsView {
        hooks: hooks_config::status(&exe_path(), a.config.port),
        sound: !a.config.muted,
        toasts: a.config.toasts,
        autostart: autostart_enabled(),
        auto_hide: a.config.auto_hide,
        translucent: a.config.translucent,
        auto_update: a.config.auto_update,
        size: a.config.size,
        language: match a.config.language.as_str() {
            "vi" => 1,
            "en" => 2,
            _ => 0,
        },
        ..Default::default()
    })
}

pub fn apply_setting(s: Setting) {
    with_app(|a| match s {
        Setting::Hooks => {
            let installed = hooks_config::status(&exe_path(), a.config.port);
            a.set_hooks(!installed);
        }
        Setting::Sound => a.on_command(CMD_MUTE),
        Setting::Toasts => {
            a.config.toasts = !a.config.toasts;
            a.config.save();
        }
        Setting::Autostart => set_autostart(!autostart_enabled()),
        Setting::AutoUpdate => {
            a.config.auto_update = !a.config.auto_update;
            a.config.save();
            crate::update::set_enabled(a.config.auto_update);
        }
        Setting::ResetPosition => {
            let (x, y) = a.default_position();
            a.move_to(x, y);
            let o = a.window.frame().origin;
            a.config.position = Some((o.x as i32, o.y as i32));
            a.config.shift = 0.0;
            a.config.lift = 0.0;
            a.config.save();
            a.repaint_if_shown();
        }
        Setting::Folder => {
            let dir = user_root();
            let _ = std::fs::create_dir_all(&dir);
            host::open_folder(&dir);
        }
        Setting::AutoHide => {
            a.config.auto_hide = !a.config.auto_hide;
            a.config.save();
            a.apply_collection_behavior();
        }
        Setting::Translucent => {
            a.config.translucent = !a.config.translucent;
            a.config.save();
            theme::set_translucent(a.config.translucent);
            a.repaint_if_shown();
        }
        // No skills on the Mac yet, so the settings page doesn't offer this.
        Setting::AutoSkills => {}
        Setting::Language(n) => {
            a.config.language = ["auto", "vi", "en"][n.min(2) as usize].into();
            a.config.save();
            crate::i18n::apply(&a.config.language);
            a.update_status();
            a.rebuild_status_menu();
            a.repaint_if_shown();
        }
        Setting::Size(n) => {
            a.config.size = n.min(2);
            a.config.save();
            a.apply_character();
            a.clamp_offset();
            a.repaint_if_shown();
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

pub fn library_changed(edited: Option<&str>) {
    with_app(|a| {
        a.library = library::load_all(&user_root());
        if edited == Some(a.config.character.as_str()) {
            a.apply_character();
        }
        a.rebuild_status_menu();
    });
}

/// The menu a Mac app needs for ⌘C, ⌘V and the like to reach text boxes, even with no menu bar of its own.
fn edit_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let bar = NSMenu::new(mtm);
    let add = |menu: &NSMenu, label: &str, action: objc2::runtime::Sel, key: &str| {
        let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(label), Some(action), &NSString::from_str(key)) };
        menu.addItem(&item);
    };
    let holder = NSMenuItem::new(mtm);
    let edit = NSMenu::new(mtm);
    add(&edit, "Undo", sel!(undo:), "z");
    add(&edit, "Redo", sel!(redo:), "Z");
    add(&edit, "Cut", sel!(cut:), "x");
    add(&edit, "Copy", sel!(copy:), "c");
    add(&edit, "Paste", sel!(paste:), "v");
    add(&edit, "Select All", sel!(selectAll:), "a");
    add(&edit, "Close", sel!(performClose:), "w");
    holder.setSubmenu(Some(&edit));
    bar.addItem(&holder);
    bar
}

pub fn run() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    theme::reload();
    let config = Config::load();
    theme::set_translucent(config.translucent);
    crate::update::start(config.auto_update);
    let shared = Arc::new(Shared::default());
    shared.store.lock().unwrap().restore(now_ms(), process_alive);
    if let Err(e) = server::start(shared.clone(), config.port) {
        eprintln!("{}", crate::tf!("Không mở được cổng {}: {e}", "Could not open port {}: {e}", config.port));
        std::process::exit(1);
    }
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon: it lives in the menu bar.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.setMainMenu(Some(&edit_menu(mtm)));
    let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::alloc(mtm), init] };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    let size = NSSize::new(WIN_W as CGFloat, WIN_H as CGFloat);
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), size);
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let window = NSPanel::initWithContentRect_styleMask_backing_defer(NSPanel::alloc(mtm), frame, style, NSBackingStoreType::Buffered, false);
    unsafe { window.setReleasedWhenClosed(false) };
    window.setFloatingPanel(true);
    window.setBecomesKeyOnlyIfNeeded(true);
    window.setHidesOnDeactivate(false);
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(false);
    window.setLevel(NSFloatingWindowLevel);
    window.setIgnoresMouseEvents(true);

    let view: Retained<BuddyView> = unsafe { msg_send![BuddyView::alloc(mtm), initWithFrame: frame] };
    view.setWantsLayer(true);
    window.setContentView(Some(&view));

    let status = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let scale = window.backingScaleFactor() as f32;
    let Some(canvas) = Canvas::new((WIN_W * scale).ceil() as u32, (WIN_H * scale).ceil() as u32) else { return };
    let ask_hooks = !config.asked_hooks && !hooks_config::status(&exe_path(), config.port);
    let mut state = App {
        shared,
        library: library::load_all(&user_root()),
        config,
        canvas,
        text: TextDrawer::new(scale),
        scale,
        hits: Vec::new(),
        state: State::Idle,
        hovering: false,
        hover_until: 0,
        panel_until: 0,
        bubble_until: 0,
        scan_at: 0,
        next_scan: now_ms() + USAGE_SCAN_MS,
        known_states: Vec::new(),
        tray_state: State::Idle,
        visual: Visual::Vector(0),
        current_sounds: [None, None],
        load_token: 0,
        state_since: Instant::now(),
        sessions: Vec::new(),
        panel_open: false,
        hover: None,
        visible: true,
        drag: None,
        started: Instant::now(),
        frame_ms: 0,
        note_until: 0,
        ignoring: true,
        window,
        view,
        status,
        frame_timer: None,
    };
    state.apply_collection_behavior();
    match state.config.position {
        Some((x, y)) if on_a_screen(x, y, mtm) => state.window.setFrameOrigin(NSPoint::new(x as CGFloat, y as CGFloat)),
        _ => {
            let (x, y) = state.default_position();
            state.move_to(x, y);
        }
    }
    let target: Retained<BuddyView> = state.view.clone();
    let target: &AnyObject = &target;
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(WATCH_MS, target, sel!(watchTick:), None, true);
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(HOUSEKEEPING_MS as f64 / 1000.0, target, sel!(housekeepingTick:), None, true);
    }
    state.rebuild_status_menu();
    state.update_status();
    state.apply_character();
    state.window.orderFrontRegardless();
    usage::request_scan(state.shared.clone());
    APP.with(|a| *a.borrow_mut() = Some(state));
    with_app(App::refresh);
    // CLAUDE_BUDDY_RS_OPEN_PAGE opens the main window at start, for checking it on a machine nobody sits at.
    if let Some(page) = std::env::var("CLAUDE_BUDDY_RS_OPEN_PAGE").ok().and_then(|p| p.parse().ok()) {
        picker::open(page);
    }
    if ask_hooks {
        first_run_question();
    }
    // Files named on the command line, the way a double-click arrives when run by hand.
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).filter(|p| picker::is_pack(p)).collect();
    PENDING_FILES.lock().unwrap().extend(args);
    app.run();
    drop(delegate); // the application only holds on to it weakly
}

fn on_a_screen(x: i32, y: i32, mtm: MainThreadMarker) -> bool {
    let (x, y) = (x as f64, y as f64);
    NSScreen::screens(mtm).iter().any(|s| {
        let f = s.frame();
        x >= f.origin.x && y >= f.origin.y && x < f.origin.x + f.size.width && y < f.origin.y + f.size.height
    })
}

fn first_run_question() {
    let yes = host::confirm(
        std::ptr::null_mut(),
        "Claude Buddy",
        t("Kết nối với Claude Code để Buddy hiện trạng thái các phiên?\n\nApp sẽ thêm hook của riêng nó vào settings.json của Claude Code (có sao lưu trước, hook khác giữ nguyên). Có thể gỡ bất cứ lúc nào trong Cài đặt.", "Connect to Claude Code so Buddy can show your sessions?\n\nThe app adds its own hooks to Claude Code's settings.json (after a backup; other hooks stay as they are). You can remove them at any time in Settings."),
    );
    with_app(|a| {
        a.config.asked_hooks = true;
        a.config.save();
        if yes {
            a.set_hooks(true);
        }
    });
}
