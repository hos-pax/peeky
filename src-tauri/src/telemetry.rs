// System vitals (CPU, RAM, optional NVML GPU), broad Render/Build/IO Watchdog, Game Stealth classification,
// Antigravity Agent pulse watcher, under-pill Chameleon luminance/RGB sampler, and local webhook server.
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use windows::core::{s, w};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HMODULE, HWND};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetProcessIoCounters, GetProcessTimes, GetSystemTimes, OpenProcess, TerminateProcess,
    IO_COUNTERS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use crate::{hit, settings, sys};

// ---------------------------------------------------------------- optional runtime NVIDIA NVML (zero build/runtime dep)
#[repr(C)]
#[derive(Default)]
struct NvmlUtil {
    gpu: u32,
    memory: u32,
}
#[repr(C)]
#[derive(Default)]
struct NvmlMem {
    total: u64,
    free: u64,
    used: u64,
}
struct NvmlApi {
    _lib: HMODULE,
    dev: *mut c_void,
    temp_fn: Option<unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> i32>,
    util_fn: Option<unsafe extern "C" fn(*mut c_void, *mut NvmlUtil) -> i32>,
    mem_fn: Option<unsafe extern "C" fn(*mut c_void, *mut NvmlMem) -> i32>,
}
unsafe impl Send for NvmlApi {}
unsafe impl Sync for NvmlApi {}

fn nvml() -> Option<&'static NvmlApi> {
    static CELL: OnceLock<Option<NvmlApi>> = OnceLock::new();
    CELL.get_or_init(|| unsafe {
        let lib = LoadLibraryW(w!("nvml.dll")).ok()?;
        let init_p = GetProcAddress(lib, s!("nvmlInit_v2"))?;
        let init: unsafe extern "C" fn() -> i32 = std::mem::transmute(init_p);
        if init() != 0 {
            return None;
        }
        let get_h_p = GetProcAddress(lib, s!("nvmlDeviceGetHandleByIndex_v2"))?;
        let get_h: unsafe extern "C" fn(u32, *mut *mut c_void) -> i32 = std::mem::transmute(get_h_p);
        let mut dev: *mut c_void = std::ptr::null_mut();
        if get_h(0, &mut dev) != 0 || dev.is_null() {
            return None;
        }
        let temp_fn = GetProcAddress(lib, s!("nvmlDeviceGetTemperature")).map(|p| std::mem::transmute(p));
        let util_fn = GetProcAddress(lib, s!("nvmlDeviceGetUtilizationRates")).map(|p| std::mem::transmute(p));
        let mem_fn = GetProcAddress(lib, s!("nvmlDeviceGetMemoryInfo")).map(|p| std::mem::transmute(p));
        Some(NvmlApi { _lib: lib, dev, temp_fn, util_fn, mem_fn })
    })
    .as_ref()
}

fn gpu_stats() -> (Option<u32>, Option<u32>, Option<u32>, Option<u32>) {
    let Some(n) = nvml() else { return (None, None, None, None) };
    unsafe {
        let mut temp = 0u32;
        let t_ok = n.temp_fn.is_some_and(|f| f(n.dev, 0, &mut temp) == 0);
        let mut util = NvmlUtil::default();
        let u_ok = n.util_fn.is_some_and(|f| f(n.dev, &mut util) == 0);
        let mut mem = NvmlMem::default();
        let m_ok = n.mem_fn.is_some_and(|f| f(n.dev, &mut mem) == 0 && mem.total > 0);
        (
            t_ok.then_some(temp),
            u_ok.then_some(util.gpu),
            m_ok.then_some((mem.used / (1024 * 1024)) as u32),
            m_ok.then_some((mem.total / (1024 * 1024)) as u32),
        )
    }
}

// ---------------------------------------------------------------- CPU / RAM + Broad Heavy-Process Watchdog
fn ft_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

const KNOWN_CREATIVE: &[&str] = &[
    "afterfx", "aerender", "adobe premiere pro", "adobe media encoder", "blender",
    "resolve", "cinema 4d", "ffmpeg", "handbrakecli", "handbrake", "topaz", "davinci",
];
const KNOWN_DEV_AI: &[&str] = &[
    "cargo", "rustc", "cl", "link", "lld", "node", "vite", "esbuild", "next",
    "python", "ollama", "llama-server", "docker", "msbuild",
];
const KNOWN_IO: &[&str] = &["7z", "7zg", "winrar", "yt-dlp", "aria2c"];

