//! Prepared animations, the reason the buddy costs almost nothing while it moves. Everything is
//! decoded or drawn once by a short-lived process (`--prepare`), scaled to the size it is shown at, and
//! stored compressed on disk. Playback maps that file instead of loading it, so Windows can drop the
//! pages whenever it needs the memory, and each step only touches the pixels that really changed.

use crate::i18n::t;
use crate::library::{self, Character, Source};
use crate::media::{self, Frames, Resampler};
use crate::render;
use crate::store::State;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::UNIX_EPOCH;
use tiny_skia::{PixmapMut, Transform};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, GetFileSizeEx, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, OPEN_EXISTING};
#[cfg(windows)]
use windows_sys::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READONLY};

/// Bump when the cache format or the way frames are prepared changes.
const CACHE_VERSION: u32 = 2;
const MAGIC: &[u8; 4] = b"CBA2";
/// Same limits as the original's stage, except that very large scales are capped so the character
/// and its bubble still fit in the window.
pub const BASE_HEIGHT: f32 = 190.0;
pub const MAX_WIDTH: f32 = 264.0;
pub const MAX_STAGE_HEIGHT: f32 = 300.0;
pub const THUMB_SIZE: u32 = 128;
const THUMB_PADDING: f32 = 8.0;
/// A pixel changing by no more than this between frames keeps its old value. That much is codec noise
/// nobody can see, and removing it is what lets a still part of a video stay untouched.
const NOISE_FLOOR: i32 = 3;
const VECTOR_FPS_ACTIVE: f32 = 30.0;
/// Calm states move slowly enough for fewer frames to look the same.
const VECTOR_FPS_CALM: f32 = 15.0;

/// x, y, width, height in pixels.
pub type Rect = (u32, u32, u32, u32);

#[derive(Clone, Copy, Debug, PartialEq)]
struct FrameRec {
    offset: u32,
    len: u32,
    delay: u32,
    rect: Rect,
}

#[derive(Debug, PartialEq)]
pub struct Clip {
    /// The first holds the whole picture; each next one only what differs from the one before.
    frames: Vec<FrameRec>,
    /// From the last frame back to the first, so looping is as cheap as any other step.
    wrap: Option<FrameRec>,
}

impl Clip {
    /// Time the most eager frame wants, so the timer doesn't run faster than the picture changes.
    pub fn min_delay(&self) -> u32 {
        self.frames.iter().map(|f| f.delay).min().unwrap_or(0)
    }

    pub fn is_still(&self) -> bool {
        self.frames.len() <= 1
    }
}

/// A read-only file mapped into memory.
struct Mapping {
    ptr: *const u8,
    len: usize,
}

// Read-only for its whole life.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr as *mut _ }) };
        #[cfg(unix)]
        unsafe { libc::munmap(self.ptr as *mut _, self.len) };
    }
}

#[cfg(unix)]
fn map_file(path: &Path) -> Option<Mapping> {
    use std::os::fd::AsRawFd;
    let file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len() as usize;
    if len == 0 {
        return None;
    }
    let view = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
    (view != libc::MAP_FAILED).then_some(Mapping { ptr: view as *const u8, len })
}

#[cfg(windows)]
fn map_file(path: &Path) -> Option<Mapping> {
    let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let file: HANDLE = CreateFileW(wide.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut());
        if file == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut size = 0i64;
        let sized = GetFileSizeEx(file, &mut size) != 0 && size > 0;
        let mapping = if sized { CreateFileMappingW(file, std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null()) } else { std::ptr::null_mut() };
        CloseHandle(file);
        if mapping.is_null() {
            return None;
        }
        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        CloseHandle(mapping); // the view keeps it alive
        (!view.Value.is_null()).then_some(Mapping { ptr: view.Value as *const u8, len: size as usize })
    }
}

enum Bytes {
    Mapped(Mapping),
    #[cfg(test)]
    Owned(Vec<u8>),
}

impl Bytes {
    fn get(&self) -> &[u8] {
        match self {
            // The mapping lives as long as self and is never written.
            Bytes::Mapped(m) => unsafe { std::slice::from_raw_parts(m.ptr, m.len) },
            #[cfg(test)]
            Bytes::Owned(v) => v,
        }
    }
}

/// All states of one character at one size.
pub struct CharAnim {
    pub w: u32,
    pub h: u32,
    clips: Vec<Clip>,
    /// Which clip each state plays.
    state_clip: [usize; 5],
    /// States whose own file failed to decode show idle, like states with no file at all.
    pub failed: [bool; 5],
    bytes: Bytes,
}

impl CharAnim {
    pub fn clip_of(&self, state: usize) -> usize {
        self.state_clip[state]
    }
}

// ---- preparing ----

