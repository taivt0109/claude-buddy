//! The character picker window, plus its add/edit dialog. It exists only while open, so none of this
//! costs anything the rest of the time. Drawn the same way as the buddy (no toolkit, no browser);
//! only the two text boxes are the system's own controls, so typing methods and shortcuts behave normally.
//! Everything that talks to the system is in `host`.
//! Slow work (decoding, copying) runs on worker threads that post their results back.

use crate::host::{self, Edit, Surface, Win};
use crate::i18n::{self, grouped, t};
use crate::anim::{self, CharAnim, Player, THUMB_SIZE};
use crate::library::{self, Character, Draft, Slot, Source, STATE_KEYS, state_text};
use crate::media::Kind;
use crate::motion::{self, Motion};
use crate::render::{self, col, draw_badge_at, draw_character, draw_frame, oval, paint, round_rect, state_color, Font, Pen, TextDrawer};
use crate::server::now_ms;
use crate::store::State;
use crate::theme::{self, pal, Backdrop};
use crate::usage;
use crate::window::{self, user_root, Canvas, Setting, SettingsView};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tiny_skia::{FillRule, PixmapMut, Transform};

const GENERATE_W: f32 = 620.0;
const GENERATE_H: f32 = 320.0;
const PREVIEW_FRAME_MS: u32 = 33;
const TOAST_MS: u64 = 2600;
const SIDEBAR_W: f32 = 208.0;
const PAGE_USAGE: u8 = 1;
const PAGE_SETTINGS: u8 = 2;
const CHART_DAYS: i64 = 14;
/// Short on purpose: all of these sit beside the main button in the space next to the preview.
const HERO_ACTIONS: [[&str; 3]; 2] = [["Sửa", "Chia sẻ", "Xoá"], ["Edit", "Share", "Delete"]];

fn hero_actions() -> [&'static str; 3] {
    HERO_ACTIONS[if crate::i18n::vi() { 0 } else { 1 }]
}
/// The window can't get narrower than this, so that row always fits.
pub(crate) const MIN_WINDOW_W: f32 = 1080.0;
/// Enough for a slot showing everything it can: preview, file, kind, motion, background and remove.
const DIALOG_H: f32 = 510.0;
// Segoe Fluent Icons / Segoe MDL2 Assets
const ICON_CHARACTERS: &str = "\u{E77B}";
const ICON_USAGE: &str = "\u{E9D2}";
const ICON_ADD: &str = "\u{E710}";
const ICON_GENERATE: &str = "\u{E945}";
const ICON_EDIT: &str = "\u{E70F}";
const ICON_FOLDER: &str = "\u{E838}";
const ICON_DELETE: &str = "\u{E74D}";
const ICON_SHARE: &str = "\u{E72D}";
const ICON_CLOSE: &str = "\u{E711}";
const ICON_ACCEPT: &str = "\u{E73E}";
const ICON_SETTINGS: &str = "\u{E713}";
const ICON_PLUG: &str = "\u{E703}";
const ICON_SOUND: &str = "\u{E767}";
const ICON_BELL: &str = "\u{EA8F}";
const ICON_LANGUAGE: &str = "\u{F2B7}";
const ICON_POWER: &str = "\u{E7E8}";
const ICON_UPDATE: &str = "\u{E895}";
const ICON_TRANSLUCENT: &str = "\u{E771}";
const ICON_HIDE: &str = "\u{ED1A}";
const ICON_RESIZE: &str = "\u{E740}";
const ICON_MOVE: &str = "\u{E707}";
const BIG_FILE: u64 = 3 * 1024 * 1024;


/// Results handed back from worker threads.
enum JobResult {
    Thumb { id: String, px: Option<Vec<u8>> },
    Preview { token: u64, id: String, result: Result<Arc<CharAnim>, String> },
    SlotInfo { token: u64, index: usize, info: Result<library::FileInfo, String>, thumb: Option<Vec<u8>> },
    Saved { result: Result<(String, Option<String>), String> },
    Generated { result: Result<String, String> },
    Zipped { result: Result<String, String> },
}

static RESULTS: Mutex<Vec<JobResult>> = Mutex::new(Vec::new());
/// The open picker window, for posting to it from anywhere; 0 when closed.
static HWND_OPEN: AtomicIsize = AtomicIsize::new(0);

/// The window while it's open, otherwise null.
pub fn window() -> Win {
    HWND_OPEN.load(Ordering::Acquire) as Win
}

fn finish_job(hwnd: isize, r: JobResult) {
    RESULTS.lock().unwrap().push(r);
    host::post_job(hwnd);
}

fn spawn(work: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name("picker-job".into()).spawn(work);
}

#[derive(Clone, PartialEq, Debug)]
enum Action {
    Select(String),
    Use,
    Edit,
    Share,
    Remove,
    Tab(usize),
    Sort(bool),
    Add,
    Generate,
    GenStart,
    GenStop,
    GenClose,
    GenCopy,
    Redraw,
    Page(u8),
    Set(Setting),
    /// Hovering a day in the usage chart.
    Day(usize),
    DlgClose,
    DlgPickFiles,
    DlgPickSlot(usize),
    DlgRemoveSlot(usize),
    DlgToggleWhite(usize),
    DlgMotion(usize),
    DlgScale,
    DlgImport,
    DlgSave,
    DlgJson,
}

struct Hit {
    rect: (f32, f32, f32, f32),
    action: Action,
}

#[derive(Clone)]
struct SlotView {
    /// Original file name, or the kept file's name.
    label: String,
    path: Option<PathBuf>,
    keep: bool,
    ext: Option<String>,
    kind: Option<Kind>,
    size: u64,
    white_bg: bool,
    remove_white: bool,
    motion: Option<Motion>,
    thumb: Option<Vec<u8>>,
    /// Ready to save: inspected and accepted.
    ready: bool,
}

/// The "describe a character" window, and Codex working on it.
struct Generating {
    running: bool,
    error: String,
    started: std::time::Instant,
    done: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    /// What's on screen, to redraw only when it changes.
    shown: (usize, u64),
    /// Redrawing this character, instead of creating a new one.
    redrawing: Option<Character>,
    /// A line of good news, such as the prompt having been copied.
    info: String,
}

struct Dialog {
    editing: Option<Character>,
    scale: f32,
    slots: [Option<SlotView>; 5],
    error: String,
    show_json: bool,
    saving: bool,
    /// Results of older inspections of a slot are ignored. Per slot: reading a file for one slot must
    /// not cancel another slot's.
    tokens: [u64; 5],
}

impl Dialog {
    fn draft(&self, name: &str) -> Draft {
        let slots = std::array::from_fn(|i| match &self.slots[i] {
            None => Slot::Empty,
            Some(s) if s.keep => Slot::Keep { motion: s.motion },
            Some(s) => Slot::New { path: s.path.clone().unwrap_or_default(), motion: s.motion, remove_white: s.remove_white },
        });
        Draft { name: name.to_string(), scale: self.scale, slots }
    }
}

struct Picker {
    hwnd: Win,
    surface: Surface,
    canvas: Canvas,
    backdrop: Backdrop,
    text: Rc<TextDrawer>,
    scale: f32,
    characters: Vec<Character>,
    current: String,
    usage: usage::Days,
    usage_ready: bool,
    page: u8,
    settings: SettingsView,
    selected: String,
    preview_state: usize,
    sort_by_name: bool,
    scroll: f32,
    /// The settings page's own, once its rows outgrow a small window, and how far it can go.
    settings_scroll: f32,
    settings_max: f32,
    hits: Vec<Hit>,
    hover: Option<Action>,
    tracking: bool,
    thumbs: HashMap<String, Vec<u8>>,
    thumbs_asked: HashMap<String, bool>,
    player: Option<Player>,
    preview_error: Option<String>,
    preview_token: u64,
    preview_loading: bool,
    /// Pixel rectangle of the preview stage, the only part repainted while it animates.
    stage: (i32, i32, i32, i32),
    started: Instant,
    state_since: Instant,
    toast: Option<(String, u64)>,
    dialog: Option<Dialog>,
    edit_name: Edit,
    edit_prompt: Edit,
    generating: Option<Generating>,
    dragging_scale: bool,
    slider: (f32, f32),
}