#[derive(Clone)]
struct ProcSample {
    cpu_ticks: u64,
    io_bytes: u64,
}
struct ActiveHeavy {
    name: String,
    kind: &'static str,
    started: Instant,
    last_hot: Instant,
    notified: bool,
}

pub struct TelemetryState {
    prev_sys: Option<(u64, u64, u64)>, // idle, kernel, user
    prev_procs: HashMap<u32, ProcSample>,
    prev_tick: Instant,
    active_heavy: HashMap<u32, ActiveHeavy>,
    agent_working_since: Option<Instant>,
    last_agent_state: String,
    last_agent_seq: u64,
    last_ambient: Option<[u8; 3]>,
}
impl TelemetryState {
    pub fn new() -> Self {
        Self {
            prev_sys: None,
            prev_procs: HashMap::new(),
            prev_tick: Instant::now(),
            active_heavy: HashMap::new(),
            agent_working_since: None,
            last_agent_state: "idle".into(),
            last_agent_seq: 0,
            last_ambient: None,
        }
    }
}

static LATEST_SNAPSHOT: OnceLock<Mutex<Value>> = OnceLock::new();
pub fn latest_snapshot() -> Value {
    LATEST_SNAPSHOT
        .get_or_init(|| Mutex::new(json!({})))
        .lock()
        .unwrap()
        .clone()
}

pub fn classify_fullscreen_is_game(me: u32, island_h: HWND) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || sys::pid_of(fg) == me {
            return false;
        }
        let fg_mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        let isl_mon = MonitorFromWindow(island_h, MONITOR_DEFAULTTONEAREST);
        if fg_mon.0 != isl_mon.0 {
            return false;
        }
        let c = sys::class_of(fg).to_ascii_lowercase();
        if matches!(c.as_str(), "progman" | "workerw" | "shell_traywnd" | "applicationframewindow") {
            return false;
        }
        let exe = sys::exe_of_pid(sys::pid_of(fg)).unwrap_or_default().to_ascii_lowercase();
        if exe.is_empty() {
            return false;
        }
        let stem = Path::new(&exe).file_stem().and_then(|s| s.to_str()).unwrap_or("");
        // Browsers, video players, IDEs, office tools fullscreen -> not treated as games
        if matches!(
            stem,
            "chrome" | "msedge" | "brave" | "opera" | "firefox" | "vivaldi" | "yandex" |
            "vlc" | "mpv" | "mpc-hc64" | "mpc-hc" | "mpc-be64" | "potplayermini64" | "potplayer" | "kmplayer" |
            "code" | "antigravity" | "devenv" | "explorer" | "wt" | "windowsterminal" | "cmd" | "powershell" | "pwsh" |
            "telegram" | "discord" | "afterfx" | "adobe premiere pro" | "photoshop"
        ) {
            return false;
        }
        true
    }
}