/// Drops changes under the noise floor (rewriting `cur` to match `prev`) and returns the area that
/// still changed.
fn settle_noise(prev: &[u8], cur: &mut [u8], w: usize, h: usize) -> Option<Rect> {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        let start = y * w * 4;
        let row_prev = &prev[start..start + w * 4];
        let row_cur = &mut cur[start..start + w * 4];
        for x in 0..w {
            let (a, b) = (&row_prev[x * 4..x * 4 + 4], &mut row_cur[x * 4..x * 4 + 4]);
            if a == &*b {
                continue;
            }
            if a.iter().zip(b.iter()).all(|(p, c)| (*p as i32 - *c as i32).abs() <= NOISE_FLOOR) {
                b.copy_from_slice(a);
            } else {
                x0 = x0.min(x);
                x1 = x1.max(x + 1);
                y0 = y0.min(y);
                y1 = y1.max(y + 1);
            }
        }
    }
    (x1 > x0).then(|| (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// The area where two pictures differ at all.
fn exact_rect(a: &[u8], b: &[u8], w: usize, h: usize) -> Option<Rect> {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        let s = y * w * 4;
        let (ra, rb) = (&a[s..s + w * 4], &b[s..s + w * 4]);
        if ra == rb {
            continue;
        }
        y0 = y0.min(y);
        y1 = y1.max(y + 1);
        for x in 0..w {
            if ra[x * 4..x * 4 + 4] != rb[x * 4..x * 4 + 4] {
                x0 = x0.min(x);
                x1 = x1.max(x + 1);
            }
        }
    }
    (x1 > x0).then(|| (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// Compressed XOR of two pictures over one rectangle, appended to the blob store.
fn pack_diff(prev: &[u8], cur: &[u8], w: usize, rect: Rect, delay: u32, store: &mut Vec<u8>) -> FrameRec {
    let (rx, ry, rw, rh) = (rect.0 as usize, rect.1 as usize, rect.2 as usize, rect.3 as usize);
    let mut diff = Vec::with_capacity(rw * rh * 4);
    for y in ry..ry + rh {
        let s = (y * w + rx) * 4;
        diff.extend(prev[s..s + rw * 4].iter().zip(&cur[s..s + rw * 4]).map(|(a, b)| a ^ b));
    }
    let data = lz4_flex::compress_prepend_size(&diff);
    let offset = store.len() as u32;
    store.extend_from_slice(&data);
    FrameRec { offset, len: data.len() as u32, delay, rect }
}

/// Turns pictures into a clip. Frames identical after noise removal add their time to the one before,
/// so the timer doesn't even wake up for them.
fn encode_clip(frames: impl Iterator<Item = (Vec<u8>, u32)>, w: u32, h: u32, store: &mut Vec<u8>) -> Clip {
    let (wu, hu) = (w as usize, h as usize);
    let zero = vec![0u8; wu * hu * 4];
    let mut recs: Vec<FrameRec> = Vec::new();
    let mut first: Option<Vec<u8>> = None;
    let mut prev: Option<Vec<u8>> = None;
    for (mut px, delay) in frames {
        match &prev {
            None => {
                recs.push(pack_diff(&zero, &px, wu, (0, 0, w, h), delay, store));
                first = Some(px.clone());
            }
            Some(p) => match settle_noise(p, &mut px, wu, hu) {
                Some(rect) => recs.push(pack_diff(p, &px, wu, rect, delay, store)),
                None => {
                    if let Some(last) = recs.last_mut() {
                        last.delay = last.delay.saturating_add(delay);
                    }
                }
            },
        }
        prev = Some(px);
    }
    // The loop lands on exactly the first picture, so no noise settling here. Nothing to change means
    // nothing to store: playback treats a missing step on a moving clip as "unpack the first frame".
    let wrap = match (&first, &prev) {
        (Some(f), Some(p)) if recs.len() > 1 => {
            let rect = exact_rect(p, f, wu, hu).unwrap_or((0, 0, 1, 1));
            Some(pack_diff(p, f, wu, rect, 0, store))
        }
        _ => None,
    };
    Clip { frames: recs, wrap }
}

/// Where one decoded file goes on the common stage.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Place {
    left: f32,
    top: f32,
    factor: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct SourceBox {
    sw: u32,
    sh: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// One fixed stage for the whole character, so switching states never resizes or jumps:
/// - all files share one canvas size (one artist's export): one scale and offset from the union of
///   their visible areas, keeping the framing exactly as drawn;
/// - otherwise each is fitted into the stage on its own, centered and standing on the bottom.
fn plan_stage(boxes: &[SourceBox], max_w: f32, max_h: f32) -> (f32, f32, Vec<Place>) {
    let same = boxes.iter().all(|b| b.sw == boxes[0].sw && b.sh == boxes[0].sh);
    if same {
        let x0 = boxes.iter().map(|b| b.x).fold(f32::MAX, f32::min);
        let y0 = boxes.iter().map(|b| b.y).fold(f32::MAX, f32::min);
        let x1 = boxes.iter().map(|b| b.x + b.w).fold(0.0, f32::max);
        let y1 = boxes.iter().map(|b| b.y + b.h).fold(0.0, f32::max);
        let f = (max_h / (y1 - y0)).min(max_w / (x1 - x0));
        let place = Place { left: -x0 * f, top: -y0 * f, factor: f };
        return ((x1 - x0) * f, (y1 - y0) * f, vec![place; boxes.len()]);
    }
    let fits: Vec<f32> = boxes.iter().map(|b| (max_h / b.h).min(max_w / b.w)).collect();
    let w = boxes.iter().zip(&fits).map(|(b, f)| b.w * f).fold(0.0, f32::max);
    let h = boxes.iter().zip(&fits).map(|(b, f)| b.h * f).fold(0.0, f32::max);
    let places = boxes
        .iter()
        .zip(&fits)
        .map(|(b, f)| Place { left: (w - b.w * f) / 2.0 - b.x * f, top: h - b.h * f - b.y * f, factor: *f })
        .collect();
    (w, h, places)
}

fn box_of(f: &Frames) -> SourceBox {
    let (x, y, w, h) = media::alpha_box(&f.frames, f.w, f.h).unwrap_or((0, 0, f.w, f.h));
    SourceBox { sw: f.w, sh: f.h, x: x as f32, y: y as f32, w: w as f32, h: h as f32 }
}

struct Built {
    w: u32,
    h: u32,
    clips: Vec<Clip>,
    state_clip: [usize; 5],
    failed: [bool; 5],
    store: Vec<u8>,
}

/// Decodes and scales every state of a media character.
fn build_media(c: &Character, dir: &Path, max_w: f32, max_h: f32, dpi: f32) -> Result<Built, String> {
    // Different states can share a file (a missing state shows idle): decode each only once.
    let mut files: Vec<String> = Vec::new();
    let mut state_file = [0usize; 5];
    for (i, st) in c.states.iter().enumerate() {
        state_file[i] = files.iter().position(|f| *f == st.file).unwrap_or_else(|| {
            files.push(st.file.clone());
            files.len() - 1
        });
    }
    // Each file is decoded twice, one at a time: once to measure it for the stage, once to scale it.
    // Holding the raw frames of every state at once is more than big clips allow: five 1024 clips
    // came to nearly 3 GB, against about one clip's worth this way.
    let mut measured: Vec<Option<SourceBox>> = Vec::with_capacity(files.len());
    let mut idle_error = None;
    for (i, f) in files.iter().enumerate() {
        match media::decode_file(&dir.join(f), false) {
            Ok(d) => measured.push(Some(box_of(&d))),
            Err(e) => {
                if i == state_file[0] {
                    idle_error = Some(format!("{f}: {e}"));
                }
                measured.push(None);
            }
        }
    }
    if let Some(e) = idle_error {
        return Err(e);
    }
    let live: Vec<usize> = (0..measured.len()).filter(|i| measured[*i].is_some()).collect();
    let boxes: Vec<SourceBox> = live.iter().filter_map(|i| measured[*i]).collect();
    let (lw, lh, places) = plan_stage(&boxes, max_w, max_h);
    let (w, h) = (((lw * dpi).ceil() as u32).max(1), ((lh * dpi).ceil() as u32).max(1));

    let mut store = Vec::new();
    let mut clip_of_file = vec![usize::MAX; measured.len()];
    let mut clips = Vec::with_capacity(live.len());
    for (n, i) in live.iter().enumerate() {
        let f = match media::decode_file(&dir.join(&files[*i]), false) {
            Ok(f) => f,
            // Read fine a moment ago: changed on disk since. Only idle can't do without.
            Err(e) if *i == state_file[0] => return Err(format!("{}: {e}", files[*i])),
            Err(_) => continue,
        };
        let p = places[n];
        let per_pixel = 1.0 / (p.factor * dpi);
        let mut r = Resampler::new(f.w, f.h, -p.left / p.factor, -p.top / p.factor, per_pixel, w, h);
        let clip = encode_clip(f.frames.into_iter().map(|(px, d)| (r.run(&px), d)), w, h, &mut store);
        clip_of_file[*i] = clips.len();
        clips.push(clip);
    }
    let idle_clip = clip_of_file[state_file[0]];
    let mut state_clip = [idle_clip; 5];
    let mut failed = [false; 5];
    for i in 0..5 {
        let c = clip_of_file[state_file[i]];
        if c == usize::MAX {
            failed[i] = true;
        } else {
            state_clip[i] = c;
        }
    }
    Ok(Built { w, h, clips, state_clip, failed, store })
}

/// Draws a whole loop of each state of a built-in character once, so playing it is no more work than
/// a video: drawing vector shapes every frame is what would actually cost CPU.
fn build_vector(kind: usize, dpi: f32) -> Built {
    let size = (render::CHAR_SIZE * dpi).ceil() as u32;
    let mut store = Vec::new();
    let mut clips = Vec::new();
    for state in library::STATES {
        let cycle = render::cycle_of(state);
        let fps = if matches!(state, State::Idle | State::Error) { VECTOR_FPS_CALM } else { VECTOR_FPS_ACTIVE };
        let count = (cycle * fps).round().max(1.0) as u32;
        let delay = (1000.0 / fps).round() as u32;
        let frames = (0..count).map(|i| {
            let mut px = vec![0u8; (size * size * 4) as usize];
            if let Some(mut pm) = PixmapMut::from_bytes(&mut px, size, size) {
                let k = render::CHAR_SIZE * dpi / 200.0;
                render::draw_character(&mut pm, Transform::from_scale(k, k), kind, state, i as f32 * cycle / count as f32);
            }
            (px, delay)
        });
        clips.push(encode_clip(frames, size, size, &mut store));
    }
    Built { w: size, h: size, clips, state_clip: [0, 1, 2, 3, 4], failed: [false; 5], store }
}

// ---- disk format ----

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_rec(out: &mut Vec<u8>, r: &FrameRec) {
    for v in [r.offset, r.len, r.delay, r.rect.0, r.rect.1, r.rect.2, r.rect.3] {
        put_u32(out, v);
    }
}

fn serialize(b: &Built) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.store.len() + 4096);
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, b.w);
    put_u32(&mut out, b.h);
    for i in 0..5 {
        put_u32(&mut out, b.state_clip[i] as u32);
        out.push(b.failed[i] as u8);
    }
    put_u32(&mut out, b.clips.len() as u32);
    for clip in &b.clips {
        put_u32(&mut out, clip.frames.len() as u32);
        clip.frames.iter().for_each(|f| put_rec(&mut out, f));
        out.push(clip.wrap.is_some() as u8);
        if let Some(w) = &clip.wrap {
            put_rec(&mut out, w);
        }
    }
    out.extend_from_slice(&b.store);
    out
}

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.p.checked_add(n)?;
        let s = self.d.get(self.p..end)?;
        self.p = end;
        Some(s)
    }

    fn u32(&mut self) -> Option<u32> {
        let b = self.bytes(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }

    fn rec(&mut self) -> Option<FrameRec> {
        Some(FrameRec { offset: self.u32()?, len: self.u32()?, delay: self.u32()?, rect: (self.u32()?, self.u32()?, self.u32()?, self.u32()?) })
    }
}

/// A frame must not be able to read or write outside what it should.
fn valid_rec(r: &FrameRec, blob_len: usize, w: u32, h: u32) -> bool {
    let (x, y, rw, rh) = r.rect;
    let in_frame = rw > 0 && rh > 0 && x.checked_add(rw).is_some_and(|e| e <= w) && y.checked_add(rh).is_some_and(|e| e <= h);
    let in_blob = (r.offset as usize).checked_add(r.len as usize).is_some_and(|e| e <= blob_len);
    in_frame && in_blob && r.len >= 4
}

/// None for anything malformed: a damaged cache file is rebuilt, never trusted.
fn parse(bytes: Bytes) -> Option<CharAnim> {
    let d = bytes.get();
    let mut r = Reader { d, p: 0 };
    if r.bytes(4)? != MAGIC {
        return None;
    }
    let (w, h) = (r.u32()?, r.u32()?);
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return None;
    }
    let mut state_clip = [0usize; 5];
    let mut failed = [false; 5];
    for i in 0..5 {
        state_clip[i] = r.u32()? as usize;
        failed[i] = r.u8()? != 0;
    }
    let count = r.u32()? as usize;
    if count == 0 || count > 16 || state_clip.iter().any(|c| *c >= count) {
        return None;
    }
    let mut clips = Vec::with_capacity(count);
    for _ in 0..count {
        let n = r.u32()? as usize;
        if n == 0 || n > 100_000 {
            return None;
        }
        let mut frames = Vec::with_capacity(n.min(d.len() / 28));
        for _ in 0..n {
            frames.push(r.rec()?);
        }
        let wrap = if r.u8()? != 0 { Some(r.rec()?) } else { None };
        clips.push(Clip { frames, wrap });
    }
    let blob_start = r.p;
    let blob_len = d.len() - blob_start;
    let all_valid = clips.iter().all(|c| c.frames.iter().chain(c.wrap.iter()).all(|f| valid_rec(f, blob_len, w, h)));
    if !all_valid || clips.iter().any(|c| c.frames[0].rect != (0, 0, w, h)) {
        return None;
    }
    // Offsets are stored relative to the blob; they become absolute here, once, for simple lookups.
    let blob_start = u32::try_from(blob_start).ok()?;
    for c in &mut clips {
        for f in c.frames.iter_mut().chain(c.wrap.iter_mut()) {
            f.offset = f.offset.checked_add(blob_start)?;
        }
    }
    Some(CharAnim { w, h, clips, state_clip, failed, bytes })
}

