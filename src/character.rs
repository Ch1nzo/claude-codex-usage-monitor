//! Pixel character system: a separate transparent, click-through, top-most
//! window that floats just above the taskbar widget. It draws a roaming pixel
//! cat and/or dog (composed from solid blocks straight into a 32-bit DIB so we
//! get exact per-pixel alpha) and shows speech bubbles on usage-threshold
//! events.
//!
//! The drawing is intentionally isolated behind `draw_character` so the block
//! art can later be swapped for PNG sprite sheets without touching the rest.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::localization::LanguageId;
use crate::native_interop;

const CLASS_NAME: &str = "ClaudeCodexCharacter";
const TIMER_ANIM: usize = 101;
const ANIM_INTERVAL_MS: u32 = 120;

// Art is laid out on a unit grid; each unit is `u` device pixels.
const GRID_W: i32 = 14;
const GRID_H: i32 = 14;
// Window is wider/taller than one character to allow roaming and a bubble.
const WIN_UW: i32 = 64;
const WIN_UH: i32 = 34;

// Bubble visible duration in animation ticks (~120ms each).
const BUBBLE_TTL: u32 = 42;
// Minimum ticks between random idle encouragements.
const IDLE_COOLDOWN: u64 = 250;

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
enum Trigger {
    IdleEncourage,
    SoftWarn,
    UrgentWarn,
    Rest,
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
}

struct CharState {
    hwnd: isize,
    enabled: bool,
    kind: CharacterKind,
    lang: LanguageId,
    u: i32,
    win_w: i32,
    win_h: i32,
    frame: u64,
    cat_x: f32,
    cat_dir: f32,
    dog_x: f32,
    dog_dir: f32,
    bubble: Option<Bubble>,
    react_ticks: u32,
    last_band: Band,
    last_idle_frame: u64,
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
            .unwrap_or(0x9E3779B97F4A7C15)
            | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RNG.store(x, Ordering::Relaxed);
    x
}

