// Mouse, focus, multi-monitor placement, free 2D slingshot drag, spring snap, and dual-island merge.
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::SeqCst};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Manager, WebviewWindow};
use windows::Win32::Foundation::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{poll, settings, sys};

pub const WIN_W: f64 = 700.0;
pub const WIN_H: f64 = 430.0;

pub static DRAGGING: AtomicBool = AtomicBool::new(false); // a shelf file is on its way out
pub static OFFSET_Y: AtomicIsize = AtomicIsize::new(0); // debug runs sit lower, next to an installed copy
static SYNCING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug)]
struct MonInfo {
    name: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    scale: f64,
    primary: bool,
}

#[derive(Clone, Debug)]
struct PillDrag {
    grab_dx_norm: f64,
    grab_dy_norm: f64,
    start_cursor_x: i32,
    start_cursor_y: i32,
    last_cursor_x: i32,
    last_cursor_y: i32,
    vx: f64,
    vy: f64,
    max_pull_dist: i32,
    merge_active: bool,
}

#[derive(Clone, Debug)]
struct SnapAnim {
    cx: f64,
    y: f64,
    vx: f64,
    vy: f64,
    to_mon: String,
    step: u8,
    max_steps: u8,
    switched: bool,
    merge_into_main: bool,
    impact_speed: f64,
}

struct WinSlot {
    label: String,
    hwnd_raw: isize,
    interactive: bool,
    typing: bool,
    heartbeat: u64,
    prev_fg: isize,
    last_cursor: (i32, i32),
    last_emit: Instant,
    was_near: bool,
    down: bool,
    ours: bool,
    said_at: u64,
    monitor_name: String,
    is_primary: bool,
    last_state_json: String,
    pill_drag: Option<PillDrag>,
    snap_anim: Option<SnapAnim>,
}

static SLOTS: Mutex<Vec<WinSlot>> = Mutex::new(Vec::new());
static MON_CACHE: Mutex<Option<(Instant, Vec<MonInfo>)>> = Mutex::new(None);

fn slot_hwnd(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

fn collect_monitors(app: &AppHandle) -> Vec<MonInfo> {
    let prim = app.primary_monitor().ok().flatten();
    let list = app.available_monitors().unwrap_or_default();
    let mut out: Vec<MonInfo> = list
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            let name = m.name().cloned().unwrap_or_else(|| format!("DISPLAY-{i}"));
            let pos = m.position();
            let sz = m.size();
            let scale = m.scale_factor().max(1.0);
            let primary = prim.as_ref().is_some_and(|p| {
                p.name() == m.name() && p.position().x == pos.x && p.position().y == pos.y
            });
            MonInfo {
                name,
                x: pos.x,
                y: pos.y,
                w: sz.width as i32,
                h: sz.height as i32,
                scale,
                primary,
            }
        })
        .collect();
    if out.is_empty() {
        let w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(800);
        let h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(600);
        out.push(MonInfo {
            name: "PRIMARY".into(),
            x: 0,
            y: 0,
            w,
            h,
            scale: 1.0,
            primary: true,
        });
    } else if !out.iter().any(|m| m.primary) {
        if let Some(p) = out.iter_mut().find(|m| m.x == 0 && m.y == 0) {
            p.primary = true;
        } else {
            out[0].primary = true;
        }
    }
    out
}

fn cached_monitors(app: &AppHandle, max_age_ms: u64) -> Vec<MonInfo> {
    if max_age_ms > 0 {
        let g = MON_CACHE.lock().unwrap();
        if let Some((at, ref list)) = *g {
            if at.elapsed() < Duration::from_millis(max_age_ms) && !list.is_empty() {
                return list.clone();
            }
        }
    }
    let fresh = collect_monitors(app);
    *MON_CACHE.lock().unwrap() = Some((Instant::now(), fresh.clone()));
    fresh
}

fn center_rect_on(m: &MonInfo) -> (i32, i32, i32, i32) {
    let w = (WIN_W * m.scale).round() as i32;
    let hgt = (WIN_H * m.scale).round() as i32;
    let x = m.x + (m.w - w) / 2;
    let y = m.y + OFFSET_Y.load(SeqCst) as i32;
    (x, y, w, hgt)
}

fn find_mon_idx(mons: &[MonInfo], name: &str) -> usize {
    if !name.is_empty() {
        if let Some(i) = mons.iter().position(|m| m.name == name) {
            return i;
        }
    }
    mons.iter().position(|m| m.primary).unwrap_or(0)
}

fn main_monitor_name() -> String {
    SLOTS
        .lock()
        .unwrap()
        .iter()
        .find(|s| s.label == "main")
        .map(|s| s.monitor_name.clone())
        .unwrap_or_default()
}

fn assign_monitor_for_label(label: &str, mons: &[MonInfo], dual: bool) -> MonInfo {
    let prim_idx = mons.iter().position(|m| m.primary).unwrap_or(0);
    if dual && mons.len() >= 2 {
        if label == "secondary" {
            let sec_idx = mons.iter().position(|m| !m.primary).unwrap_or((prim_idx + 1) % mons.len());
            return mons[sec_idx].clone();
        }
        return mons[prim_idx].clone();
    }
    let saved = settings::get("monitorName");
    let saved_str = saved.as_str().unwrap_or("");
    let idx = find_mon_idx(mons, saved_str);
    mons[idx].clone()
}