pub fn cache_dir() -> PathBuf {
    crate::config::dir().join("cache")
}

fn fnv(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= *b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

/// Changes whenever anything that affects the prepared frames does.
fn cache_key(c: &Character, max_w: f32, max_h: f32, dpi: f32) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    fnv(&mut h, &CACHE_VERSION.to_le_bytes());
    fnv(&mut h, c.id.as_bytes());
    for v in [max_w, max_h, dpi] {
        fnv(&mut h, &v.to_le_bytes());
    }
    if let Some(dir) = c.dir() {
        for st in &c.states {
            fnv(&mut h, st.file.as_bytes());
            if let Ok(m) = fs::metadata(dir.join(&st.file)) {
                fnv(&mut h, &m.len().to_le_bytes());
                let t = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
                fnv(&mut h, &t.to_le_bytes());
            }
        }
    }
    h
}

fn cache_file(c: &Character, max_w: f32, max_h: f32, dpi: f32) -> PathBuf {
    cache_dir().join(format!("{}-{:016x}.anim", c.id, cache_key(c, max_w, max_h, dpi)))
}

/// `--prepare <id> <max_w> <max_h> <dpi>`: does the slow part in a process of its own, because decoding
/// a long video can take hundreds of MB, and exiting is the one sure way to give all of that back.
pub fn prepare_main(args: &[String]) -> i32 {
    let num = |i: usize| args.get(i).and_then(|v| v.parse::<f32>().ok());
    let (Some(id), Some(max_w), Some(max_h), Some(dpi)) = (args.first(), num(1), num(2), num(3)) else {
        return 2;
    };
    #[cfg(unix)]
    unsafe { libc::nice(5) };
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    let all = library::load_all(&crate::config::user_root());
    let Some(c) = all.iter().find(|c| c.id == *id) else {
        eprint!("{}", crate::tf!("không tìm thấy nhân vật {id}", "character {id} not found"));
        return 1;
    };
    let built = match &c.source {
        Source::Vector(kind) => Ok(build_vector(*kind, dpi)),
        Source::Folder(dir) => build_media(c, dir, max_w, max_h, dpi),
    };
    match built {
        Ok(b) => {
            let folder = cache_dir();
            if let Err(e) = fs::create_dir_all(&folder) {
                eprint!("{e}");
                return 1;
            }
            drop_stale(&folder, id);
            let file = cache_file(c, max_w, max_h, dpi);
            let tmp = file.with_extension(format!("tmp{}", std::process::id()));
            if fs::write(&tmp, serialize(&b)).is_err() || fs::rename(&tmp, &file).is_err() {
                let _ = fs::remove_file(&tmp);
                eprint!("{}", crate::i18n::t("không ghi được cache", "could not write the cache"));
                return 1;
            }
            0
        }
        Err(e) => {
            eprint!("{e}");
            1
        }
    }
}