fn pick(pool: &[&'static str]) -> &'static str {
    if pool.is_empty() {
        return "";
    }
    pool[(rng_next() as usize) % pool.len()]
}

/// Create (once) and show the character window. Safe to call again to update
/// the enabled state / kind / anchor.
pub fn init(anchor: RECT, enabled: bool, kind: CharacterKind, lang: LanguageId) {
    {
        let guard = STATE.lock().unwrap();
        if guard.is_some() {
            drop(guard);
            set_enabled(enabled);
            set_kind(kind);
            set_language(lang);
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
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
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
    let u = ((4 * dpi as i32) / 96).max(3);
    let win_w = WIN_UW * u;
    let win_h = WIN_UH * u;

    let state = CharState {
        hwnd: hwnd.0 as isize,
        enabled,
        kind,
        lang,
        u,
        win_w,
        win_h,
        frame: 0,
        cat_x: 2.0,
        cat_dir: 1.0,
        dog_x: (WIN_UW - GRID_W - 2) as f32,
        dog_dir: -1.0,
        bubble: None,
        react_ticks: 0,
        last_band: Band::Low,
        last_idle_frame: 0,
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

/// Place the window just above the supplied widget rectangle (screen coords).
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

/// Drive threshold-based messages from the latest usage percentage.
pub fn on_usage_update(max_percent: f64, lang: LanguageId) {
    let mut guard = STATE.lock().unwrap();
    let Some(s) = guard.as_mut() else {
        return;
    };
    s.lang = lang;
    if !s.enabled {
        return;
    }

    let band = band_for(max_percent);
    let trigger = match (s.last_band, band) {
        (prev, Band::Urgent) if prev != Band::Urgent => Some(Trigger::UrgentWarn),
        (prev, Band::Soft) if prev == Band::Low => Some(Trigger::SoftWarn),
        (prev, Band::Low) if prev != Band::Low => Some(Trigger::Rest),
        (Band::Low, Band::Low) => {
            if s.frame.saturating_sub(s.last_idle_frame) >= IDLE_COOLDOWN
                && rng_next() % 5 == 0
            {
                s.last_idle_frame = s.frame;
                Some(Trigger::IdleEncourage)
            } else {
                None
            }
        }
        _ => None,
    };
    s.last_band = band;

    if let Some(trigger) = trigger {
        let text = pick(message_pool(s.lang, trigger)).to_string();
        s.bubble = Some(Bubble {
            text,
            ttl: BUBBLE_TTL,
        });
        s.react_ticks = 8;
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
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_ANIM).ok();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
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

        let max_x = (WIN_UW - GRID_W) as f32;
        let speed = 0.35_f32;
        if s.kind.shows_cat() {
            s.cat_x += s.cat_dir * speed;
            if s.cat_x <= 0.0 {
                s.cat_x = 0.0;
                s.cat_dir = 1.0;
            } else if s.cat_x >= max_x {
                s.cat_x = max_x;
                s.cat_dir = -1.0;
            }
        }
        if s.kind.shows_dog() {
            s.dog_x += s.dog_dir * speed * 0.9;
            if s.dog_x <= 0.0 {
                s.dog_x = 0.0;
                s.dog_dir = 1.0;
            } else if s.dog_x >= max_x {
                s.dog_x = max_x;
                s.dog_dir = -1.0;
            }
        }

        if s.react_ticks > 0 {
            s.react_ticks -= 1;
        }
        if let Some(b) = s.bubble.as_mut() {
            if b.ttl > 0 {
                b.ttl -= 1;
            }
            if b.ttl == 0 {
                s.bubble = None;
            }
        }
    }
    render();
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
        for xx in x..(x + bw) {
            if xx < 0 || xx >= w {
                continue;
            }
            bits[(yy * w + xx) as usize] = color;
        }
    }
}

#[derive(Clone, Copy)]
enum Part {
    Body,
    Light,
    Eye,
    Nose,
    Ear,
}

fn palette(kind_is_cat: bool, part: Part) -> u32 {
    if kind_is_cat {
        match part {
            Part::Body => bgra(150, 154, 160),
            Part::Light => bgra(215, 218, 221),
            Part::Eye => bgra(40, 40, 45),
            Part::Nose => bgra(220, 140, 150),
            Part::Ear => bgra(230, 170, 180),
        }
    } else {
        match part {
            Part::Body => bgra(196, 150, 98),
            Part::Light => bgra(228, 210, 176),
            Part::Eye => bgra(50, 40, 35),
            Part::Nose => bgra(60, 45, 40),
            Part::Ear => bgra(150, 110, 70),
        }
    }
}

/// Block layout (ux, uy, uw, uh, part) for a character facing right.
fn character_blocks(is_cat: bool, leg_phase: bool) -> Vec<(i32, i32, i32, i32, Part)> {
    let mut v: Vec<(i32, i32, i32, i32, Part)> = Vec::new();
    if is_cat {
        // pointy ears
        v.push((2, 0, 2, 3, Part::Body));
        v.push((10, 0, 2, 3, Part::Body));
        v.push((2, 1, 1, 1, Part::Ear));
        v.push((11, 1, 1, 1, Part::Ear));
        // head
        v.push((2, 3, 10, 7, Part::Body));
        // eyes
        v.push((4, 5, 2, 2, Part::Eye));
        v.push((8, 5, 2, 2, Part::Eye));
        // nose
        v.push((6, 7, 2, 1, Part::Nose));
        // body + belly
        v.push((3, 10, 8, 3, Part::Body));
        v.push((5, 11, 4, 2, Part::Light));
        // tail (left)
        v.push((0, 8, 2, 1, Part::Body));
        v.push((0, 7, 1, 2, Part::Body));
    } else {
        // floppy ears
        v.push((1, 2, 2, 5, Part::Ear));
        v.push((11, 2, 2, 5, Part::Ear));
        // head
        v.push((3, 2, 8, 8, Part::Body));
        // snout
        v.push((4, 7, 6, 3, Part::Light));
        // eyes
        v.push((5, 4, 2, 2, Part::Eye));
        v.push((8, 4, 2, 2, Part::Eye));
        // nose
        v.push((6, 7, 2, 2, Part::Nose));
        // body + belly
        v.push((3, 10, 8, 3, Part::Body));
        v.push((5, 11, 4, 2, Part::Light));
        // tail (left)
        v.push((0, 9, 2, 1, Part::Body));
        v.push((0, 8, 1, 2, Part::Body));
    }
    // legs (alternate for a simple walk cycle)
    if leg_phase {
        v.push((3, 13, 2, 1, Part::Body));
        v.push((9, 13, 2, 1, Part::Body));
    } else {
        v.push((4, 13, 2, 1, Part::Body));
        v.push((8, 13, 2, 1, Part::Body));
    }
    v
}

#[allow(clippy::too_many_arguments)]
fn draw_character(
    bits: &mut [u32],
    w: i32,
    h: i32,
    is_cat: bool,
    ox: i32,
    oy: i32,
    u: i32,
    mirror: bool,
    leg_phase: bool,
) {
    for (ux, uy, uw, uh, part) in character_blocks(is_cat, leg_phase) {
        let ax = if mirror { GRID_W - ux - uw } else { ux };
        let color = palette(is_cat, part);
        fill_block(bits, w, h, ox + ax * u, oy + uy * u, uw * u, uh * u, color);
    }
}

fn render() {
    let (hwnd, win_w, win_h, u, kind, cat_x, cat_dir, dog_x, dog_dir, frame, bubble_text, react) = {
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
            s.u,
            s.kind,
            s.cat_x,
            s.cat_dir,
            s.dog_x,
            s.dog_dir,
            s.frame,
            s.bubble.as_ref().map(|b| b.text.clone()),
            s.react_ticks > 0,
        )
    };

    unsafe {
        let screen_dc = GetDC(hwnd);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: win_w,
                biHeight: -win_h, // top-down
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
        // Transparent background.
        for px in bits.iter_mut() {
            *px = 0;
        }

        // Vertical bob: 0 or 1 unit, larger while reacting.
        let bob = if react {
            if (frame / 2) % 2 == 0 {
                0
            } else {
                2 * u
            }
        } else if (frame / 4) % 2 == 0 {
            0
        } else {
            u
        };
        let base_oy = win_h - (GRID_H + 1) * u - bob;
        let leg_phase = (frame / 3) % 2 == 0;

        if kind.shows_cat() {
            let ox = (cat_x * u as f32) as i32;
            draw_character(bits, win_w, win_h, true, ox, base_oy, u, cat_dir < 0.0, leg_phase);
        }
        if kind.shows_dog() {
            let ox = (dog_x * u as f32) as i32;
            draw_character(
                bits,
                win_w,
                win_h,
                false,
                ox,
                base_oy,
                u,
                dog_dir < 0.0,
                !leg_phase,
            );
        }

        if let Some(text) = bubble_text {
            draw_bubble(mem_dc, bits, win_w, win_h, u, &text);
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
            AlphaFormat: 1, // AC_SRC_ALPHA
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

/// Draw a rounded speech bubble in the top area. The background is written
/// directly (opaque), the text via GDI, then the whole bubble rect is forced to
/// alpha 255 so the GDI text (which leaves the alpha byte at 0) stays visible.
fn draw_bubble(mem_dc: HDC, bits: &mut [u32], w: i32, _h: i32, u: i32, text: &str) {
    let pad = u;
    let bubble_h = 6 * u;
    let bubble_w = (w - 4 * u).max(8 * u);
    let bx = 2 * u;
    let by = u;

    let bg = bgra(250, 250, 250);
    let border = bgra(120, 120, 120);

    // Border then inner fill (simple 1-unit border).
    fill_block(bits, w, _h, bx, by, bubble_w, bubble_h, border);
    fill_block(
        bits,
        w,
        _h,
        bx + 1,
        by + 1,
        bubble_w - 2,
        bubble_h - 2,
        bg,
    );
    // Little tail pointing down toward the character.
    fill_block(bits, w, _h, bx + 3 * u, by + bubble_h, u, u, bg);

    unsafe {
        let font_name = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            -(2 * u),
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
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
        let _ = SetTextColor(mem_dc, COLORREF(native_interop::colorref(30, 30, 30)));

        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut rect = RECT {
            left: bx + pad,
            top: by + 1,
            right: bx + bubble_w - pad,
            bottom: by + bubble_h - 1,
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

    // Force the whole bubble region opaque so the GDI-drawn text is visible.
    for yy in by..(by + bubble_h) {
        if yy < 0 || yy >= _h {
            continue;
        }
        for xx in bx..(bx + bubble_w) {
            if xx < 0 || xx >= w {
                continue;
            }
            let idx = (yy * w + xx) as usize;
            bits[idx] |= 0xFF00_0000;
        }
    }
}

// ----- localized message pools ----------------------------------------------

fn message_pool(lang: LanguageId, trigger: Trigger) -> &'static [&'static str] {
    match lang {
        LanguageId::Japanese => match trigger {
            Trigger::IdleEncourage => &["いい調子！", "まだ余裕あるよ！", "その調子！"],
            Trigger::SoftWarn => &["そろそろ気をつけてね", "8割こえたよ", "ペース配分しよ？"],
            Trigger::UrgentWarn => &["もうすぐ上限！", "あと少しで限界だよ！", "9割こえた！注意！"],
            Trigger::Rest => &["お疲れさま！", "今日もよくがんばったね！", "ひと休みしよう"],
        },
        // English (also the fallback for languages without a dedicated pool yet).
        _ => match trigger {
            Trigger::IdleEncourage => {
                &["Looking good!", "Plenty left, keep going!", "Nice and steady."]
            }
            Trigger::SoftWarn => &[
                "Getting a bit high...",
                "Maybe ease up soon.",
                "You're past 80%.",
            ],
            Trigger::UrgentWarn => &[
                "Almost at the limit!",
                "Careful - nearly maxed!",
                "Over 90%! Slow down.",
            ],
            Trigger::Rest => &[
                "Take a break, you earned it.",
                "Nice work today!",
                "Time to relax.",
            ],
        },
    }
}