fn cpu_total_pct(state: &mut TelemetryState) -> u32 {
    unsafe {
        let (mut idle, mut kern, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        if GetSystemTimes(Some(&mut idle), Some(&mut kern), Some(&mut user)).is_err() {
            return 0;
        }
        let now = (ft_u64(idle), ft_u64(kern), ft_u64(user));
        let pct = if let Some(prev) = state.prev_sys {
            let di = now.0.saturating_sub(prev.0);
            let dk = now.1.saturating_sub(prev.1);
            let du = now.2.saturating_sub(prev.2);
            let tot = dk + du;
            if tot > 0 { ((tot.saturating_sub(di)) as f64 * 100.0 / tot as f64).round() as u32 } else { 0 }
        } else {
            0
        };
        state.prev_sys = Some(now);
        pct.min(100)
    }
}

fn ram_stats() -> (u32, f64, f64) {
    unsafe {
        let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        if GlobalMemoryStatusEx(&mut m).is_ok() && m.ullTotalPhys > 0 {
            let tot_gb = (m.ullTotalPhys as f64) / (1024.0 * 1024.0 * 1024.0);
            let used_gb = ((m.ullTotalPhys - m.ullAvailPhys) as f64) / (1024.0 * 1024.0 * 1024.0);
            return (m.dwMemoryLoad, (used_gb * 10.0).round() / 10.0, (tot_gb * 10.0).round() / 10.0);
        }
        (0, 0.0, 0.0)
    }
}

fn sample_heavy_processes(state: &mut TelemetryState) -> (Vec<Value>, Vec<(String, u64)>) {
    let elapsed_ticks = state.prev_tick.elapsed().as_secs_f64().max(0.4) * 10_000_000.0;
    state.prev_tick = Instant::now();
    let logical_cpus = std::thread::available_parallelism().map(|n| n.get() as f64).unwrap_or(8.0);
    let me = std::process::id();

    let mut next_samples: HashMap<u32, ProcSample> = HashMap::new();
    let mut rows: Vec<(f64, Value)> = Vec::new();
    let mut finished: Vec<(String, u64)> = Vec::new();

    unsafe {
        if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            let mut pe = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            if Process32FirstW(snap, &mut pe).is_ok() {
                loop {
                    let pid = pe.th32ProcessID;
                    if pid > 4 && pid != me {
                        let raw_name = sys::from_wide(&pe.szExeFile);
                        let stem = raw_name.trim_end_matches(".exe").trim_end_matches(".EXE").to_ascii_lowercase();
                        let kind = if KNOWN_CREATIVE.iter().any(|k| stem.contains(k)) {
                            Some("creative")
                        } else if KNOWN_DEV_AI.iter().any(|k| stem == *k || stem.starts_with(k)) {
                            Some("dev")
                        } else if KNOWN_IO.iter().any(|k| stem.contains(k)) {
                            Some("io")
                        } else {
                            None
                        };
                        if let Ok(hp) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                            let (mut cr, mut ex, mut kt, mut ut) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
                            let ok_t = GetProcessTimes(hp, &mut cr, &mut ex, &mut kt, &mut ut).is_ok();
                            let mut io = IO_COUNTERS::default();
                            let ok_io = GetProcessIoCounters(hp, &mut io).is_ok();
                            let _ = CloseHandle(hp);
                            if ok_t {
                                let ticks = ft_u64(kt) + ft_u64(ut);
                                let io_b = if ok_io { io.ReadTransferCount + io.WriteTransferCount } else { 0 };
                                if let Some(prev) = state.prev_procs.get(&pid) {
                                    let dt = ticks.saturating_sub(prev.cpu_ticks) as f64;
                                    let cpu_pct = (dt * 100.0 / (elapsed_ticks * logical_cpus)).min(100.0);
                                    let io_mb_s = (io_b.saturating_sub(prev.io_bytes) as f64) / (1024.0 * 1024.0) / (elapsed_ticks / 10_000_000.0);
                                    let is_hot = cpu_pct >= 18.0 || (kind.is_some() && (cpu_pct >= 7.5 || io_mb_s >= 15.0));
                                    let watch = kind.is_some() || cpu_pct >= 26.0 || io_mb_s >= 35.0;
                                    if watch {
                                        let label = raw_name.trim_end_matches(".exe").to_string();
                                        if is_hot {
                                            let ent = state.active_heavy.entry(pid).or_insert_with(|| ActiveHeavy {
                                                name: label.clone(),
                                                kind: kind.unwrap_or("heavy"),
                                                started: Instant::now(),
                                                last_hot: Instant::now(),
                                                notified: false,
                                            });
                                            ent.last_hot = Instant::now();
                                            if ent.started.elapsed() >= Duration::from_secs(6) {
                                                ent.notified = true;
                                            }
                                        }
                                        if cpu_pct >= 4.0 || io_mb_s >= 4.0 {
                                            rows.push((
                                                cpu_pct + io_mb_s * 0.5,
                                                json!({
                                                    "pid": pid,
                                                    "name": label,
                                                    "kind": kind.unwrap_or("heavy"),
                                                    "cpu": (cpu_pct * 10.0).round() / 10.0,
                                                    "io": (io_mb_s * 10.0).round() / 10.0,
                                                    "hot": is_hot,
                                                }),
                                            ));
                                        }
                                    }
                                }
                                next_samples.insert(pid, ProcSample { cpu_ticks: ticks, io_bytes: io_b });
                            }
                        }
                    }
                    if Process32NextW(snap, &mut pe).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
    }
    state.prev_procs = next_samples;

    let mut done_pids = Vec::new();
    for (pid, ah) in &state.active_heavy {
        let dead = !state.prev_procs.contains_key(pid);
        let cooled = ah.last_hot.elapsed() > Duration::from_secs(4);
        if dead || cooled {
            if ah.notified && settings::flag("watchdogAlerts") {
                finished.push((ah.name.clone(), ah.started.elapsed().as_secs()));
            }
            done_pids.push(*pid);
        }
    }
    for pid in done_pids {
        state.active_heavy.remove(&pid);
    }

    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let jobs: Vec<Value> = rows.into_iter().take(6).map(|(_, v)| v).collect();
    (jobs, finished)
}

// ---------------------------------------------------------------- Antigravity Agent Pulse (filesystem-safe)
fn poll_antigravity(state: &mut TelemetryState) -> (String, Option<String>, Option<u64>, bool) {
    if !settings::flag("agentPulse") {
        return ("off".into(), None, None, false);
    }
    let Some(profile) = std::env::var_os("USERPROFILE") else {
        return ("idle".into(), None, None, false);
    };
    let brain = PathBuf::from(profile).join(".gemini").join("antigravity").join("brain");
    let Ok(rd) = std::fs::read_dir(&brain) else {
        return ("idle".into(), None, None, false);
    };
    let mut best_log: Option<(SystemTime, PathBuf, String)> = None;
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.len() < 12 {
            continue;
        }
        let log_p = ent.path().join(".system_generated").join("logs").join("transcript.jsonl");
        if let Ok(meta) = std::fs::metadata(&log_p) {
            if let Ok(mt) = meta.modified() {
                if best_log.as_ref().is_none_or(|(bt, _, _)| mt > *bt) {
                    best_log = Some((mt, log_p, name));
                }
            }
        }
    }
    let Some((mt, log_p, conv_id)) = best_log else {
        return ("idle".into(), None, None, false);
    };
    let age_s = SystemTime::now().duration_since(mt).map(|d| d.as_secs()).unwrap_or(999);
    // Read strictly tail 4KB of active transcript.jsonl
    let mut working = age_s <= 4;
    let mut step_idx: u64 = 0;
    if let Ok(mut f) = std::fs::File::open(&log_p) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let seek = len.saturating_sub(4096);
        if f.seek(SeekFrom::Start(seek)).is_ok() {
            let mut s = String::new();
            if f.read_to_string(&mut s).is_ok() {
                if let Some(last_line) = s.lines().rfind(|l| !l.trim().is_empty()) {
                    if let Ok(v) = serde_json::from_str::<Value>(last_line) {
                        step_idx = v.get("step_index").and_then(|x| x.as_u64()).unwrap_or(0);
                        let src = v.get("source").and_then(|x| x.as_str()).unwrap_or("");
                        let st = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
                        if st != "DONE" || (src == "USER" && age_s < 45) {
                            working = true;
                        }
                    }
                }
            }
        }
    }
    let mut just_finished = false;
    let cur_state = if working {
        if state.agent_working_since.is_none() {
            state.agent_working_since = Some(Instant::now());
        }
        "working".to_string()
    } else {
        if let Some(since) = state.agent_working_since.take() {
            if since.elapsed() >= Duration::from_secs(4) && step_idx != state.last_agent_seq {
                just_finished = true;
            }
        }
        state.last_agent_seq = step_idx;
        if age_s < 180 { "ready".to_string() } else { "idle".to_string() }
    };
    state.last_agent_state = cur_state.clone();
    (cur_state, Some(conv_id), Some(step_idx), just_finished)
}

