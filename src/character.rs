//! Pixel character system: a separate top-most, layered window that floats just
//! above the taskbar widget and shows a roaming pixel cat and/or dog.
//!
//! The window is layered (UpdateLayeredWindow with per-pixel alpha) but NOT
//! click-through, so mouse input is delivered only over the character's opaque
//! pixels (Windows passes clicks through fully transparent pixels for layered
//! windows automatically). That lets us support hover and click reactions while
//! still letting the empty area click through to whatever is behind it.
//!
//! Sprites are composed from solid blocks straight into a 32-bit DIB on a 32x32
//! logical grid, scaled by an integer factor for crisp (non-blurry) output. The
//! art is isolated in `character_blocks` / `draw_character` so it can later be
//! swapped for PNG sprite sheets without touching the rest.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::localization::LanguageId;
use crate::native_interop;

const CLASS_NAME: &str = "ClaudeCodexCharacter";
const TIMER_ANIM: usize = 101;
const ANIM_INTERVAL_MS: u32 = 80;
// Not exported by the windows crate's WindowsAndMessaging glob in this version.
const WM_MOUSELEAVE: u32 = 0x02A3;

// Sprite is drawn on a 32x32 logical grid (the target render size at 96 DPI).
const SPRITE: i32 = 32;
// Window in logical pixels (room for two roaming characters + bubbles above).
const WIN_LW: i32 = 184;
const WIN_LH: i32 = 72;
// Character baseline (top of sprite) in logical pixels.
const BASE_OY: i32 = WIN_LH - SPRITE - 2;

// Timing (ticks of ANIM_INTERVAL_MS).
const HOVER_DELAY_TICKS: u64 = 12; // ~1.0s before the hover message shows
const CLICK_BUBBLE_TICKS: u32 = 31; // ~2.5s
const CLICKED_POSE_TICKS: u32 = 6; // ~0.4s
const HOVER_BUBBLE_TICKS: u32 = 36;
const THRESHOLD_BUBBLE_TICKS: u32 = 50;
const IDLE_BUBBLE_TICKS: u32 = 30;
const FADE_TICKS: u32 = 4;
const IDLE_COOLDOWN: u64 = 220;

// Bubble priorities (higher wins; click must not override an active threshold).
const PRI_IDLE: u8 = 0;
const PRI_HOVER: u8 = 1;
const PRI_CLICK: u8 = 2;
const PRI_THRESHOLD: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharacterKind {
    Cat,
    Dog,
    Both,
}