/// Prepared frames of a character, from the cache when possible. Waits for a helper process on a
/// miss: worker threads only.
/// The same file asked for again while its helper runs (a character clicked again while it loads)
/// waits for that helper instead of starting another one beside it.
pub fn load(c: &Character, max_w: f32, max_h: f32, dpi: f32) -> Result<Arc<CharAnim>, String> {
    let file = cache_file(c, max_w, max_h, dpi);
    let open = || map_file(&file).and_then(|m| parse(Bytes::Mapped(m)));
    if let Some(a) = open() {
        return Ok(Arc::new(a));
    }
    let _busy = loop {
        let mut busy = PREPARING.lock().unwrap_or_else(|e| e.into_inner());
        if !busy.contains(&file) {
            // A helper that finished just now has left the file behind.
            if let Some(a) = open() {
                return Ok(Arc::new(a));
            }
            busy.push(file.clone());
            break Preparing(file.clone());
        }
        while busy.contains(&file) {
            busy = PREPARED.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        drop(busy);
        // Ready now, unless that helper failed: then this one tries for itself.
        if let Some(a) = open() {
            return Ok(Arc::new(a));
        }
    };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = std::process::Command::new(exe)
        .args(["--prepare", &c.id, &max_w.to_string(), &max_h.to_string(), &dpi.to_string()])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    open().map(Arc::new).ok_or_else(|| t("không đọc được file đã chuẩn bị", "could not read the prepared file").to_string())
}

/// Cache files a helper process of this app is writing now.
static PREPARING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static PREPARED: Condvar = Condvar::new();

/// Marks a file as being prepared until dropped, however the helper ends.
struct Preparing(PathBuf);

impl Drop for Preparing {
    fn drop(&mut self) {
        PREPARING.lock().unwrap_or_else(|e| e.into_inner()).retain(|f| *f != self.0);
        PREPARED.notify_all();
    }
}

/// The pixel density at which a media character's frames hold one source pixel each, judged from the
/// first frame of idle: preparing any denser only stores upscaled blur. None for drawn characters.
pub fn native_dpi(c: &Character, max_w: f32, max_h: f32) -> Option<f32> {
    let dir = c.dir()?;
    let f = media::decode_file(&dir.join(&c.states[0].file), true).ok()?;
    let b = box_of(&f);
    Some((b.h / max_h).max(b.w / max_w)).filter(|d| d.is_finite() && *d > 0.0)
}

/// Keeps the cache from growing forever: only the newest few builds of a character stay, enough for
/// its buddy and preview sizes.
fn drop_stale(folder: &Path, id: &str) {
    const KEEP: usize = 3;
    let prefix = format!("{id}-");
    let Ok(entries) = fs::read_dir(folder) else { return };
    let mut mine: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            // A 16-digit key right after the prefix: "bear-2-…" must not count as "bear".
            n.strip_prefix(&prefix).is_some_and(|rest| rest.len() == 16 + ".anim".len() && rest.ends_with(".anim"))
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if mine.len() < KEEP {
        return;
    }
    mine.sort();
    for (_, p) in &mine[..mine.len() + 1 - KEEP] {
        let _ = fs::remove_file(p); // one still in use stays until next time
    }
}

/// Everything cached for a character that is gone or changed.
pub fn forget(id: &str) {
    let prefix = format!("{id}-");
    let Ok(entries) = fs::read_dir(cache_dir()) else { return };
    for e in entries.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.strip_prefix(&prefix).is_some_and(|rest| rest.len() == 16 + ".anim".len() || rest.len() == 16 + ".thumb".len()) {
            let _ = fs::remove_file(e.path());
        }
    }
}