// ---------------------------------------------------------------- Adaptive Chameleon Screen Edge Sampler
fn sample_ambient(state: &mut TelemetryState) {
    if !settings::flag("chameleonGlass") {
        return;
    }
    let Some((cx, top_y, _w, _h, visible)) = hit::main_bounds_for_ambient() else { return };
    if !visible {
        return;
    }
    unsafe {
        let sdc = GetDC(None);
        if sdc.0.is_null() {
            return;
        }
        let mdc = CreateCompatibleDC(Some(sdc));
        if mdc.0.is_null() {
            let _ = ReleaseDC(None, sdc);
            return;
        }
        // Sample two tiny 8x2 strips just outside/below top bezel (10px wide away from glass center or below y+46)
        let w = 16i32;
        let h = 2i32;
        let bmp = CreateCompatibleBitmap(sdc, w, h);
        if bmp.0.is_null() {
            let _ = DeleteDC(mdc);
            let _ = ReleaseDC(None, sdc);
            return;
        }
        let old = SelectObject(mdc, HGDIOBJ(bmp.0));
        // Sample cleanly right below the compact pill (top_y + 48) so it captures the true window under the pill
        let sx = (cx - w / 2).max(0);
        let sy = (top_y + 48).max(0);
        let ok = BitBlt(mdc, 0, 0, w, h, Some(sdc), sx, sy, SRCCOPY).is_ok();
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let got = if ok { GetDIBits(mdc, bmp, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS) } else { 0 };
        SelectObject(mdc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mdc);
        let _ = ReleaseDC(None, sdc);
        if got <= 0 {
            return;
        }
        let (mut rs, mut gs, mut bs, mut n) = (0u64, 0u64, 0u64, 0u64);
        for p in px.chunks(4) {
            bs += p[0] as u64;
            gs += p[1] as u64;
            rs += p[2] as u64;
            n += 1;
        }
        if n == 0 {
            return;
        }
        let rgb = [(rs / n) as u8, (gs / n) as u8, (bs / n) as u8];
        let changed = state.last_ambient.is_none_or(|p| {
            let dr = (rgb[0] as i32 - p[0] as i32).abs();
            let dg = (rgb[1] as i32 - p[1] as i32).abs();
            let db = (rgb[2] as i32 - p[2] as i32).abs();
            dr + dg + db >= 18
        });
        if changed {
            state.last_ambient = Some(rgb);
            let luma = (0.2126 * rgb[0] as f64 + 0.7152 * rgb[1] as f64 + 0.0722 * rgb[2] as f64) / 255.0;
            sys::helper(json!({
                "t": "ambient",
                "rgb": rgb,
                "luma": (luma * 100.0).round() / 100.0,
            }));
        }
    }
}

