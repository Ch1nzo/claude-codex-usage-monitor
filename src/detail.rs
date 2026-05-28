//! Hover detail panel: a small, opaque, top-most, click-through card shown
//! above the widget while the cursor hovers it. It surfaces the same kind of
//! breakdown as Claude Code's `/usage` view - per-model 5h/7d bars with the
//! percentage and reset countdown - so routine monitoring needs no dashboard.
//!
//! The card is rendered fully opaque into a 32-bit DIB and pushed with
//! UpdateLayeredWindow (text drawn with GDI, then the whole card forced to
//! alpha 255 since GDI text leaves the alpha byte at 0).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::native_interop;

const CLASS_NAME: &str = "ClaudeCodexDetail";

// Logical layout (scaled by an integer DPI factor).
const PAD: i32 = 10;
const ROW_LABEL_W: i32 = 22;
const BAR_W: i32 = 132;
const BAR_H: i32 = 9;
const PCT_W: i32 = 38;
const RESET_W: i32 = 54;
const LINE_H: i32 = 18;
const HEADER_H: i32 = 20;
const ROW_GAP: i32 = 8;
const CARD_W: i32 = PAD + ROW_LABEL_W + BAR_W + 6 + PCT_W + 6 + RESET_W + PAD;

pub struct DetailRow {
    pub name: String,
    pub session_pct: f64,
    pub session_reset: String,
    pub weekly_pct: f64,
    pub weekly_reset: String,
}

pub struct DetailData {
    pub rows: Vec<DetailRow>,
    pub is_dark: bool,
}

struct DetailState {
    hwnd: isize,
    s: i32, // integer DPI scale
    visible: bool,
    data: DetailData,
}

static STATE: Mutex<Option<DetailState>> = Mutex::new(None);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

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

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

pub fn init() {
    {
        if STATE.lock().unwrap().is_some() {
            return;
        }
    }
    register_class();
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let class = native_interop::wide_str(CLASS_NAME);
        let title = native_interop::wide_str("Claude & Codex Usage Details");
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
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
    let s = ((dpi as i32 + 48) / 96).max(1);
    *STATE.lock().unwrap() = Some(DetailState {
        hwnd: hwnd.0 as isize,
        s,
        visible: false,
        data: DetailData {
            rows: Vec::new(),
            is_dark: true,
        },
    });
}