// ---- thumbnails ----

/// First frame of a file, trimmed and centered on a square: premultiplied BGRA.
pub fn thumb_of_file(path: &Path) -> Result<Vec<u8>, String> {
    let f = media::decode_file(path, true)?;
    let (x, y, w, h) = media::alpha_box(&f.frames, f.w, f.h).unwrap_or((0, 0, f.w, f.h));
    let s = THUMB_SIZE as f32;
    let k = (s - THUMB_PADDING) / w.max(h) as f32;
    let (ox, oy) = ((s - w as f32 * k) / 2.0, (s - h as f32 * k) / 2.0);
    let mut r = Resampler::new(f.w, f.h, x as f32 - ox / k, y as f32 - oy / k, 1.0 / k, THUMB_SIZE, THUMB_SIZE);
    let (px, _) = &f.frames[0];
    Ok(r.run(px))
}

/// Thumbnail of a character's idle state, cached like the frames.
pub fn thumb_of(c: &Character) -> Result<Vec<u8>, String> {
    let Some(dir) = c.dir() else { return Err(t("nhân vật này không có file media", "this character has no media file").into()) };
    let key = cache_key(c, 0.0, 0.0, 0.0);
    let file = cache_dir().join(format!("{}-{key:016x}.thumb", c.id));
    let want = (THUMB_SIZE * THUMB_SIZE * 4) as usize;
    if let Some(px) = fs::read(&file).ok().and_then(|d| lz4_flex::decompress_size_prepended(&d).ok()).filter(|p| p.len() == want) {
        return Ok(px);
    }
    let px = thumb_of_file(&dir.join(&c.states[0].file))?;
    if fs::create_dir_all(cache_dir()).is_ok() {
        let _ = fs::write(&file, lz4_flex::compress_prepend_size(&px));
    }
    Ok(px)
}

// ---- playback ----

fn union(a: Option<Rect>, b: Rect) -> Rect {
    match a {
        None => b,
        Some(a) => {
            let (x0, y0) = (a.0.min(b.0), a.1.min(b.1));
            let (x1, y1) = ((a.0 + a.2).max(b.0 + b.2), (a.1 + a.3).max(b.1 + b.3));
            (x0, y0, x1 - x0, y1 - y0)
        }
    }
}