thread_local! {
    static PICKER: RefCell<Option<Picker>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Picker) -> R) -> Option<R> {
    PICKER.with(|p| p.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

#[cfg(windows)]
pub use host::open_folder;
#[cfg(windows)]
pub use host::forward_escape;

/// Something changed that the picker shows, e.g. a session count or the current character.
pub fn notify_changed() {
    // The picker may be the one in use further up the stack, so this only ever posts.
    let h = HWND_OPEN.load(Ordering::Relaxed);
    if h != 0 {
        host::post_refresh(h);
    }
}

/// Opens on a page (characters, usage, settings), or brings the window forward when already open.
pub fn open(page: u8) {
    let existing = with(|p| {
        p.page = page;
        p.invalidate();
        p.hwnd
    });
    if let Some(h) = existing {
        host::bring_forward(h);
        return;
    }
    let Some(snapshot) = window::library_snapshot() else { return };
    let Some(made) = host::create_window() else { return };
    let (hwnd, size) = (made.hwnd, (made.canvas.w, made.canvas.h));
    let selected = snapshot.current.clone();
    let picker = Picker {
        hwnd,
        surface: made.surface,
        canvas: made.canvas,
        backdrop: made.backdrop,
        text: Rc::new(TextDrawer::new_ui(made.scale)),
        scale: made.scale,
        characters: snapshot.characters,
        current: snapshot.current,
        usage: snapshot.usage,
        usage_ready: snapshot.usage_ready,
        page,
        settings: window::settings_view().unwrap_or_default(),
        selected,
        preview_state: 0,
        sort_by_name: false,
        scroll: 0.0,
        settings_scroll: 0.0,
        settings_max: 0.0,
        hits: Vec::new(),
        hover: None,
        tracking: false,
        thumbs: HashMap::new(),
        thumbs_asked: HashMap::new(),
        player: None,
        preview_error: None,
        preview_token: 0,
        preview_loading: false,
        stage: (0, 0, 0, 0),
        started: Instant::now(),
        state_since: Instant::now(),
        toast: None,
        dialog: None,
        edit_prompt: made.edit_prompt,
        generating: None,
        edit_name: made.edit_name,
        dragging_scale: false,
        slider: (0.0, 0.0),
    };
    PICKER.with(|p| *p.borrow_mut() = Some(picker));
    HWND_OPEN.store(hwnd as isize, Ordering::Relaxed);
    with(|p| {
        p.ask_thumbs();
        p.load_preview();
    });
    host::show_window(hwnd, size, PREVIEW_FRAME_MS);
}

impl Picker {
    /// After the theme or the translucency choice changed.
    fn apply_look(&mut self) {
        self.backdrop = theme::apply_backdrop(self.hwnd);
        self.text = Rc::new(TextDrawer::new_ui(self.scale));
        self.invalidate();
    }

    fn k(&self) -> f32 {
        self.scale
    }

    /// Logical window size.
    fn logical(&self) -> (f32, f32) {
        (self.canvas.w as f32 / self.scale, self.canvas.h as f32 / self.scale)
    }

    fn selected_character(&self) -> Option<&Character> {
        self.characters.iter().find(|c| c.id == self.selected)
    }

    fn ask_thumbs(&mut self) {
        let hwnd = self.hwnd as isize;
        let todo: Vec<Character> = self
            .characters
            .iter()
            .filter(|c| !c.is_builtin() && !self.thumbs.contains_key(&c.id) && !self.thumbs_asked.contains_key(&c.id))
            .cloned()
            .collect();
        for c in &todo {
            self.thumbs_asked.insert(c.id.clone(), true);
        }
        if !todo.is_empty() {
            spawn(move || {
                for c in todo {
                    let px = anim::thumb_of(&c).ok();
                    finish_job(hwnd, JobResult::Thumb { id: c.id, px });
                }
            });
        }
    }

    fn load_preview(&mut self) {
        self.preview_token += 1;
        self.player = None;
        self.preview_error = None;
        self.preview_loading = false;
        self.state_since = Instant::now();
        let Some(c) = self.selected_character().cloned() else { return };
        if c.is_builtin() {
            return;
        }
        self.preview_loading = true;
        let (token, hwnd, dpi) = (self.preview_token, self.hwnd as isize, self.scale);
        spawn(move || {
            let max_h = (420.0 * c.scale.max(0.5)).min(PREVIEW_MAX_H);
            let result = anim::load(&c, PREVIEW_MAX_W, max_h, dpi);
            finish_job(hwnd, JobResult::Preview { token, id: c.id, result });
        });
    }

    fn select(&mut self, id: &str) {
        if self.selected == id {
            return;
        }
        self.selected = id.to_string();
        self.load_preview();
        self.invalidate();
    }

    fn invalidate(&self) {
        host::invalidate(self.hwnd);
    }

    fn refresh(&mut self, focus: Option<&str>) {
        let Some(s) = window::library_snapshot() else { return };
        let changed = s.characters != self.characters;
        self.characters = s.characters;
        self.current = s.current;
        self.usage = s.usage;
        self.usage_ready = s.usage_ready;
        self.settings = window::settings_view().unwrap_or(self.settings);
        let prev = self.selected.clone();
        let exists = |id: &str| self.characters.iter().any(|c| c.id == id);
        let next = [focus, Some(prev.as_str()), Some(self.current.as_str())]
            .into_iter()
            .flatten()
            .find(|id| exists(id))
            .map(str::to_string)
            .or_else(|| self.characters.first().map(|c| c.id.clone()))
            .unwrap_or_default();
        self.selected = next;
        if changed || self.selected != prev || focus.is_some() {
            if let Some(f) = focus {
                self.thumbs.remove(f);
                self.thumbs_asked.remove(f);
            }
            self.ask_thumbs();
            self.load_preview();
        }
        self.invalidate();
    }

    fn toast(&mut self, s: impl Into<String>) {
        self.toast = Some((s.into(), now_ms() + TOAST_MS));
        self.invalidate();
    }

    fn is_previewing_animated(&self) -> bool {
        match self.selected_character() {
            None => false,
            Some(c) if c.is_builtin() => true,
            Some(c) => {
                let m = &c.states[self.preview_state];
                m.motion != Motion::None || m.fallback || self.player.as_ref().is_some_and(|p| !p.clip().is_still())
            }
        }
    }

    // ---- painting ----

    fn paint_all(&mut self) {
        let (w, h) = (self.canvas.w, self.canvas.h);
        let buf = unsafe { std::slice::from_raw_parts_mut(self.canvas.bits, (w * h * 4) as usize) };
        let mut hits = Vec::new();
        let text = self.text.clone();
        let mut g = Gfx { buf, w, h, k: self.scale, text: &text, ox: SIDEBAR_W, backdrop: self.backdrop, solid_cards: false };
        g.fill_page();
        let (lw, lh) = (w as f32 / self.scale, h as f32 / self.scale);
        let content = lw - SIDEBAR_W;
        if self.page == PAGE_USAGE {
            self.paint_header(&mut g, content, TOP_H);
            self.paint_usage(&mut g, content, lh, &mut hits);
        } else if self.page == PAGE_SETTINGS {
            self.paint_settings(&mut g, content, lh, &mut hits);
            // The rows scroll under the title, and what is under it can't be clicked.
            self.paint_header(&mut g, content, SETTINGS_TOP - 4.0);
            hits.retain(|h| h.rect.1 >= SETTINGS_TOP - 4.0);
        } else {
            self.paint_grid(&mut g, content, lh, &mut hits);
            self.paint_header(&mut g, content, TOP_H);
            self.paint_top(&mut g, content, &mut hits);
        }
        hits.iter_mut().for_each(|h: &mut Hit| h.rect.0 += SIDEBAR_W);
        g.ox = 0.0;
        self.paint_sidebar(&mut g, lh, &mut hits);
        if self.dialog.is_some() {
            hits.clear(); // everything behind a modal dialog is out of reach
            g.solid_cards = true;
            self.paint_dialog(&mut g, lw, lh, &mut hits);
            g.solid_cards = false;
        }
        if self.generating.is_some() {
            hits.clear();
            g.solid_cards = true;
            self.paint_generate(&mut g, lw, lh, &mut hits);
            g.solid_cards = false;
        }
        if let Some((msg, until)) = &self.toast {
            if now_ms() < *until {
                let tw = (g.text_w(msg, Font::Normal) + 32.0).min(lw - 40.0);
                let (x, y) = ((lw - tw) / 2.0, lh - 52.0);
                g.rrect(x, y, tw, 34.0, 8.0, pal().ink);
                g.label(x + 16.0, y, tw - 32.0, 34.0, msg, Font::Normal, pal().bg);
            }
        }
        self.hits = hits;
        self.place_edits();
    }

    /// Navigation on the left, like Windows Settings.
    fn paint_sidebar(&mut self, g: &mut Gfx, lh: f32, hits: &mut Vec<Hit>) {
        let p = pal();
        g.page_rect(0.0, 0.0, SIDEBAR_W, lh);
        let hover = self.hover.clone();
        let items = [(ICON_CHARACTERS, t("Nhân vật", "Characters")), (ICON_USAGE, t("Mức dùng", "Usage")), (ICON_SETTINGS, t("Cài đặt", "Settings"))];
        for (i, (icon, name)) in items.iter().enumerate() {
            let y = 12.0 + i as f32 * 40.0;
            let act = Action::Page(i as u8);
            let on = self.page == i as u8;
            if on || hover.as_ref() == Some(&act) {
                g.rrect(6.0, y, SIDEBAR_W - 12.0, 36.0, 4.0, p.hover);
            }
            if on {
                g.rrect(6.0, y + 9.0, 3.0, 18.0, 1.5, p.accent);
            }
            g.icon(18.0, y, 24.0, 36.0, icon, p.ink);
            g.label(52.0, y, SIDEBAR_W - 60.0, 36.0, name, Font::Normal, p.ink);
            hits.push(Hit { rect: (6.0, y, SIDEBAR_W - 12.0, 36.0), action: act });
        }
        let using = self.characters.iter().find(|c| c.id == self.current).map_or("—".to_string(), |c| c.name.clone());
        g.label(18.0, lh - 50.0, SIDEBAR_W - 30.0, 18.0, t("Đang dùng", "In use"), Font::Small, p.muted);
        g.label(18.0, lh - 32.0, SIDEBAR_W - 30.0, 20.0, &using, Font::Normal, p.ink);
    }

    /// The page's title, on a band `h` tall that hides whatever scrolled under it.
    fn paint_header(&mut self, g: &mut Gfx, lw: f32, h: f32) {
        g.page_rect(0.0, 0.0, lw, h);
        let title = [t("Nhân vật", "Characters"), t("Mức dùng", "Usage"), t("Cài đặt", "Settings")][self.page.min(2) as usize];
        g.label(16.0, 10.0, lw - 32.0, 44.0, title, Font::Display, pal().ink);
    }

    /// One card per period, then the last two weeks day by day.
    fn paint_usage(&mut self, g: &mut Gfx, lw: f32, lh: f32, hits: &mut Vec<Hit>) {
        let p = pal();
        let today = usage::today();
        let periods = usage::periods(&self.usage, today);
        let titles = [
            (t("Hôm nay", "Today"), i18n::date(today)),
            (t("Tuần này", "This week"), crate::tf!("từ thứ Hai, {}", "from Monday, {}", i18n::date(usage::week_start(today)))),
            (t("Tháng này", "This month"), crate::tf!("từ {}", "from {}", i18n::date(usage::month_start(today)))),
        ];
        let rows = [t("Đầu vào", "Input"), t("Đầu ra", "Output"), t("Ghi cache", "Cache write"), t("Đọc cache", "Cache read")];
        let gap = 12.0;
        let (x0, y) = (16.0, 60.0);
        let w = (lw - 32.0 - gap * 2.0) / 3.0;
        const CARD_HEIGHT: f32 = 210.0;
        for (i, c) in periods.iter().enumerate() {
            let x = x0 + i as f32 * (w + gap);
            g.rrect(x, y, w, CARD_HEIGHT, 8.0, p.surface);
            g.stroke_rrect(x, y, w, CARD_HEIGHT, 8.0, p.stroke, 1.0);
            g.label(x + 16.0, y + 10.0, w - 32.0, 20.0, titles[i].0, Font::Bold, p.ink);
            g.label(x + 16.0, y + 29.0, w - 32.0, 18.0, &titles[i].1, Font::Small, p.muted);
            let big = if self.usage_ready { usage::short(usage::total(c)) } else { "…".to_string() };
            g.label(x + 16.0, y + 48.0, w - 32.0, 40.0, &big, Font::Display, p.ink);
            for (r, name) in rows.iter().enumerate() {
                // Cache reads sit under a rule: they are not part of the big number above.
                let ry = y + 94.0 + r as f32 * 26.0 + if r == 3 { 6.0 } else { 0.0 };
                if r == 3 {
                    g.rect(x + 16.0, ry - 2.0, w - 32.0, 1.0, p.stroke);
                }
                let color = if r == 3 { p.muted } else { p.ink };
                g.label(x + 16.0, ry, w * 0.5, 22.0, name, Font::Normal, color);
                let val = if self.usage_ready { grouped(c[r]) } else { "…".to_string() };
                let vw = g.text_w(&val, Font::Normal) + 2.0;
                g.label(x + w - 16.0 - vw, ry, vw, 22.0, &val, Font::Normal, color);
            }
        }
        let note_y = lh - 44.0;
        let cy = y + CARD_HEIGHT + gap;
        self.paint_chart(g, 16.0, cy, lw - 32.0, note_y - cy - 8.0, today, hits);
        let first = if self.usage_ready {
            t("Số lớn = đầu vào + đầu ra + ghi cache. Đọc cache tính riêng vì mỗi lượt đọc lại cùng ngữ cảnh.", "The big number is input + output + cache writes. Cache reads are separate: each turn re-reads the same context.")
        } else {
            t("Đang đếm từ toàn bộ transcript trên máy này, thường mất vài giây…", "Counting every transcript on this computer, usually takes a few seconds…")
        };
        g.label(18.0, note_y, lw - 36.0, 18.0, first, Font::Small, p.muted);
        g.label(18.0, note_y + 18.0, lw - 36.0, 18.0, t("Lấy từ transcript trên máy này, cả subagent; không gồm Claude dùng ở máy khác hay trên web.", "From transcripts on this computer, subagents included; Claude used on other machines or the web isn't counted."), Font::Small, p.muted);
    }

    /// A single series, so the title names it and no legend is needed. Exact numbers come on hover,
    /// and only the peak and today are labelled so the bars stay readable.
    #[allow(clippy::too_many_arguments)]
    fn paint_chart(&self, g: &mut Gfx, x: f32, y: f32, w: f32, h: f32, today: u32, hits: &mut Vec<Hit>) {
        if h < 90.0 {
            return; // too short a window to say anything; the cards above still have the numbers
        }
        let p = pal();
        g.rrect(x, y, w, h, 8.0, p.surface);
        g.stroke_rrect(x, y, w, h, 8.0, p.stroke, 1.0);
        g.label(x + 16.0, y + 8.0, w - 32.0, 22.0, t("14 ngày gần nhất", "Last 14 days"), Font::Bold, p.ink);
        let days = usage::recent_days(&self.usage, today, CHART_DAYS);
        let max = days.iter().map(|d| d.1).max().unwrap_or(0);
        let (px, py, pw, ph) = (x + 16.0, y + 54.0, w - 32.0, h - 54.0 - 26.0);
        let base = py + ph;
        g.rect(px, base, pw, 1.0, p.stroke);
        let slot = pw / days.len() as f32;
        let bar_w = (slot - 2.0).clamp(4.0, 22.0);
        let hover = self.hover.clone();
        for (i, (day, n)) in days.iter().enumerate() {
            let cx = px + slot * (i as f32 + 0.5);
            let is_today = i + 1 == days.len();
            let hot = hover == Some(Action::Day(i));
            if hot {
                g.rrect(cx - slot / 2.0 + 1.0, py - 4.0, slot - 2.0, ph + 4.0, 4.0, p.hover);
            }
            if max > 0 && *n > 0 {
                let bh = (ph * (*n as f32 / max as f32)).max(2.0);
                let top = base - bh;
                // Rounded at the data end only; the foot stands flat on the baseline.
                let color = p.accent;
                let alpha = if is_today || hot { 1.0 } else { 0.55 };
                g.bar(cx - bar_w / 2.0, top, bar_w, bh, color, alpha);
                if (*n == max || is_today) && !hot {
                    let label = usage::short(*n);
                    g.center(cx - slot, (top - 18.0).max(py - 18.0), slot * 2.0, 16.0, &label, Font::Small, p.ink);
                }
            }
            // Every other date, counted back from today, so today is always named.
            if (days.len() - 1 - i) % 2 == 0 {
                let text = if is_today { t("Hôm nay", "Today").to_string() } else { i18n::short_date(*day) };
                g.center(cx - slot, base + 5.0, slot * 2.0, 16.0, &text, Font::Small, p.muted);
            }
            hits.push(Hit { rect: (cx - slot / 2.0, py - 4.0, slot, ph + 24.0), action: Action::Day(i) });
        }
        if let Some(Action::Day(i)) = hover {
            if let Some((day, n)) = days.get(i) {
                let text = crate::tf!("{}: {} token", "{}: {} tokens", i18n::date(*day), grouped(*n));
                let tw = g.text_w(&text, Font::Small) + 20.0;
                let cx = px + slot * (i as f32 + 0.5);
                let tx = (cx - tw / 2.0).clamp(x + 6.0, x + w - tw - 6.0);
                g.rrect(tx, y + 8.0, tw, 24.0, 4.0, p.ink);
                g.label(tx + 10.0, y + 8.0, tw - 20.0, 24.0, &text, Font::Small, p.bg);
            }
        }
    }

    /// Windows Settings' card rows: what it is on the left, the control on the right.
    fn paint_settings(&mut self, g: &mut Gfx, lw: f32, lh: f32, hits: &mut Vec<Hit>) {
        let s = self.settings;
        let hover = self.hover.clone();
        let (x, w) = (16.0, lw - 32.0);
        self.settings_scroll = self.settings_scroll.clamp(0.0, self.settings_max);
        let mut y = SETTINGS_TOP - self.settings_scroll;
        let row = |g: &mut Gfx, icon: &str, title: &str, desc: &str, y0: &mut f32| {
            let p = pal();
            g.rrect(x, *y0, w, 60.0, 8.0, p.surface);
            g.stroke_rrect(x, *y0, w, 60.0, 8.0, p.stroke, 1.0);
            g.icon(x + 14.0, *y0, 26.0, 60.0, icon, p.ink);
            g.label(x + 52.0, *y0 + 10.0, w - 260.0, 20.0, title, Font::Normal, p.ink);
            g.label(x + 52.0, *y0 + 30.0, w - 260.0, 18.0, desc, Font::Small, p.muted);
            *y0 += 64.0;
            *y0 - 64.0
        };
        let update_note = crate::tf!("Đang ở bản {}. Không cài khi cửa sổ này đang mở", "On version {}. Doesn't install while this window is open", env!("CARGO_PKG_VERSION"));
        let keys = s.skill_keys.map(|k| if k == 0 { "-".to_string() } else { (k as char).to_string() }).join("/");
        let skills = crate::tf!(
            "Phân thân khi làm lâu, dịch chuyển khi xong hoặc cần bạn. Phím Ctrl+{keys}, đổi trong config.json",
            "Clones on long work, a teleport when done or needed. Keys Ctrl+{keys}, set in config.json"
        );
        let toggles: [(&str, &str, &str, bool, Setting); 8] = [
            (ICON_PLUG, t("Kết nối Claude Code", "Connect to Claude Code"), if s.hooks { t("Đang nhận trạng thái từ các phiên mới", "Receiving status from new sessions") } else { t("Chưa kết nối: buddy không biết Claude đang làm gì", "Not connected: Buddy can't tell what Claude is doing") }, s.hooks, Setting::Hooks),
            (ICON_HIDE, t("Tự ẩn khi toàn màn hình", "Hide during full screen"), t("Ẩn khi trình chiếu, xem phim hay chơi game, rồi tự hiện lại", "Hides for presentations, movies and games, then comes back"), s.auto_hide, Setting::AutoHide),
            (ICON_SOUND, t("Âm báo", "Sounds"), t("Khi Claude xong việc, cần bạn hoặc gặp lỗi", "When Claude finishes, needs you or hits an error"), s.sound, Setting::Sound),
            (ICON_BELL, if cfg!(windows) { t("Thông báo Windows", "Windows notifications") } else { t("Thông báo", "Notifications") }, if cfg!(windows) { t("Không hiện khi trình chiếu hoặc bật Focus Assist", "Not shown during presentations or Focus Assist") } else { t("Hiện ở góc màn hình khi Claude xong việc hoặc cần bạn", "Shown in the corner of the screen when Claude finishes or needs you") }, s.toasts, Setting::Toasts),
            (ICON_POWER, if cfg!(windows) { t("Khởi động cùng Windows", "Start with Windows") } else { t("Mở khi đăng nhập", "Open at login") }, t("Mở Claude Buddy khi đăng nhập", "Open Claude Buddy when you sign in"), s.autostart, Setting::Autostart),
            (ICON_UPDATE, t("Tự động cập nhật", "Update automatically"), &update_note, s.auto_update, Setting::AutoUpdate),
            (ICON_TRANSLUCENT, t("Hiệu ứng trong mờ", "Transparency effects"), t("Nền cửa sổ trong mờ theo Windows; tự tắt khi Windows tắt Transparency effects", "See-through window backgrounds; turns off when Windows turns off transparency effects"), s.translucent, Setting::Translucent),
            (ICON_GENERATE, t("Tự dùng skill theo phiên", "Skills follow the sessions"), &skills, s.auto_skills, Setting::AutoSkills),
        ];
        // Pages are solid on a Mac, so there is nothing for this one to change; nor skills there yet.
        for (icon, title, desc, on, setting) in toggles.into_iter().filter(|x| cfg!(windows) || !matches!(x.4, Setting::Translucent | Setting::AutoSkills)) {
            let ry = row(g, icon, title, desc, &mut y);
            g.toggle(x + w - 62.0, ry + 20.0, on, Action::Set(setting), &hover, hits);
        }
        let ry = row(g, ICON_LANGUAGE, t("Ngôn ngữ", "Language"), t("Áp dụng ngay cho toàn bộ app", "Applies to the whole app right away"), &mut y);
        let labels = [if cfg!(windows) { t("Theo Windows", "Windows default") } else { t("Theo hệ thống", "System default") }, "Tiếng Việt", "English"];
        let mut cx = x + w - 16.0 - labels.iter().map(|l| g.chip_w(l) + 4.0).sum::<f32>();
        for (i, l) in labels.iter().enumerate() {
            cx = g.chip(cx, ry + 16.0, l, s.language == i as u8, Action::Set(Setting::Language(i as u8)), &hover, hits) + 4.0;
        }
        let ry = row(g, ICON_RESIZE, t("Kích thước nhân vật", "Character size"), if cfg!(windows) { t("Hoặc lăn chuột trên nhân vật", "Or scroll on the character") } else { t("Nhân vật càng nhỏ càng ít che màn hình", "A smaller character covers less of your screen") }, &mut y);
        let labels = [t("Nhỏ", "Small"), t("Vừa", "Medium"), t("Lớn", "Large")];
        let mut cx = x + w - 16.0 - labels.iter().map(|l| g.chip_w(l) + 4.0).sum::<f32>();
        for (i, l) in labels.iter().enumerate() {
            cx = g.chip(cx, ry + 16.0, l, s.size == i as u8, Action::Set(Setting::Size(i as u8)), &hover, hits) + 4.0;
        }
        let ry = row(g, ICON_MOVE, t("Vị trí", "Position"), t("Kéo nhân vật tới bất kỳ đâu trên màn hình", "Drag the character anywhere on the screen"), &mut y);
        let bw = g.button_w(None, t("Về vị trí mặc định", "Reset position"));
        g.button(x + w - 16.0 - bw, ry + 14.0, None, t("Về vị trí mặc định", "Reset position"), Style::Normal, true, Action::Set(Setting::ResetPosition), &hover, hits);
        let ry = row(g, ICON_FOLDER, t("Thư mục nhân vật", "Character folder"), t("Nhân vật bạn thêm được lưu ở đây", "Characters you add are stored here"), &mut y);
        let bw = g.button_w(None, t("Mở thư mục", "Open folder"));
        g.button(x + w - 16.0 - bw, ry + 14.0, None, t("Mở thư mục", "Open folder"), Style::Normal, true, Action::Set(Setting::Folder), &hover, hits);
        // Too many rows for the window: they scroll, with the same thin marker as the character grid.
        let (content, view) = (y + self.settings_scroll - SETTINGS_TOP, lh - SETTINGS_TOP - 8.0);
        self.settings_max = (content - view).max(0.0);
        if self.settings_max > 0.0 {
            let th = (view * view / content).max(24.0);
            g.rrect(lw - 7.0, SETTINGS_TOP + self.settings_scroll / self.settings_max * (view - th), 3.0, th, 1.5, pal().muted);
        }
    }

    fn paint_top(&mut self, g: &mut Gfx, lw: f32, hits: &mut Vec<Hit>) {
        // Hero card: live preview on the left, details on the right
        let (bx, by, bw) = (16.0, 60.0, lw - 32.0);
        g.rrect(bx, by, bw, HERO_H, 8.0, pal().surface);
        g.stroke_rrect(bx, by, bw, HERO_H, 8.0, pal().stroke, 1.0);
        let (sx, sy, sw, sh) = (bx + 14.0, by + 14.0, PREVIEW_MAX_W, HERO_H - 28.0);
        let k = self.k();
        self.stage = (((sx + g.ox) * k) as i32, (sy * k) as i32, (sw * k) as i32, (sh * k) as i32);
        let ox = std::mem::take(&mut g.ox); // the stage rectangle is already absolute
        self.paint_stage(g);
        g.ox = ox;

        let (ix, iw) = (sx + sw + 22.0, bw - sw - 64.0);
        let mut y = by + 16.0;
        let Some(c) = self.selected_character().cloned() else { return };
        let in_use = c.id == self.current;
        g.label(ix, y, iw, 30.0, &c.name, Font::Title, pal().ink);
        y += 32.0;
        if !c.description.is_empty() {
            g.label(ix, y, iw, 18.0, &c.description, Font::Small, pal().muted);
            y += 20.0;
        }
        let hover = self.hover.clone();
        let meta = match (c.is_builtin(), c.author.is_empty()) {
            (true, _) => t("Có sẵn trong app", "Built in").to_string(),
            (false, true) => t("Do bạn thêm", "Added by you").to_string(),
            (false, false) => crate::tf!("Do bạn thêm · tác giả {}", "Added by you · by {}", c.author),
        };
        g.label(ix, y, iw, 18.0, &meta, Font::Small, pal().muted);
        if !c.is_builtin() && generated_dir(&c.id).join("idle.png").is_file() {
            let link = t("Sửa bằng Codex…", "Edit with Codex…");
            let (lx, lw) = (ix + g.text_w(&meta, Font::Small) + 18.0, g.text_w(link, Font::Small) + 4.0);
            g.link(lx, y, lw, link, Action::Redraw, &hover, hits);
        }
        y += 28.0;
        // These rows choose what the preview beside them shows.
        g.label(ix, y, iw, 18.0, t("Xem thử trạng thái", "Preview states"), Font::Small, pal().muted);
        y += 20.0;
        for (i, name) in state_text().iter().enumerate() {
            let act = Action::Tab(i);
            let on = self.preview_state == i;
            if on || hover.as_ref() == Some(&act) {
                g.rrect(ix - 6.0, y, iw.min(340.0), 26.0, 4.0, pal().hover);
            }
            if on {
                g.rrect(ix - 6.0, y + 6.0, 3.0, 14.0, 1.5, pal().accent);
            }
            g.circle(ix + 9.0, y + 13.0, 4.5, state_color(library::STATES[i]));
            let m = &c.states[i];
            g.label(ix + 22.0, y, 90.0, 26.0, name, if on { Font::Bold } else { Font::Normal }, pal().ink);
            if !c.is_builtin() {
                let how = if m.fallback {
                    t("dùng hình Rảnh", "uses the Idle image")
                } else if m.kind != Kind::Static && m.motion == Motion::None {
                    kind_text(m.kind)
                } else {
                    m.motion.text()
                };
                g.label(ix + 116.0, y, iw.min(340.0) - 126.0, 26.0, how, Font::Small, pal().muted);
            }
            hits.push(Hit { rect: (ix - 6.0, y, iw.min(340.0), 26.0), action: act });
            y += 27.0;
        }
        // Buttons, at the bottom of the band
        let mut x = ix;
        let by2 = by + HERO_H - 46.0;
        let use_label = if in_use { t("Đang dùng", "In use") } else { t("Dùng nhân vật", "Use character") };
        let use_icon = if in_use { Some(ICON_ACCEPT) } else { None };
        x = g.button(x, by2, use_icon, use_label, Style::Primary, !in_use, Action::Use, &hover, hits) + 8.0;
        if !c.is_builtin() {
            x = g.button(x, by2, Some(ICON_EDIT), hero_actions()[0], Style::Normal, true, Action::Edit, &hover, hits) + 8.0;
            x = g.button(x, by2, Some(ICON_SHARE), hero_actions()[1], Style::Normal, true, Action::Share, &hover, hits) + 8.0;
            if !in_use {
                g.button(x, by2, Some(ICON_DELETE), hero_actions()[2], Style::Danger, true, Action::Remove, &hover, hits);
            }
        }

        // Count and sort, kept quiet: this row organises, it isn't what you came for.
        let y = by + HERO_H + 12.0;
        let count = crate::tf!("{} nhân vật", "{} characters", self.characters.len());
        g.label(18.0, y, 300.0, 28.0, &count, Font::Normal, pal().muted);
        let right = lw - 16.0;
        let name_w = g.chip_w(t("Tên", "Name"));
        let def_w = g.chip_w(t("Mặc định", "Default"));
        let sx = right - name_w - def_w - 4.0;
        g.chip(sx, y, t("Mặc định", "Default"), !self.sort_by_name, Action::Sort(false), &hover, hits);
        g.chip(sx + def_w + 4.0, y, t("Tên", "Name"), self.sort_by_name, Action::Sort(true), &hover, hits);
        g.label(sx - 72.0, y, 66.0, 28.0, t("Sắp xếp", "Sort"), Font::Small, pal().muted);
    }

    fn paint_stage(&mut self, g: &mut Gfx) {
        let k = self.k();
        let (x, y, w, h) = self.stage;
        let (lx, ly, lw, lh) = (x as f32 / k, y as f32 / k, w as f32 / k, h as f32 / k);
        g.rrect(lx, ly, lw, lh, 6.0, pal().bg);
        let Some(c) = self.selected_character().cloned() else { return };
        let state_i = self.preview_state;
        let state = library::STATES[state_i];
        let t = self.started.elapsed().as_secs_f32();
        match &c.source {
            Source::Vector(kind) => {
                let size = lw.min(lh) * 0.92;
                let f = size * k / 200.0;
                let base = Transform::from_translate((lx + (lw - size) / 2.0) * k, (ly + lh - size - 4.0) * k).pre_scale(f, f);
                if let Some(mut pm) = PixmapMut::from_bytes(g.buf, g.w, g.h) {
                    draw_character(&mut pm, base, *kind, state, t);
                }
            }
            Source::Folder(_) => {
                if let Some(p) = self.player.as_mut() {
                    p.advance(now_ms());
                    let m = &c.states[state_i];
                    let xf = m.motion.at(self.state_since.elapsed().as_secs_f32());
                    let (fw, fh) = (p.anim.w, p.anim.h);
                    let feet_x = (lx + lw / 2.0) * k;
                    let feet_y = (ly + lh - 6.0) * k;
                    if let Some(mut pm) = PixmapMut::from_bytes(g.buf, g.w, g.h) {
                        draw_frame(&mut pm, &p.pixels, fw, fh, feet_x, feet_y, k, xf);
                        if (m.fallback || p.anim.failed[state_i]) && state_i != 0 {
                            draw_badge_at(&mut pm, feet_x + fw as f32 / 2.0 - 18.0 * k, feet_y - fh as f32 + 18.0 * k, k, state, t);
                        }
                    }
                } else if let Some(e) = &self.preview_error {
                    g.label(lx + 12.0, ly + lh / 2.0 - 20.0, lw - 24.0, 20.0, i18n::t("Không tải được hình", "Could not load media"), Font::Bold, pal().danger);
                    g.label(lx + 12.0, ly + lh / 2.0, lw - 24.0, 18.0, e, Font::Small, pal().muted);
                } else if self.preview_loading {
                    g.label(lx + 12.0, ly + lh / 2.0 - 10.0, lw - 24.0, 20.0, i18n::t("Đang chuẩn bị…", "Preparing…"), Font::Normal, pal().muted);
                }
            }
        }
    }

    fn sorted(&self) -> Vec<Character> {
        let mut list = self.characters.clone();
        if self.sort_by_name {
            list.sort_by_key(|c| library::slugify(&c.name));
        }
        list
    }

    fn grid_metrics(&self, lw: f32, count: usize) -> (usize, f32) {
        let usable = lw - 32.0;
        let cols = ((usable + GRID_GAP) / (CARD_W + GRID_GAP)).floor().max(1.0) as usize;
        let rows = (count + 1).div_ceil(cols);
        (cols, rows as f32 * (CARD_H + GRID_GAP))
    }

    fn clamp_scroll(&mut self) {
        let (lw, lh) = self.logical();
        let (_, content) = self.grid_metrics(lw - SIDEBAR_W, self.characters.len() + 1);
        let view = lh - GRID_TOP - 8.0;
        self.scroll = self.scroll.clamp(0.0, (content - view).max(0.0));
    }

    fn paint_grid(&mut self, g: &mut Gfx, lw: f32, lh: f32, hits: &mut Vec<Hit>) {
        self.clamp_scroll();
        // Thin scroll indicator when not everything fits, as Windows 11 lists have
        let (_, content_h) = self.grid_metrics(lw, self.characters.len() + 1);
        let view_h = lh - GRID_TOP - 8.0;
        let scroll_marker = (content_h > view_h).then(|| {
            let th = (view_h * view_h / content_h).max(24.0);
            (GRID_TOP + self.scroll / (content_h - view_h) * (view_h - th), th)
        });
        let list = self.sorted();
        let (cols, _) = self.grid_metrics(lw, list.len() + 1);
        let hover = self.hover.clone();
        let total = list.len() + 2;
        for n in 0..total {
            let (row, col) = (n / cols, n % cols);
            let x = 16.0 + col as f32 * (CARD_W + GRID_GAP);
            let y = GRID_TOP + row as f32 * (CARD_H + GRID_GAP) - self.scroll;
            if y + CARD_H < GRID_TOP || y > lh {
                continue;
            }
            if n == list.len() {
                // "Add" card
                let hot = hover == Some(Action::Add);
                g.rrect(x, y, CARD_W, CARD_H, 8.0, if hot { pal().hover } else { pal().surface });
                g.stroke_rrect(x, y, CARD_W, CARD_H, 8.0, pal().stroke, 1.0);
                g.icon(x, y + 36.0, CARD_W, 36.0, ICON_ADD, pal().accent);
                g.center(x, y + 76.0, CARD_W, 20.0, t("Thêm nhân vật", "Add character"), Font::Bold, pal().ink);
                g.center(x, y + 96.0, CARD_W, 18.0, t("hoặc thả file vào", "or drop files here"), Font::Small, pal().muted);
                hits.push(Hit { rect: (x, y, CARD_W, CARD_H), action: Action::Add });
                continue;
            }
            if n == list.len() + 1 {
                let hot = hover == Some(Action::Generate);
                g.rrect(x, y, CARD_W, CARD_H, 8.0, if hot { pal().hover } else { pal().surface });
                g.stroke_rrect(x, y, CARD_W, CARD_H, 8.0, pal().stroke, 1.0);
                g.icon(x, y + 36.0, CARD_W, 36.0, ICON_GENERATE, pal().accent);
                g.center(x, y + 76.0, CARD_W, 20.0, t("Tạo bằng Codex", "Create with Codex"), Font::Bold, pal().ink);
                g.center(x, y + 96.0, CARD_W, 18.0, t("từ mô tả của bạn", "from your description"), Font::Small, pal().muted);
                hits.push(Hit { rect: (x, y, CARD_W, CARD_H), action: Action::Generate });
                continue;
            }
            let c = &list[n];
            let action = Action::Select(c.id.clone());
            let picked = c.id == self.selected;
            g.rrect(x, y, CARD_W, CARD_H, 8.0, if hover.as_ref() == Some(&action) { pal().hover } else { pal().surface });
            g.stroke_rrect(x, y, CARD_W, CARD_H, 8.0, if picked { pal().accent } else { pal().stroke }, if picked { 2.0 } else { 1.0 });
            let (tx, ty, ts) = (x + (CARD_W - THUMB_SHOWN) / 2.0, y + 8.0, THUMB_SHOWN);
            match &c.source {
                Source::Vector(kind) => {
                    let f = ts * self.scale / 200.0;
                    let base = Transform::from_translate((tx + g.ox) * self.scale, ty * self.scale).pre_scale(f, f);
                    if let Some(mut pm) = PixmapMut::from_bytes(g.buf, g.w, g.h) {
                        draw_character(&mut pm, base, *kind, State::Idle, 0.9); // thumbnails hold still
                    }
                }
                Source::Folder(_) => match self.thumbs.get(&c.id) {
                    Some(px) => g.thumb(px, tx, ty, ts),
                    None => g.center(tx, ty + ts / 2.0 - 14.0, ts, 28.0, &c.name.chars().next().map_or("?".into(), |x| x.to_uppercase().to_string()), Font::Title, pal().muted),
                },
            }
            g.center(x + 6.0, y + 112.0, CARD_W - 12.0, 20.0, &c.name, Font::Bold, pal().ink);
            if c.id == self.current {
                g.center(x + 6.0, y + 132.0, CARD_W - 12.0, 18.0, t("Đang dùng", "In use"), Font::Small, pal().accent);
            }
            hits.push(Hit { rect: (x, y, CARD_W, CARD_H), action });
        }
        if let Some((ty, th)) = scroll_marker {
            g.rrect(lw - 7.0, ty, 3.0, th, 1.5, pal().muted);
        }
    }

    fn paint_generate(&mut self, g: &mut Gfx, lw: f32, lh: f32, hits: &mut Vec<Hit>) {
        g.rect_a(0.0, 0.0, lw, lh, 0x000000, 0.4);
        let (dw, dh) = (GENERATE_W.min(lw - 32.0), GENERATE_H.min(lh - 32.0));
        let (dx, dy) = ((lw - dw) / 2.0, (lh - dh) / 2.0);
        g.rrect(dx, dy, dw, dh, 8.0, pal().surface);
        g.stroke_rrect(dx, dy, dw, dh, 8.0, pal().stroke, 1.0);
        let hover = self.hover.clone();
        let Some(gen) = self.generating.as_ref() else { return };
        let (running, error, info) = (gen.running, gen.error.clone(), gen.info.clone());
        let (done, minutes) = (gen.done.load(Ordering::Relaxed), gen.started.elapsed().as_secs() / 60);
        let redrawing = gen.redrawing.as_ref().map(|c| c.name.clone());
        let title = match &redrawing {
            Some(name) => crate::tf!("Sửa {name} bằng Codex", "Edit {name} with Codex"),
            None => t("Tạo nhân vật bằng Codex", "Create a character with Codex").to_string(),
        };
        g.label(dx + 18.0, dy + 12.0, dw - 70.0, 28.0, &title, Font::Title, pal().ink);
        let (bx, by) = (dx + dw - 18.0, dy + dh - 50.0);
        if running {
            g.label(dx + 18.0, dy + 70.0, dw - 36.0, 22.0, &crate::tf!("Codex đang vẽ: xong {done}/5 ảnh · {minutes} phút", "Codex is drawing: {done} of 5 images · {minutes} min"), Font::Bold, pal().ink);
            let bar = dw - 36.0;
            g.rrect(dx + 18.0, dy + 102.0, bar, 6.0, 3.0, pal().stroke);
            g.rrect(dx + 18.0, dy + 102.0, (bar * (done as f32 + 0.3) / 5.0).min(bar), 6.0, 3.0, pal().accent);
            g.label(dx + 18.0, dy + 122.0, dw - 36.0, 18.0, t("Thường mất 6 đến 10 phút. Bạn cứ làm việc khác, nhưng đừng đóng cửa sổ này.", "Usually takes 6 to 10 minutes. Carry on with other work, but keep this window open."), Font::Small, pal().muted);
            let w = g.button_w(None, t("Huỷ", "Cancel"));
            g.button(bx - w, by, None, t("Huỷ", "Cancel"), Style::Normal, true, Action::GenStop, &hover, hits);
            return;
        }
        let close = (dx + dw - 40.0, dy + 10.0, 28.0, 28.0);
        if hover == Some(Action::GenClose) {
            g.rrect(close.0, close.1, close.2, close.3, 4.0, pal().hover);
        }
        g.icon(close.0, close.1, close.2, close.3, ICON_CLOSE, pal().ink);
        hits.push(Hit { rect: close, action: Action::GenClose });
        let (heading, example) = if redrawing.is_some() {
            (t("Bạn muốn đổi gì?", "What should change?"), t("Ví dụ: đội thêm mũ đỏ, tai to hơn, đổi áo sang màu xanh", "For example: add a red hat, bigger ears, make the suit blue"))
        } else {
            (t("Mô tả nhân vật bạn muốn", "Describe the character you want"), t("Ví dụ: chú mèo phi hành gia tròn trịa, mặc áo đỏ, tính cách vui vẻ", "For example: a round astronaut cat in a red suit, cheerful"))
        };
        g.label(dx + 18.0, dy + 46.0, dw - 36.0, 20.0, heading, Font::Bold, pal().ink);
        g.label(dx + 18.0, dy + 66.0, dw - 36.0, 18.0, example, Font::Small, pal().muted);
        // The text box itself is placed in place_edits().
        let focused = self.edit_prompt.is_focused();
        let (ex, ey, ew, eh) = (dx + 18.0, dy + 90.0, dw - 36.0, 118.0);
        g.rrect(ex, ey, ew, eh, 4.0, pal().surface);
        g.stroke_rrect(ex, ey, ew, eh, 4.0, pal().stroke, 1.0);
        g.rect(ex + 2.0, ey + eh - 2.0, ew - 4.0, if focused { 2.0 } else { 1.0 }, if focused { pal().accent } else { pal().muted });
        let has_codex = crate::codex::find().is_some();
        let (line, color) = if !error.is_empty() {
            (error.chars().take(96).collect::<String>(), pal().danger)
        } else if !info.is_empty() {
            (info, pal().accent)
        } else if !has_codex {
            (t("Không tìm thấy Codex trên máy này. Bạn vẫn có thể tạo bằng ChatGPT.", "Codex wasn't found on this computer. You can still create it with ChatGPT.").to_string(), pal().muted)
        } else {
            (t("Mất khoảng 6 đến 10 phút và dùng hạn mức Codex của bạn.", "Takes about 6 to 10 minutes and uses your Codex quota.").to_string(), pal().muted)
        };
        g.label(dx + 18.0, dy + 214.0, dw - 36.0, 18.0, &line, Font::Small, color);
        if redrawing.is_none() {
            let copy = t("Sao chép prompt cho ChatGPT", "Copy prompt for ChatGPT");
            let cw = g.button_w(None, copy);
            g.button(dx + 18.0, by, None, copy, Style::Normal, true, Action::GenCopy, &hover, hits);
            let _ = cw;
        }
        let mut right = bx;
        if has_codex {
            let label = if redrawing.is_some() { t("Vẽ lại", "Redraw") } else { t("Tạo nhân vật", "Create character") };
            let w = g.button_w(Some(ICON_GENERATE), label);
            right -= w;
            g.button(right, by, Some(ICON_GENERATE), label, Style::Primary, true, Action::GenStart, &hover, hits);
            right -= 8.0;
        }
        let cancel = t("Huỷ", "Cancel");
        let cw = g.button_w(None, cancel);
        g.button(right - cw, by, None, cancel, Style::Normal, true, Action::GenClose, &hover, hits);
    }

    fn open_generate(&mut self, redrawing: Option<Character>) {
        self.generating = Some(Generating { running: false, error: String::new(), started: std::time::Instant::now(), done: Arc::new(AtomicUsize::new(0)), cancel: Arc::new(AtomicBool::new(false)), shown: (0, 0), redrawing, info: String::new() });
        self.edit_prompt.set_text("");
        self.place_edits();
        self.edit_prompt.focus();
        self.invalidate();
    }

    fn copy_chatgpt_prompt(&mut self) {
        let description = self.edit_prompt.text();
        let hwnd = self.hwnd;
        let Some(gen) = self.generating.as_mut() else { return };
        gen.error.clear();
        gen.info.clear();
        if description.trim().is_empty() {
            gen.error = t("Hãy mô tả nhân vật bạn muốn.", "Describe the character you want.").into();
        } else if host::copy_text(hwnd, &crate::codex::chatgpt_prompt_for(&description)) {
            gen.info = t("Đã sao chép. Dán vào ChatGPT, tải file zip về rồi kéo vào cửa sổ này.", "Copied. Paste it into ChatGPT, download the ZIP, then drag it onto this window.").into();
        } else {
            gen.error = t("Không sao chép được, hãy thử lại.", "Could not copy it, try again.").into();
        }
        self.invalidate();
    }

    /// A ZIP from ChatGPT holds the same sheets Codex would have drawn.
    fn add_zip(&mut self, path: &Path) {
        let hwnd = self.hwnd as isize;
        let (path, taken) = (path.to_path_buf(), self.characters.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
        self.toast(t("Đang thêm nhân vật từ file zip…", "Adding the character from the ZIP…"));
        spawn(move || {
            let result = character_from_zip(&path, &taken);
            finish_job(hwnd, JobResult::Zipped { result });
        });
        self.invalidate();
    }

    /// Everything after the button runs on a worker: drawing takes minutes.
    fn start_generate(&mut self) {
        let description = self.edit_prompt.text();
        let hwnd = self.hwnd as isize;
        let Some(gen) = self.generating.as_mut() else { return };
        if description.trim().is_empty() {
            gen.error = if gen.redrawing.is_some() { t("Hãy nói bạn muốn đổi gì.", "Say what should change.") } else { t("Hãy mô tả nhân vật bạn muốn.", "Describe the character you want.") }.into();
            self.invalidate();
            return;
        }
        gen.running = true;
        gen.error.clear();
        gen.started = std::time::Instant::now();
        gen.done.store(0, Ordering::Relaxed);
        gen.cancel.store(false, Ordering::Relaxed);
        gen.info.clear();
        let (cancel, done, redrawing) = (gen.cancel.clone(), gen.done.clone(), gen.redrawing.clone());
        let taken: Vec<String> = self.characters.iter().map(|c| c.id.clone()).collect();
        spawn(move || {
            let result = match &redrawing {
                Some(c) => redraw_character(c, &description, &cancel, &done).map(|()| c.id.clone()),
                None => generate_character(&description, &taken, &cancel, &done),
            };
            finish_job(hwnd, JobResult::Generated { result });
        });
        self.place_edits();
        self.invalidate();
    }

    fn paint_dialog(&mut self, g: &mut Gfx, lw: f32, lh: f32, hits: &mut Vec<Hit>) {
        g.rect_a(0.0, 0.0, lw, lh, 0x000000, 0.4);
        let (dw, dh) = (lw.min(920.0) - 32.0, (lh - 32.0).min(DIALOG_H));
        let (dx, dy) = ((lw - dw) / 2.0, (lh - dh) / 2.0);
        g.rrect(dx, dy, dw, dh, 8.0, pal().surface);
        g.stroke_rrect(dx, dy, dw, dh, 8.0, pal().stroke, 1.0);
        let hover = self.hover.clone();
        let Some(d) = self.dialog.as_ref() else { return };
        let title = match &d.editing {
            Some(c) => crate::tf!("Sửa nhân vật: {}", "Edit character: {}", c.name),
            None => t("Thêm nhân vật", "Add character").to_string(),
        };
        g.label(dx + 18.0, dy + 12.0, dw - 70.0, 28.0, &title, Font::Title, pal().ink);
        let close = (dx + dw - 40.0, dy + 10.0, 28.0, 28.0);
        if hover == Some(Action::DlgClose) {
            g.rrect(close.0, close.1, close.2, close.3, 4.0, pal().hover);
        }
        g.icon(close.0, close.1, close.2, close.3, ICON_CLOSE, pal().ink);
        hits.push(Hit { rect: close, action: Action::DlgClose });

        // Name and scale. The text box themselves are placed in place_edits().
        let y = dy + 50.0;
        g.label(dx + 18.0, y, 40.0, 32.0, t("Tên", "Name"), Font::Bold, pal().ink);
        // The text box's frame, with the accent line Windows 11 inputs have
        let focused = self.edit_name.is_focused();
        g.rrect(dx + 56.0, y, 214.0, 32.0, 4.0, pal().surface);
        g.stroke_rrect(dx + 56.0, y, 214.0, 32.0, 4.0, pal().stroke, 1.0);
        g.rect(dx + 58.0, y + 30.0, 210.0, if focused { 2.0 } else { 1.0 }, if focused { pal().accent } else { pal().muted });
        let sl = (dx + 380.0, 200.0);
        g.label(dx + 286.0, y, 90.0, 32.0, &crate::tf!("Cỡ {:.0}%", "Size {:.0}%", d.scale * 100.0), Font::Bold, pal().ink);
        g.rrect(sl.0, y + 14.0, sl.1, 4.0, 2.0, pal().stroke);
        let f = (d.scale - 0.5).clamp(0.0, 1.0);
        g.rrect(sl.0, y + 14.0, (sl.1 * f).max(4.0), 4.0, 2.0, pal().accent);
        g.circle(sl.0 + sl.1 * f, y + 16.0, 10.0, pal().surface);
        g.circle(sl.0 + sl.1 * f, y + 16.0, 6.0, pal().accent);
        hits.push(Hit { rect: (sl.0 - 10.0, y, sl.1 + 20.0, 32.0), action: Action::DlgScale });
        self.slider = sl;

        // Drop zone
        let zy = y + 42.0;
        let zone = (dx + 18.0, zy, dw - 36.0, 48.0);
        g.rrect(zone.0, zone.1, zone.2, zone.3, 4.0, if hover == Some(Action::DlgPickFiles) { pal().hover } else { pal().bg });
        g.stroke_rrect(zone.0, zone.1, zone.2, zone.3, 4.0, pal().stroke, 1.0);
        g.center(zone.0, zone.1 + 4.0, zone.2, 20.0, t("Thả các file vào đây: tên có idle, work, wait, done, error sẽ tự vào đúng ô", "Drop files here: names with idle, work, wait, done or error go to the right slot"), Font::Normal, pal().ink);
        g.center(zone.0, zone.1 + 23.0, zone.2, 18.0, t("hoặc bấm để chọn. Nhận webm, mp4, png, apng, gif, webp, svg, jpg", "or click to choose. Accepts webm, mp4, png, apng, gif, webp, svg, jpg"), Font::Small, pal().muted);
        hits.push(Hit { rect: zone, action: Action::DlgPickFiles });

        // Footer
        let fy = dy + dh - 46.0;
        let mut footer_x = dx + 18.0;
        if d.editing.is_none() {
            footer_x = g.button(footer_x, fy, Some(ICON_FOLDER), t("Nhập thư mục nhân vật có sẵn…", "Import a character folder…"), Style::Normal, true, Action::DlgImport, &hover, hits) + 8.0;
        }
        let json_label = if d.show_json { t("Ẩn character.json", "Hide character.json") } else { t("Xem character.json", "Show character.json") };
        g.button(footer_x, fy, None, json_label, Style::Normal, true, Action::DlgJson, &hover, hits);
        let save_label = if d.saving {
            t("Đang lưu…", "Saving…")
        } else if d.editing.is_some() {
            t("Lưu thay đổi", "Save changes")
        } else {
            t("Lưu nhân vật", "Save character")
        };
        let sw = g.button_w(None, save_label);
        g.button(dx + dw - 18.0 - sw, fy, None, save_label, Style::Primary, !d.saving, Action::DlgSave, &hover, hits);

        // Messages above the footer
        let warnings = warnings_of(d);
        let mut my = fy - 6.0;
        if !d.error.is_empty() {
            my -= 20.0;
            g.label(dx + 18.0, my, dw - 36.0, 20.0, &d.error, Font::Bold, pal().danger);
        }
        for w in warnings.iter().rev().take(2) {
            my -= 18.0;
            g.label(dx + 18.0, my, dw - 36.0, 18.0, &format!("⚠ {w}"), Font::Small, pal().warning);
        }

        // Slots, or the manifest they would produce
        let area = (dx + 18.0, zy + 56.0, dw - 36.0, my - (zy + 56.0) - 4.0);
        if d.show_json {
            let name = self.edit_name.text();
            let id = d.editing.as_ref().map_or_else(|| library::slugify(&name), |c| c.id.clone());
            let draft = d.draft(&name);
            let exts = std::array::from_fn(|i| d.slots[i].as_ref().and_then(|s| if s.remove_white { Some(".png".to_string()) } else { s.ext.clone() }));
            let kinds = std::array::from_fn(|i| d.slots[i].as_ref().and_then(|s| s.kind));
            let json = library::manifest_preview(&draft, if id.is_empty() { "…" } else { &id }, &exts, &kinds);
            g.rrect(area.0, area.1, area.2, area.3, 4.0, pal().bg);
            let mut ly = area.1 + 8.0;
            for l in json.lines() {
                if ly + 16.0 > area.1 + area.3 {
                    break;
                }
                g.label(area.0 + 12.0, ly, area.2 - 24.0, 16.0, l, Font::Mono, pal().ink);
                ly += 16.0;
            }
        } else {
            let sw = (area.2 - 4.0 * 8.0) / 5.0;
            for i in 0..5 {
                self.paint_slot(g, i, area.0 + i as f32 * (sw + 8.0), area.1, sw, area.3, &hover, hits);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_slot(&self, g: &mut Gfx, i: usize, x: f32, y: f32, w: f32, h: f32, hover: &Option<Action>, hits: &mut Vec<Hit>) {
        let Some(d) = self.dialog.as_ref() else { return };
        g.rrect(x, y, w, h, 8.0, pal().bg);
        g.stroke_rrect(x, y, w, h, 8.0, pal().stroke, 1.0);
        let title = format!("{}{}", state_text()[i], if i == 0 { " *" } else { "" });
        g.label(x + 10.0, y + 6.0, w - 20.0, 20.0, &title, Font::Bold, pal().ink);
        g.label(x + 10.0, y + 25.0, w - 20.0, 16.0, STATE_KEYS[i], Font::Mono, pal().muted);
        let box_size = (w - 20.0).min(h - 150.0).clamp(50.0, 150.0);
        let (bx, by) = (x + (w - box_size) / 2.0, y + 44.0);
        let pick = Action::DlgPickSlot(i);
        g.rrect(bx, by, box_size, box_size, 4.0, if hover.as_ref() == Some(&pick) { pal().hover } else { pal().surface });
        g.stroke_rrect(bx, by, box_size, box_size, 4.0, pal().stroke, 1.0);
        hits.push(Hit { rect: (bx, by, box_size, box_size), action: pick });
        let mut ty = by + box_size + 6.0;
        let Some(s) = &d.slots[i] else {
            g.center(bx, by + box_size / 2.0 - 18.0, box_size, 18.0, t("Thả file", "Drop a file"), Font::Small, pal().muted);
            g.center(bx, by + box_size / 2.0, box_size, 18.0, t("hoặc bấm chọn", "or click to choose"), Font::Small, pal().muted);
            let note = if i == 0 { t("Bắt buộc", "Required") } else { t("Bỏ trống: dùng idle", "Empty: uses idle") };
            g.label(x + 10.0, ty, w - 20.0, 18.0, note, Font::Small, pal().muted);
            return;
        };
        match &s.thumb {
            Some(px) => g.thumb(px, bx + 4.0, by + 4.0, box_size - 8.0),
            None if !s.ready => g.center(bx, by + box_size / 2.0 - 9.0, box_size, 18.0, t("Đang đọc…", "Reading…"), Font::Small, pal().muted),
            None => g.center(bx, by + box_size / 2.0 - 9.0, box_size, 18.0, t("(file hiện tại)", "(current file)"), Font::Small, pal().muted),
        }
        g.label(x + 10.0, ty, w - 20.0, 16.0, &s.label, Font::Small, pal().ink);
        ty += 17.0;
        if let Some(kind) = s.kind {
            let motion = s.motion.unwrap_or(default_motion(i, kind));
            let how = if kind == Kind::Static { format!("{} · {}", kind_text(kind), motion.name()) } else { kind_text(kind).to_string() };
            g.label(x + 10.0, ty, w - 20.0, 16.0, &how, Font::Small, pal().muted);
            ty += 18.0;
            if kind == Kind::Static {
                let act = Action::DlgMotion(i);
                let lab = if s.motion.is_some() { t("Đổi chuyển động (chỉnh tay)", "Change motion (custom)") } else { t("Đổi chuyển động", "Change motion") };
                g.link(x + 10.0, ty, w - 20.0, lab, act, hover, hits);
                ty += 18.0;
            }
        }
        if s.white_bg {
            let act = Action::DlgToggleWhite(i);
            let lab = if s.remove_white { t("☑ Tách nền trắng", "☑ Remove white background") } else { t("☐ Tách nền trắng", "☐ Remove white background") };
            g.link(x + 10.0, ty, w - 20.0, lab, act, hover, hits);
            ty += 18.0;
        }
        g.link(x + 10.0, ty.min(y + h - 20.0), w - 20.0, t("Bỏ file này", "Remove this file"), Action::DlgRemoveSlot(i), hover, hits);
    }

    fn place_edits(&mut self) {
        let k = self.k();
        let (lw, lh) = self.logical();
        if self.dialog.is_some() {
            let (dw, dh) = (lw.min(920.0) - 32.0, (lh - 32.0).min(DIALOG_H));
            let (dx, dy) = ((lw - dw) / 2.0, (lh - dh) / 2.0);
            let y = (dy + 50.0) * k;
            self.edit_name.place(Some((((dx + 64.0) * k) as i32, (y + 6.0 * k) as i32, (198.0 * k) as i32, (20.0 * k) as i32)));
        } else {
            self.edit_name.place(None);
        }
        if self.generating.as_ref().is_some_and(|g| !g.running) {
            let (dw, dh) = (GENERATE_W.min(lw - 32.0), GENERATE_H.min(lh - 32.0));
            let (dx, dy) = ((lw - dw) / 2.0, (lh - dh) / 2.0);
            self.edit_prompt.place(Some((((dx + 24.0) * k) as i32, ((dy + 96.0) * k) as i32, ((dw - 48.0) * k) as i32, (104.0 * k) as i32)));
        } else {
            self.edit_prompt.place(None);
        }
    }

    // ---- input ----

    fn action_at(&self, x: f32, y: f32) -> Option<Action> {
        self.hits
            .iter()
            .rev()
            .find(|h| x >= h.rect.0 && x < h.rect.0 + h.rect.2 && y >= h.rect.1 && y < h.rect.1 + h.rect.3)
            .map(|h| h.action.clone())
    }

    fn set_scale_from(&mut self, lx: f32) {
        let (start, width) = self.slider;
        let f = ((lx - start) / width).clamp(0.0, 1.0);
        if let Some(d) = self.dialog.as_mut() {
            // 0.05 steps
            d.scale = ((0.5 + f) * 20.0).round() / 20.0;
        }
        self.invalidate();
    }

    fn open_dialog(&mut self, editing: Option<Character>, files: &[PathBuf]) {
        let mut slots: [Option<SlotView>; 5] = Default::default();
        let (name, scale) = match &editing {
            Some(c) => {
                for (i, m) in c.states.iter().enumerate() {
                    if m.fallback {
                        continue;
                    }
                    slots[i] = Some(SlotView {
                        label: m.file.clone(),
                        path: None,
                        keep: true,
                        ext: Path::new(&m.file).extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())),
                        kind: Some(m.kind),
                        size: 0,
                        white_bg: false,
                        remove_white: false,
                        motion: if m.manual { Some(m.motion) } else { None },
                        thumb: None,
                        ready: true,
                    });
                }
                (c.name.clone(), c.scale)
            }
            None => (String::new(), 1.0),
        };
        self.edit_name.set_text(&name);
        self.dialog = Some(Dialog { editing, scale: scale.clamp(0.5, 1.5), slots, error: String::new(), show_json: false, saving: false, tokens: [0; 5] });
        self.ask_kept_thumbs();
        self.add_files(files);
        self.invalidate();
        self.edit_name.focus();
    }

    /// Current files of a character being edited get their previews too.
    fn ask_kept_thumbs(&mut self) {
        let Some(d) = self.dialog.as_ref() else { return };
        let Some(c) = &d.editing else { return };
        let Some(dir) = c.dir().map(Path::to_path_buf) else { return };
        let tokens = d.tokens;
        let hwnd = self.hwnd as isize;
        let files: Vec<(usize, PathBuf)> = (0..5).filter(|i| d.slots[*i].is_some()).map(|i| (i, dir.join(&c.states[i].file))).collect();
        spawn(move || {
            for (index, path) in files {
                let thumb = anim::thumb_of_file(&path).ok();
                finish_job(hwnd, JobResult::SlotInfo { token: tokens[index], index, info: Err(String::new()), thumb });
            }
        });
    }

    fn set_slot_file(&mut self, index: usize, path: &Path) {
        let hwnd = self.hwnd as isize;
        let Some(d) = self.dialog.as_mut() else { return };
        d.tokens[index] += 1;
        let token = d.tokens[index];
        d.error.clear();
        d.slots[index] = Some(SlotView {
            label: library::display_name(path),
            path: Some(path.to_path_buf()),
            keep: false,
            ext: None,
            kind: None,
            size: 0,
            white_bg: false,
            remove_white: false,
            motion: None,
            thumb: None,
            ready: false,
        });
        let path = path.to_path_buf();
        spawn(move || {
            let info = library::inspect_file(&path);
            let thumb = info.as_ref().ok().and_then(|_| anim::thumb_of_file(&path).ok());
            finish_job(hwnd, JobResult::SlotInfo { token, index, info, thumb });
        });
        self.invalidate();
    }

    /// Assigns several files: by name first, otherwise the first empty slot.
    fn add_files(&mut self, files: &[PathBuf]) {
        let mut used = [false; 5];
        for f in files {
            let by_name = library::guess_state(&library::display_name(f)).filter(|s| !used[*s]);
            let empty = |p: &Picker, used: &[bool; 5]| (0..5).find(|i| !used[*i] && p.dialog.as_ref().is_some_and(|d| d.slots[*i].is_none()));
            let Some(state) = by_name.or_else(|| empty(self, &used)) else {
                self.toast(t("Đã đủ 5 trạng thái, các file còn lại bị bỏ qua", "All 5 states are filled, the other files were skipped"));
                break;
            };
            used[state] = true;
            self.set_slot_file(state, f);
        }
    }

    fn start_save(&mut self) {
        let name = self.edit_name.text();
        let hwnd = self.hwnd as isize;
        let taken: Vec<String> = self.characters.iter().map(|c| c.id.clone()).collect();
        let Some(d) = self.dialog.as_mut() else { return };
        if d.saving {
            return;
        }
        if name.trim().is_empty() {
            d.error = t("Hãy đặt tên cho nhân vật.", "Give the character a name.").into();
            self.invalidate();
            return;
        }
        if d.slots[0].is_none() {
            d.error = t("Cần ít nhất file cho trạng thái Rảnh (idle).", "The Idle state needs a file.").into();
            self.invalidate();
            return;
        }
        if d.slots.iter().flatten().any(|s| !s.ready) {
            d.error = t("Đang đọc file, thử lại sau giây lát.", "Still reading the files, try again in a moment.").into();
            self.invalidate();
            return;
        }
        d.saving = true;
        d.error.clear();
        let draft = d.draft(&name);
        let editing = d.editing.clone();
        spawn(move || {
            let root = user_root();
            let result = match &editing {
                Some(c) => library::update(c, &draft, &root).map(|()| (c.id.clone(), Some(c.id.clone()))),
                None => library::create(&draft, &root, &|id| taken.iter().any(|t| t == id)).map(|id| (id, None)),
            };
            finish_job(hwnd, JobResult::Saved { result });
        });
        self.invalidate();
    }

    fn on_job(&mut self, r: JobResult) {
        match r {
            JobResult::Thumb { id, px } => {
                if let Some(px) = px {
                    self.thumbs.insert(id, px);
                }
            }
            JobResult::Preview { token, id, result } => {
                if token != self.preview_token || id != self.selected {
                    return;
                }
                self.preview_loading = false;
                match result {
                    Ok(a) => {
                        self.player = Some(Player::new(a, self.preview_state, now_ms()));
                        self.state_since = Instant::now();
                    }
                    Err(e) => self.preview_error = Some(e),
                }
            }
            JobResult::SlotInfo { token, index, info, thumb } => {
                let Some(d) = self.dialog.as_mut() else { return };
                // An inspection older than the slot's current file: drop it.
                if token != d.tokens[index] {
                    return;
                }
                let Some(slot) = d.slots[index].as_mut() else { return };
                if slot.keep {
                    slot.thumb = thumb;
                } else {
                    match info {
                        Ok(i) => {
                            slot.kind = Some(i.kind);
                            slot.ext = Some(i.ext.to_string());
                            slot.size = i.size;
                            slot.white_bg = i.white_bg;
                            slot.remove_white = i.white_bg;
                            slot.thumb = thumb;
                            slot.ready = true;
                        }
                        Err(e) => {
                            d.slots[index] = None;
                            d.error = e;
                        }
                    }
                }
            }
            JobResult::Generated { result } => match result {
                Ok(id) => {
                    let redrew = self.generating.as_ref().is_some_and(|g| g.redrawing.is_some());
                    self.generating = None;
                    self.place_edits();
                    self.edit_prompt.set_text("");
                    if redrew {
                        anim::forget(&id);
                    }
                    window::library_changed(if redrew { Some(id.as_str()) } else { None });
                    self.refresh(Some(&id));
                    self.toast(if redrew { t("Đã vẽ lại nhân vật", "The character was redrawn") } else { t("Đã tạo xong nhân vật", "The character is ready") });
                }
                Err(e) if crate::codex::was_cancelled(&e) => {
                    if let Some(gen) = self.generating.as_mut() {
                        gen.running = false;
                    }
                    self.place_edits();
                }
                Err(e) => {
                    if let Some(gen) = self.generating.as_mut() {
                        gen.running = false;
                        gen.error = e;
                    }
                    self.place_edits();
                }
            },
            JobResult::Zipped { result } => match result {
                Ok(id) => {
                    self.generating = None;
                    self.place_edits();
                    window::library_changed(None);
                    self.refresh(Some(&id));
                    self.toast(t("Đã thêm nhân vật từ file zip", "Added the character from the ZIP"));
                }
                // Stays on screen with what went wrong, unlike a toast.
                Err(e) => {
                    if self.generating.is_none() {
                        self.open_generate(None);
                    }
                    if let Some(gen) = self.generating.as_mut() {
                        gen.info.clear();
                        gen.error = e;
                    }
                }
            },
            JobResult::Saved { result } => {
                let Some(d) = self.dialog.as_mut() else { return };
                d.saving = false;
                match result {
                    Ok((id, edited)) => {
                        let name = self.edit_name.text();
                        let was_edit = edited.is_some();
                        self.dialog = None;
                        anim::forget(&id);
                        window::library_changed(edited.as_deref());
                        self.refresh(Some(&id));
                        let name = name.trim().to_string();
                        self.toast(if was_edit { crate::tf!("Đã lưu thay đổi cho {name}", "Saved changes to {name}") } else { crate::tf!("Đã thêm {name}", "Added {name}") });
                    }
                    Err(e) => d.error = e,
                }
            }
        }
        self.invalidate();
    }
}

const TOP_H: f32 = 412.0;
const HERO_H: f32 = 300.0;
const GRID_TOP: f32 = 412.0;
/// Where the settings rows start, below the page title.
const SETTINGS_TOP: f32 = 60.0;
const GRID_GAP: f32 = 12.0;
const CARD_W: f32 = 136.0;
const CARD_H: f32 = 156.0;
const THUMB_SHOWN: f32 = 100.0;
const PREVIEW_MAX_W: f32 = 300.0;
const PREVIEW_MAX_H: f32 = 264.0;

fn kind_text(k: Kind) -> &'static str {
    match k {
        Kind::Video => "video",
        Kind::Animated => t("ảnh động", "animation"),
        Kind::Static => t("ảnh tĩnh", "still image"),
        Kind::Unknown => "?",
    }
}

fn default_motion(index: usize, kind: Kind) -> Motion {
    if kind == Kind::Static {
        [Motion::Bob, Motion::Type, Motion::Nudge, Motion::Hop, Motion::Nudge][index]
    } else {
        Motion::None
    }
}

fn warnings_of(d: &Dialog) -> Vec<String> {
    let mut out = Vec::new();
    for (i, s) in d.slots.iter().enumerate() {
        let Some(s) = s else { continue };
        let key = STATE_KEYS[i];
        match s.ext.as_deref() {
            Some(".mp4") => out.push(crate::tf!("{key}: mp4 không có nền trong suốt, nên dùng webm", "{key}: mp4 has no transparent background, webm is better")),
            Some(".gif") => out.push(crate::tf!("{key}: gif nặng, nên đổi sang webm hoặc webp", "{key}: gif is heavy, webm or webp is better")),
            Some(".svg") if s.ready && !s.keep => out.push(crate::tf!("{key}: svg hiển thị như ảnh tĩnh, hiệu ứng trong file không chạy", "{key}: svg shows as a still image, its effects won't play")),
            _ => {}
        }
        if s.size > BIG_FILE {
            out.push(crate::tf!("{key}: {:.1} MB, hơi nặng", "{key}: {:.1} MB, a bit heavy", s.size as f64 / 1024.0 / 1024.0));
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Primary,
    Normal,
    Danger,
}

/// Drawing helpers working in logical pixels.
struct Gfx<'a> {
    buf: &'a mut [u8],
    w: u32,
    h: u32,
    k: f32,
    text: &'a TextDrawer,
    /// Everything drawn is moved right by this, so pages needn't know about the sidebar.
    ox: f32,
    backdrop: Backdrop,
    /// Set while drawing a dialog: it must hide what is behind it, or the page shows through its text.
    solid_cards: bool,
}

impl Gfx<'_> {
    fn pen(&mut self) -> Option<PixmapMut<'_>> {
        PixmapMut::from_bytes(self.buf, self.w, self.h)
    }

    /// The page colour for what the backdrop leaves to the app, in premultiplied BGRA.
    fn page_pixel(&self) -> [u8; 4] {
        let (r, g, b) = render::rgb(pal().bg);
        let premul = |v: u8, a: f32| (v as f32 * a).round() as u8;
        match self.backdrop {
            Backdrop::Solid => [b, g, r, 255],
            Backdrop::Mica => [0, 0, 0, 0],
            Backdrop::Blur => {
                let a = theme::BLUR_TINT_ALPHA;
                [premul(b, a), premul(g, a), premul(r, a), (a * 255.0) as u8]
            }
        }
    }

    fn fill_page(&mut self) {
        let px = self.page_pixel();
        for p in self.buf.chunks_exact_mut(4) {
            p.copy_from_slice(&px);
        }
    }

    /// Resets an area to bare page, replacing what is there rather than painting over it.
    fn page_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let px = self.page_pixel();
        let k = self.k;
        let (ww, wh) = (self.w as usize, self.h as usize);
        let x0 = (((x + self.ox) * k) as usize).min(ww);
        let x1 = (((x + self.ox + w) * k).ceil() as usize).min(ww);
        for row in ((y * k) as usize).min(wh)..((((y + h) * k).ceil()) as usize).min(wh) {
            for p in self.buf[(row * ww + x0) * 4..(row * ww + x1) * 4].chunks_exact_mut(4) {
                p.copy_from_slice(&px);
            }
        }
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, hex: u32) {
        self.rect_a(x, y, w, h, hex, 1.0);
    }

    fn rect_a(&mut self, x: f32, y: f32, w: f32, h: f32, hex: u32, a: f32) {
        let k = self.k;
        let x = x + self.ox;
        let Some(mut pm) = self.pen() else { return };
        if let Some(r) = tiny_skia::Rect::from_xywh(x * k, y * k, w * k, h * k) {
            pm.fill_rect(r, &paint(col(hex, a)), Transform::identity(), None);
        }
    }

    fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, hex: u32) {
        // Cards let a little of the backdrop through, like Windows 11's own.
        let a = if hex == pal().surface && self.backdrop != Backdrop::Solid && !self.solid_cards { theme::CARD_ALPHA } else { 1.0 };
        self.rrect_a(x, y, w, h, r, hex, a);
    }

    #[allow(clippy::too_many_arguments)]
    fn rrect_a(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, hex: u32, a: f32) {
        let k = self.k;
        let x = x + self.ox;
        let Some(mut pm) = self.pen() else { return };
        if let Some(p) = round_rect(x, y, w, h, r.min(w / 2.0).min(h / 2.0)) {
            pm.fill_path(&p, &paint(col(hex, a)), FillRule::Winding, Transform::from_scale(k, k), None);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn stroke_rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, hex: u32, width: f32) {
        let k = self.k;
        let x = x + self.ox;
        let Some(mut pm) = self.pen() else { return };
        let mut pen = Pen { pm: &mut pm, tf: Transform::from_scale(k, k) };
        pen.stroke(round_rect(x, y, w, h, r), hex, width);
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, hex: u32) {
        let k = self.k;
        let cx = cx + self.ox;
        let Some(mut pm) = self.pen() else { return };
        let mut pen = Pen { pm: &mut pm, tf: Transform::from_scale(k, k) };
        pen.fill(oval(cx, cy, r, r), hex, 1.0);
    }

    #[allow(clippy::too_many_arguments)]
    fn label(&mut self, x: f32, y: f32, w: f32, h: f32, s: &str, f: Font, color: u32) {
        self.text_at(x, y, w, h, s, f, color, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn text_at(&mut self, x: f32, y: f32, w: f32, h: f32, s: &str, f: Font, color: u32, centered: bool) {
        let k = self.k;
        let (px, py, pw, ph) = (((x + self.ox) * k) as i32, (y * k) as i32, (w * k) as i32, (h * k) as i32);
        let tw = if centered { self.text.width(s, f).min(pw) } else { pw };
        let dx = if centered { (pw - tw) / 2 } else { 0 };
        self.text.draw(self.buf, self.w, self.h, px + dx, py, tw + i32::from(centered), ph, s, f, color);
    }

    /// An icon font glyph, centered in the box.
    fn icon(&mut self, x: f32, y: f32, w: f32, h: f32, glyph: &str, color: u32) {
        self.text_at(x, y, w, h, glyph, Font::Icon, color, true);
    }

    fn text_w(&self, s: &str, f: Font) -> f32 {
        self.text.width(s, f) as f32 / self.k
    }

    #[allow(clippy::too_many_arguments)]
    fn center(&mut self, x: f32, y: f32, w: f32, h: f32, s: &str, f: Font, color: u32) {
        let tw = self.text_w(s, f).min(w);
        self.label(x + (w - tw) / 2.0, y, tw + 1.0, h, s, f, color);
    }

    fn button_w(&self, icon: Option<&str>, s: &str) -> f32 {
        self.text_w(s, Font::Normal) + 28.0 + if icon.is_some() { 24.0 } else { 0.0 }
    }

    /// Returns the x just past the button.
    #[allow(clippy::too_many_arguments)]
    fn button(&mut self, x: f32, y: f32, icon: Option<&str>, s: &str, style: Style, enabled: bool, action: Action, hover: &Option<Action>, hits: &mut Vec<Hit>) -> f32 {
        let w = self.button_w(icon, s);
        let p = pal();
        let hot = enabled && hover.as_ref() == Some(&action);
        let (bg, fg) = match (style, enabled) {
            (Style::Primary, true) => (if hot { p.accent_hover } else { p.accent }, p.on_accent),
            (_, false) => (p.hover, p.muted),
            (Style::Danger, _) => (if hot { p.hover } else { p.surface }, p.danger),
            (Style::Normal, _) => (if hot { p.hover } else { p.surface }, p.ink),
        };
        self.rrect(x, y, w, 32.0, 4.0, bg);
        if style != Style::Primary && enabled {
            self.stroke_rrect(x, y, w, 32.0, 4.0, p.stroke, 1.0);
        }
        let mut tx = x + 14.0;
        if let Some(i) = icon {
            self.icon(tx - 2.0, y, 20.0, 32.0, i, fg);
            tx += 24.0;
        }
        self.label(tx, y, w - (tx - x) - 12.0, 32.0, s, Font::Normal, fg);
        if enabled {
            hits.push(Hit { rect: (x, y, w, 32.0), action });
        }
        x + w
    }

    fn chip_w(&self, s: &str) -> f32 {
        self.text_w(s, Font::Normal) + 24.0
    }

    #[allow(clippy::too_many_arguments)]
    fn chip(&mut self, x: f32, y: f32, s: &str, on: bool, action: Action, hover: &Option<Action>, hits: &mut Vec<Hit>) -> f32 {
        let w = self.chip_w(s);
        let hot = hover.as_ref() == Some(&action);
        // The accent colour is kept for the one main action and for data; a selection here is quieter.
        self.rrect(x, y, w, 28.0, 4.0, if on || hot { pal().hover } else { pal().surface });
        self.stroke_rrect(x, y, w, 28.0, 4.0, if on { pal().muted } else { pal().stroke }, 1.0);
        self.center(x, y, w, 28.0, s, if on { Font::Bold } else { Font::Normal }, pal().ink);
        hits.push(Hit { rect: (x, y, w, 28.0), action });
        x + w
    }

    /// A chart bar: rounded at the data end, flat where it stands on the baseline.
    fn bar(&mut self, x: f32, y: f32, w: f32, h: f32, hex: u32, a: f32) {
        let k = self.k;
        let x = x + self.ox;
        let r = 4.0_f32.min(h).min(w / 2.0);
        let mut b = tiny_skia::PathBuilder::new();
        b.move_to(x, y + h);
        b.line_to(x, y + r);
        b.quad_to(x, y, x + r, y);
        b.line_to(x + w - r, y);
        b.quad_to(x + w, y, x + w, y + r);
        b.line_to(x + w, y + h);
        b.close();
        let Some(path) = b.finish() else { return };
        let Some(mut pm) = self.pen() else { return };
        pm.fill_path(&path, &paint(col(hex, a)), FillRule::Winding, Transform::from_scale(k, k), None);
    }

    /// The Windows on/off switch.
    fn toggle(&mut self, x: f32, y: f32, on: bool, action: Action, hover: &Option<Action>, hits: &mut Vec<Hit>) {
        let p = pal();
        let hot = hover.as_ref() == Some(&action);
        if on {
            self.rrect(x, y, 40.0, 20.0, 10.0, if hot { p.accent_hover } else { p.accent });
            self.circle(x + 30.0, y + 10.0, if hot { 7.0 } else { 6.0 }, p.on_accent);
        } else {
            self.rrect(x, y, 40.0, 20.0, 10.0, if hot { p.hover } else { p.surface });
            self.stroke_rrect(x, y, 40.0, 20.0, 10.0, p.muted, 1.0);
            self.circle(x + 10.0, y + 10.0, if hot { 6.0 } else { 5.0 }, p.muted);
        }
        hits.push(Hit { rect: (x - 8.0, y - 8.0, 56.0, 36.0), action });
    }

    #[allow(clippy::too_many_arguments)]
    fn link(&mut self, x: f32, y: f32, w: f32, s: &str, action: Action, hover: &Option<Action>, hits: &mut Vec<Hit>) {
        let hot = hover.as_ref() == Some(&action);
        self.label(x, y, w, 17.0, s, Font::Small, if hot { pal().accent_hover } else { pal().accent });
        hits.push(Hit { rect: (x, y, w, 17.0), action });
    }

    fn thumb(&mut self, px: &[u8], x: f32, y: f32, size: f32) {
        let k = self.k;
        let f = size * k / THUMB_SIZE as f32;
        let Some(src) = tiny_skia::PixmapRef::from_bytes(px, THUMB_SIZE, THUMB_SIZE) else { return };
        let tf = Transform::from_translate((x + self.ox) * k, y * k).pre_scale(f, f);
        let Some(mut pm) = self.pen() else { return };
        let p = tiny_skia::PixmapPaint { opacity: 1.0, blend_mode: tiny_skia::BlendMode::SourceOver, quality: tiny_skia::FilterQuality::Bilinear };
        pm.draw_pixmap(0, 0, src, &p, tf, None);
    }
}

// Dialogs block in their own message loop, so they are only ever called with no borrow held.

/// Where a generated character's sheets and Codex conversation are kept, outside the character itself, so they
/// can be worked on again and never get shared along with it.
fn generated_dir(id: &str) -> PathBuf {
    crate::config::dir().join("generated").join(id)
}

/// The app's own copy, in a process of its own: see sheet::cut_main.
fn cut_sheets(dir: &Path, out: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let cut = std::process::Command::new(exe).arg("--cut-sheets").arg(dir).arg(out).output().map_err(|e| e.to_string())?;
    if cut.status.success() {
        return Ok(());
    }
    Err(crate::tf!("Không dùng được các ảnh này: {}", "These images can't be used: {}", String::from_utf8_lossy(&cut.stderr)))
}

fn animated_slots(out: &Path) -> [Slot; 5] {
    std::array::from_fn(|i| Slot::New { path: out.join(format!("{}.png", STATE_KEYS[i])), motion: None, remove_white: false })
}

/// Sheets in `dir` become a new character, and `dir` moves to where it's kept. Returns its id.
fn add_from_sheets(dir: &Path, name: String, taken: &[String]) -> Result<String, String> {
    let out = dir.join("animated");
    cut_sheets(dir, &out)?;
    let draft = Draft { name, scale: 1.0, slots: animated_slots(&out) };
    let id = library::create(&draft, &user_root(), &|id| taken.iter().any(|t| t == id))?;
    let _ = fs::remove_dir_all(&out);
    let keep = generated_dir(&id);
    let _ = fs::remove_dir_all(&keep);
    let _ = fs::rename(dir, keep);
    Ok(id)
}

fn work_dir() -> Result<PathBuf, String> {
    let dir = crate::config::dir().join("generated").join(format!(".working-{}", now_ms()));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn generate_character(description: &str, taken: &[String], cancel: &AtomicBool, done: &AtomicUsize) -> Result<String, String> {
    let dir = work_dir()?;
    let finish = (|| {
        let drawn = crate::codex::draw(description, &dir, cancel, done)?;
        let _ = fs::write(dir.join("description.txt"), description);
        if let Some(session) = &drawn.session {
            let _ = fs::write(dir.join("codex-session.txt"), session);
        }
        add_from_sheets(&dir, drawn.name, taken)
    })();
    if finish.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    finish
}

/// The character only changes once all five new sheets are drawn and cut, so a failure leaves it as it was.
fn redraw_character(c: &Character, change: &str, cancel: &AtomicBool, done: &AtomicUsize) -> Result<(), String> {
    let base = generated_dir(&c.id);
    let next = base.join(".next");
    let _ = fs::remove_dir_all(&next);
    fs::create_dir_all(&next).map_err(|e| e.to_string())?;
    let finish = (|| {
        let session = fs::read_to_string(base.join("codex-session.txt")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let session = crate::codex::redraw(change, session.as_deref(), &base.join("idle.png"), &next, cancel, done)?;
        let out = next.join("animated");
        cut_sheets(&next, &out)?;
        let draft = Draft { name: c.name.clone(), scale: c.scale, slots: animated_slots(&out) };
        library::update(c, &draft, &user_root())?;
        for key in STATE_KEYS {
            let name = format!("{key}.png");
            fs::rename(next.join(&name), base.join(&name)).map_err(|e| e.to_string())?;
        }
        if let Some(session) = session {
            let _ = fs::write(base.join("codex-session.txt"), session);
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&next);
    finish
}

fn character_from_zip(path: &Path, taken: &[String]) -> Result<String, String> {
    let dir = work_dir()?;
    let finish = (|| {
        let name = crate::sheet::extract_zip(path, &dir)?;
        let fallback = path.file_stem().map_or_else(|| "ChatGPT".to_string(), |s| s.to_string_lossy().chars().take(40).collect());
        add_from_sheets(&dir, name.unwrap_or(fallback), taken)
    })();
    if finish.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    finish
}

/// Whether a path is a shared character rather than media to build one from.
pub fn is_pack(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case(library::PACK_EXTENSION))
}

/// Adds a character someone shared. Opens the window on it, so you see what you got.
pub fn import_pack(path: &Path) {
    open(0);
    let Some(hwnd) = with(|p| p.hwnd) else { return };
    let Some(taken) = with(|p| p.characters.iter().map(|c| c.id.clone()).collect::<Vec<_>>()) else { return };
    match library::import_pack(path, &user_root(), &|id| taken.iter().any(|t| t == id)) {
        Ok(id) => {
            window::library_changed(None);
            with(|p| {
                p.dialog = None;
                p.refresh(Some(&id));
                p.toast(t("Đã thêm nhân vật được chia sẻ", "Added the shared character"));
            });
        }
        Err(e) => host::show_error(hwnd, t("Thêm nhân vật", "Add character"), &crate::tf!("Không thêm được nhân vật.\n\n{e}", "Could not add the character.\n\n{e}")),
    }
}

fn pick_motion(owner: Win, index: usize, kind: Kind, current: Option<Motion>) -> Option<Option<Motion>> {
    let mut items = vec![(crate::tf!("tự động ({})", "automatic ({})", default_motion(index, kind).name()), current.is_none())];
    items.extend(motion::ALL.iter().map(|x| (format!("{} ({})", x.name(), x.text()), current == Some(*x))));
    match host::popup_choice(owner, &items)? {
        0 => Some(None),
        n => motion::ALL.get(n - 1).map(|x| Some(*x)),
    }
}

fn run_action(hwnd: Win, action: Action) {
    match action {
        Action::Select(id) => {
            with(|p| p.select(&id));
        }
        Action::Use => {
            let Some(id) = with(|p| p.selected.clone()) else { return };
            if window::choose_character(&id) {
                with(|p| {
                    p.current = id.clone();
                    let name = p.characters.iter().find(|c| c.id == id).map_or(id.clone(), |c| c.name.clone());
                    p.toast(crate::tf!("Đã đổi sang {name}", "Switched to {name}"));
                });
            }
        }
        Action::Edit => {
            with(|p| {
                let c = p.selected_character().cloned();
                if let Some(c) = c.filter(|c| !c.is_builtin()) {
                    p.open_dialog(Some(c), &[]);
                }
            });
        }
        Action::Share => {
            let Some(Some(c)) = with(|p| p.selected_character().cloned()) else { return };
            let Some(dest) = host::pick_save_file(hwnd, &c.id) else { return };
            match library::export_pack(&c, &dest) {
                Ok(()) => {
                    with(|p| p.toast(crate::tf!("Đã lưu {}. Gửi file này cho người khác là được.", "Saved {}. Send this file to anyone.", dest.file_name().unwrap_or_default().to_string_lossy())));
                }
                Err(e) => host::show_error(hwnd, t("Chia sẻ nhân vật", "Share character"), &e),
            }
        }
        Action::Remove => {
            let Some((c, current)) = with(|p| (p.selected_character().cloned(), p.current.clone())) else { return };
            let Some(c) = c else { return };
            if let Some(why) = library::remove_blocker(&c, &current) {
                host::show_error(hwnd, t("Xoá nhân vật", "Delete character"), &why);
                return;
            }
            let dir = c.dir().map_or(String::new(), |d| d.display().to_string());
            if !host::confirm(hwnd, t("Xoá nhân vật", "Delete character"), &crate::tf!("Xoá \"{}\"?\n\nThư mục {dir} sẽ bị xoá vĩnh viễn.", "Delete \"{}\"?\n\nThe folder {dir} will be permanently deleted.", c.name)) {
                return;
            }
            match library::remove_dir(&c, &user_root()) {
                Ok(()) => {
                    anim::forget(&c.id);
                    let _ = fs::remove_dir_all(generated_dir(&c.id));
                    window::library_changed(None);
                    with(|p| {
                        p.refresh(None);
                        p.toast(crate::tf!("Đã xoá {}", "Deleted {}", c.name));
                    });
                }
                Err(e) => host::show_error(hwnd, t("Xoá nhân vật", "Delete character"), &e),
            }
        }
        Action::Tab(i) => {
            with(|p| {
                p.preview_state = i;
                p.state_since = Instant::now();
                if let Some(pl) = p.player.as_mut() {
                    pl.set_state(i, now_ms());
                }
                p.invalidate();
            });
        }
        Action::Sort(by_name) => {
            with(|p| {
                p.sort_by_name = by_name;
                p.invalidate();
            });
        }
        Action::Add => {
            with(|p| p.open_dialog(None, &[]));
        }
        Action::Generate => {
            with(|p| p.open_generate(None));
        }
        Action::Redraw => {
            with(|p| {
                let c = p.selected_character().cloned();
                p.open_generate(c);
            });
        }
        Action::GenCopy => {
            with(|p| p.copy_chatgpt_prompt());
        }
        Action::GenStart => {
            with(|p| p.start_generate());
        }
        Action::GenStop => {
            with(|p| {
                if let Some(gen) = &p.generating {
                    gen.cancel.store(true, Ordering::Relaxed);
                }
            });
        }
        Action::GenClose => {
            with(|p| {
                p.generating = None;
                p.place_edits();
                p.invalidate();
            });
        }
        Action::Page(n) => {
            with(|p| {
                p.page = n;
                p.hover = None;
                p.invalidate();
            });
        }
        Action::Set(setting) => {
            window::apply_setting(setting);
            with(|p| {
                if setting == Setting::Translucent {
                    p.apply_look();
                }
                p.refresh(None);
            });
        }
        Action::Day(_) => {}
        Action::DlgClose => {
            with(|p| {
                p.dialog = None;
                p.invalidate();
            });
        }
        Action::DlgPickFiles => {
            let files = host::pick_files(hwnd, true);
            with(|p| p.add_files(&files));
        }
        Action::DlgPickSlot(i) => {
            if let Some(f) = host::pick_files(hwnd, false).first() {
                with(|p| p.set_slot_file(i, f));
            }
        }
        Action::DlgRemoveSlot(i) => {
            with(|p| {
                if let Some(d) = p.dialog.as_mut() {
                    d.slots[i] = None;
                }
                p.invalidate();
            });
        }
        Action::DlgToggleWhite(i) => {
            with(|p| {
                if let Some(s) = p.dialog.as_mut().and_then(|d| d.slots[i].as_mut()) {
                    s.remove_white = !s.remove_white;
                }
                p.invalidate();
            });
        }
        Action::DlgMotion(i) => {
            let Some(Some((kind, cur))) = with(|p| p.dialog.as_ref().and_then(|d| d.slots[i].as_ref()).and_then(|s| s.kind.map(|k| (k, s.motion)))) else { return };
            if let Some(chosen) = pick_motion(hwnd, i, kind, cur) {
                with(|p| {
                    if let Some(s) = p.dialog.as_mut().and_then(|d| d.slots[i].as_mut()) {
                        s.motion = chosen;
                    }
                    p.invalidate();
                });
            }
        }
        Action::DlgScale => {}
        Action::DlgImport => {
            let Some(dir) = host::pick_folder(hwnd) else { return };
            let Some(taken) = with(|p| p.characters.iter().map(|c| c.id.clone()).collect::<Vec<_>>()) else { return };
            match library::import_dir(&dir, &user_root(), &|id| taken.iter().any(|t| t == id)) {
                Ok(id) => {
                    window::library_changed(None);
                    with(|p| {
                        p.dialog = None;
                        p.refresh(Some(&id));
                        p.toast(t("Đã thêm nhân vật từ thư mục", "Added the character from the folder"));
                    });
                }
                Err(e) => host::show_error(hwnd, t("Thêm nhân vật", "Add character"), &crate::tf!("Không thêm được nhân vật.\n\n{e}", "Could not add the character.\n\n{e}")),
            }
        }
        Action::DlgSave => {
            with(|p| p.start_save());
        }
        Action::DlgJson => {
            with(|p| {
                if let Some(d) = p.dialog.as_mut() {
                    d.show_json = !d.show_json;
                }
                p.invalidate();
            });
        }
    }
}

/// Whether the window is in use, rather than merely open.
#[cfg(target_os = "macos")]
pub(crate) fn window_is_key() -> bool {
    host::is_key(window())
}

#[cfg(windows)]
pub(crate) fn surface<R>(f: impl FnOnce(&mut Surface) -> R) -> Option<R> {
    with(|p| f(&mut p.surface))
}

pub(crate) fn paint_into(f: impl FnOnce(&Canvas)) {
    with(|p| {
        p.paint_all();
        f(&p.canvas);
    });
}

pub(crate) fn with_canvas(f: impl FnOnce(&Canvas)) {
    with(|p| f(&p.canvas));
}

pub(crate) fn resized(w: u32, h: u32) {
    if w > 0 && h > 0 {
        with(|p| {
            if let Some(c) = Canvas::new(w, h) {
                p.canvas = c;
            }
            p.invalidate();
        });
    }
}

pub(crate) fn scale_changed(scale: f32) {
    with(|p| {
        p.scale = scale;
        p.text = Rc::new(TextDrawer::new_ui(p.scale));
        p.load_preview();
    });
}

/// The window's regular beat. Returns the part of the canvas that was redrawn and needs to reach the screen.
pub(crate) fn timer_tick(minimized: bool) -> Option<(i32, i32, i32, i32)> {
    with(|p| {
        if minimized {
            return None;
        }
        if let Some(gen) = p.generating.as_mut().filter(|g| g.running) {
            let now = (gen.done.load(Ordering::Relaxed), gen.started.elapsed().as_secs() / 60);
            if now != gen.shown {
                gen.shown = now;
                p.invalidate();
            }
        }
        if p.toast.as_ref().is_some_and(|(_, until)| now_ms() >= *until) {
            p.toast = None;
            p.invalidate();
        }
        // Behind a dialog or sitting still, the preview costs nothing.
        if p.dialog.is_none() && p.generating.is_none() && p.page == 0 && p.is_previewing_animated() {
            let (w, h) = (p.canvas.w, p.canvas.h);
            let buf = unsafe { std::slice::from_raw_parts_mut(p.canvas.bits, (w * h * 4) as usize) };
            let text = p.text.clone();
            let mut g = Gfx { buf, w, h, k: p.scale, text: &text, ox: 0.0, backdrop: p.backdrop, solid_cards: false };
            p.paint_stage(&mut g);
            return Some(p.stage);
        }
        None
    })
    .flatten()
}

pub(crate) fn theme_changed() {
    with(|p| {
        p.apply_look();
        p.invalidate();
    });
}

pub(crate) fn jobs_done() {
    let results = std::mem::take(&mut *RESULTS.lock().unwrap());
    for r in results {
        with(|p| p.on_job(r));
    }
}

pub(crate) fn refresh_requested() {
    with(|p| p.refresh(None));
}

/// Coordinates are logical. Returns whether the pointer needs to start being watched for leaving.
pub(crate) fn mouse_moved(x: f32, y: f32) -> bool {
    with(|p| {
        let start_tracking = !std::mem::replace(&mut p.tracking, true);
        if p.dragging_scale {
            p.set_scale_from(x);
            return start_tracking;
        }
        let h = p.action_at(x, y);
        if h != p.hover {
            p.hover = h;
            p.invalidate();
        }
        start_tracking
    })
    .unwrap_or(false)
}

pub(crate) fn mouse_left() {
    with(|p| {
        p.tracking = false;
        if p.hover.take().is_some() {
            p.invalidate();
        }
    });
}

/// Returns whether this started dragging the scale slider.
pub(crate) fn left_down(x: f32, y: f32) -> bool {
    with(|p| {
        if p.action_at(x, y) == Some(Action::DlgScale) {
            p.dragging_scale = true;
            p.set_scale_from(x);
            return true;
        }
        false
    })
    .unwrap_or(false)
}

pub(crate) fn current_scale() -> f32 {
    with(|p| p.scale).unwrap_or(1.0)
}

/// Returns whether this ended a drag of the slider.
pub(crate) fn end_drag() -> bool {
    with(|p| std::mem::take(&mut p.dragging_scale)).unwrap_or(false)
}

pub(crate) fn left_up(hwnd: Win, x: f32, y: f32) {
    if let Some(a) = with(|p| p.action_at(x, y)).flatten() {
        run_action(hwnd, a);
    }
}

/// `notches`: one is one click of a wheel, away from you positive.
pub(crate) fn wheel(notches: f32) {
    with(|p| {
        if p.dialog.is_none() && p.generating.is_none() && p.page == 0 {
            p.scroll -= notches * 60.0;
            p.clamp_scroll();
            p.invalidate();
        } else if p.dialog.is_none() && p.page == PAGE_SETTINGS && p.settings_max > 0.0 {
            p.settings_scroll = (p.settings_scroll - notches * 60.0).clamp(0.0, p.settings_max);
            p.invalidate();
        }
    });
}

pub(crate) fn prompt_changed() {
    with(|p| {
        if let Some(gen) = p.generating.as_mut().filter(|g| !g.error.is_empty()) {
            gen.error.clear();
            p.invalidate();
        }
    });
}

pub(crate) fn name_changed() {
    with(|p| {
        if let Some(d) = p.dialog.as_mut() {
            d.error.clear();
            if d.show_json {
                p.invalidate();
            }
        }
    });
}

pub(crate) fn files_dropped(files: Vec<PathBuf>, x: f32, y: f32) {
    let (packs, files): (Vec<PathBuf>, Vec<PathBuf>) = files.into_iter().partition(|f| is_pack(f));
    for pack in &packs {
        import_pack(pack);
    }
    if files.is_empty() {
        return;
    }
    if let Some(zip) = files.iter().find(|f| f.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"))) {
        with(|p| {
            if !p.generating.as_ref().is_some_and(|g| g.running) {
                p.add_zip(zip);
            }
        });
        return;
    }
    if with(|p| p.generating.is_some()).unwrap_or(false) {
        return;
    }
    with(|p| {
        if p.dialog.is_none() {
            p.open_dialog(None, &files);
            return;
        }
        // Dropped right onto a slot: that slot, whatever the file is called.
        match p.action_at(x, y) {
            Some(Action::DlgPickSlot(i)) if !files.is_empty() => p.set_slot_file(i, &files[0]),
            _ => p.add_files(&files),
        }
    });
}

/// Esc closes the dialog, then the window. Drawing is only ever stopped on purpose. Returns whether a
/// dialog took it, so the window stays.
pub(crate) fn escape() -> bool {
    with(|p| {
        if p.generating.as_ref().is_some_and(|g| g.running) {
            return true;
        }
        let had = p.dialog.is_some() || p.generating.is_some();
        p.dialog = None;
        p.generating = None;
        p.place_edits();
        p.invalidate();
        had
    })
    .unwrap_or(false)
}

/// Returns whether the window may close.
pub(crate) fn may_close(hwnd: Win) -> bool {
    let drawing = with(|p| p.generating.as_ref().is_some_and(|g| g.running)).unwrap_or(false);
    !drawing || host::confirm(hwnd, t("Tạo nhân vật bằng Codex", "Create a character with Codex"), t("Đóng cửa sổ sẽ huỷ nhân vật đang tạo. Đóng?", "Closing this window cancels the character being created. Close?"))
}

/// Hands back what the platform has to clean up.
pub(crate) fn closed() -> Option<Surface> {
    with(|p| {
        if let Some(gen) = &p.generating {
            gen.cancel.store(true, Ordering::Relaxed);
        }
    });
    HWND_OPEN.store(0, Ordering::Release);
    window::sync_buddy_with_picker();
    window::trim_memory_soon();
    let taken = PICKER.with(|p| p.try_borrow_mut().ok().and_then(|mut b| b.take()));
    RESULTS.lock().unwrap().clear();
    taken.map(|p| p.surface)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(ext: &str, size: u64) -> SlotView {
        SlotView {
            label: String::new(),
            path: None,
            keep: false,
            ext: Some(ext.into()),
            kind: Some(Kind::Static),
            size,
            white_bg: false,
            remove_white: false,
            motion: None,
            thumb: None,
            ready: true,
        }
    }

    #[test]
    fn warns_about_formats_that_will_disappoint() {
        let mut d = Dialog { editing: None, scale: 1.0, slots: Default::default(), error: String::new(), show_json: false, saving: false, tokens: [0; 5] };
        assert!(warnings_of(&d).is_empty());
        d.slots[0] = Some(slot(".mp4", 10));
        d.slots[1] = Some(slot(".gif", 10));
        d.slots[2] = Some(slot(".svg", 10));
        d.slots[3] = Some(slot(".webm", BIG_FILE + 1));
        let w = warnings_of(&d);
        assert_eq!(w.len(), 4);
        assert!(w[0].starts_with("idle: mp4"));
        assert!(w[3].contains("hơi nặng"));
    }

    #[test]
    fn draft_reflects_the_slots() {
        let mut d = Dialog { editing: None, scale: 1.25, slots: Default::default(), error: String::new(), show_json: false, saving: false, tokens: [0; 5] };
        let mut s = slot(".png", 1);
        s.path = Some("a.png".into());
        s.remove_white = true;
        s.motion = Some(Motion::Hop);
        d.slots[0] = Some(s);
        let mut kept = slot(".png", 1);
        kept.keep = true;
        d.slots[3] = Some(kept);
        let draft = d.draft("Na");
        assert_eq!(draft.name, "Na");
        assert_eq!(draft.slots[0], Slot::New { path: "a.png".into(), motion: Some(Motion::Hop), remove_white: true });
        assert_eq!(draft.slots[1], Slot::Empty);
        assert_eq!(draft.slots[3], Slot::Keep { motion: None });
    }

    #[test]
    fn numbers_and_dates_read_the_vietnamese_way() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1.000");
        assert_eq!(grouped(1234567), "1.234.567");
        assert_eq!(i18n::date(20261104), "4/11/2026");
    }

    const HERO_MAIN_LABELS: [[&str; 2]; 2] = [["Dùng nhân vật", "Đang dùng"], ["Use character", "In use"]];
    /// Frame and shadow Windows adds around the page at the narrowest allowed size.
    const FRAME_W: f32 = 26.0;

    /// What cut off the delete button: buttons are laid out in a row and nothing stops them running
    /// past the card, so the narrowest window has to leave room for the widest row at every scale.
    #[test]
    fn the_character_buttons_fit_at_the_narrowest_window() {
        for scale in [1.0_f32, 1.25, 1.5, 2.0, 2.5] {
            let text = TextDrawer::new_ui(scale);
            let w = |icon: bool, s: &str| text.width(s, Font::Normal) as f32 / scale + 28.0 + if icon { 24.0 } else { 0.0 } + 8.0;
            for language in 0..2 {
                let widest_main = HERO_MAIN_LABELS[language].iter().map(|l| w(true, l)).fold(0.0, f32::max);
                let row = widest_main + HERO_ACTIONS[language].iter().map(|l| w(true, l)).sum::<f32>();
                // Same arithmetic as paint_top: page, then the card, then the preview beside the buttons.
                let page = MIN_WINDOW_W - FRAME_W - SIDEBAR_W;
                let room = page - 32.0 - PREVIEW_MAX_W - 64.0;
                assert!(row <= room, "at {}% the buttons need {row} but have {room}", scale * 100.0);
            }
        }
    }

    #[test]
    fn default_motions_only_for_stills() {
        assert_eq!(default_motion(3, Kind::Static), Motion::Hop);
        assert_eq!(default_motion(3, Kind::Video), Motion::None);
    }
}
