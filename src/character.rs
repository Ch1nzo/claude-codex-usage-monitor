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
const KISS_TICKS: u32 = 26; // ~2.1s of kiss face + rising hearts (girl only)
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
    Girl,
    Both,
}

impl CharacterKind {
    pub fn code(self) -> &'static str {
        match self {
            CharacterKind::Cat => "cat",
            CharacterKind::Dog => "dog",
            CharacterKind::Girl => "girl",
            CharacterKind::Both => "both",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "cat" => Some(CharacterKind::Cat),
            "dog" => Some(CharacterKind::Dog),
            "girl" => Some(CharacterKind::Girl),
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

    fn shows_girl(self) -> bool {
        matches!(self, CharacterKind::Girl)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Species {
    Cat,
    Dog,
    Girl,
}

struct Critter {
    species: Species,
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
    // Kiss reaction (girl only): countdown ticks for the puckered face + the
    // floating hearts. 0 = not kissing.
    kiss: u32,
    kiss_max: u32,
}

impl Critter {
    fn new(species: Species, variant: u8, x: f32, dir: f32) -> Self {
        Self {
            species,
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
            kiss: 0,
            kiss_max: 0,
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
    girl: Critter,
    usage: f64,
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
    let msgs = message_pool(lang, critter.species, pool);
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
pub fn init(
    anchor: RECT,
    enabled: bool,
    kind: CharacterKind,
    cat_variant: u8,
    dog_variant: u8,
    lang: LanguageId,
) {
    {
        let guard = STATE.lock().unwrap();
        if guard.is_some() {
            drop(guard);
            set_kind(kind);
            set_variant(true, cat_variant);
            set_variant(false, dog_variant);
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
        cat: Critter::new(Species::Cat, cat_variant & 1, 8.0, 1.0),
        dog: Critter::new(Species::Dog, dog_variant & 1, max_x - 8.0, -1.0),
        girl: Critter::new(Species::Girl, 0, 8.0, 1.0),
        usage: 0.0,
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

pub fn set_variant(is_cat: bool, variant: u8) {
    {
        let mut guard = STATE.lock().unwrap();
        let Some(s) = guard.as_mut() else {
            return;
        };
        let v = variant & 1;
        if is_cat {
            if s.cat.variant == v {
                return;
            }
            s.cat.variant = v;
        } else {
            if s.dog.variant == v {
                return;
            }
            s.dog.variant = v;
        }
    }
    render();
}

pub fn cat_variant() -> u8 {
    STATE
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.cat.variant)
        .unwrap_or(0)
}

pub fn dog_variant() -> u8 {
    STATE
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.dog.variant)
        .unwrap_or(0)
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
    s.usage = max_percent; // drives the girl character's clothing stage
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
        if kind.shows_girl() {
            let text = pick_message(&mut s.girl, lang, pool);
            set_bubble(&mut s.girl, text, THRESHOLD_BUBBLE_TICKS, PRI_THRESHOLD);
            if do_react {
                s.girl.react_ticks = 12;
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
                    if kind.shows_girl() {
                        let text = girl_prediction(lang, &hhmm);
                        set_bubble(&mut s.girl, text, THRESHOLD_BUBBLE_TICKS, PRI_CLICK);
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
        (LanguageId::German, true) => format!("in dem Tempo... um {hhmm} vorbei."),
        (LanguageId::German, false) => format!("oje, 100% um {hhmm}!!"),
        (LanguageId::Dutch, true) => format!("in dit tempo... om {hhmm} op."),
        (LanguageId::Dutch, false) => format!("oei, 100% om {hhmm}!!"),
        (LanguageId::Spanish, true) => format!("a este ritmo... listo a las {hhmm}."),
        (LanguageId::Spanish, false) => format!("¡¡uy, 100% a las {hhmm}!!"),
        (LanguageId::French, true) => format!("à ce rythme... fini à {hhmm}."),
        (LanguageId::French, false) => format!("oh là, 100% à {hhmm} !!"),
        (LanguageId::Korean, true) => format!("이 속도면... {hhmm}에 끝."),
        (LanguageId::Korean, false) => format!("이런, {hhmm}에 100%!!"),
        (LanguageId::TraditionalChinese, true) => format!("照這速度…{hhmm}就到頂了。"),
        (LanguageId::TraditionalChinese, false) => format!("糟糕，{hhmm}就100%！！"),
        (LanguageId::English, true) => format!("at this pace... done by {hhmm}."),
        (LanguageId::English, false) => format!("uh oh, 100% by {hhmm}!!"),
    }
}

fn girl_prediction(lang: LanguageId, hhmm: &str) -> String {
    match lang {
        LanguageId::Japanese => format!("このペースだと{hhmm}には危ないかも…"),
        LanguageId::German => format!("in dem Tempo... {hhmm} wird knapp!"),
        LanguageId::Dutch => format!("in dit tempo... {hhmm} wordt spannend!"),
        LanguageId::Spanish => format!("a este ritmo... ¡{hhmm} pinta mal!"),
        LanguageId::French => format!("à ce rythme... {hhmm} ça craint !"),
        LanguageId::Korean => format!("이 속도면... {hhmm}쯤 위험해!"),
        LanguageId::TraditionalChinese => format!("照這速度…{hhmm}就危險了！"),
        LanguageId::English => format!("at this pace... {hhmm} looks risky!"),
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
    if kind.shows_girl() {
        let over = point_in(&s.girl, uscale, x, y);
        changed |= update_hover(&mut s.girl, over, frame);
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
    if s.girl.hovering {
        s.girl.hovering = false;
        s.girl.hover_since = None;
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
    if kind.shows_girl() && point_in(&s.girl, uscale, x, y) {
        // The girl blows a kiss (puckered face + floating hearts) instead of the
        // generic click hop the cat/dog use.
        s.girl.kiss = KISS_TICKS;
        s.girl.kiss_max = KISS_TICKS;
        let text = pick_message(&mut s.girl, lang, Pool::Click);
        set_bubble(&mut s.girl, text, CLICK_BUBBLE_TICKS, PRI_CLICK);
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
        if kind.shows_girl() {
            step_critter(&mut s.girl, frame, lang, max_x, 0.5);
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
    if c.kiss > 0 {
        c.kiss -= 1;
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
    let frozen = c.hovering || c.clicked_ticks > 0 || c.kiss > 0;
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
}

fn anim_for(pose: Pose, frame: u64, blink_ctr: u32) -> Anim {
    let mut a = Anim {
        body_dy: 0,
        ear_dy: 0,
        leg_front_dx: 0,
        leg_back_dx: 0,
        tail_dy: 0,
        blink: false,
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
            // quick jump
            a.body_dy = -4;
            a.ear_dy = -2;
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

/// Clothing stage for the girl character: higher usage sheds more layers, ending
/// in swimwear near 100%. 0 = fully dressed, 3 = bikini only.
fn girl_stage(usage: f64) -> u8 {
    if usage >= 90.0 {
        3
    } else if usage >= 75.0 {
        2
    } else if usage >= 60.0 {
        1
    } else {
        0
    }
}

/// Draw a small pixel heart at grid coords (`hx`,`hy`). `big` selects a slightly
/// larger shape. Mirrored consistently with the girl sprite so hearts sit on her
/// facing side.
#[allow(clippy::too_many_arguments)]
fn draw_heart(
    bits: &mut [u32],
    w: i32,
    h: i32,
    ox: i32,
    oy: i32,
    u: i32,
    mirror: bool,
    hx: i32,
    hy: i32,
    big: bool,
) {
    let pink = bgra(240, 92, 132);
    let pink_hi = bgra(255, 156, 184);
    let mut cell = |gx: i32, gy: i32, color: u32| {
        let ax = if mirror { SPRITE - gx - 1 } else { gx };
        fill_block(bits, w, h, ox + ax * u, oy + gy * u, u, u, color);
    };
    let pat: &[(i32, i32)] = if big {
        &[
            (0, 0), (1, 0), (3, 0), (4, 0),
            (0, 1), (1, 1), (2, 1), (3, 1), (4, 1),
            (1, 2), (2, 2), (3, 2),
            (2, 3),
        ]
    } else {
        &[(0, 0), (2, 0), (0, 1), (1, 1), (2, 1), (1, 2)]
    };
    for (px, py) in pat {
        cell(hx + px, hy + py, pink);
    }
    cell(hx, hy, pink_hi);
}

/// Draw the chibi girl on the 32x32 grid (facing right; `mirror` flips her).
/// As `usage` rises she sheds outer layers down to swimwear (see `girl_stage`).
#[allow(clippy::too_many_arguments)]
fn draw_girl(
    bits: &mut [u32],
    w: i32,
    h: i32,
    ox: i32,
    oy: i32,
    u: i32,
    mirror: bool,
    anim: &Anim,
    mood: Band,
    usage: f64,
    kiss: u32,
    kiss_max: u32,
    frame: u64,
) {
    let dy = anim.body_dy;
    let stage = girl_stage(usage);
    let kissing = kiss > 0;

    let skin = bgra(255, 222, 196);
    let skin_sh = bgra(232, 190, 165);
    let hair = bgra(116, 80, 62);
    let hair_hi = bgra(150, 108, 82);
    let eye = bgra(96, 134, 176);
    let white = bgra(255, 255, 255);
    let mouth = bgra(196, 96, 96);
    let blush = bgra(255, 170, 170);
    let bikini = bgra(232, 84, 120);
    let bikini_tr = bgra(198, 56, 96);
    let shirt = bgra(250, 250, 252);
    let shirt_sh = bgra(224, 226, 234);
    let skirt = bgra(92, 120, 200);
    let skirt_sh = bgra(70, 96, 168);
    let jacket = bgra(245, 198, 86);

    // Place a block addressed on the 32-grid facing right; `mirror` flips it and
    // `bob` applies the breathing/jump offset (legs stay planted, so pass false).
    let place = |bits: &mut [u32], x: i32, y: i32, bw: i32, bh: i32, color: u32, bob: bool| {
        let yy = if bob { y + dy } else { y };
        let ax = if mirror { SPRITE - x - bw } else { x };
        fill_block(bits, w, h, ox + ax * u, oy + yy * u, bw * u, bh * u, color);
    };

    // Roughly 4-head proportions so the body (not a chibi head) is the focus.
    // Vertical layout on the 32 grid:
    //   head  3..11, neck 11..12, bust 12..17, waist 17..19,
    //   hips 19..23, thighs 23..27, calves 27..31.
    let leg_sh = bgra(214, 172, 148);

    // Hair behind the head + long twin tails down the sides.
    let tail = anim.tail_dy;
    place(bits, 10, 2, 12, 11, hair, true);
    for tx in [7, 23] {
        let ax = if mirror { SPRITE - tx - 2 } else { tx };
        fill_block(bits, w, h, ox + ax * u, oy + (5 + dy + tail) * u, 2 * u, 13 * u, hair);
    }
    // Tail ties (small accent near the top of each tail).
    place(bits, 7, 6, 2, 1, bikini, true);
    place(bits, 23, 6, 2, 1, bikini, true);

    // Head (slimmer oval) with a soft jaw shadow.
    place(bits, 11, 3, 10, 8, skin, true);
    place(bits, 12, 10, 8, 1, skin_sh, true);

    // Neck.
    place(bits, 14, 11, 4, 2, skin, true);
    place(bits, 14, 12, 4, 1, skin_sh, true);

    // ---- Bare body (drawn first; clothing layers over it) ----
    // Shoulders.
    place(bits, 11, 13, 10, 1, skin, true);
    // Bust: widest at the chest, with an under-bust shadow for roundness.
    place(bits, 10, 14, 12, 3, skin, true);
    place(bits, 10, 16, 12, 1, skin_sh, true);
    // Cinched waist.
    place(bits, 12, 17, 8, 2, skin, true);
    // Hips: flare back out below the waist.
    place(bits, 10, 19, 12, 4, skin, true);
    place(bits, 10, 21, 1, 2, skin_sh, true); // side contour shading
    place(bits, 21, 21, 1, 2, skin_sh, true);
    // Slim arms along the sides.
    place(bits, 8, 14, 2, 7, skin, true);
    place(bits, 22, 14, 2, 7, skin, true);
    place(bits, 8, 18, 2, 3, skin_sh, true);
    place(bits, 22, 18, 2, 3, skin_sh, true);

    // Legs (planted; animate with the walk offsets) — thighs taper to calves.
    let lb = 12 + anim.leg_back_dx;
    let lf = 17 + anim.leg_front_dx;
    let axb = if mirror { SPRITE - lb - 3 } else { lb };
    let axf = if mirror { SPRITE - lf - 3 } else { lf };
    // thighs (3 wide)
    fill_block(bits, w, h, ox + axb * u, oy + 23 * u, 3 * u, 4 * u, skin);
    fill_block(bits, w, h, ox + axf * u, oy + 23 * u, 3 * u, 4 * u, skin);
    // calves (2 wide, inset) + foot shade
    fill_block(bits, w, h, ox + (axb + 1) * u, oy + 27 * u, 2 * u, 4 * u, skin);
    fill_block(bits, w, h, ox + (axf + 1) * u, oy + 27 * u, 2 * u, 4 * u, skin);
    fill_block(bits, w, h, ox + axb * u, oy + 30 * u, 3 * u, u, leg_sh);
    fill_block(bits, w, h, ox + axf * u, oy + 30 * u, 3 * u, u, leg_sh);

    // ---- Clothing layers ----
    // Bottom layer: flared skirt while dressed, bikini bottom at the top stage.
    if stage <= 2 {
        place(bits, 9, 19, 14, 4, skirt, true);
        place(bits, 9, 22, 14, 1, skirt_sh, true);
    } else {
        // Hip-hugging bikini bottom with side ties. Starts at y19 (the top of
        // the hips) so no bare strip shows above the waistband when there is no
        // skirt to back it up.
        place(bits, 10, 19, 12, 4, bikini, true);
        place(bits, 10, 19, 12, 1, bikini_tr, true);
        place(bits, 9, 19, 1, 2, bikini_tr, true);
        place(bits, 22, 19, 1, 2, bikini_tr, true);
    }
    // Top layer: shirt, then bikini top as the shirt comes off.
    if stage <= 1 {
        place(bits, 10, 13, 12, 6, shirt, true);
        place(bits, 10, 16, 12, 1, shirt_sh, true);
        place(bits, 8, 14, 2, 4, shirt, true);
        place(bits, 22, 14, 2, 4, shirt, true);
    } else {
        // Triangle bikini top hugging the bustline + neck/shoulder straps. The
        // cup spans y14-16 so it covers the bare under-bust shadow at y16 (else
        // the cup edge reads as ending too high above the midriff).
        place(bits, 10, 14, 12, 3, bikini, true);
        place(bits, 10, 16, 12, 1, bikini_tr, true);
        place(bits, 14, 14, 4, 1, bikini_tr, true); // cleavage notch
        place(bits, 11, 13, 1, 1, bikini_tr, true); // straps
        place(bits, 20, 13, 1, 1, bikini_tr, true);
    }
    // Jacket: only at the fully-dressed stage (open over the shirt).
    if stage == 0 {
        place(bits, 8, 13, 2, 9, jacket, true);
        place(bits, 22, 13, 2, 9, jacket, true);
        place(bits, 10, 13, 12, 1, jacket, true);
    }

    // Hair front (bangs + side locks) over the forehead. Highlight centered on
    // the sprite axis (x15.5) so it isn't biased to one side.
    place(bits, 11, 3, 10, 3, hair, true);
    place(bits, 14, 3, 4, 2, hair_hi, true);
    place(bits, 9, 5, 2, 8, hair, true);
    place(bits, 21, 5, 2, 8, hair, true);

    // Face. Eyes around y=6-8, mouth at y=9 on the slimmer head.
    let lips = bgra(228, 72, 96);
    if kissing {
        // Happy closed "^ ^" eyes.
        place(bits, 12, 7, 3, 1, eye, true);
        place(bits, 13, 8, 1, 1, eye, true);
        place(bits, 17, 7, 3, 1, eye, true);
        place(bits, 18, 8, 1, 1, eye, true);
    } else if anim.blink {
        place(bits, 12, 8, 3, 1, skin_sh, true);
        place(bits, 17, 8, 3, 1, skin_sh, true);
    } else {
        place(bits, 12, 6, 3, 3, eye, true);
        place(bits, 17, 6, 3, 3, eye, true);
        place(bits, 13, 6, 1, 1, white, true);
        place(bits, 18, 6, 1, 1, white, true);
    }
    place(bits, 11, 8, 2, 1, blush, true);
    place(bits, 19, 8, 2, 1, blush, true);

    if kissing {
        // Stronger blush + a small puckered mouth.
        place(bits, 10, 8, 2, 2, blush, true);
        place(bits, 20, 8, 2, 2, blush, true);
        place(bits, 15, 9, 2, 2, lips, true);
        place(bits, 15, 10, 2, 1, bgra(196, 56, 80), true);
    } else {
        place(bits, 15, 9, 2, 1, mouth, true);
        match mood {
            Band::Soft => {
                place(bits, 21, 4, 1, 2, bgra(150, 205, 240), true);
            }
            Band::Urgent => {
                place(bits, 10, 8, 2, 2, blush, true);
                place(bits, 20, 8, 2, 2, blush, true);
                place(bits, 15, 9, 2, 2, mouth, true);
            }
            Band::Low => {}
        }
    }

    // Floating hearts during the kiss: they rise and fade as the timer runs out.
    if kissing && kiss_max > 0 {
        let elapsed = kiss_max - kiss; // 0..kiss_max
        // Base near the mouth, drifting up and to the facing side.
        let drift = (elapsed as i32) / 2; // rows risen
        let side = if mirror { -1 } else { 1 };
        let hearts = [
            (16, 7 - drift, frame % 2 == 0),
            (19, 9 - drift + 2, (frame / 2) % 2 == 0),
            (13, 8 - drift + 4, (frame / 3) % 2 == 0),
        ];
        for (i, (hx, hy, big)) in hearts.iter().enumerate() {
            // Stagger so later hearts only appear after the first has risen.
            if (elapsed as usize) < i * 4 {
                continue;
            }
            let wobble = if *big { side } else { 0 };
            draw_heart(bits, w, h, ox, oy, u, mirror, *hx + wobble, *hy, *big);
        }
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
        usage,
        cat_snapshot,
        dog_snapshot,
        girl_snapshot,
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
            s.usage,
            snapshot(&s.cat, s.mood),
            snapshot(&s.dog, s.mood),
            snapshot(&s.girl, s.mood),
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
        if kind.shows_girl() {
            let ox = (girl_snapshot.x * u as f32) as i32 + shake(girl_snapshot.mood);
            let anim = anim_for(girl_snapshot.pose, frame, girl_snapshot.blink_ctr);
            draw_girl(
                bits,
                win_w,
                win_h,
                ox,
                base_oy,
                u,
                girl_snapshot.dir < 0.0,
                &anim,
                girl_snapshot.mood,
                usage,
                girl_snapshot.kiss,
                girl_snapshot.kiss_max,
                frame,
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
        if kind.shows_girl() {
            if let Some(b) = &girl_snapshot.bubble {
                draw_bubble(
                    mem_dc,
                    bits,
                    win_w,
                    win_h,
                    u,
                    girl_snapshot.x,
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
    kiss: u32,
    kiss_max: u32,
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
        kiss: c.kiss,
        kiss_max: c.kiss_max,
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
    let by = u;
    // Avoid overlapping an existing bubble by shifting horizontally if needed.
    for (ux0, ux1) in used.iter() {
        if bx < *ux1 && bx + bubble_w > *ux0 {
            bx = (*ux1 + 2 * u).min((w - bubble_w).max(0));
        }
    }
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
// All eight UI languages have their own pools (translations preserve each
// character's personality rather than being literal).

fn message_pool(lang: LanguageId, species: Species, pool: Pool) -> &'static [&'static str] {
    if species == Species::Girl {
        return girl_pool(lang, pool);
    }
    let is_cat = species == Species::Cat;
    match lang {
        LanguageId::Japanese => ja_pool(is_cat, pool),
        LanguageId::German => de_pool(is_cat, pool),
        LanguageId::Dutch => nl_pool(is_cat, pool),
        LanguageId::Spanish => es_pool(is_cat, pool),
        LanguageId::French => fr_pool(is_cat, pool),
        LanguageId::Korean => ko_pool(is_cat, pool),
        LanguageId::TraditionalChinese => zh_pool(is_cat, pool),
        LanguageId::English => en_pool(is_cat, pool),
    }
}

/// The girl character: cute and upbeat, growing flustered as usage climbs (her
/// outfit thins out toward swimwear). One pool set, localized per language.
fn girl_pool(lang: LanguageId, pool: Pool) -> &'static [&'static str] {
    match lang {
        LanguageId::Japanese => match pool {
            Pool::Hover => &["やっほー♪", "どうしたの？", "こっち見てる？"],
            Pool::Click => &["きゃっ♪", "なになに？", "えへへ"],
            Pool::Idle => &["ふんふん♪", "ひまだなあ", "…"],
            Pool::Soft => &["8割こえちゃった…", "そろそろ注意してね？", "ちょっとペース速いかも"],
            Pool::Urgent => &["もう9割っ…！", "み、見ないで〜！", "限界きちゃう！"],
            Pool::Rest => &["おつかれさま♪", "今日もがんばったね", "ひと休みしよ？"],
            Pool::Encourage => &["いい調子だよ♪", "その調子！", "まだ余裕だね"],
        },
        LanguageId::English => match pool {
            Pool::Hover => &["hi there♪", "what's up?", "you're looking?"],
            Pool::Click => &["eek♪", "yes? hehe", "what is it?"],
            Pool::Idle => &["la la la♪", "kinda bored", "..."],
            Pool::Soft => &["past 80%...", "careful, okay?", "slowing down maybe?"],
            Pool::Urgent => &["over 90%...!", "d-don't look~!", "I'm at my limit!"],
            Pool::Rest => &["nice work♪", "you did great today", "let's take a break?"],
            Pool::Encourage => &["doing great♪", "keep it up!", "still room to go"],
        },
        LanguageId::German => match pool {
            Pool::Hover => &["hallöchen♪", "was ist los?", "schaust du?"],
            Pool::Click => &["hach♪", "ja? hihi", "was denn?"],
            Pool::Idle => &["lalala♪", "etwas langweilig", "..."],
            Pool::Soft => &["über 80%...", "vorsicht, ja?", "wird etwas schnell"],
            Pool::Urgent => &["über 90%...!", "n-nicht hinsehen~!", "ich bin am Limit!"],
            Pool::Rest => &["gut gemacht♪", "tolle Arbeit heute", "kleine Pause?"],
            Pool::Encourage => &["läuft super♪", "weiter so!", "noch genug Luft"],
        },
        LanguageId::Dutch => match pool {
            Pool::Hover => &["hoi♪", "wat is er?", "kijk je?"],
            Pool::Click => &["hihi♪", "ja? hihi", "wat dan?"],
            Pool::Idle => &["lalala♪", "beetje saai", "..."],
            Pool::Soft => &["boven 80%...", "voorzichtig, hè?", "gaat wat snel"],
            Pool::Urgent => &["boven 90%...!", "n-niet kijken~!", "ik zit aan m'n grens!"],
            Pool::Rest => &["goed gedaan♪", "top vandaag", "even pauze?"],
            Pool::Encourage => &["gaat goed♪", "ga zo door!", "nog ruimte zat"],
        },
        LanguageId::Spanish => match pool {
            Pool::Hover => &["¡holaa♪", "¿qué pasa?", "¿me miras?"],
            Pool::Click => &["¡ay♪", "¿sí? jiji", "¿qué pasa?"],
            Pool::Idle => &["lalala♪", "qué aburrido", "..."],
            Pool::Soft => &["más del 80%...", "cuidado, ¿sí?", "vas un poco rápido"],
            Pool::Urgent => &["¡más del 90%...!", "¡n-no mires~!", "¡estoy al límite!"],
            Pool::Rest => &["¡bien hecho♪", "hoy lo hiciste genial", "¿un descanso?"],
            Pool::Encourage => &["¡vas genial♪", "¡sigue así!", "aún queda margen"],
        },
        LanguageId::French => match pool {
            Pool::Hover => &["coucou♪", "qu'y a-t-il ?", "tu regardes ?"],
            Pool::Click => &["hihi♪", "oui ? héhé", "quoi donc ?"],
            Pool::Idle => &["lalala♪", "un peu ennuyée", "..."],
            Pool::Soft => &["plus de 80%...", "attention, hein ?", "ça va un peu vite"],
            Pool::Urgent => &["plus de 90%...!", "n-ne regarde pas~!", "je suis à la limite !"],
            Pool::Rest => &["bien joué♪", "super boulot aujourd'hui", "une petite pause ?"],
            Pool::Encourage => &["ça roule♪", "continue !", "encore de la marge"],
        },
        LanguageId::Korean => match pool {
            Pool::Hover => &["야호♪", "무슨 일이야?", "보고 있어?"],
            Pool::Click => &["꺄♪", "응? 헤헤", "왜왜?"],
            Pool::Idle => &["흥얼흥얼♪", "좀 심심해", "..."],
            Pool::Soft => &["80% 넘었어...", "조심하자, 응?", "조금 빠른 듯"],
            Pool::Urgent => &["90% 넘었어...!", "보, 보지 마~!", "한계야!"],
            Pool::Rest => &["수고했어♪", "오늘 정말 잘했어", "좀 쉬자?"],
            Pool::Encourage => &["잘하고 있어♪", "그 기세야!", "아직 여유 있어"],
        },
        LanguageId::TraditionalChinese => match pool {
            Pool::Hover => &["哈囉♪", "怎麼了？", "在看我嗎？"],
            Pool::Click => &["呀♪", "嗯？嘿嘿", "幹嘛呀？"],
            Pool::Idle => &["哼哼♪", "有點無聊", "..."],
            Pool::Soft => &["超過八成了...", "小心一點喔？", "好像有點快"],
            Pool::Urgent => &["超過九成了...！", "別、別看啦～！", "快到極限了！"],
            Pool::Rest => &["辛苦了♪", "今天表現很棒", "休息一下？"],
            Pool::Encourage => &["狀態很好♪", "繼續加油！", "還有餘裕呢"],
        },
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

fn de_pool(is_cat: bool, pool: Pool) -> &'static [&'static str] {
    if is_cat {
        match pool {
            Pool::Hover => &[
                "...was willst du.",
                "ach, du bist's.",
                "starrst du mich an?",
                "ja?",
                "...na gut, schau.",
            ],
            Pool::Click => &[
                "muss das sein.",
                "...pff.",
                "schon wieder? meinetwegen.",
                "stört's dich?",
                "na schön.",
            ],
            Pool::Idle => &["zzz...", "so langweilig.", "...", "*streck*"],
            Pool::Soft => &["mach mal langsam?", "über 80%, weißt du.", "das ist viel..."],
            Pool::Urgent => &["das ist schlecht.", "fast am Limit.", "über 90%. nicht mein Problem."],
            Pool::Rest => &["...gut gemacht.", "nicht übel, schätze ich.", "ruh dich aus."],
            Pool::Encourage => &["läuft doch.", "noch genug übrig.", "pff, nicht schlecht."],
        }
    } else {
        match pool {
            Pool::Hover => &[
                "HI HI HI!! 🐾",
                "spielen?? spielen??",
                "schau mich an!!",
                "oh Mann oh Mann!",
                "hier rüber!!",
            ],
            Pool::Click => &[
                "JAJAJA!!",
                "nochmal nochmal!! 🐾",
                "bester Tag ever!!",
                "streichel mich mehr!!",
                "juhuu!!",
            ],
            Pool::Idle => &["so langweilig wuff", "lass uns spielen!", "*wedelt*", "Gassi??"],
            Pool::Soft => &["vorsicht jetzt!", "über 80%!", "das ist ganz schön viel!"],
            Pool::Urgent => &["oh oh!!", "fast am Limit!!", "über 90%! vorsicht!!"],
            Pool::Rest => &["super gemacht heute!!", "geschafft!!", "so stolz!!"],
            Pool::Encourage => &["läuft super!!", "weiter so!!", "du schaffst das!!"],
        }
    }
}

fn nl_pool(is_cat: bool, pool: Pool) -> &'static [&'static str] {
    if is_cat {
        match pool {
            Pool::Hover => &[
                "...wat wil je.",
                "oh, jij bent het.",
                "zit je te staren?",
                "ja?",
                "...goed, kijk dan.",
            ],
            Pool::Click => &[
                "moet dat nou.",
                "...pff.",
                "alweer? mij best.",
                "stoor ik?",
                "vooruit dan maar.",
            ],
            Pool::Idle => &["zzz...", "zo saai.", "...", "*rek uit*"],
            Pool::Soft => &["rustig aan?", "boven de 80%, hoor.", "dat is veel..."],
            Pool::Urgent => &["dit is slecht.", "bijna op.", "boven 90%. niet mijn probleem."],
            Pool::Rest => &["...goed gedaan.", "niet slecht, denk ik.", "ga maar rusten."],
            Pool::Encourage => &["gaat prima.", "nog genoeg over.", "pff, niet slecht."],
        }
    } else {
        match pool {
            Pool::Hover => &[
                "HOI HOI HOI!! 🐾",
                "spelen?? spelen??",
                "kijk naar mij!!",
                "oh jee oh jee!",
                "hierheen!!",
            ],
            Pool::Click => &[
                "JAJAJA!!",
                "nog een keer!! 🐾",
                "beste dag ooit!!",
                "aai me meer!!",
                "joepie!!",
            ],
            Pool::Idle => &["zo saai woef", "laten we spelen!", "*kwispel*", "wandelen??"],
            Pool::Soft => &["voorzichtig nu!", "boven de 80%!", "dat is best veel!"],
            Pool::Urgent => &["oei oei!!", "bijna op!!", "boven 90%! pas op!!"],
            Pool::Rest => &["goed gedaan vandaag!!", "het is gelukt!!", "zo trots!!"],
            Pool::Encourage => &["gaat goed!!", "ga zo door!!", "jij kan dit!!"],
        }
    }
}

fn es_pool(is_cat: bool, pool: Pool) -> &'static [&'static str] {
    if is_cat {
        match pool {
            Pool::Hover => &[
                "...qué quieres.",
                "ah, eres tú.",
                "¿me estás mirando?",
                "¿sí?",
                "...vale, mira.",
            ],
            Pool::Click => &[
                "¿en serio?",
                "...bah.",
                "¿otra vez? como quieras.",
                "¿te importa?",
                "está bien, supongo.",
            ],
            Pool::Idle => &["zzz...", "qué aburrimiento.", "...", "*estira*"],
            Pool::Soft => &["¿bajas el ritmo?", "más del 80%, ¿eh?", "eso es mucho..."],
            Pool::Urgent => &["esto va mal.", "casi al límite.", "más del 90%. no es mi problema."],
            Pool::Rest => &["...buen trabajo.", "no está mal, supongo.", "ve a descansar."],
            Pool::Encourage => &["vas bien.", "queda de sobra.", "bah, no está mal."],
        }
    } else {
        match pool {
            Pool::Hover => &[
                "¡¡HOLA HOLA!! 🐾",
                "¿¿jugamos??",
                "¡¡mírame!!",
                "¡ay qué emoción!",
                "¡¡por aquí!!",
            ],
            Pool::Click => &[
                "¡¡SÍSÍSÍ!!",
                "¡¡otra vez!! 🐾",
                "¡¡el mejor día!!",
                "¡¡acaríciame más!!",
                "¡¡yupi!!",
            ],
            Pool::Idle => &["qué aburrido guau", "¡a jugar!", "*mueve la cola*", "¿¿paseo??"],
            Pool::Soft => &["¡cuidado ya!", "¡más del 80%!", "¡eso es bastante!"],
            Pool::Urgent => &["¡¡ay no!!", "¡¡casi al límite!!", "¡¡más del 90%! ¡cuidado!!"],
            Pool::Rest => &["¡¡buen trabajo hoy!!", "¡¡lo lograste!!", "¡¡qué orgullo!!"],
            Pool::Encourage => &["¡¡vas genial!!", "¡¡sigue así!!", "¡¡tú puedes!!"],
        }
    }
}

fn fr_pool(is_cat: bool, pool: Pool) -> &'static [&'static str] {
    if is_cat {
        match pool {
            Pool::Hover => &[
                "...qu'est-ce que tu veux.",
                "ah, c'est toi.",
                "tu me fixes ?",
                "ouais ?",
                "...bon, regarde.",
            ],
            Pool::Click => &[
                "il le faut vraiment.",
                "...pff.",
                "encore ? si tu veux.",
                "ça te dérange ?",
                "bon, d'accord.",
            ],
            Pool::Idle => &["zzz...", "tellement ennuyeux.", "...", "*s'étire*"],
            Pool::Soft => &["tu ralentis ?", "plus de 80%, tu sais.", "ça fait beaucoup..."],
            Pool::Urgent => &["c'est mauvais.", "presque au max.", "plus de 90%. pas mon problème."],
            Pool::Rest => &["...bien joué.", "pas mal, j'imagine.", "va te reposer."],
            Pool::Encourage => &["ça roule.", "il reste de la marge.", "pff, pas mal."],
        }
    } else {
        match pool {
            Pool::Hover => &[
                "COUCOU COUCOU !! 🐾",
                "on joue ?? on joue ??",
                "regarde-moi !!",
                "oh là là !",
                "par ici !!",
            ],
            Pool::Click => &[
                "OUIOUIOUI !!",
                "encore encore !! 🐾",
                "meilleur jour !!",
                "caresse-moi encore !!",
                "youpi !!",
            ],
            Pool::Idle => &["trop ennuyeux ouaf", "on joue !", "*remue la queue*", "promenade ??"],
            Pool::Soft => &["attention !", "plus de 80% !", "ça fait pas mal !"],
            Pool::Urgent => &["oh oh !!", "presque au max !!", "plus de 90% ! attention !!"],
            Pool::Rest => &["bravo aujourd'hui !!", "tu as réussi !!", "trop fier !!"],
            Pool::Encourage => &["ça va super !!", "continue !!", "tu gères !!"],
        }
    }
}

fn ko_pool(is_cat: bool, pool: Pool) -> &'static [&'static str] {
    if is_cat {
        match pool {
            Pool::Hover => &["...뭐야.", "아, 너구나.", "쳐다보는 거야?", "응?", "...그래, 봐."],
            Pool::Click => &[
                "꼭 그래야겠어?",
                "...흥.",
                "또? 마음대로 해.",
                "방해되는데.",
                "뭐, 좋아.",
            ],
            Pool::Idle => &["zzz...", "심심하다냥.", "...", "*기지개*"],
            Pool::Soft => &["슬슬 줄이지?", "80% 넘었어.", "좀 많은데..."],
            Pool::Urgent => &["위험하다냥!", "거의 한계야.", "90% 넘음. 난 몰라."],
            Pool::Rest => &["...수고했어.", "뭐, 나쁘진 않네.", "이제 좀 쉬어."],
            Pool::Encourage => &["좋은데.", "아직 여유 있잖아.", "흥, 나쁘지 않네."],
        }
    } else {
        match pool {
            Pool::Hover => &["안녕안녕!! 🐾", "놀자?? 놀자??", "나 좀 봐봐!!", "우와 우와!", "여기야 여기!!"],
            Pool::Click => &["좋아좋아!!", "한 번 더!! 🐾", "최고의 날!!", "더 쓰다듬어줘!!", "야호!!"],
            Pool::Idle => &["심심해 멍", "놀자!", "*꼬리 흔들흔들*", "산책 갈까??"],
            Pool::Soft => &["이제 조심해!", "80% 넘었어!", "좀 많은 것 같아!"],
            Pool::Urgent => &["큰일이야 멍!", "거의 한계야!", "90%! 조심해!!"],
            Pool::Rest => &["오늘도 잘했어!!", "해냈어!!", "정말 대단해!!"],
            Pool::Encourage => &["잘하고 있어!!", "그 기세야!!", "넌 할 수 있어!!"],
        }
    }
}

fn zh_pool(is_cat: bool, pool: Pool) -> &'static [&'static str] {
    if is_cat {
        match pool {
            Pool::Hover => &["...你想幹嘛。", "喔，是你啊。", "在盯著我看？", "幹嘛？", "...好啦，看吧。"],
            Pool::Click => &["非要這樣嗎。", "...哼。", "又來？隨便你。", "很煩耶。", "好吧，隨便。"],
            Pool::Idle => &["zzz...", "好無聊。", "...", "*伸懶腰*"],
            Pool::Soft => &["差不多該收手了吧？", "超過八成了喔。", "有點多耶..."],
            Pool::Urgent => &["不妙喔。", "快到上限了。", "超過九成。不關我的事。"],
            Pool::Rest => &["...辛苦了。", "還不錯啦，我想。", "去休息吧。"],
            Pool::Encourage => &["還行啊。", "還有餘裕呢。", "哼，不賴嘛。"],
        }
    } else {
        match pool {
            Pool::Hover => &["嗨嗨嗨!! 🐾", "玩嗎?? 玩嗎??", "看我看我!!", "天啊天啊!", "這邊這邊!!"],
            Pool::Click => &["好耶好耶!!", "再一次!! 🐾", "最棒的一天!!", "再摸摸我!!", "耶!!"],
            Pool::Idle => &["好無聊汪", "來玩吧!", "*搖尾巴*", "去散步??"],
            Pool::Soft => &["小心點囉!", "超過八成了!", "好像有點多!"],
            Pool::Urgent => &["糟糕!!", "快到上限了!!", "超過九成! 小心!!"],
            Pool::Rest => &["今天也辛苦了!!", "你做到了!!", "好驕傲!!"],
            Pool::Encourage => &["表現很好!!", "繼續加油!!", "你可以的!!"],
        }
    }
}