/// Plays one clip of a CharAnim into a pixel buffer, unpacking at most what the clock calls for.
pub struct Player {
    pub anim: Arc<CharAnim>,
    clip: usize,
    frame: usize,
    /// Current frame; premultiplied BGRA.
    pub pixels: Vec<u8>,
    scratch: Vec<u8>,
    next_at: u64,
}

impl Player {
    pub fn new(anim: Arc<CharAnim>, state: usize, now: u64) -> Player {
        let len = (anim.w * anim.h * 4) as usize;
        let mut p = Player { clip: anim.clip_of(state), anim, frame: 0, pixels: vec![0; len], scratch: Vec::new(), next_at: 0 };
        p.restart(now);
        p
    }

    fn apply(&mut self, r: FrameRec) {
        let anim = self.anim.clone();
        let data = anim.bytes.get().get(r.offset as usize..r.offset as usize + r.len as usize).unwrap_or(&[]);
        let (rx, ry, rw, rh) = (r.rect.0 as usize, r.rect.1 as usize, r.rect.2 as usize, r.rect.3 as usize);
        let need = rw * rh * 4;
        self.scratch.resize(need, 0);
        // Checked sizes: a frame that won't unpack to exactly its rectangle is skipped, not guessed at.
        if data.len() < 4 || !matches!(lz4_flex::decompress_into(&data[4..], &mut self.scratch), Ok(n) if n == need) {
            return;
        }
        let w = anim.w as usize;
        for (row, diff) in self.scratch.chunks_exact(rw * 4).enumerate() {
            let s = ((ry + row) * w + rx) * 4;
            self.pixels[s..s + rw * 4].iter_mut().zip(diff).for_each(|(p, d)| *p ^= *d);
        }
    }

    fn restart(&mut self, now: u64) {
        self.frame = 0;
        self.pixels.fill(0);
        let first = self.anim.clips[self.clip].frames[0];
        self.apply(first);
        self.next_at = now + first.delay.max(1) as u64;
    }

    pub fn set_state(&mut self, state: usize, now: u64) {
        let c = self.anim.clip_of(state);
        if c != self.clip {
            self.clip = c;
            self.restart(now);
        }
    }

    pub fn clip(&self) -> &Clip {
        &self.anim.clips[self.clip]
    }

    /// Moves on to where the clock says. Returns the area of the picture that changed, if any.
    pub fn advance(&mut self, now: u64) -> Option<Rect> {
        let anim = self.anim.clone();
        let clip = &anim.clips[self.clip];
        if clip.frames.len() <= 1 || now < self.next_at {
            return None;
        }
        // Behind by a lot (machine was asleep, window was hidden): skip ahead instead of racing through.
        let total: u64 = clip.frames.iter().map(|f| f.delay.max(1) as u64).sum();
        if now - self.next_at > total {
            self.next_at = now;
        }
        let mut dirty = None;
        while now >= self.next_at {
            let next = self.frame + 1;
            let step = if next >= clip.frames.len() {
                self.frame = 0;
                clip.wrap
            } else {
                self.frame = next;
                Some(clip.frames[next])
            };
            match step {
                Some(r) => {
                    self.apply(r);
                    dirty = Some(union(dirty, r.rect));
                }
                None => {
                    self.pixels.fill(0);
                    self.apply(clip.frames[0]);
                    dirty = Some((0, 0, anim.w, anim.h));
                }
            }
            self.next_at += clip.frames[self.frame].delay.max(1) as u64;
        }
        dirty
    }
}