impl CharacterKind {
    pub fn code(self) -> &'static str {
        match self {
            CharacterKind::Cat => "cat",
            CharacterKind::Dog => "dog",
            CharacterKind::Both => "both",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "cat" => Some(CharacterKind::Cat),
            "dog" => Some(CharacterKind::Dog),
            "both" => Some(CharacterKind::Both),
            _ => None,
        }
    }

    fn shows_cat(self) -> bool {
        matches!(self, CharacterKind::Cat | CharacterKind::Both)
    }

    fn shows_dog(self) -> bool {
        matches!(self, CharacterKind::Dog | CharacterKind::Both)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pose {
    Idle,
    Walk,
    React,
    Hover,
    Clicked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pool {
    Hover,
    Click,
    Idle,
    Soft,
    Urgent,
    Rest,
    Encourage,
}

const POOL_COUNT: usize = 7;

fn pool_index(p: Pool) -> usize {
    match p {
        Pool::Hover => 0,
        Pool::Click => 1,
        Pool::Idle => 2,
        Pool::Soft => 3,
        Pool::Urgent => 4,
        Pool::Rest => 5,
        Pool::Encourage => 6,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Band {
    Low,
    Soft,
    Urgent,
}

fn band_for(percent: f64) -> Band {
    if percent >= 90.0 {
        Band::Urgent
    } else if percent >= 80.0 {
        Band::Soft
    } else {
        Band::Low
    }
}

struct Bubble {
    text: String,
    ttl: u32,
    max_ttl: u32,
    pri: u8,
}

struct Critter {
    is_cat: bool,
    variant: u8,
    x: f32,
    dir: f32,
    moving: bool,
    move_ticks: u32,
    pose: Pose,
    clicked_ticks: u32,
    react_ticks: u32,
    hovering: bool,
    hover_since: Option<u64>,
    blink_ctr: u32,
    bubble: Option<Bubble>,
    last_idx: [i32; POOL_COUNT],
}

impl Critter {
    fn new(is_cat: bool, variant: u8, x: f32, dir: f32) -> Self {
        Self {
            is_cat,
            variant,
            x,
            dir,
            moving: true,
            move_ticks: 0,
            pose: Pose::Walk,
            clicked_ticks: 0,
            react_ticks: 0,
            hovering: false,
            hover_since: None,
            blink_ctr: 0,
            bubble: None,
            last_idx: [-1; POOL_COUNT],
        }
    }

    fn pose_now(&self) -> Pose {
        if self.clicked_ticks > 0 {
            Pose::Clicked
        } else if self.hovering {
            Pose::Hover
        } else if self.react_ticks > 0 {
            Pose::React
        } else if self.moving {
            Pose::Walk
        } else {
            Pose::Idle
        }
    }
}

struct CharState {
    hwnd: isize,
    enabled: bool,
    kind: CharacterKind,
    lang: LanguageId,
    uscale: i32,
    win_w: i32,
    win_h: i32,
    frame: u64,
    cat: Critter,
    dog: Critter,
    last_band: Band,
    mood: Band,
    last_idle_frame: u64,
    last_pred_frame: u64,
    tracking_leave: bool,
}

static STATE: Mutex<Option<CharState>> = Mutex::new(None);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
static RNG: AtomicU64 = AtomicU64::new(0);

fn rng_next() -> u64 {
    let mut x = RNG.load(Ordering::Relaxed);
    if x == 0 {
        x = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RNG.store(x, Ordering::Relaxed);
    x
}

/// Pick a random message from the pool without repeating the previous one.
fn pick_message(critter: &mut Critter, lang: LanguageId, pool: Pool) -> String {
    let msgs = message_pool(lang, critter.is_cat, pool);
    if msgs.is_empty() {
        return String::new();
    }
    if msgs.len() == 1 {
        return msgs[0].to_string();
    }
    let slot = pool_index(pool);
    let last = critter.last_idx[slot];
    let mut idx = (rng_next() as usize % msgs.len()) as i32;
    if idx == last {
        idx = ((idx as usize + 1) % msgs.len()) as i32;
    }
    critter.last_idx[slot] = idx;
    msgs[idx as usize].to_string()
}

fn set_bubble(critter: &mut Critter, text: String, ttl: u32, pri: u8) {
    // Don't let a lower-priority message replace a still-visible higher one.
    if let Some(b) = critter.bubble.as_ref() {
        if b.ttl > 0 && b.pri > pri {
            return;
        }
    }
    if text.is_empty() {
        return;
    }
    critter.bubble = Some(Bubble {
        text,
        ttl,
        max_ttl: ttl,
        pri,
    });
}

/// Create (once) and show the character window. Safe to call again to refresh
/// the enabled state / kind / language / anchor.
pub fn init(anchor: RECT, enabled: bool, kind: CharacterKind, lang: LanguageId) {
    {
        let guard = STATE.lock().unwrap();
        if guard.is_some() {
            drop(guard);
            set_kind(kind);
            set_language(lang);
            set_enabled(enabled);
            reposition(anchor);
            return;
        }
    }

    register_class();

    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let class = native_interop::wide_str(CLASS_NAME);
        let title = native_interop::wide_str("Claude & Codex Character");
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            10,
            10,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap_or_default()
    };
    if hwnd.is_invalid() {
        return;
    }

    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let dpi = if dpi == 0 { 96 } else { dpi };
    let uscale = ((dpi as i32 + 48) / 96).max(1);
    let win_w = WIN_LW * uscale;
    let win_h = WIN_LH * uscale;

    let max_x = (WIN_LW - SPRITE) as f32;
    let state = CharState {
        hwnd: hwnd.0 as isize,
        enabled,
        kind,
        lang,
        uscale,
        win_w,
        win_h,
        frame: 0,
        cat: Critter::new(true, 0, 8.0, 1.0),
        dog: Critter::new(false, 0, max_x - 8.0, -1.0),
        last_band: Band::Low,
        mood: Band::Low,
        last_idle_frame: 0,
        last_pred_frame: 0,
        tracking_leave: false,
    };
    *STATE.lock().unwrap() = Some(state);

    reposition(anchor);
    if enabled {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetTimer(hwnd, TIMER_ANIM, ANIM_INTERVAL_MS, None);
        }
        render();
    }
}

fn register_class() {
    if CLASS_REGISTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let class = native_interop::wide_str(CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR::from_raw(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

pub fn set_enabled(enabled: bool) {
    let hwnd = {
        let mut guard = STATE.lock().unwrap();
        let Some(s) = guard.as_mut() else {
            return;
        };
        if s.enabled == enabled {
            return;
        }
        s.enabled = enabled;
        HWND(s.hwnd as *mut _)
    };
    unsafe {
        if enabled {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetTimer(hwnd, TIMER_ANIM, ANIM_INTERVAL_MS, None);
        } else {
            KillTimer(hwnd, TIMER_ANIM).ok();
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
    if enabled {
        render();
    }
}

pub fn set_kind(kind: CharacterKind) {
    {
        let mut guard = STATE.lock().unwrap();
        let Some(s) = guard.as_mut() else {
            return;
        };
        if s.kind == kind {
            return;
        }
        s.kind = kind;
    }
    render();
}

pub fn set_language(lang: LanguageId) {
    let mut guard = STATE.lock().unwrap();
    if let Some(s) = guard.as_mut() {
        s.lang = lang;
    }
}

pub fn is_enabled() -> bool {
    STATE
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.enabled)
        .unwrap_or(false)
}

pub fn current_kind() -> CharacterKind {
    STATE
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.kind)
        .unwrap_or(CharacterKind::Cat)
}

pub fn reposition(anchor: RECT) {
    let (hwnd, win_w, win_h, enabled) = {
        let guard = STATE.lock().unwrap();
        let Some(s) = guard.as_ref() else {
            return;
        };
        (HWND(s.hwnd as *mut _), s.win_w, s.win_h, s.enabled)
    };
    let x = anchor.left;
    let y = anchor.top - win_h;
    unsafe {
        let flags = SWP_NOACTIVATE | if enabled { SWP_SHOWWINDOW } else { SWP_HIDEWINDOW };
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, win_w, win_h, flags);
    }
}

/// Drive mood, threshold messages, and burn-rate predictions from the latest
/// usage. `session_percent` / `session_resets_at` describe the 5h window used
/// for the pace projection; `max_percent` drives the warning bands.
pub fn on_usage_update(
    max_percent: f64,
    session_percent: f64,
    session_resets_at: Option<SystemTime>,
    lang: LanguageId,
) {
    let mut guard = STATE.lock().unwrap();
    let Some(s) = guard.as_mut() else {
        return;
    };
    s.lang = lang;
    if !s.enabled {
        return;
    }

    let band = band_for(max_percent);
    s.mood = band; // persistent facial expression

    let (pool, do_react) = match (s.last_band, band) {
        (prev, Band::Urgent) if prev != Band::Urgent => (Some(Pool::Urgent), true),
        (Band::Low, Band::Soft) => (Some(Pool::Soft), true),
        (prev, Band::Low) if prev != Band::Low => (Some(Pool::Rest), false),
        (Band::Low, Band::Low) => {
            if s.frame.saturating_sub(s.last_idle_frame) >= IDLE_COOLDOWN && rng_next() % 4 == 0 {
                s.last_idle_frame = s.frame;
                (Some(Pool::Encourage), false)
            } else {
                (None, false)
            }
        }
        _ => (None, false),
    };
    s.last_band = band;

    if let Some(pool) = pool {
        let lang = s.lang;
        let kind = s.kind;
        if kind.shows_cat() {
            let text = pick_message(&mut s.cat, lang, pool);
            set_bubble(&mut s.cat, text, THRESHOLD_BUBBLE_TICKS, PRI_THRESHOLD);
            if do_react {
                s.cat.react_ticks = 12;
            }
        }
        if kind.shows_dog() {
            let text = pick_message(&mut s.dog, lang, pool);
            set_bubble(&mut s.dog, text, THRESHOLD_BUBBLE_TICKS, PRI_THRESHOLD);
            if do_react {
                s.dog.react_ticks = 12;
            }
        }
        return; // don't also fire a prediction this update
    }

    // Burn-rate prediction: if at the current pace we'll hit 100% before the
    // 5h window resets, occasionally warn with the projected clock time.
    if session_percent >= 25.0 && session_percent < 95.0 {
        if let Some(reset) = session_resets_at {
            let now = SystemTime::now();
            if let (Some(secs_to_full), Ok(to_reset)) =
                (secs_until_full(session_percent, reset), reset.duration_since(now))
            {
                let secs_to_reset = to_reset.as_secs() as f64;
                if secs_to_full < secs_to_reset
                    && s.frame.saturating_sub(s.last_pred_frame) >= PRED_COOLDOWN
                {
                    s.last_pred_frame = s.frame;
                    let hhmm = local_hhmm_after(secs_to_full as u64);
                    let lang = s.lang;
                    let kind = s.kind;
                    if kind.shows_cat() {
                        let text = prediction_message(lang, true, &hhmm);
                        set_bubble(&mut s.cat, text, THRESHOLD_BUBBLE_TICKS, PRI_CLICK);
                    }
                    if kind.shows_dog() {
                        let text = prediction_message(lang, false, &hhmm);
                        set_bubble(&mut s.dog, text, THRESHOLD_BUBBLE_TICKS, PRI_CLICK);
                    }
                }
            }
        }
    }
}

const WINDOW_5H_SECS: f64 = 5.0 * 3600.0;
const PRED_COOLDOWN: u64 = 1800; // ~2.4 min at 80ms/tick

/// Seconds until the 5h window would reach 100% at the current consumption
/// pace, or None if it can't be estimated yet.
fn secs_until_full(session_percent: f64, reset: SystemTime) -> Option<f64> {
    let now = SystemTime::now();
    let to_reset = reset.duration_since(now).ok()?.as_secs() as f64;
    let elapsed = WINDOW_5H_SECS - to_reset;
    if elapsed < 60.0 || session_percent <= 0.0 {
        return None;
    }
    let rate = session_percent / elapsed; // percent per second
    if rate <= 0.0 {
        return None;
    }
    Some((100.0 - session_percent) / rate)
}

/// Local wall-clock "HH:MM" `secs` from now (wraps within a day).
fn local_hhmm_after(secs: u64) -> String {
    let st = unsafe { GetLocalTime() };
    let sod = st.wHour as u64 * 3600 + st.wMinute as u64 * 60 + st.wSecond as u64;
    let proj = (sod + secs) % 86_400;
    format!("{:02}:{:02}", proj / 3600, (proj % 3600) / 60)
}

fn prediction_message(lang: LanguageId, is_cat: bool, hhmm: &str) -> String {
    match (lang, is_cat) {
        (LanguageId::Japanese, true) => format!("このペースだと{hhmm}に上限かも…"),
        (LanguageId::Japanese, false) => format!("このままだと{hhmm}で限界だワン！"),
        (_, true) => format!("at this pace... done by {hhmm}."),
        (_, false) => format!("uh oh, 100% by {hhmm}!!"),
    }
}

pub fn destroy() {
    let hwnd = {
        let mut guard = STATE.lock().unwrap();
        match guard.take() {
            Some(s) => HWND(s.hwnd as *mut _),
            None => return,
        }
    };
    unsafe {
        KillTimer(hwnd, TIMER_ANIM).ok();
        let _ = DestroyWindow(hwnd);
    }
}

// ----- window proc & input ---------------------------------------------------

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER => {
            if wparam.0 == TIMER_ANIM {
                tick();
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            on_mouse_move(hwnd, x, y);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            on_mouse_leave();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            on_click(x, y);
            LRESULT(0)
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_ANIM).ok();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Character bounding box in client device pixels.
fn critter_bounds(c: &Critter, uscale: i32) -> (i32, i32, i32, i32) {
    let left = (c.x * uscale as f32) as i32;
    let top = BASE_OY * uscale;
    (left, top, SPRITE * uscale, SPRITE * uscale)
}

fn point_in(c: &Critter, uscale: i32, x: i32, y: i32) -> bool {
    let (l, t, w, h) = critter_bounds(c, uscale);
    x >= l && x < l + w && y >= t && y < t + h
}

fn on_mouse_move(hwnd: HWND, x: i32, y: i32) {
    let mut guard = STATE.lock().unwrap();
    let Some(s) = guard.as_mut() else {
        return;
    };
    if !s.tracking_leave {
        unsafe {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
        }
        s.tracking_leave = true;
    }

    let uscale = s.uscale;
    let frame = s.frame;
    let kind = s.kind;
    let mut changed = false;
    if kind.shows_cat() {
        let over = point_in(&s.cat, uscale, x, y);
        changed |= update_hover(&mut s.cat, over, frame);
    }
    if kind.shows_dog() {
        let over = point_in(&s.dog, uscale, x, y);
        changed |= update_hover(&mut s.dog, over, frame);
    }
    if changed {
        drop(guard);
        render();
    }
}

fn update_hover(c: &mut Critter, over: bool, frame: u64) -> bool {
    if over && !c.hovering {
        c.hovering = true;
        c.hover_since = Some(frame);
        true
    } else if !over && c.hovering {
        c.hovering = false;
        c.hover_since = None;
        true
    } else {
        false
    }
}

fn on_mouse_leave() {
    let mut guard = STATE.lock().unwrap();
    let Some(s) = guard.as_mut() else {
        return;
    };
    s.tracking_leave = false;
    let mut changed = false;
    if s.cat.hovering {
        s.cat.hovering = false;
        s.cat.hover_since = None;
        changed = true;
    }
    if s.dog.hovering {
        s.dog.hovering = false;
        s.dog.hover_since = None;
        changed = true;
    }
    if changed {
        drop(guard);
        render();
    }
}

fn on_click(x: i32, y: i32) {
    let mut guard = STATE.lock().unwrap();
    let Some(s) = guard.as_mut() else {
        return;
    };
    let uscale = s.uscale;
    let kind = s.kind;
    let lang = s.lang;
    let mut hit = false;
    if kind.shows_cat() && point_in(&s.cat, uscale, x, y) {
        s.cat.clicked_ticks = CLICKED_POSE_TICKS;
        let text = pick_message(&mut s.cat, lang, Pool::Click);
        set_bubble(&mut s.cat, text, CLICK_BUBBLE_TICKS, PRI_CLICK);
        hit = true;
    }
    if kind.shows_dog() && point_in(&s.dog, uscale, x, y) {
        s.dog.clicked_ticks = CLICKED_POSE_TICKS;
        let text = pick_message(&mut s.dog, lang, Pool::Click);
        set_bubble(&mut s.dog, text, CLICK_BUBBLE_TICKS, PRI_CLICK);
        hit = true;
    }
    if hit {
        drop(guard);
        render();
    }
}

fn tick() {
    {
        let mut guard = STATE.lock().unwrap();
        let Some(s) = guard.as_mut() else {
            return;
        };
        if !s.enabled {
            return;
        }
        s.frame = s.frame.wrapping_add(1);
        let frame = s.frame;
        let lang = s.lang;
        let max_x = (WIN_LW - SPRITE) as f32;

        let kind = s.kind;
        if kind.shows_cat() {
            step_critter(&mut s.cat, frame, lang, max_x, 0.55);
        }
        if kind.shows_dog() {
            step_critter(&mut s.dog, frame, lang, max_x, 0.7);
        }
    }
    render();
}

fn step_critter(c: &mut Critter, frame: u64, lang: LanguageId, max_x: f32, speed: f32) {
    // Transient pose timers.
    if c.clicked_ticks > 0 {
        c.clicked_ticks -= 1;
    }
    if c.react_ticks > 0 {
        c.react_ticks -= 1;
    }

    // Hover bubble after the dwell delay.
    if c.hovering {
        if let Some(since) = c.hover_since {
            if frame.saturating_sub(since) == HOVER_DELAY_TICKS {
                let text = pick_message(c, lang, Pool::Hover);
                set_bubble(c, text, HOVER_BUBBLE_TICKS, PRI_HOVER);
            }
        }
    }

    // Idle <-> walk cycling (don't move while hovering or clicked).
    let frozen = c.hovering || c.clicked_ticks > 0;
    if !frozen {
        if c.move_ticks == 0 {
            c.moving = !c.moving;
            c.move_ticks = if c.moving {
                40 + (rng_next() % 60) as u32
            } else {
                20 + (rng_next() % 30) as u32
            };
            if c.moving && rng_next() % 2 == 0 {
                c.dir = -c.dir;
            }
        }
        c.move_ticks -= 1;
        if c.moving {
            c.x += c.dir * speed;
            if c.x <= 0.0 {
                c.x = 0.0;
                c.dir = 1.0;
            } else if c.x >= max_x {
                c.x = max_x;
                c.dir = -1.0;
            }
        }
    }

    // Blink counter (used by idle pose).
    if c.blink_ctr == 0 {
        c.blink_ctr = 30 + (rng_next() % 40) as u32;
    }
    c.blink_ctr -= 1;

    c.pose = c.pose_now();

    // Occasional idle chatter when standing still and quiet.
    if c.pose == Pose::Idle && c.bubble.is_none() && rng_next() % 220 == 0 {
        let text = pick_message(c, lang, Pool::Idle);
        set_bubble(c, text, IDLE_BUBBLE_TICKS, PRI_IDLE);
    }

    if let Some(b) = c.bubble.as_mut() {
        if b.ttl > 0 {
            b.ttl -= 1;
        }
        if b.ttl == 0 {
            c.bubble = None;
        }
    }
}

// ----- rendering -------------------------------------------------------------

#[inline]
fn bgra(r: u8, g: u8, b: u8) -> u32 {
    0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

fn fill_block(bits: &mut [u32], w: i32, h: i32, x: i32, y: i32, bw: i32, bh: i32, color: u32) {
    for yy in y..(y + bh) {
        if yy < 0 || yy >= h {
            continue;
        }
        let row = yy * w;
        for xx in x..(x + bw) {
            if xx < 0 || xx >= w {
                continue;
            }
            bits[(row + xx) as usize] = color;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Body,
    Dark,
    Light,
    Eye,
    EyeHi,
    Nose,
    Mouth,
    Ear,
    Collar,
    Buckle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    None,
    Ear,
    LegFront,
    LegBack,
    Eyes,
    Tail,
}

fn palette(is_cat: bool, variant: u8, part: Part) -> u32 {
    // Shared feature colors.
    match part {
        Part::Eye => return bgra(40, 38, 45),
        Part::EyeHi => return bgra(255, 255, 255),
        Part::Mouth => return bgra(70, 55, 55),
        Part::Collar => return bgra(210, 70, 70),
        Part::Buckle => return bgra(240, 210, 90),
        _ => {}
    }
    if is_cat {
        // variant 0: orange tabby, variant 1: grey
        let (body, dark, light, ear, nose) = if variant == 0 {
            (
                bgra(235, 160, 80),
                bgra(200, 120, 50),
                bgra(250, 205, 140),
                bgra(240, 175, 185),
                bgra(220, 120, 130),
            )
        } else {
            (
                bgra(150, 154, 160),
                bgra(110, 114, 122),
                bgra(205, 208, 214),
                bgra(235, 180, 188),
                bgra(210, 130, 140),
            )
        };
        match part {
            Part::Body => body,
            Part::Dark => dark,
            Part::Light => light,
            Part::Ear => ear,
            Part::Nose => nose,
            _ => body,
        }
    } else {
        // variant 0: brown, variant 1: black
        let (body, dark, light, ear, nose) = if variant == 0 {
            (
                bgra(184, 134, 84),
                bgra(140, 95, 55),
                bgra(222, 188, 142),
                bgra(150, 105, 65),
                bgra(60, 45, 42),
            )
        } else {
            (
                bgra(92, 92, 100),
                bgra(58, 58, 66),
                bgra(140, 140, 150),
                bgra(70, 70, 78),
                bgra(40, 38, 42),
            )
        };
        match part {
            Part::Body => body,
            Part::Dark => dark,
            Part::Light => light,
            Part::Ear => ear,
            Part::Nose => nose,
            _ => body,
        }
    }
}

/// Block layout (lx, ly, lw, lh, part, tag) for a character facing right on the
/// 32x32 grid.
fn character_blocks(is_cat: bool) -> Vec<(i32, i32, i32, i32, Part, Tag)> {
    let mut v: Vec<(i32, i32, i32, i32, Part, Tag)> = Vec::new();
    if is_cat {
        // tail
        v.push((2, 14, 3, 2, Part::Body, Tag::Tail));
        v.push((1, 10, 2, 5, Part::Body, Tag::Tail));
        v.push((1, 9, 2, 2, Part::Dark, Tag::Tail));
        // ears
        v.push((6, 2, 5, 5, Part::Body, Tag::Ear));
        v.push((21, 2, 5, 5, Part::Body, Tag::Ear));
        v.push((8, 3, 2, 2, Part::Ear, Tag::Ear));
        v.push((22, 3, 2, 2, Part::Ear, Tag::Ear));
        // head
        v.push((6, 5, 20, 14, Part::Body, Tag::None));
        v.push((7, 6, 7, 3, Part::Light, Tag::None)); // top-left highlight
        v.push((8, 16, 16, 2, Part::Dark, Tag::None)); // chin shade
        v.push((11, 13, 10, 5, Part::Light, Tag::None)); // muzzle
        // eyes
        v.push((11, 9, 3, 4, Part::Eye, Tag::Eyes));
        v.push((19, 9, 3, 4, Part::Eye, Tag::Eyes));
        v.push((12, 9, 1, 1, Part::EyeHi, Tag::Eyes));
        v.push((20, 9, 1, 1, Part::EyeHi, Tag::Eyes));
        // nose + mouth
        v.push((15, 13, 2, 2, Part::Nose, Tag::None));
        v.push((14, 15, 1, 1, Part::Mouth, Tag::None));
        v.push((17, 15, 1, 1, Part::Mouth, Tag::None));
        // body
        v.push((9, 19, 14, 9, Part::Body, Tag::None));
        v.push((12, 22, 8, 5, Part::Light, Tag::None)); // belly
        // collar
        v.push((9, 19, 14, 2, Part::Collar, Tag::None));
        v.push((15, 19, 2, 2, Part::Buckle, Tag::None));
        // legs
        v.push((10, 28, 4, 3, Part::Body, Tag::LegBack));
        v.push((18, 28, 4, 3, Part::Body, Tag::LegFront));
    } else {
        // tail
        v.push((26, 16, 3, 2, Part::Body, Tag::Tail));
        v.push((28, 12, 2, 5, Part::Body, Tag::Tail));
        // floppy ears
        v.push((4, 5, 5, 10, Part::Ear, Tag::Ear));
        v.push((23, 5, 5, 10, Part::Ear, Tag::Ear));
        // head
        v.push((7, 4, 18, 15, Part::Body, Tag::None));
        v.push((8, 5, 7, 3, Part::Light, Tag::None));
        v.push((11, 13, 11, 6, Part::Light, Tag::None)); // snout
        v.push((9, 16, 14, 2, Part::Dark, Tag::None));
        // eyes
        v.push((12, 8, 3, 4, Part::Eye, Tag::Eyes));
        v.push((18, 8, 3, 4, Part::Eye, Tag::Eyes));
        v.push((13, 8, 1, 1, Part::EyeHi, Tag::Eyes));
        v.push((19, 8, 1, 1, Part::EyeHi, Tag::Eyes));
        // nose + mouth
        v.push((15, 13, 3, 3, Part::Nose, Tag::None));
        v.push((13, 18, 6, 1, Part::Mouth, Tag::None));
        // body
        v.push((9, 19, 14, 9, Part::Body, Tag::None));
        v.push((12, 22, 8, 5, Part::Light, Tag::None));
        // collar
        v.push((9, 19, 14, 2, Part::Collar, Tag::None));
        v.push((15, 19, 2, 2, Part::Buckle, Tag::None));
        // legs
        v.push((10, 28, 4, 3, Part::Body, Tag::LegBack));
        v.push((18, 28, 4, 3, Part::Body, Tag::LegFront));
    }
    v
}

struct Anim {
    body_dy: i32,
    ear_dy: i32,
    leg_front_dx: i32,
    leg_back_dx: i32,
    tail_dy: i32,
    blink: bool,
    squash: i32,
}

fn anim_for(pose: Pose, frame: u64, blink_ctr: u32) -> Anim {
    let mut a = Anim {
        body_dy: 0,
        ear_dy: 0,
        leg_front_dx: 0,
        leg_back_dx: 0,
        tail_dy: 0,
        blink: false,
        squash: 0,
    };
    match pose {
        Pose::Idle => {
            // gentle breathing bob + occasional blink
            a.body_dy = if (frame / 6) % 2 == 0 { 0 } else { 1 };
            a.blink = blink_ctr < 2;
            a.tail_dy = if (frame / 8) % 2 == 0 { 0 } else { -1 };
        }
        Pose::Walk => {
            // 4-frame leg cycle + bob
            let phase = (frame / 2) % 4;
            let (f, b) = match phase {
                0 => (2, -2),
                1 => (0, 0),
                2 => (-2, 2),
                _ => (0, 0),
            };
            a.leg_front_dx = f;
            a.leg_back_dx = b;
            a.body_dy = if phase % 2 == 0 { 0 } else { 1 };
            a.tail_dy = if (frame / 3) % 2 == 0 { -1 } else { 0 };
        }
        Pose::React => {
            a.ear_dy = -2;
            a.tail_dy = if (frame / 2) % 2 == 0 { -3 } else { -1 }; // wag
            a.body_dy = if (frame / 3) % 2 == 0 { 0 } else { 1 };
        }
        Pose::Hover => {
            a.ear_dy = -1;
            a.body_dy = if (frame / 4) % 2 == 0 { 0 } else { 1 };
            a.tail_dy = if (frame / 2) % 2 == 0 { -2 } else { 0 };
        }
        Pose::Clicked => {
            // quick jump + squash
            a.body_dy = -4;
            a.ear_dy = -2;
            a.squash = 1;
            a.tail_dy = -3;
        }
    }
    a
}

#[allow(clippy::too_many_arguments)]
fn draw_character(
    bits: &mut [u32],
    w: i32,
    h: i32,
    is_cat: bool,
    variant: u8,
    ox: i32,
    oy: i32,
    u: i32,
    mirror: bool,
    anim: &Anim,
    mood: Band,
) {
    for (lx, ly, lw, lh, part, tag) in character_blocks(is_cat) {
        if anim.blink && tag == Tag::Eyes {
            continue; // eyes closed; eyelid drawn below
        }
        let mut x = lx;
        let mut y = ly;
        match tag {
            Tag::Ear => y += anim.ear_dy,
            Tag::LegFront => x += anim.leg_front_dx,
            Tag::LegBack => x += anim.leg_back_dx,
            Tag::Tail => y += anim.tail_dy,
            _ => {}
        }
        // global body bob (legs stay planted)
        if !matches!(tag, Tag::LegFront | Tag::LegBack) {
            y += anim.body_dy;
        }
        let ax = if mirror { SPRITE - x - lw } else { x };
        let color = palette(is_cat, variant, part);
        fill_block(bits, w, h, ox + ax * u, oy + y * u, lw * u, lh * u, color);
    }

    if anim.blink {
        // closed-eye line
        let eye_color = palette(is_cat, variant, Part::Dark);
        let (e1, e2, ey) = if is_cat { (11, 19, 11) } else { (12, 18, 10) };
        for ex in [e1, e2] {
            let ax = if mirror { SPRITE - ex - 3 } else { ex };
            fill_block(
                bits,
                w,
                h,
                ox + ax * u,
                oy + (ey + anim.body_dy) * u,
                3 * u,
                u,
                eye_color,
            );
        }
    }

    let _ = anim.squash; // reserved for future squash/stretch tuning

    // Mood overlays on top of the face.
    let dy = anim.body_dy;
    match mood {
        Band::Soft => {
            // worried sweat drop near the top of the head
            let drop = bgra(150, 205, 240);
            let dx = if is_cat { 24 } else { 23 };
            let ax = if mirror { SPRITE - dx - 1 } else { dx };
            fill_block(bits, w, h, ox + ax * u, oy + (6 + dy) * u, u, 2 * u, drop);
        }
        Band::Urgent => {
            // open mouth + raised "shock" brows
            let mouth = palette(is_cat, variant, Part::Mouth);
            let white = bgra(255, 255, 255);
            if is_cat {
                fill_block(bits, w, h, ox + 14 * u, oy + (15 + dy) * u, 4 * u, 3 * u, mouth);
                for ex in [11, 19] {
                    let ax = if mirror { SPRITE - ex - 3 } else { ex };
                    fill_block(bits, w, h, ox + ax * u, oy + (7 + dy) * u, 3 * u, u, white);
                }
            } else {
                fill_block(bits, w, h, ox + 14 * u, oy + (17 + dy) * u, 5 * u, 2 * u, mouth);
                for ex in [12, 18] {
                    let ax = if mirror { SPRITE - ex - 3 } else { ex };
                    fill_block(bits, w, h, ox + ax * u, oy + (6 + dy) * u, 3 * u, u, white);
                }
            }
        }
        Band::Low => {}
    }
}

fn render() {
    let (
        hwnd,
        win_w,
        win_h,
        u,
        kind,
        frame,
        cat_snapshot,
        dog_snapshot,
    ) = {
        let guard = STATE.lock().unwrap();
        let Some(s) = guard.as_ref() else {
            return;
        };
        if !s.enabled {
            return;
        }
        (
            HWND(s.hwnd as *mut _),
            s.win_w,
            s.win_h,
            s.uscale,
            s.kind,
            s.frame,
            snapshot(&s.cat, s.mood),
            snapshot(&s.dog, s.mood),
        )
    };

    unsafe {
        let screen_dc = GetDC(hwnd);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: win_w,
                biHeight: -win_h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0)
            .unwrap_or_default();
        if dib.is_invalid() || bits_ptr.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(hwnd, screen_dc);
            return;
        }
        let old_bmp = SelectObject(mem_dc, dib);

        let pixel_count = (win_w * win_h) as usize;
        let bits = std::slice::from_raw_parts_mut(bits_ptr as *mut u32, pixel_count);
        for px in bits.iter_mut() {
            *px = 0;
        }

        let base_oy = BASE_OY * u;
        // Panic mood adds a small horizontal shake.
        let shake = |mood: Band| -> i32 {
            if mood == Band::Urgent {
                if frame % 2 == 0 {
                    u
                } else {
                    -u
                }
            } else {
                0
            }
        };
        if kind.shows_cat() {
            let ox = (cat_snapshot.x * u as f32) as i32 + shake(cat_snapshot.mood);
            let anim = anim_for(cat_snapshot.pose, frame, cat_snapshot.blink_ctr);
            draw_character(
                bits,
                win_w,
                win_h,
                true,
                cat_snapshot.variant,
                ox,
                base_oy,
                u,
                cat_snapshot.dir < 0.0,
                &anim,
                cat_snapshot.mood,
            );
        }
        if kind.shows_dog() {
            let ox = (dog_snapshot.x * u as f32) as i32 + shake(dog_snapshot.mood);
            let anim = anim_for(dog_snapshot.pose, frame, dog_snapshot.blink_ctr);
            draw_character(
                bits,
                win_w,
                win_h,
                false,
                dog_snapshot.variant,
                ox,
                base_oy,
                u,
                dog_snapshot.dir < 0.0,
                &anim,
                dog_snapshot.mood,
            );
        }

        // Bubbles (drawn after characters so they sit on top). When both are
        // visible, the dog bubble is nudged up so they don't overlap.
        let mut bubble_rows_used: Vec<(i32, i32)> = Vec::new();
        if kind.shows_cat() {
            if let Some(b) = &cat_snapshot.bubble {
                draw_bubble(
                    mem_dc,
                    bits,
                    win_w,
                    win_h,
                    u,
                    cat_snapshot.x,
                    &b.text,
                    b.ttl,
                    b.max_ttl,
                    &mut bubble_rows_used,
                );
            }
        }
        if kind.shows_dog() {
            if let Some(b) = &dog_snapshot.bubble {
                draw_bubble(
                    mem_dc,
                    bits,
                    win_w,
                    win_h,
                    u,
                    dog_snapshot.x,
                    &b.text,
                    b.ttl,
                    b.max_ttl,
                    &mut bubble_rows_used,
                );
            }
        }

        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: win_w,
            cy: win_h,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

struct Snapshot {
    x: f32,
    dir: f32,
    pose: Pose,
    variant: u8,
    blink_ctr: u32,
    mood: Band,
    bubble: Option<BubbleSnap>,
}

struct BubbleSnap {
    text: String,
    ttl: u32,
    max_ttl: u32,
}

fn snapshot(c: &Critter, mood: Band) -> Snapshot {
    Snapshot {
        x: c.x,
        dir: c.dir,
        pose: c.pose,
        variant: c.variant,
        blink_ctr: c.blink_ctr,
        mood,
        bubble: c.bubble.as_ref().map(|b| BubbleSnap {
            text: b.text.clone(),
            ttl: b.ttl,
            max_ttl: b.max_ttl,
        }),
    }
}

/// Premultiply RGB by alpha for the ULW_ALPHA blend during fade-out.
#[inline]
fn premul(color: u32, alpha: u32) -> u32 {
    let r = ((color >> 16) & 0xFF) * alpha / 255;
    let g = ((color >> 8) & 0xFF) * alpha / 255;
    let b = (color & 0xFF) * alpha / 255;
    (alpha << 24) | (r << 16) | (g << 8) | b
}

#[allow(clippy::too_many_arguments)]
fn draw_bubble(
    mem_dc: HDC,
    bits: &mut [u32],
    w: i32,
    h: i32,
    u: i32,
    char_x_logical: f32,
    text: &str,
    ttl: u32,
    max_ttl: u32,
    used: &mut Vec<(i32, i32)>,
) {
    let bubble_h = 18 * u;
    let bubble_w = 84 * u;
    // Center above the character, clamped to the window.
    let cx = (char_x_logical as i32 + SPRITE / 2) * u;
    let mut bx = (cx - bubble_w / 2).clamp(0, (w - bubble_w).max(0));
    let mut by = 1 * u;
    // Avoid overlapping an existing bubble by shifting horizontally if needed.
    for (ux0, ux1) in used.iter() {
        if bx < *ux1 && bx + bubble_w > *ux0 {
            bx = (*ux1 + 2 * u).min((w - bubble_w).max(0));
        }
    }
    let _ = &mut by;
    used.push((bx, bx + bubble_w));

    let bg = bgra(252, 252, 250);
    let border = bgra(120, 120, 128);
    fill_block(bits, w, h, bx, by, bubble_w, bubble_h, border);
    fill_block(bits, w, h, bx + u, by + u, bubble_w - 2 * u, bubble_h - 2 * u, bg);
    // tail pointing down toward the character
    let tail_x = (cx - u).clamp(bx + 2 * u, bx + bubble_w - 3 * u);
    fill_block(bits, w, h, tail_x, by + bubble_h, 2 * u, 2 * u, bg);

    unsafe {
        let font_name = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            -(11 * u),
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            ANTIALIASED_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(mem_dc, font);
        let _ = SetBkMode(mem_dc, TRANSPARENT);
        let _ = SetTextColor(mem_dc, COLORREF(native_interop::colorref(30, 30, 36)));
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut rect = RECT {
            left: bx + 2 * u,
            top: by + u,
            right: bx + bubble_w - 2 * u,
            bottom: by + bubble_h - u,
        };
        let _ = DrawTextW(
            mem_dc,
            &mut text_wide,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_WORD_ELLIPSIS,
        );
        SelectObject(mem_dc, old_font);
        let _ = DeleteObject(font);
    }

    // Determine fade alpha for the last few ticks.
    let alpha: u32 = if ttl < FADE_TICKS && max_ttl > FADE_TICKS {
        (64 + (ttl * 191 / FADE_TICKS)).min(255)
    } else {
        255
    };

    // Force the bubble region's alpha (GDI text leaves the alpha byte at 0).
    // During fade we also premultiply so partial alpha composites correctly.
    let y0 = by;
    let y1 = (by + bubble_h + 2 * u).min(h);
    let x0 = bx.max(0);
    let x1 = (bx + bubble_w).min(w);
    for yy in y0..y1 {
        if yy < 0 {
            continue;
        }
        for xx in x0..x1 {
            let idx = (yy * w + xx) as usize;
            let px = bits[idx];
            if alpha >= 255 {
                bits[idx] = px | 0xFF00_0000;
            } else if px & 0x00FF_FFFF != 0 || (yy < by + bubble_h && xx < bx + bubble_w) {
                bits[idx] = premul(px & 0x00FF_FFFF, alpha);
            }
        }
    }
}

// ----- localized message pools ----------------------------------------------
// Cat = aloof / sarcastic / occasionally cute. Dog = eager / energetic.
// Japanese and English are provided; other languages fall back to English,
// consistent with keeping all strings embedded in the binary.

fn message_pool(lang: LanguageId, is_cat: bool, pool: Pool) -> &'static [&'static str] {
    match lang {
        LanguageId::Japanese => ja_pool(is_cat, pool),
        _ => en_pool(is_cat, pool),
    }
}

fn ja_pool(is_cat: bool, pool: Pool) -> &'static [&'static str] {
    if is_cat {
        match pool {
            Pool::Hover => &["なに？", "…べつに嬉しくないし", "みてるの？", "ひま？", "なでる気？"],
            Pool::Click => &["やめてよね", "…ふん", "もう一回？しょうがないにゃ", "なに用？", "ま、いいけど"],
            Pool::Idle => &["zzz…", "ひまだにゃ", "…", "のびーっ"],
            Pool::Soft => &["そろそろ気をつけたら？", "8割こえたにゃ", "ちょっと多いんじゃない"],
            Pool::Urgent => &["やばいにゃ！", "もう限界だってば！", "9割こえた、知らないよ"],
            Pool::Rest => &["…おつかれ", "ま、がんばったんじゃない", "ひと休みしたら？"],
            Pool::Encourage => &["いい調子じゃない", "まだ余裕でしょ", "ふん、悪くないね"],
        }
    } else {
        match pool {
            Pool::Hover => &["なになに！？", "あそぶ！？", "みて！みて！", "わくわく！", "こっちこっち！"],
            Pool::Click => &["やったー！！", "もっとなでて〜！！", "わーい🐾", "うれしー！", "もう一回！もう一回！"],
            Pool::Idle => &["たいくつだワン", "あそぼー！", "そわそわ…", "おさんぽ行きたい！"],
            Pool::Soft => &["そろそろ気をつけて！", "8割こえたよ！", "ちょっと多いかも！"],
            Pool::Urgent => &["たいへんだワン！", "もうすぐ限界だよー！", "9割！きをつけて！"],
            Pool::Rest => &["きょうもがんばったね！", "おつかれさま！", "えらいぞ！"],
            Pool::Encourage => &["いい調子だワン！", "その調子その調子！", "がんばってるね！"],
        }
    }
}

fn en_pool(is_cat: bool, pool: Pool) -> &'static [&'static str] {
    if is_cat {
        match pool {
            Pool::Hover => &[
                "...what do you want.",
                "oh, it's you.",
                "are you staring?",
                "yeah?",
                "...fine, look.",
            ],
            Pool::Click => &[
                "must you.",
                "...hmph.",
                "again? whatever.",
                "do you mind?",
                "fine, I guess.",
            ],
            Pool::Idle => &["zzz...", "so bored.", "...", "*stretch*"],
            Pool::Soft => &[
                "maybe ease up?",
                "past 80%, you know.",
                "that's a lot...",
            ],
            Pool::Urgent => &[
                "this is bad.",
                "you're nearly maxed.",
                "over 90%. not my problem.",
            ],
            Pool::Rest => &["...good job.", "not bad, I suppose.", "go rest already."],
            Pool::Encourage => &["doing fine.", "plenty left.", "hmph, not bad."],
        }
    } else {
        match pool {
            Pool::Hover => &[
                "HI HI HI!! 🐾",
                "play?? play??",
                "look at me!!",
                "oh boy oh boy!",
                "over here!!",
            ],
            Pool::Click => &[
                "YESYESYES!!",
                "again again!! 🐾",
                "best day ever!!",
                "pet me more!!",
                "wheee!!",
            ],
            Pool::Idle => &["so bored woof", "let's play!", "*wags tail*", "walkies??"],
            Pool::Soft => &["careful now!", "past 80%!", "that's kinda lots!"],
            Pool::Urgent => &["uh oh!!", "almost maxed!!", "over 90%! careful!!"],
            Pool::Rest => &["great job today!!", "you did it!!", "so proud!!"],
            Pool::Encourage => &["doing great!!", "keep going!!", "you got this!!"],
        }
    }
}
