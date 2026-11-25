// Mouse, focus, multi-monitor placement, and 60fps magnetic drag handling for island windows.
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering::SeqCst};
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
    start_cursor_x: i32,
    start_cursor_y: i32,
    last_cursor_x: i32,
    vx: f64,
    max_pull_dist: i32,
}

#[derive(Clone, Debug)]
struct SnapAnim {
    from_x: i32,
    from_y: i32,
    to_mon: String,
    step: u8,
    steps: u8,
    switched: bool,
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
    // Nearest monitor center horizontally/vertically if slightly outside corners
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
    let mons = sys::APP.get().map(collect_monitors).unwrap_or_default();
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
    let mons = sys::APP.get().map(collect_monitors).unwrap_or_default();
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

/// Brings the wanted state onto the real window. Cheap when nothing changed.
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

/// Top centre of the assigned monitor, at the exact scale that monitor asks for.
unsafe fn place_hwnd(h: HWND, mon: &MonInfo) {
    let (x, y, w, hgt) = center_rect_on(mon);
    let r = sys::rect_of(h);
    if r.left != x || r.top != y || r.right - r.left != w || r.bottom - r.top != hgt {
        let _ = SetWindowPos(h, None, x, y, w, hgt, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// Starts a smooth 60fps native drag of the pill window under the cursor.
pub fn pill_drag_start(label: &str) {
    let vk = unsafe { if GetSystemMetrics(SM_SWAPBUTTON) != 0 { VK_RBUTTON } else { VK_LBUTTON } };
    if unsafe { GetAsyncKeyState(vk.0 as i32) >= 0 } {
        return;
    }
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt).is_err() } {
        return;
    }
    let style_state = {
        let mut slots = SLOTS.lock().unwrap();
        if let Some(s) = slots.iter_mut().find(|s| s.label == label) {
            let h = slot_hwnd(s.hwnd_raw);
            let r = sys::rect_of(h);
            let scale = unsafe { GetDpiForWindow(h).max(96) as f64 / 96.0 };
            let cur_cx = (r.left + r.right) / 2;
            let grab_dx_norm = (pt.x - cur_cx) as f64 / scale;
            s.snap_anim = None;
            s.interactive = true;
            s.heartbeat = sys::now_ms();
            s.pill_drag = Some(PillDrag {
                grab_dx_norm,
                start_cursor_x: pt.x,
                start_cursor_y: pt.y,
                last_cursor_x: pt.x,
                vx: 0.0,
                max_pull_dist: 0,
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
    let mons = collect_monitors(app);
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
            s.monitor_name = dest.name.clone();
            s.is_primary = dest.primary;
            s.pill_drag = None;
            s.snap_anim = Some(SnapAnim {
                from_x: r.left,
                from_y: r.top,
                to_mon: dest.name.clone(),
                step: 0,
                steps: 14,
                switched: true,
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
        let mons = collect_monitors(&app);
        let dual = settings::flag("dualMonitors") && mons.len() >= 2;

        if dual {
            if app.get_webview_window("secondary").is_none() {
                let _ = crate::build_island_window(&app, "secondary");
            }
        } else if let Some(sec) = app.get_webview_window("secondary") {
            unregister_window("secondary");
            let _ = sec.destroy();
        }

        // Re-align remaining windows cleanly
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

/// What the user is looking at (window title and program): used to suggest a name when a password is saved.
unsafe fn context_of(w: HWND) -> serde_json::Value {
    let exe = sys::exe_of_pid(sys::pid_of(w)).unwrap_or_default();
    json!({ "title": sys::title_of(w), "exe": exe.rsplit('\\').next().unwrap_or("") })
}
/// The same, for the window in front right now, without taking the focus from it ({} when an island itself is in front).
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
        loop {
            std::thread::sleep(Duration::from_millis(16));
            tick += 1;

            let Some(app) = sys::APP.get() else { continue };
            let mons = collect_monitors(app);
            let dual = settings::flag("dualMonitors") && mons.len() >= 2;
            if tick % 120 == 0 && mons.len() != last_mon_count {
                last_mon_count = mons.len();
                sync_windows();
            } else if last_mon_count == 0 {
                last_mon_count = mons.len();
            }

            let mut pt = POINT::default();
            let has_pt = GetCursorPos(&mut pt).is_ok();
            let d = GetAsyncKeyState(vk.0 as i32) < 0;
            let now = sys::now_ms();

            // Gather current window labels so we process outside locks
            let labels: Vec<String> = SLOTS.lock().unwrap().iter().map(|s| s.label.clone()).collect();
            for label in labels {
                let Some((h, interactive, typing, is_shelf_drag)) = ({
                    let mut slots = SLOTS.lock().unwrap();
                    slots.iter_mut().find(|s| s.label == label).map(|s| {
                        let h = slot_hwnd(s.hwnd_raw);
                        if s.interactive && !s.typing && s.pill_drag.is_none() && now.saturating_sub(s.heartbeat) > 4000 {
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

                // 1) Handle active native pill drag (60fps magnetic cursor tracking)
                let drag_action = {
                    let mut slots = SLOTS.lock().unwrap();
                    slots.iter_mut().find(|s| s.label == label).and_then(|s| {
                        if let Some(ref mut pd) = s.pill_drag {
                            if d && has_pt {
                                let cur_mon_idx = monitor_idx_at(&mons, pt.x, pt.y);
                                let cur_mon = mons[cur_mon_idx].clone();
                                pd.max_pull_dist = pd.max_pull_dist.max((pt.x - pd.start_cursor_x).abs());
                                let inst_vx = (pt.x - pd.last_cursor_x) as f64;
                                pd.vx = pd.vx * 0.55 + inst_vx * 0.45;
                                pd.last_cursor_x = pt.x;

                                let w = (WIN_W * cur_mon.scale).round() as i32;
                                let hgt = (WIN_H * cur_mon.scale).round() as i32;
                                let raw_cx = pt.x - (pd.grab_dx_norm * cur_mon.scale).round() as i32;
                                let mon_cx = cur_mon.x + cur_mon.w / 2;
                                let dist = raw_cx - mon_cx;
                                let well = (150.0 * cur_mon.scale) as i32;
                                let (attracted_cx, magnet) = if dist.abs() < well {
                                    let ratio = dist.abs() as f64 / well.max(1) as f64;
                                    let curve = ratio * (0.45 + 0.55 * ratio);
                                    let off = (dist.signum() as f64 * (well as f64 * curve)).round() as i32;
                                    (mon_cx + off, (1.0 - ratio).clamp(0.0, 1.0))
                                } else {
                                    (raw_cx, 0.0)
                                };

                                let base_y = cur_mon.y + OFFSET_Y.load(SeqCst) as i32;
                                let raw_dy = (pt.y - pd.start_cursor_y).max(0);
                                let pull_y = if raw_dy <= 40 {
                                    (raw_dy as f64 * 0.2).round() as i32
                                } else {
                                    8 + (((raw_dy - 40) as f64).sqrt() * 2.8).round().min(28.0) as i32
                                };
                                let tx = attracted_cx - w / 2;
                                let ty = base_y + pull_y;
                                let vx_norm = (pd.vx / cur_mon.scale).clamp(-45.0, 45.0);
                                let py_norm = pull_y as f64 / cur_mon.scale;
                                let switched = cur_mon.name != s.monitor_name;
                                Some((true, tx, ty, w, hgt, vx_norm, py_norm, magnet, switched))
                            } else {
                                // Released! Pick destination monitor cleanly.
                                let pd_done = s.pill_drag.take().unwrap();
                                let cur_r = sys::rect_of(h);
                                let start_idx = find_mon_idx(&mons, &s.monitor_name);
                                let mut dest_idx = if dual {
                                    start_idx
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
                                        let fast_flick = (pd_done.vx > 24.0 && other.x >= sm.x + sm.w - 10)
                                            || (pd_done.vx < -24.0 && other.x + other.w <= sm.x + 10);
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
                                s.snap_anim = Some(SnapAnim {
                                    from_x: cur_r.left,
                                    from_y: cur_r.top,
                                    to_mon: dest.name.clone(),
                                    step: 0,
                                    steps: 14,
                                    switched,
                                });
                                Some((false, 0, 0, 0, 0, 0.0, 0.0, 0.0, switched))
                            }
                        } else {
                            None
                        }
                    })
                };

                if let Some((active, tx, ty, w, hgt, vx, py, magnet, switched)) = drag_action {
                    if active {
                        let _ = SetWindowPos(h, Some(HWND_TOPMOST), tx, ty, w, hgt, SWP_NOACTIVATE);
                        sys::emit_to(
                            &label,
                            "drag-fx",
                            json!({ "active": true, "vx": vx, "pullY": py, "magnet": magnet, "switched": switched }),
                        );
                    } else {
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
                        sys::emit_to(&label, "drag-fx", json!({ "active": false }));
                    }
                    continue;
                }

                // 2) Handle elastic glide-to-center SnapAnim
                let anim_step = {
                    let mut slots = SLOTS.lock().unwrap();
                    slots.iter_mut().find(|s| s.label == label).and_then(|s| {
                        if let Some(ref mut sa) = s.snap_anim {
                            sa.step += 1;
                            let dest_idx = find_mon_idx(&mons, &sa.to_mon);
                            let dest = mons[dest_idx].clone();
                            let (tx, ty, tw, th) = center_rect_on(&dest);
                            let t = (sa.step as f64 / sa.steps as f64).min(1.0);
                            let ease = 1.0 - (1.0 - t).powi(3);
                            let cx = sa.from_x + ((tx - sa.from_x) as f64 * ease).round() as i32;
                            let cy = sa.from_y + ((ty - sa.from_y) as f64 * ease).round() as i32;
                            let done = sa.step >= sa.steps;
                            let switched = sa.switched;
                            if done {
                                s.snap_anim = None;
                            }
                            Some((cx, cy, tx, ty, tw, th, done, dest, switched))
                        } else {
                            None
                        }
                    })
                };

                if let Some((cx, cy, tx, ty, tw, th, done, dest, switched)) = anim_step {
                    if done {
                        let _ = SetWindowPos(h, Some(HWND_TOPMOST), tx, ty, tw, th, SWP_NOACTIVATE);
                        emit_monitor_state(&label, &dest, mons.len(), dual);
                        sys::emit_to(&label, "pill-landed", json!({ "isPrimary": dest.primary, "switched": switched }));
                    } else {
                        let _ = SetWindowPos(h, Some(HWND_TOPMOST), cx, cy, tw, th, SWP_NOACTIVATE);
                    }
                    continue;
                }

                // 3) Regular idle maintenance (styles, topmost, exact DPI placement, cursor/click relay)
                if !is_shelf_drag {
                    apply_style(h, interactive, typing);
                    if tick % 300 == 0 {
                        let _ = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                    }
                    if tick % 120 == 0 {
                        let mon = {
                            let mut slots = SLOTS.lock().unwrap();
                            slots.iter_mut().find(|s| s.label == label).map(|s| {
                                let idx = if dual && mons.len() >= 2 {
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
                            emit_monitor_state(&label, &m, mons.len(), dual);
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