#[cfg(test)]
impl CharAnim {
    fn from_built(b: &Built) -> CharAnim {
        parse(Bytes::Owned(serialize(b))).expect("valid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![v; (w * h * 4) as usize]
    }

    fn built(frames: &[(u8, u32)]) -> Built {
        let mut store = Vec::new();
        let clip = encode_clip(frames.iter().map(|(v, d)| (frame(4, 4, *v), *d)), 4, 4, &mut store);
        Built { w: 4, h: 4, clips: vec![clip], state_clip: [0; 5], failed: [false; 5], store }
    }

    fn anim(frames: &[(u8, u32)]) -> Arc<CharAnim> {
        Arc::new(CharAnim::from_built(&built(frames)))
    }

    #[test]
    fn plays_frames_on_time_and_loops() {
        let mut p = Player::new(anim(&[(10, 100), (20, 100), (30, 100)]), 0, 0);
        assert_eq!(p.pixels[0], 10);
        assert_eq!(p.advance(50), None);
        assert!(p.advance(100).is_some());
        assert_eq!(p.pixels[0], 20);
        assert_eq!(p.advance(150), None);
        assert!(p.advance(200).is_some());
        assert_eq!(p.pixels[0], 30);
        assert!(p.advance(300).is_some());
        assert_eq!(p.pixels[0], 10, "wrapped to the start");
        assert!(p.advance(400).is_some());
        assert_eq!(p.pixels[0], 20);
    }

    #[test]
    fn catches_up_when_a_timer_tick_was_late() {
        let mut p = Player::new(anim(&[(10, 100), (20, 100), (30, 100), (40, 100)]), 0, 0);
        assert!(p.advance(250).is_some());
        assert_eq!(p.pixels[0], 30);
    }

    #[test]
    fn a_long_sleep_does_not_spin() {
        let mut p = Player::new(anim(&[(10, 33), (20, 33)]), 0, 0);
        assert!(p.advance(u32::MAX as u64 * 100).is_some());
        assert!(p.pixels[0] == 10 || p.pixels[0] == 20);
    }

    #[test]
    fn still_clips_never_redraw() {
        let mut p = Player::new(anim(&[(10, 0)]), 0, 0);
        assert_eq!(p.advance(1_000_000), None);
        assert!(p.clip().is_still());
    }

    #[test]
    fn identical_and_noise_only_frames_add_time_instead_of_wakeups() {
        let a = anim(&[(50, 100), (50, 100), (52, 100), (49, 100), (90, 100)]);
        assert_eq!(a.clips[0].frames.len(), 2);
        assert_eq!(a.clips[0].frames[0].delay, 400);
        let mut p = Player::new(a, 0, 0);
        assert_eq!(p.advance(399), None);
        assert!(p.advance(400).is_some());
        assert_eq!(p.pixels[0], 90);
    }

    #[test]
    fn only_the_changed_area_is_reported_and_unpacked() {
        let (w, h) = (8u32, 8u32);
        let a = frame(w, h, 0);
        let mut b = a.clone();
        b[((2 * w + 3) * 4) as usize..((2 * w + 4) * 4) as usize].copy_from_slice(&[200, 200, 200, 255]); // (3,2)
        b[((5 * w + 6) * 4) as usize..((5 * w + 7) * 4) as usize].copy_from_slice(&[200, 200, 200, 255]); // (6,5)
        let mut store = Vec::new();
        let clip = encode_clip([(a.clone(), 100), (b.clone(), 100)].into_iter(), w, h, &mut store);
        assert_eq!(clip.frames[1].rect, (3, 2, 4, 4));
        let an = Arc::new(CharAnim::from_built(&Built { w, h, clips: vec![clip], state_clip: [0; 5], failed: [false; 5], store }));
        let mut p = Player::new(an, 0, 0);
        assert_eq!(p.pixels, a);
        assert_eq!(p.advance(100), Some((3, 2, 4, 4)));
        assert_eq!(p.pixels, b);
        assert_eq!(p.advance(200), Some((3, 2, 4, 4)), "looping back only touches the same area");
        assert_eq!(p.pixels, a, "and lands exactly on the first picture");
    }

    #[test]
    fn noise_never_accumulates_into_visible_drift() {
        // A pixel creeping up slowly must eventually move, not stay frozen at its first value forever.
        let frames: Vec<(u8, u32)> = (0..40).map(|i| (100 + i, 33)).collect();
        let a = anim(&frames);
        assert!(a.clips[0].frames.len() > 5, "slow change still produces steps");
        let mut p = Player::new(a, 0, 0);
        let mut t = 0;
        let mut worst = 0i32;
        for i in 0..40u8 {
            let _ = p.advance(t);
            worst = worst.max((p.pixels[0] as i32 - (100 + i) as i32).abs());
            t += 33;
        }
        assert!(worst <= NOISE_FLOOR + 1, "drifted by {worst}");
    }

    #[test]
    fn switching_states_restarts_only_on_a_different_clip() {
        let mut store = Vec::new();
        let c0 = encode_clip([(frame(2, 2, 10), 100), (frame(2, 2, 90), 100)].into_iter(), 2, 2, &mut store);
        let c1 = encode_clip([(frame(2, 2, 200), 100)].into_iter(), 2, 2, &mut store);
        let a = Arc::new(CharAnim::from_built(&Built { w: 2, h: 2, clips: vec![c0, c1], state_clip: [0, 1, 0, 0, 0], failed: [false; 5], store }));
        let mut p = Player::new(a, 0, 0);
        p.advance(100);
        assert_eq!(p.pixels[0], 90);
        p.set_state(2, 110); // same clip: keeps playing
        assert_eq!(p.pixels[0], 90);
        p.set_state(1, 120);
        assert_eq!(p.pixels[0], 200);
    }

    #[test]
    fn unchanging_frames_pack_tightly() {
        let mut store = Vec::new();
        let _ = encode_clip((0..10).map(|_| (frame(100, 100, 77), 33)), 100, 100, &mut store);
        assert!(store.len() < 1_000, "{} bytes for one flat 40 KB frame", store.len());
    }

    #[test]
    fn damaged_caches_never_cause_harm() {
        let good = serialize(&built(&[(10, 100), (90, 50)]));
        assert!(parse(Bytes::Owned(good.clone())).is_some());
        for cut in 0..good.len() {
            if let Some(a) = parse(Bytes::Owned(good[..cut].to_vec())) {
                // Only compressed data can be missing, and playback checks that.
                let mut p = Player::new(Arc::new(a), 0, 0);
                p.advance(100);
            }
        }
        assert!(parse(Bytes::Owned(b"nope".to_vec())).is_none());
        // A state pointing at a clip that doesn't exist
        let mut bad_clip = good.clone();
        bad_clip[12] = 9;
        assert!(parse(Bytes::Owned(bad_clip)).is_none());
        // Flipping any byte must never panic later, whatever parsing decides
        for i in 0..good.len() {
            let mut d = good.clone();
            d[i] ^= 0xA5;
            if let Some(a) = parse(Bytes::Owned(d)) {
                let mut p = Player::new(Arc::new(a), 0, 0);
                for t in 0..5 {
                    p.advance(t * 100);
                }
            }
        }
    }

    #[test]
    fn built_in_characters_loop_seamlessly() {
        for state in library::STATES {
            let cycle = render::cycle_of(state);
            let draw = |t: f32| {
                let mut px = vec![0u8; 200 * 200 * 4];
                let mut pm = PixmapMut::from_bytes(&mut px, 200, 200).unwrap();
                render::draw_character(&mut pm, Transform::identity(), 0, state, t);
                px
            };
            // The instant before the loop restarts must look like its start, or it would visibly jump.
            let (a, b) = (draw(0.0), draw(cycle - 0.0001));
            let changed = a.iter().zip(&b).filter(|(x, y)| (**x as i32 - **y as i32).abs() > 24).count();
            assert!(changed < 40, "{state:?} jumps at the loop: {changed} bytes differ");
        }
    }

    #[test]
    fn built_in_characters_build_small() {
        let b = build_vector(0, 2.0);
        let total = serialize(&b).len();
        assert!(total < 8_000_000, "{total} bytes");
        assert_eq!(CharAnim::from_built(&b).w, 320);
    }

    fn sb(sw: u32, sh: u32, x: f32, y: f32, w: f32, h: f32) -> SourceBox {
        SourceBox { sw, sh, x, y, w, h }
    }

    #[test]
    fn same_canvas_shares_one_framing() {
        let (w, h, p) = plan_stage(&[sb(100, 100, 10.0, 20.0, 50.0, 50.0), sb(100, 100, 30.0, 10.0, 60.0, 40.0)], 264.0, 190.0);
        // Union: x 10..90, y 10..70, so 80x60; height allows 3.17, width allows 3.3
        let f = 190.0 / 60.0;
        assert!((h - 190.0).abs() < 0.01 && (w - 80.0 * f).abs() < 0.01);
        assert_eq!(p[0], p[1]);
        assert!((p[0].left + 10.0 * f).abs() < 0.01 && (p[0].top + 10.0 * f).abs() < 0.01);
    }

    #[test]
    fn wide_characters_are_limited_by_width() {
        let (w, h, _) = plan_stage(&[sb(1000, 100, 0.0, 0.0, 1000.0, 100.0)], 264.0, 190.0);
        assert!((w - 264.0).abs() < 0.01);
        assert!((h - 26.4).abs() < 0.01);
    }

    #[test]
    fn mixed_canvases_stand_on_the_bottom_centered() {
        let (w, h, p) = plan_stage(&[sb(100, 100, 0.0, 0.0, 100.0, 100.0), sb(200, 400, 0.0, 0.0, 200.0, 200.0)], 200.0, 100.0);
        assert_eq!((w, h), (100.0, 100.0));
        assert_eq!(p[0], Place { left: 0.0, top: 0.0, factor: 1.0 });
        assert_eq!(p[1].factor, 0.5);
    }
}

/// Whole-pipeline checks on real files: BUDDY_SAMPLES=<characters folder> cargo test --release real_ -- --ignored --nocapture
#[cfg(test)]
mod real_files {
    use super::*;