pub fn tick(state: &mut TelemetryState) {
    let cpu = cpu_total_pct(state);
    let (ram_pct, ram_used_gb, ram_tot_gb) = ram_stats();
    let (gpu_temp, gpu_util, vram_used_mb, vram_tot_mb) = gpu_stats();
    let (jobs, finished) = sample_heavy_processes(state);
    let (agent_state, agent_conv, agent_step, agent_done) = poll_antigravity(state);

    let payload = json!({
        "t": "telemetry",
        "cpu": cpu,
        "ramPct": ram_pct,
        "ramUsed": ram_used_gb,
        "ramTotal": ram_tot_gb,
        "gpuTemp": gpu_temp,
        "gpuUtil": gpu_util,
        "vramUsedMb": vram_used_mb,
        "vramTotalMb": vram_tot_mb,
        "jobs": jobs,
        "agent": {
            "state": agent_state,
            "conv": agent_conv,
            "step": agent_step,
            "justFinished": agent_done,
        }
    });
    *LATEST_SNAPSHOT.get_or_init(|| Mutex::new(json!({}))).lock().unwrap() = payload.clone();
    sys::helper(payload);

    for (name, secs) in finished {
        sys::helper(json!({ "t": "heavy-done", "name": name, "secs": secs }));
    }
    if agent_done {
        sys::helper(json!({ "t": "agent-done", "step": agent_step }));
    }
}

pub fn tick_ambient(state: &mut TelemetryState) {
    sample_ambient(state);
}