/// Show (or refresh) the panel anchored above the widget rectangle.
pub fn show(anchor: RECT, data: DetailData) {
    let (hwnd, s, w, h) = {
        let mut guard = STATE.lock().unwrap();
        let Some(st) = guard.as_mut() else {
            return;
        };
        let s = st.s;
        let rows = data.rows.len().max(1);
        let per_row = (HEADER_H + 2 * LINE_H + ROW_GAP) * s;
        let w = CARD_W * s;
        let h = PAD * 2 * s + per_row * rows as i32;
        st.data = data;
        st.visible = true;
        (HWND(st.hwnd as *mut _), s, w, h)
    };
    let _ = s;
    let x = (anchor.right - w).max(0);
    let y = (anchor.top - h - 4).max(0);
    unsafe {
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
    render();
}

/// Refresh content if the panel is currently visible.
pub fn update(data: DetailData) {
    {
        let mut guard = STATE.lock().unwrap();
        let Some(st) = guard.as_mut() else {
            return;
        };
        if !st.visible {
            return;
        }
        st.data = data;
    }
    render();
}

pub fn hide() {
    let hwnd = {
        let mut guard = STATE.lock().unwrap();
        let Some(st) = guard.as_mut() else {
            return;
        };
        if !st.visible {
            return;
        }
        st.visible = false;
        HWND(st.hwnd as *mut _)
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

pub fn is_visible() -> bool {
    STATE
        .lock()
        .unwrap()
        .as_ref()
        .map(|st| st.visible)
        .unwrap_or(false)
}

pub fn destroy() {
    let hwnd = {
        let mut guard = STATE.lock().unwrap();
        match guard.take() {
            Some(st) => HWND(st.hwnd as *mut _),
            None => return,
        }
    };
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

#[inline]
fn bgra(r: u8, g: u8, b: u8) -> u32 {
    0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

fn fill(bits: &mut [u32], w: i32, h: i32, x: i32, y: i32, bw: i32, bh: i32, color: u32) {
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

fn threshold_color(pct: f64, is_dark: bool) -> u32 {
    let p = pct.clamp(0.0, 100.0);
    if is_dark {
        if p >= 95.0 {
            bgra(255, 92, 92)
        } else if p >= 90.0 {
            bgra(255, 138, 76)
        } else if p >= 80.0 {
            bgra(242, 193, 78)
        } else {
            bgra(76, 199, 107)
        }
    } else if p >= 95.0 {
        bgra(211, 47, 47)
    } else if p >= 90.0 {
        bgra(232, 115, 28)
    } else if p >= 80.0 {
        bgra(201, 162, 30)
    } else {
        bgra(46, 158, 79)
    }
}

fn render() {
    let (hwnd, s, is_dark, rows_snapshot) = {
        let guard = STATE.lock().unwrap();
        let Some(st) = guard.as_ref() else {
            return;
        };
        if !st.visible {
            return;
        }
        let rows: Vec<(String, f64, String, f64, String)> = st
            .data
            .rows
            .iter()
            .map(|r| {
                (
                    r.name.clone(),
                    r.session_pct,
                    r.session_reset.clone(),
                    r.weekly_pct,
                    r.weekly_reset.clone(),
                )
            })
            .collect();
        (HWND(st.hwnd as *mut _), st.s, st.data.is_dark, rows)
    };

    let row_count = rows_snapshot.len().max(1);
    let per_row = (HEADER_H + 2 * LINE_H + ROW_GAP) * s;
    let w = CARD_W * s;
    let h = PAD * 2 * s + per_row * row_count as i32;

    let bg = if is_dark { bgra(32, 33, 36) } else { bgra(248, 248, 248) };
    let border = if is_dark { bgra(70, 72, 78) } else { bgra(200, 200, 205) };
    let track = if is_dark { bgra(60, 62, 68) } else { bgra(214, 214, 220) };
    let text_rgb = if is_dark {
        native_interop::colorref(225, 226, 230)
    } else {
        native_interop::colorref(40, 42, 48)
    };
    let sub_rgb = if is_dark {
        native_interop::colorref(150, 152, 158)
    } else {
        native_interop::colorref(110, 112, 118)
    };

    unsafe {
        let screen_dc = GetDC(hwnd);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
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
        let bits = std::slice::from_raw_parts_mut(bits_ptr as *mut u32, (w * h) as usize);

        // Opaque card with a 1px border.
        for px in bits.iter_mut() {
            *px = bg;
        }
        fill(bits, w, h, 0, 0, w, s, border);
        fill(bits, w, h, 0, h - s, w, s, border);
        fill(bits, w, h, 0, 0, s, h, border);
        fill(bits, w, h, w - s, 0, s, h, border);

        // Fonts.
        let name_font = make_font(13 * s, FW_SEMIBOLD.0 as i32);
        let small_font = make_font(12 * s, FW_NORMAL.0 as i32);
        let _ = SetBkMode(mem_dc, TRANSPARENT);

        let mut y = PAD * s;
        for (name, s_pct, s_reset, w_pct, w_reset) in &rows_snapshot {
            // Model name header.
            SelectObject(mem_dc, name_font);
            let _ = SetTextColor(mem_dc, COLORREF(text_rgb));
            draw_text(mem_dc, name, PAD * s, y, (CARD_W - 2 * PAD) * s, HEADER_H * s, DT_LEFT);
            y += HEADER_H * s;

            SelectObject(mem_dc, small_font);
            for (label, pct, reset) in [("5h", *s_pct, s_reset), ("7d", *w_pct, w_reset)] {
                let lx = PAD * s;
                let _ = SetTextColor(mem_dc, COLORREF(sub_rgb));
                draw_text(mem_dc, label, lx, y, ROW_LABEL_W * s, LINE_H * s, DT_LEFT);

                // Bar.
                let bx = lx + ROW_LABEL_W * s;
                let by = y + (LINE_H * s - BAR_H * s) / 2;
                fill(bits, w, h, bx, by, BAR_W * s, BAR_H * s, track);
                let fillw = (BAR_W as f64 * pct.clamp(0.0, 100.0) / 100.0) as i32 * s;
                if fillw > 0 {
                    fill(bits, w, h, bx, by, fillw, BAR_H * s, threshold_color(pct, is_dark));
                }

                // Percentage.
                let px = bx + BAR_W * s + 6 * s;
                let _ = SetTextColor(mem_dc, COLORREF(text_rgb));
                draw_text(mem_dc, &format!("{pct:.0}%"), px, y, PCT_W * s, LINE_H * s, DT_LEFT);

                // Reset countdown.
                if !reset.is_empty() {
                    let rx = px + PCT_W * s + 6 * s;
                    let _ = SetTextColor(mem_dc, COLORREF(sub_rgb));
                    draw_text(mem_dc, reset, rx, y, RESET_W * s, LINE_H * s, DT_RIGHT);
                }
                y += LINE_H * s;
            }
            y += ROW_GAP * s;
        }

        // GDI text left the alpha byte at 0 on the glyph pixels; force the whole
        // card opaque so it composites correctly.
        for px in bits.iter_mut() {
            *px |= 0xFF00_0000;
        }

        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE { cx: w, cy: h };
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
        let _ = DeleteObject(name_font);
        let _ = DeleteObject(small_font);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    let name = native_interop::wide_str("Segoe UI");
    CreateFontW(
        -height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_TT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        ANTIALIASED_QUALITY.0 as u32,
        (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR::from_raw(name.as_ptr()),
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_text(hdc: HDC, text: &str, x: i32, y: i32, w: i32, h: i32, align: DRAW_TEXT_FORMAT) {
    if text.is_empty() {
        return;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    let _ = DrawTextW(hdc, &mut wide, &mut rect, align | DT_VCENTER | DT_SINGLELINE);
}