    fn samples() -> PathBuf {
        PathBuf::from(std::env::var("BUDDY_SAMPLES").expect("BUDDY_SAMPLES"))
    }

    #[test]
    #[ignore]
    fn real_characters_prepare_and_play_cheaply() {
        for id in ["bear", "black-cat", "guidang-potato", "minato", "uncle-steve"] {
            let c = library::load_dir(&samples().join(id)).unwrap();
            let t0 = std::time::Instant::now();
            let b = build_media(&c, c.dir().unwrap(), MAX_WIDTH, (BASE_HEIGHT * c.scale).min(MAX_STAGE_HEIGHT), 2.0).unwrap();
            let built = t0.elapsed().as_secs_f64();
            let bytes = serialize(&b).len();
            let a = Arc::new(CharAnim::from_built(&b));
            assert!(a.failed.iter().all(|f| !f), "{id}: a state failed to decode");
            let full = a.w as u64 * a.h as u64;
            let (mut frames_total, mut area, mut worst) = (0u64, 0u64, 0f64);
            for state in 0..5 {
                let mut p = Player::new(a.clone(), state, 0);
                let n = p.clip().frames.len();
                frames_total += n as u64;
                area += p.clip().frames.iter().skip(1).map(|f| f.rect.2 as u64 * f.rect.3 as u64).sum::<u64>();
                let t1 = std::time::Instant::now();
                let mut clock = 0u64;
                for _ in 0..n * 3 {
                    clock += p.clip().frames.iter().map(|f| f.delay as u64).max().unwrap_or(33);
                    p.advance(clock);
                }
                worst = worst.max(t1.elapsed().as_secs_f64() * 1000.0 / (n * 3) as f64);
            }
            println!(
                "{id}: stage {}x{}, {frames_total} frames, build {built:.2}s, cache {:.1} MB, average changed area {:.0}%, unpack worst {worst:.2} ms/frame",
                a.w,
                a.h,
                bytes as f64 / 1e6,
                area as f64 * 100.0 / (frames_total.max(1) * full) as f64
            );
        }
    }

    /// Writes one still of each character's states on gray for eyeballing, into $PREVIEW_DIR.
    #[test]
    #[ignore]
    fn real_preview_sheet() {
        let dir = std::env::var("PREVIEW_DIR").expect("PREVIEW_DIR");
        for id in ["bear", "minato"] {
            let c = library::load_dir(&samples().join(id)).unwrap();
            let b = build_media(&c, c.dir().unwrap(), MAX_WIDTH, BASE_HEIGHT, 1.0).unwrap();
            let a = Arc::new(CharAnim::from_built(&b));
            let (w, h) = (a.w as usize, a.h as usize);
            let mut sheet = vec![0u8; w * 5 * h * 4];
            for s in 0..5 {
                let p = Player::new(a.clone(), s, 0);
                for y in 0..h {
                    let d = (y * w * 5 + s * w) * 4;
                    sheet[d..d + w * 4].copy_from_slice(&p.pixels[y * w * 4..(y + 1) * w * 4]);
                }
            }
            render::write_test_png(&format!("{dir}/{id}.png"), &sheet, (w * 5) as u32, h as u32);
        }
    }
}