// ---------------------------------------------------------------- Cache scanner & cleaner (on demand ONLY)
fn dir_size_limited(dir: &Path, depth: u8, files_budget: &mut usize) -> (u64, usize) {
    if depth == 0 || *files_budget == 0 {
        return (0, 0);
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return (0, 0) };
    let mut bytes = 0u64;
    let mut count = 0usize;
    for ent in rd.flatten() {
        if *files_budget == 0 {
            break;
        }
        *files_budget -= 1;
        let Ok(m) = ent.metadata() else { continue };
        if m.is_file() {
            bytes += m.len();
            count += 1;
        } else if m.is_dir() {
            let (b, c) = dir_size_limited(&ent.path(), depth - 1, files_budget);
            bytes += b;
            count += c;
        }
    }
    (bytes, count)
}

pub fn scan_caches() -> Value {
    let mut targets: Vec<(&str, PathBuf)> = Vec::new();
    let temp = std::env::temp_dir();
    targets.push(("Windows Temp", temp));
    if let Some(app) = std::env::var_os("APPDATA") {
        let ac = PathBuf::from(&app).join("Adobe").join("Common").join("Media Cache Files");
        if ac.exists() {
            targets.push(("Adobe Media Cache", ac));
        }
        let ac2 = PathBuf::from(&app).join("Adobe").join("Common").join("Media Cache");
        if ac2.exists() {
            targets.push(("Adobe Peak Cache", ac2));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let npm = PathBuf::from(&local).join("npm-cache");
        if npm.exists() {
            targets.push(("npm Cache", npm));
        }
    }
    let mut out = Vec::new();
    for (name, path) in targets {
        let mut budget = 4500usize;
        let (bytes, files) = dir_size_limited(&path, 3, &mut budget);
        out.push(json!({
            "name": name,
            "path": path.to_string_lossy(),
            "mb": (bytes as f64 / (1024.0 * 1024.0)).round() as u64,
            "files": files,
        }));
    }
    Value::Array(out)
}

pub fn clean_temp_safe() -> Value {
    let temp = std::env::temp_dir();
    let mut freed: u64 = 0;
    let mut removed: u64 = 0;
    let cutoff = SystemTime::now().checked_sub(Duration::from_secs(3600)).unwrap_or(UNIX_EPOCH);
    if let Ok(rd) = std::fs::read_dir(&temp) {
        for ent in rd.flatten().take(3000) {
            let p = ent.path();
            let name = ent.file_name().to_string_lossy().to_string();
            if name.eq_ignore_ascii_case("PeekyShelf") {
                continue;
            }
            let Ok(m) = ent.metadata() else { continue };
            if m.modified().is_ok_and(|mt| mt > cutoff) {
                continue;
            }
            if m.is_file() {
                let sz = m.len();
                if std::fs::remove_file(&p).is_ok() {
                    freed += sz;
                    removed += 1;
                }
            } else if m.is_dir() {
                let mut b = 800;
                let (sz, _) = dir_size_limited(&p, 2, &mut b);
                if std::fs::remove_dir_all(&p).is_ok() {
                    freed += sz;
                    removed += 1;
                }
            }
        }
    }
    json!({
        "ok": true,
        "freedMb": (freed as f64 / (1024.0 * 1024.0) * 10.0).round() / 10.0,
        "removed": removed,
    })
}

pub fn kill_pid(pid: u32) -> bool {
    if pid <= 4 || pid == std::process::id() {
        return false;
    }
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) else { return false };
        let ok = TerminateProcess(h, 1).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

// ---------------------------------------------------------------- Localhost Micro-Webhook (127.0.0.1:49152/peeky)
static WEBHOOK_STARTED: AtomicBool = AtomicBool::new(false);
pub fn start_webhook() {
    if WEBHOOK_STARTED.swap(true, SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let Ok(listener) = TcpListener::bind("127.0.0.1:49152") else {
            return;
        };
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
            let req_body = {
                let mut reader = BufReader::new(&stream);
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).is_err() {
                    continue;
                }
                let mut content_len = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            content_len = v.trim().parse().unwrap_or(0).min(16_384);
                        }
                    }
                }
                if req_line.starts_with("POST /peeky") && content_len > 0 {
                    let mut buf = vec![0u8; content_len];
                    if reader.read_exact(&mut buf).is_ok() {
                        serde_json::from_slice::<Value>(&buf).ok()
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(v) = req_body {
                sys::helper(json!({ "t": "webhook", "data": v }));
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 11\r\n\r\n{\"ok\":true}");
        }
    });
}