fn monitor_idx_at(mons: &[MonInfo], px: i32, py: i32) -> usize {
    if let Some(i) = mons
        .iter()
        .position(|m| px >= m.x && px < m.x + m.w && py >= m.y && py < m.y + m.h)
    {
        return i;
    }
    mons.iter()
        .enumerate()
        .min_by_key(|(_, m)| {
            let cx = px.clamp(m.x, m.x + m.w - 1);
            let cy = py.clamp(m.y, m.y + m.h - 1);
            let dx = (px - cx) as i64;
            let dy = (py - cy) as i64;
            dx * dx + dy * dy
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

pub fn windows_snapshot() -> Vec<(String, HWND)> {
    SLOTS
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.hwnd_raw != 0)
        .map(|s| (s.label.clone(), slot_hwnd(s.hwnd_raw)))
        .collect()
}

pub fn is_island_hwnd(h: HWND) -> bool {
    let raw = h.0 as isize;
    SLOTS.lock().unwrap().iter().any(|s| s.hwnd_raw == raw)
}

pub fn register_window(win: &WebviewWindow) {
    let Ok(hwnd) = win.hwnd() else { return };
    let h = HWND(hwnd.0);
    let label = win.label().to_string();
    if label == "main" {
        sys::set_hwnd(h);
    }
    let mons = sys::APP.get().map(|a| cached_monitors(a, 0)).unwrap_or_default();
    let dual = settings::flag("dualMonitors") && mons.len() >= 2;
    let mon = assign_monitor_for_label(&label, &mons, dual);
    let (x, y, w, hgt) = center_rect_on(&mon);

    {
        let mut slots = SLOTS.lock().unwrap();
        slots.retain(|s| s.label != label);
        slots.push(WinSlot {
            label: label.clone(),
            hwnd_raw: h.0 as isize,
            interactive: false,
            typing: false,
            heartbeat: 0,
            prev_fg: 0,
            last_cursor: (i32::MIN, i32::MIN),
            last_emit: Instant::now(),
            was_near: false,
            down: false,
            ours: false,
            said_at: 0,
            monitor_name: mon.name.clone(),
            is_primary: mon.primary,
            last_state_json: String::new(),
            pill_drag: None,
            snap_anim: None,
        });
    }

    unsafe {
        apply_style(h, false, false);
        let _ = SetWindowPos(h, None, x, y, w, hgt, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    emit_monitor_state(&label, &mon, mons.len(), dual);
    if label == "secondary" {
        poll::send(poll::Cmd::Resync);
    }
}

pub fn unregister_window(label: &str) {
    SLOTS.lock().unwrap().retain(|s| s.label != label);
}

fn emit_monitor_state(label: &str, mon: &MonInfo, count: usize, dual: bool) {
    let payload = json!({
        "label": label,
        "isPrimary": mon.primary,
        "monitorName": mon.name,
        "monitorsCount": count,
        "dualMonitors": dual,
    });
    let text = payload.to_string();
    let changed = {
        let mut slots = SLOTS.lock().unwrap();
        if let Some(s) = slots.iter_mut().find(|s| s.label == label) {
            if s.last_state_json == text {
                false
            } else {
                s.last_state_json = text;
                true
            }
        } else {
            false
        }
    };
    if changed {
        sys::emit_to(label, "monitor-state", payload);
    }
}

pub fn monitor_state_payload(label: &str) -> Value {
    if label == "secondary" {
        poll::send(poll::Cmd::Resync);
    }
    let mons = sys::APP.get().map(|a| cached_monitors(a, 1500)).unwrap_or_default();
    let dual = settings::flag("dualMonitors") && mons.len() >= 2;
    let slots = SLOTS.lock().unwrap();
    if let Some(s) = slots.iter().find(|s| s.label == label) {
        return json!({
            "label": s.label,
            "isPrimary": s.is_primary,
            "monitorName": s.monitor_name,
            "monitorsCount": mons.len(),
            "dualMonitors": dual,
        });
    }
    json!({
        "label": label,
        "isPrimary": true,
        "monitorName": "",
        "monitorsCount": mons.len(),
        "dualMonitors": dual,
    })
}

pub fn set_interactive(label: &str, on: bool) {
    let state = {
        let mut slots = SLOTS.lock().unwrap();
        if let Some(s) = slots.iter_mut().find(|s| s.label == label) {
            s.interactive = on;
            s.heartbeat = sys::now_ms();
            Some((slot_hwnd(s.hwnd_raw), s.interactive, s.typing))
        } else {
            None
        }
    };
    if !DRAGGING.load(SeqCst) {
        if let Some((h, interactive, typing)) = state {
            unsafe { apply_style(h, interactive, typing) };
        }
    }
}

unsafe fn apply_style(h: HWND, interactive: bool, typing: bool) {
    if h.0.is_null() {
        return;
    }
    let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
    let through = WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0;
    let mut want = ex | WS_EX_TOOLWINDOW.0;
    if interactive { want &= !through } else { want |= through }
    if typing { want &= !WS_EX_NOACTIVATE.0 } else { want |= WS_EX_NOACTIVATE.0 }
    if want != ex {
        SetWindowLongPtrW(h, GWL_EXSTYLE, want as isize);
        if want & WS_EX_LAYERED.0 != 0 && ex & WS_EX_LAYERED.0 == 0 {
            let _ = SetLayeredWindowAttributes(h, COLORREF(0), 255, LWA_ALPHA);
        }
    }
}

unsafe fn place_hwnd(h: HWND, mon: &MonInfo) {
    let (x, y, w, hgt) = center_rect_on(mon);
    let r = sys::rect_of(h);
    if r.left != x || r.top != y || r.right - r.left != w || r.bottom - r.top != hgt {
        let _ = SetWindowPos(h, None, x, y, w, hgt, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Starts a smooth 60fps native 2D drag of the pill window under the cursor.
pub fn pill_drag_start(label: &str) {
    let vk = unsafe { if GetSystemMetrics(SM_SWAPBUTTON) != 0 { VK_RBUTTON } else { VK_LBUTTON } };
    if unsafe { GetAsyncKeyState(vk.0 as i32) >= 0 } {
        return;
    }
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt).is_err() } {
        return;
    }
    if let Some(app) = sys::APP.get() {
        let _ = cached_monitors(app, 0);
    }
    let style_state = {
        let mut slots = SLOTS.lock().unwrap();
        if let Some(s) = slots.iter_mut().find(|s| s.label == label) {
            let h = slot_hwnd(s.hwnd_raw);
            let r = sys::rect_of(h);
            let scale = unsafe { GetDpiForWindow(h).max(96) as f64 / 96.0 };
            let cur_cx = (r.left + r.right) / 2;
            let grab_dx_norm = (pt.x - cur_cx) as f64 / scale;
            let grab_dy_norm = ((pt.y - r.top) as f64 / scale).clamp(14.0, 44.0);
            s.snap_anim = None;
            s.interactive = true;
            s.heartbeat = sys::now_ms();
            s.pill_drag = Some(PillDrag {
                grab_dx_norm,
                grab_dy_norm,
                start_cursor_x: pt.x,
                start_cursor_y: pt.y,
                last_cursor_x: pt.x,
                last_cursor_y: pt.y,
                vx: 0.0,
                vy: 0.0,
                max_pull_dist: 0,
                merge_active: false,
            });
            Some((h, s.typing))
        } else {
            None
        }
    };
    if let Some((h, typing)) = style_state {
        unsafe { apply_style(h, true, typing) };
    }
}

/// Switches a window to the other monitor (button click in Single mode; disables Dual if in Dual mode).
pub fn switch_monitor(label: &str) {
    let Some(app) = sys::APP.get() else { return };
    let mons = cached_monitors(app, 0);
    if mons.len() < 2 {
        return;
    }
    if settings::flag("dualMonitors") {
        settings::apply("dualMonitors", &Value::Bool(false));
        return;
    }

    let anim_info = {
        let mut slots = SLOTS.lock().unwrap();
        if let Some(s) = slots.iter_mut().find(|s| s.label == label) {
            let cur_idx = find_mon_idx(&mons, &s.monitor_name);
            let next_idx = (cur_idx + 1) % mons.len();
            let dest = mons[next_idx].clone();
            let r = sys::rect_of(slot_hwnd(s.hwnd_raw));
            let cur_w = (r.right - r.left) as f64;
            s.monitor_name = dest.name.clone();
            s.is_primary = dest.primary;
            s.pill_drag = None;
            s.snap_anim = Some(SnapAnim {
                cx: r.left as f64 + cur_w / 2.0,
                y: r.top as f64,
                vx: if dest.x >= mons[cur_idx].x { 750.0 } else { -750.0 },
                vy: 0.0,
                to_mon: dest.name.clone(),
                step: 0,
                max_steps: 42,
                switched: true,
                merge_into_main: false,
                impact_speed: 1400.0,
            });
            Some(dest.name)
        } else {
            None
        }
    };
    if let Some(name) = anim_info {
        settings::put("monitorName", name.into());
    }
}

pub fn on_settings_changed() {
    sync_windows();
}

pub fn sync_windows() {
    if SYNCING.swap(true, SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let Some(app) = sys::APP.get().cloned() else {
            SYNCING.store(false, SeqCst);
            return;
        };
        let mons = cached_monitors(&app, 0);
        let dual = settings::flag("dualMonitors") && mons.len() >= 2;

        if dual {
            if app.get_webview_window("secondary").is_none() {
                let _ = crate::build_island_window(&app, "secondary");
            }
        } else if let Some(sec) = app.get_webview_window("secondary") {
            unregister_window("secondary");
            let _ = sec.destroy();
        }

        let updates: Vec<(String, HWND, MonInfo)> = {
            let mut slots = SLOTS.lock().unwrap();
            let mut res = Vec::new();
            for s in slots.iter_mut() {
                if s.pill_drag.is_some() || s.snap_anim.is_some() {
                    continue;
                }
                let mon = assign_monitor_for_label(&s.label, &mons, dual);
                s.monitor_name = mon.name.clone();
                s.is_primary = mon.primary;
                res.push((s.label.clone(), slot_hwnd(s.hwnd_raw), mon));
            }
            res
        };
        for (label, h, mon) in updates {
            unsafe { place_hwnd(h, &mon) };
            emit_monitor_state(&label, &mon, mons.len(), dual);
        }
        SYNCING.store(false, SeqCst);
    });
}

fn finish_merge_into_main(main_mon: &MonInfo, impact: f64) {
    unregister_window("secondary");
    if let Some(app) = sys::APP.get() {
        if let Some(sec) = app.get_webview_window("secondary") {
            if let Ok(hwnd) = sec.hwnd() {
                unsafe {
                    let _ = ShowWindow(HWND(hwnd.0), SW_HIDE);
                }
            }
            let app_c = app.clone();
            std::thread::spawn(move || {
                if let Some(w) = app_c.get_webview_window("secondary") {
                    let _ = w.destroy();
                }
            });
        }
    }
    settings::put("dualMonitors", Value::Bool(false));
    settings::put("monitorName", Value::String(main_mon.name.clone()));
    {
        let mut slots = SLOTS.lock().unwrap();
        if let Some(m) = slots.iter_mut().find(|s| s.label == "main") {
            m.monitor_name = main_mon.name.clone();
            m.is_primary = main_mon.primary;
        }
    }
    let count = sys::APP.get().map(|a| cached_monitors(a, 1500).len()).unwrap_or(2);
    emit_monitor_state("main", main_mon, count, false);
    settings::broadcast();
    sys::emit_to("main", "merge-pull", json!({ "active": false }));
    sys::emit_to("main", "merge-complete", json!({ "impact": impact }));
}

/// A text field opened or closed on the pill. While one is open the window is an ordinary focusable window.
pub fn set_typing(label: &str, on: bool, restore: bool) -> serde_json::Value {
    let mut ctx = json!({});
    let info = {
        let mut slots = SLOTS.lock().unwrap();
        slots.iter_mut().find(|s| s.label == label).map(|s| {
            let h = slot_hwnd(s.hwnd_raw);
            let old_typing = s.typing;
            if on {
                unsafe {
                    let fg = GetForegroundWindow();
                    if !is_island_hwnd(fg) {
                        s.prev_fg = fg.0 as isize;
                        ctx = context_of(fg);
                    }
                }
                s.typing = true;
                s.interactive = true;
                s.heartbeat = sys::now_ms();
                (h, true, true, 0, true)
            } else {
                let prev = if old_typing {
                    s.typing = false;
                    let p = s.prev_fg;
                    s.prev_fg = 0;
                    p
                } else {
                    0
                };
                (h, s.interactive, false, prev, old_typing)
            }
        })
    };
    if let Some((h, interactive, typing, prev_raw, changed)) = info {
        unsafe {
            if typing {
                apply_style(h, interactive, true);
                sys::force_foreground(h);
            } else if changed {
                apply_style(h, interactive, false);
                let prev = HWND(prev_raw as *mut _);
                if restore && !prev.0.is_null() && IsWindow(Some(prev)).as_bool() && sys::root_of(GetForegroundWindow()) == h {
                    let _ = SetForegroundWindow(prev);
                }
            }
        }
    }
    ctx
}

unsafe fn context_of(w: HWND) -> serde_json::Value {
    let exe = sys::exe_of_pid(sys::pid_of(w)).unwrap_or_default();
    json!({ "title": sys::title_of(w), "exe": exe.rsplit('\\').next().unwrap_or("") })
}

pub fn fg_context() -> serde_json::Value {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || is_island_hwnd(sys::root_of(fg)) {
            return json!({});
        }
        context_of(fg)
    }
}

pub fn start() {
    std::thread::spawn(|| unsafe {
        let vk = if GetSystemMetrics(SM_SWAPBUTTON) != 0 { VK_RBUTTON } else { VK_LBUTTON };
        let mut tick = 0u64;
        let mut last_mon_count = 0usize;
        let mut cached_dual = false;
        loop {
            std::thread::sleep(Duration::from_millis(16));
            tick += 1;

            let Some(app) = sys::APP.get() else { continue };
            let mons = cached_monitors(app, 1800);
            if tick % 30 == 1 {
                cached_dual = settings::flag("dualMonitors") && mons.len() >= 2;
            }
            if tick % 90 == 1 && mons.len() != last_mon_count {
                last_mon_count = mons.len();
                sync_windows();
            } else if last_mon_count == 0 {
                last_mon_count = mons.len();
                cached_dual = settings::flag("dualMonitors") && mons.len() >= 2;
            }
            let dual = cached_dual;

            let mut pt = POINT::default();
            let has_pt = GetCursorPos(&mut pt).is_ok();
            let d = GetAsyncKeyState(vk.0 as i32) < 0;
            let now = sys::now_ms();
            let main_mon_name = if dual { main_monitor_name() } else { String::new() };

            let labels: Vec<String> = SLOTS.lock().unwrap().iter().map(|s| s.label.clone()).collect();
            for label in labels {
                let Some((h, interactive, typing, is_shelf_drag)) = ({
                    let mut slots = SLOTS.lock().unwrap();
                    slots.iter_mut().find(|s| s.label == label).map(|s| {
                        let h = slot_hwnd(s.hwnd_raw);
                        if s.interactive && !s.typing && s.pill_drag.is_none() && now.saturating_sub(s.heartbeat) > 4500 {
                            s.interactive = false;
                            sys::log("page silent for 4s while holding the mouse: released it");
                        }
                        (h, s.interactive, s.typing, DRAGGING.load(SeqCst))
                    })
                }) else {
                    continue;
                };

                if h.0.is_null() || !IsWindow(Some(h)).as_bool() {
                    unregister_window(&label);
                    continue;
                }

                // 1) Free 2D slingshot drag + dual-island merge attraction
                let drag_action = {
                    let main_mon_idx = if !main_mon_name.is_empty() {
                        find_mon_idx(&mons, &main_mon_name)
                    } else {
                        mons.iter().position(|m| m.primary).unwrap_or(0)
                    };
                    let main_mon = mons[main_mon_idx].clone();
                    let mut slots = SLOTS.lock().unwrap();
                    slots.iter_mut().find(|s| s.label == label).and_then(|s| {
                        if let Some(ref mut pd) = s.pill_drag {
                            if d && has_pt {
                                let cur_mon_idx = monitor_idx_at(&mons, pt.x, pt.y);
                                let cur_mon = mons[cur_mon_idx].clone();
                                pd.max_pull_dist = pd.max_pull_dist.max((pt.x - pd.start_cursor_x).abs());
                                pd.vx = pd.vx * 0.55 + (pt.x - pd.last_cursor_x) as f64 * 0.45;
                                pd.vy = pd.vy * 0.55 + (pt.y - pd.last_cursor_y) as f64 * 0.45;
                                pd.last_cursor_x = pt.x;
                                pd.last_cursor_y = pt.y;

                                let w = (WIN_W * cur_mon.scale).round() as i32;
                                let hgt = (WIN_H * cur_mon.scale).round() as i32;
                                let raw_cx = pt.x - (pd.grab_dx_norm * cur_mon.scale).round() as i32;
                                let raw_y = pt.y - (pd.grab_dy_norm * cur_mon.scale).round() as i32;
                                let min_y = cur_mon.y - (4.0 * cur_mon.scale).round() as i32;
                                let max_y = cur_mon.y + cur_mon.h - (52.0 * cur_mon.scale).round() as i32;
                                let clamped_y = raw_y.clamp(min_y, max_y);

                                let base_y = cur_mon.y + OFFSET_Y.load(SeqCst) as i32;
                                let mon_cx = cur_mon.x + cur_mon.w / 2;
                                let dx = raw_cx - mon_cx;
                                let dy = (clamped_y - base_y).abs();
                                let well_x = (130.0 * cur_mon.scale) as i32;
                                let well_y = (120.0 * cur_mon.scale) as i32;
                                let (attracted_cx, attracted_y, magnet) = if dx.abs() < well_x && dy < well_y {
                                    let rx = dx.abs() as f64 / well_x.max(1) as f64;
                                    let ry = dy as f64 / well_y.max(1) as f64;
                                    let prox = (1.0 - (rx * rx + ry * ry).sqrt()).clamp(0.0, 1.0);
                                    let pull = prox * 0.42;
                                    let cx = raw_cx - ((dx as f64) * pull).round() as i32;
                                    let cy = clamped_y - (((clamped_y - base_y) as f64) * (pull * 0.65)).round() as i32;
                                    (cx, cy, prox)
                                } else {
                                    (raw_cx, clamped_y, 0.0)
                                };

                                // Check dual-monitor magnetic fusion (secondary -> main)
                                let mut merge_charge = 0.0;
                                let mut merge_hot = false;
                                let mut merge_dir = 0.0;
                                let mut cancel_pull = false;
                                if dual && label == "secondary" {
                                    let sec_idx = find_mon_idx(&mons, &s.monitor_name);
                                    let sm = &mons[sec_idx];
                                    let mm_cx = (main_mon.x + main_mon.w / 2) as f64;
                                    let mm_y = (main_mon.y + OFFSET_Y.load(SeqCst) as i32) as f64;
                                    let ddx = attracted_cx as f64 - mm_cx;
                                    let ddy = attracted_y as f64 - mm_y;
                                    let dist_main = (ddx * ddx + ddy * ddy).sqrt();
                                    let on_main = cur_mon_idx == main_mon_idx;
                                    let no_left = mons.len() == 2 && !mons.iter().any(|m| m.x + m.w <= sm.x + 10);
                                    let no_right = mons.len() == 2 && !mons.iter().any(|m| m.x >= sm.x + sm.w - 10);
                                    let dead_wall = pd.max_pull_dist > (120.0 * sm.scale) as i32
                                        && ((no_left && pt.x <= sm.x + 18) || (no_right && pt.x >= sm.x + sm.w - 18));
                                    merge_hot = on_main || dist_main < 460.0 * main_mon.scale || dead_wall;
                                    if merge_hot {
                                        merge_charge = if dead_wall {
                                            0.92
                                        } else {
                                            (1.0 - dist_main / (840.0 * main_mon.scale)).clamp(0.48, 1.0)
                                        };
                                    } else if dist_main < 860.0 * main_mon.scale {
                                        merge_charge = ((860.0 * main_mon.scale - dist_main) / (400.0 * main_mon.scale) * 0.48).clamp(0.0, 0.48);
                                    }
                                    merge_dir = if dead_wall {
                                        if no_right { 1.0 } else { -1.0 }
                                    } else {
                                        (ddx / (main_mon.w as f64 * 0.45)).clamp(-1.0, 1.0)
                                    };
                                    let now_pull = merge_charge > 0.03;
                                    if pd.merge_active && !now_pull {
                                        cancel_pull = true;
                                    }
                                    pd.merge_active = now_pull;
                                }

                                let tx = attracted_cx - w / 2;
                                let ty = attracted_y;
                                let vx_norm = (pd.vx / cur_mon.scale).clamp(-55.0, 55.0);
                                let vy_norm = (pd.vy / cur_mon.scale).clamp(-55.0, 55.0);
                                let stretch_y = ((clamped_y - base_y).max(0) as f64 / cur_mon.scale).min(450.0);
                                let switched = cur_mon.name != s.monitor_name;
                                Some((
                                    true,
                                    tx,
                                    ty,
                                    w,
                                    hgt,
                                    vx_norm,
                                    vy_norm,
                                    stretch_y,
                                    magnet,
                                    switched,
                                    merge_charge,
                                    merge_hot,
                                    merge_dir,
                                    cancel_pull,
                                ))
                            } else {
                                // Released! Launch real damped spring back to top-center (or fuse into main!)
                                let pd_done = s.pill_drag.take().unwrap();
                                let cur_r = sys::rect_of(h);
                                let cur_w = (cur_r.right - cur_r.left) as f64;
                                let start_cx = cur_r.left as f64 + cur_w / 2.0;
                                let start_y = cur_r.top as f64;
                                let start_idx = find_mon_idx(&mons, &s.monitor_name);

                                let mut merge_into_main = false;
                                let mut dest_idx = if dual {
                                    if label == "secondary" && has_pt {
                                        let cur_mon_idx = monitor_idx_at(&mons, pt.x, pt.y);
                                        let mm_cx = (main_mon.x + main_mon.w / 2) as f64;
                                        let mm_y = (main_mon.y + OFFSET_Y.load(SeqCst) as i32) as f64;
                                        let dist_main = ((start_cx - mm_cx).powi(2) + (start_y - mm_y).powi(2)).sqrt();
                                        let sm = &mons[start_idx];
                                        let no_left = !mons.iter().any(|m| m.x + m.w <= sm.x + 10);
                                        let no_right = !mons.iter().any(|m| m.x >= sm.x + sm.w - 10);
                                        let dead_wall = pd_done.max_pull_dist > (130.0 * sm.scale) as i32
                                            && ((no_left && pt.x <= sm.x + 14) || (no_right && pt.x >= sm.x + sm.w - 14));
                                        let fast_flick = (pd_done.vx > 20.0 && main_mon.x >= sm.x + sm.w - 10)
                                            || (pd_done.vx < -20.0 && main_mon.x + main_mon.w <= sm.x + 10);
                                        if cur_mon_idx == main_mon_idx || dist_main < 460.0 * main_mon.scale || dead_wall || fast_flick {
                                            merge_into_main = true;
                                            main_mon_idx
                                        } else {
                                            start_idx
                                        }
                                    } else {
                                        start_idx
                                    }
                                } else if has_pt {
                                    let mut idx = monitor_idx_at(&mons, pt.x, pt.y);
                                    if mons.len() == 2 && idx == start_idx {
                                        let sm = &mons[start_idx];
                                        let other_idx = 1 - start_idx;
                                        let other = &mons[other_idx];
                                        let no_left = !mons.iter().any(|m| m.x + m.w <= sm.x + 10);
                                        let no_right = !mons.iter().any(|m| m.x >= sm.x + sm.w - 10);
                                        let at_dead_wall = pd_done.max_pull_dist > (120.0 * sm.scale) as i32
                                            && ((no_left && pt.x <= sm.x + 14) || (no_right && pt.x >= sm.x + sm.w - 14));
                                        let fast_flick = (pd_done.vx > 22.0 && other.x >= sm.x + sm.w - 10)
                                            || (pd_done.vx < -22.0 && other.x + other.w <= sm.x + 10);
                                        if at_dead_wall || fast_flick {
                                            idx = other_idx;
                                        }
                                    }
                                    idx
                                } else {
                                    start_idx
                                };
                                if dest_idx >= mons.len() {
                                    dest_idx = 0;
                                }
                                let dest = &mons[dest_idx];
                                let switched = dest.name != s.monitor_name;
                                s.monitor_name = dest.name.clone();
                                s.is_primary = dest.primary;
                                let vx_sec = (pd_done.vx / 0.016).clamp(-3400.0, 3400.0);
                                let vy_sec = (pd_done.vy / 0.016).clamp(-3400.0, 3400.0);
                                s.snap_anim = Some(SnapAnim {
                                    cx: start_cx,
                                    y: start_y,
                                    vx: vx_sec,
                                    vy: vy_sec,
                                    to_mon: dest.name.clone(),
                                    step: 0,
                                    max_steps: if merge_into_main { 22 } else { 36 },
                                    switched,
                                    merge_into_main,
                                    impact_speed: (vx_sec * vx_sec + vy_sec * vy_sec).sqrt(),
                                });
                                Some((false, 0, 0, 0, 0, 0.0, 0.0, 0.0, 0.0, switched, 0.0, merge_into_main, 0.0, pd_done.merge_active))
                            }
                        } else {
                            None
                        }
                    })
                };

                if let Some((active, tx, ty, w, hgt, vx, vy, pull_y, magnet, switched, merge_charge, merge_hot, merge_dir, cancel_pull)) = drag_action {
                    if active {
                        let _ = SetWindowPos(h, Some(HWND_TOPMOST), tx, ty, w, hgt, SWP_NOACTIVATE);
                        sys::emit_to(
                            &label,
                            "drag-fx",
                            json!({
                                "active": true,
                                "vx": vx,
                                "vy": vy,
                                "pullY": pull_y,
                                "magnet": magnet,
                                "switched": switched,
                                "merge": merge_charge,
                                "mergeHot": merge_hot,
                            }),
                        );
                        if dual && label == "secondary" {
                            if merge_charge > 0.03 {
                                sys::emit_to(
                                    "main",
                                    "merge-pull",
                                    json!({ "active": true, "charge": merge_charge, "hot": merge_hot, "dir": merge_dir }),
                                );
                            } else if cancel_pull {
                                sys::emit_to("main", "merge-pull", json!({ "active": false }));
                            }
                        }
                    } else {
                        if dual && label == "secondary" && cancel_pull && !merge_hot {
                            sys::emit_to("main", "merge-pull", json!({ "active": false }));
                        }
                        if !dual && switched && label == "main" {
                            let mon_name = SLOTS
                                .lock()
                                .unwrap()
                                .iter()
                                .find(|s| s.label == label)
                                .map(|s| s.monitor_name.clone())
                                .unwrap_or_default();
                            if !mon_name.is_empty() {
                                settings::put("monitorName", mon_name.into());
                            }
                        }
                        if !merge_hot {
                            sys::emit_to(&label, "drag-fx", json!({ "active": false }));
                        }
                    }
                    continue;
                }

                // 2) Fast snappy second-order spring flight to top-center (or rocket merge into main!)
                enum AnimResult {
                    Fly(i32, i32, i32, i32, bool, f64, f64, f64),
                    Land(i32, i32, i32, i32, MonInfo, bool, f64),
                    Merged(MonInfo, f64),
                }
                let anim_res = {
                    let mut slots = SLOTS.lock().unwrap();
                    slots.iter_mut().find(|s| s.label == label).and_then(|s| {
                        let sa = s.snap_anim.as_mut()?;
                        let dest_idx = find_mon_idx(&mons, &sa.to_mon);
                        let dest = mons[dest_idx].clone();
                        let (tx, ty, tw, th) = center_rect_on(&dest);
                        let target_cx = tx as f64 + tw as f64 / 2.0;
                        let target_y = ty as f64;
                        let (k, damp) = if sa.merge_into_main { (740.0, 29.0) } else { (520.0, 25.5) };
                        let sub_dt = 0.008f64;
                        for _ in 0..2 {
                            let ax = (target_cx - sa.cx) * k - sa.vx * damp;
                            let ay = (target_y - sa.y) * k - sa.vy * damp;
                            sa.vx += ax * sub_dt;
                            sa.vy += ay * sub_dt;
                            sa.cx += sa.vx * sub_dt;
                            sa.y += sa.vy * sub_dt;
                        }
                        sa.step += 1;
                        let dist = ((target_cx - sa.cx).powi(2) + (target_y - sa.y).powi(2)).sqrt();
                        let speed = (sa.vx * sa.vx + sa.vy * sa.vy).sqrt();
                        sa.impact_speed = sa.impact_speed.max(speed);
                        let close_merge = sa.merge_into_main && ((sa.step >= 7 && dist < 34.0 * dest.scale) || sa.step >= sa.max_steps);
                        let close_snap = !sa.merge_into_main && ((dist < 1.8 * dest.scale && speed < 48.0 * dest.scale) || sa.step >= sa.max_steps);
                        let win_x = (sa.cx - tw as f64 / 2.0).round() as i32;
                        let win_y = sa.y.round() as i32;
                        if close_merge {
                            let impact = (sa.impact_speed / (2200.0 * dest.scale)).clamp(0.55, 1.25);
                            s.snap_anim = None;
                            Some(AnimResult::Merged(dest, impact))
                        } else if close_snap {
                            let impact = (sa.impact_speed / (2100.0 * dest.scale)).clamp(0.32, 1.15);
                            let switched = sa.switched;
                            s.snap_anim = None;
                            Some(AnimResult::Land(tx, ty, tw, th, dest, switched, impact))
                        } else {
                            let vx_n = (sa.vx * 0.016 / dest.scale).clamp(-58.0, 58.0);
                            let vy_n = (sa.vy * 0.016 / dest.scale).clamp(-58.0, 58.0);
                            let prog = if sa.merge_into_main {
                                (1.0 - (dist / (420.0 * dest.scale)).clamp(0.0, 1.0)).clamp(0.0, 1.0)
                            } else {
                                (1.0 - (dist / (320.0 * dest.scale)).clamp(0.0, 1.0)).clamp(0.0, 1.0)
                            };
                            Some(AnimResult::Fly(win_x, win_y, tw, th, sa.merge_into_main, prog, vx_n, vy_n))
                        }
                    })
                };

                if let Some(res) = anim_res {
                    match res {
                        AnimResult::Merged(main_mon, impact) => {
                            finish_merge_into_main(&main_mon, impact);
                        }
                        AnimResult::Land(tx, ty, tw, th, dest, switched, impact) => {
                            let _ = SetWindowPos(h, Some(HWND_TOPMOST), tx, ty, tw, th, SWP_NOACTIVATE);
                            emit_monitor_state(&label, &dest, mons.len(), dual);
                            sys::emit_to(&label, "pill-landed", json!({ "isPrimary": dest.primary, "switched": switched, "impact": impact }));
                        }
                        AnimResult::Fly(cx, cy, tw, th, is_merge, prog, vx_n, vy_n) => {
                            let _ = SetWindowPos(h, Some(HWND_TOPMOST), cx, cy, tw, th, SWP_NOACTIVATE);
                            sys::emit_to(
                                &label,
                                "drag-fx",
                                json!({
                                    "active": true,
                                    "flying": true,
                                    "absorbing": is_merge,
                                    "absorbProgress": if is_merge { prog } else { 0.0 },
                                    "vx": vx_n,
                                    "vy": vy_n,
                                    "magnet": if !is_merge { prog * 0.8 } else { 0.0 },
                                    "mergeHot": is_merge,
                                }),
                            );
                        }
                    }
                    continue;
                }

                // 3) Regular idle maintenance
                if !is_shelf_drag {
                    apply_style(h, interactive, typing);
                    if tick % 300 == 0 {
                        let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                    }
                    if tick % 120 == 0 {
                        let live_dual = settings::flag("dualMonitors") && mons.len() >= 2;
                        let mon = {
                            let mut slots = SLOTS.lock().unwrap();
                            slots.iter_mut().find(|s| s.label == label).map(|s| {
                                let idx = if live_dual && mons.len() >= 2 {
                                    if label == "secondary" {
                                        let p = mons.iter().position(|m| m.primary).unwrap_or(0);
                                        mons.iter().position(|m| !m.primary).unwrap_or((p + 1) % mons.len())
                                    } else {
                                        mons.iter().position(|m| m.primary).unwrap_or(0)
                                    }
                                } else {
                                    find_mon_idx(&mons, &s.monitor_name)
                                };
                                let m = mons[idx].clone();
                                s.monitor_name = m.name.clone();
                                s.is_primary = m.primary;
                                m
                            })
                        };
                        if let Some(m) = mon {
                            place_hwnd(h, &m);
                            emit_monitor_state(&label, &m, mons.len(), live_dual);
                        }
                    }
                }

                if !has_pt {
                    continue;
                }
                let r = sys::rect_of(h);
                let scale = GetDpiForWindow(h).max(96) as f64 / 96.0;
                let (x, y) = ((pt.x - r.left) as f64 / scale, (pt.y - r.top) as f64 / scale);
                let near = x > -80.0 && x < WIN_W + 80.0 && y > -80.0 && y < WIN_H + 80.0;

                let mut emit_cur = false;
                let mut emit_press = false;
                let mut emit_release = false;
                let mut log_stolen: Option<(HWND, String)> = None;

                {
                    let mut slots = SLOTS.lock().unwrap();
                    if let Some(s) = slots.iter_mut().find(|s| s.label == label) {
                        if (near || s.was_near) && (pt.x, pt.y) != s.last_cursor && s.last_emit.elapsed() >= Duration::from_millis(30) {
                            s.last_cursor = (pt.x, pt.y);
                            s.last_emit = Instant::now();
                            s.was_near = near;
                            emit_cur = true;
                        }
                        if d && !s.down && s.interactive && near {
                            let hit = sys::root_of(WindowFromPoint(pt));
                            if hit == h {
                                s.ours = true;
                                emit_press = true;
                            } else if now.saturating_sub(s.said_at) > 30_000 {
                                s.said_at = now;
                                let exe = sys::exe_of_pid(sys::pid_of(hit)).unwrap_or_default();
                                log_stolen = Some((
                                    h,
                                    format!("a press on the island went to another window: class={} title={:?} exe={}", sys::class_of(hit), sys::title_of(hit), exe),
                                ));
                            }
                        }
                        if !d && s.down && s.ours {
                            s.ours = false;
                            emit_release = true;
                        }
                        s.down = d;
                    }
                }

                if emit_cur {
                    sys::emit_to(&label, "cursor", json!({ "x": (x * 10.0).round() / 10.0, "y": (y * 10.0).round() / 10.0 }));
                }
                if emit_press {
                    sys::emit_to(&label, "press", json!({ "x": x, "y": y }));
                }
                if emit_release {
                    sys::emit_to(&label, "release", json!({ "x": x, "y": y }));
                }
                if let Some((wh, msg)) = log_stolen {
                    sys::log(&msg);
                    let _ = SetWindowPos(wh, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
            }
        }
    });
}

/// The quick-note shortcut (Ctrl+Alt+N). Registered on its own thread, switched with the setting.
pub static HOTKEY_WANTED: AtomicBool = AtomicBool::new(true);
pub fn start_hotkey() {
    std::thread::spawn(|| unsafe {
        let mut have = false;
        let mut failed = false;
        loop {
            let want = HOTKEY_WANTED.load(SeqCst);
            if want && !have && !failed {
                have = RegisterHotKey(None, 1, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'N' as u32).is_ok();
                if !have {
                    failed = true;
                    sys::log("Ctrl+Alt+N is taken by another program: quick-note shortcut is off");
                } else if RegisterHotKey(None, 2, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'T' as u32).is_err() {
                    sys::log("Ctrl+Alt+T is taken by another program: text-grab shortcut is off");
                }
            } else if !want && have {
                let _ = UnregisterHotKey(None, 1);
                let _ = UnregisterHotKey(None, 2);
                have = false;
            }
            if !want {
                failed = false;
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_HOTKEY && msg.wParam.0 == 2 {
                    crate::ocr::start();
                } else if msg.message == WM_HOTKEY {
                    sys::emit_to("main", "hotkey", "note");
                }
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    });
}
