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
use windows::core::{s, w, BOOL};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HMODULE, HWND, LPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetProcessIoCounters, GetProcessTimes, GetSystemTimes, OpenProcess, TerminateProcess,
    IO_COUNTERS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, IsIconic, IsWindowVisible, ShowWindow, SW_RESTORE,
};

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
    agent_had_work: bool,
    agent_settled_polls: u8,
    last_agent_conv: String,
    last_notified_step: u64,
    last_agent_preview: String,
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
            agent_had_work: false,
            agent_settled_polls: 0,
            last_agent_conv: String::new(),
            last_notified_step: 0,
            last_agent_preview: String::new(),
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

// ---------------------------------------------------------------- Antigravity Agent Pulse (turn-aware, final-reply debounced + AGY Hub)
fn strip_html_clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(600));
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                if in_tag {
                    in_tag = false;
                    out.push(' ');
                } else {
                    out.push('>');
                }
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
        if out.len() > 1400 {
            break;
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_last_code(raw: &str) -> Option<String> {
    let mut best: Option<&str> = None;
    let mut rest = raw;
    while let Some(i) = rest.find("```") {
        let after = &rest[i + 3..];
        let body_start = after.find('\n').map(|n| n + 1).unwrap_or(0);
        if let Some(end) = after[body_start..].find("```") {
            let block = after[body_start..body_start + end].trim();
            if !block.is_empty() {
                best = Some(block);
            }
            rest = &after[body_start + end + 3..];
        } else {
            break;
        }
    }
    best.map(|b| b.to_string())
}

fn read_tail_str(path: &Path, max_bytes: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let seek = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(seek)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if seek > 0 {
        if let Some(pos) = s.find('\n') {
            s = s[pos + 1..].to_string();
        }
    }
    Some(s)
}

#[derive(Default)]
struct ConvAnalysis {
    state: String,
    step: u64,
    turn_steps: usize,
    pending_bg: usize,
    is_final_planner: bool,
    preview: String,
    full_reply: String,
    last_code: String,
    age_s: u64,
}

fn analyze_conversation(conv_dir: &Path, want_full: bool) -> Option<ConvAnalysis> {
    let log_p = conv_dir.join(".system_generated").join("logs").join("transcript.jsonl");
    let meta = std::fs::metadata(&log_p).ok()?;
    let mt = meta.modified().ok()?;
    let age_s = SystemTime::now().duration_since(mt).map(|d| d.as_secs()).unwrap_or(999);
    let tail = read_tail_str(&log_p, 48 * 1024)?;
    let vals: Vec<Value> = tail
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    if vals.is_empty() {
        return None;
    }

    // Slice from latest USER_INPUT forward so earlier finished turns never bleed in
    let turn_start = vals
        .iter()
        .rposition(|v| v.get("type").and_then(|x| x.as_str()) == Some("USER_INPUT"))
        .unwrap_or(0);
    let turn = &vals[turn_start..];
    let mut started_tasks: Vec<String> = Vec::new();
    let mut finished_tasks: Vec<String> = Vec::new();
    let mut turn_tools = 0usize;
    let mut last_planner_idx: Option<u64> = None;
    let mut last_planner_raw = String::new();
    let mut last_planner_trunc = false;

    for v in turn {
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let st = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
        let c = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
        if typ == "GENERIC" {
            turn_tools += 1;
            if let Some(pos) = c.find("task id:") {
                let id_part = c[pos + 8..].split_whitespace().next().unwrap_or("").trim_matches('"').to_string();
                if !id_part.is_empty() && !started_tasks.contains(&id_part) {
                    started_tasks.push(id_part);
                }
            } else if st == "RUNNING" {
                // Unknown foreground running generic tool without explicit task id
                started_tasks.push(format!("step-{}", v.get("step_index").and_then(|x| x.as_u64()).unwrap_or(0)));
            }
        } else if typ == "SYSTEM_MESSAGE" {
            if c.contains("finished with result") {
                for tid in &started_tasks {
                    if c.contains(tid) && !finished_tasks.contains(tid) {
                        finished_tasks.push(tid.clone());
                    }
                }
            }
        } else if typ == "PLANNER_RESPONSE" && !c.trim().is_empty() {
            last_planner_idx = v.get("step_index").and_then(|x| x.as_u64());
            last_planner_raw = c.to_string();
            last_planner_trunc = v
                .get("truncated_fields")
                .and_then(|x| x.as_array())
                .is_some_and(|arr| arr.iter().any(|t| t.as_str() == Some("content")));
        }
    }

    let pending_bg = started_tasks.iter().filter(|t| !finished_tasks.contains(t)).count();
    let last = turn.last()?;
    let step = last.get("step_index").and_then(|x| x.as_u64()).unwrap_or(0);
    let last_typ = last.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let last_st = last.get("status").and_then(|x| x.as_str()).unwrap_or("");
    let last_c = last.get("content").and_then(|x| x.as_str()).unwrap_or("");
    let has_tool_calls = last.get("tool_calls").is_some()
        || last
            .get("truncated_fields")
            .and_then(|x| x.as_array())
            .is_some_and(|arr| arr.iter().any(|t| t.as_str() == Some("tool_calls")));

    // Wait-only turn (model output while background command still running) stays in waiting_bg, NOT final done!
    let is_final_planner = last_typ == "PLANNER_RESPONSE"
        && last_st == "DONE"
        && !last_c.trim().is_empty()
        && !has_tool_calls
        && pending_bg == 0;

    if (want_full || is_final_planner) && last_planner_trunc {
        if let Some(target_idx) = last_planner_idx {
            let full_p = conv_dir.join(".system_generated").join("logs").join("transcript_full.jsonl");
            if let Some(ftail) = read_tail_str(&full_p, 80 * 1024) {
                for line in ftail.lines().rev() {
                    if let Ok(fv) = serde_json::from_str::<Value>(line) {
                        if fv.get("step_index").and_then(|x| x.as_u64()) == Some(target_idx) {
                            if let Some(fc) = fv.get("content").and_then(|x| x.as_str()) {
                                last_planner_raw = fc.to_string();
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    let clean_text = strip_html_clean(&last_planner_raw);
    let preview: String = clean_text.chars().take(110).collect();
    let full_reply: String = clean_text.chars().take(3500).collect();
    let last_code = if want_full {
        extract_last_code(&last_planner_raw).unwrap_or_default()
    } else {
        String::new()
    };

    let state_str = if pending_bg > 0 {
        "waiting_bg"
    } else if !is_final_planner || age_s <= 2 {
        "working"
    } else if age_s < 240 {
        "ready"
    } else {
        "idle"
    };

    Some(ConvAnalysis {
        state: state_str.into(),
        step,
        turn_steps: turn_tools,
        pending_bg,
        is_final_planner,
        preview,
        full_reply,
        last_code,
        age_s,
    })
}

fn poll_antigravity(state: &mut TelemetryState) -> (String, Option<String>, Option<u64>, bool, usize, usize, u64, String) {
    if !settings::flag("agentPulse") {
        return ("off".into(), None, None, false, 0, 0, 0, String::new());
    }
    let Some(profile) = std::env::var_os("USERPROFILE") else {
        return ("idle".into(), None, None, false, 0, 0, 0, String::new());
    };
    let brain = PathBuf::from(profile).join(".gemini").join("antigravity").join("brain");
    let Ok(rd) = std::fs::read_dir(&brain) else {
        return ("idle".into(), None, None, false, 0, 0, 0, String::new());
    };
    let mut best_conv: Option<(SystemTime, PathBuf, String)> = None;
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.len() < 12 {
            continue;
        }
        let log_p = ent.path().join(".system_generated").join("logs").join("transcript.jsonl");
        if let Ok(meta) = std::fs::metadata(&log_p) {
            if let Ok(mt) = meta.modified() {
                if best_conv.as_ref().is_none_or(|(bt, _, _)| mt > *bt) {
                    best_conv = Some((mt, ent.path(), name));
                }
            }
        }
    }
    let Some((_, conv_dir, conv_id)) = best_conv else {
        return ("idle".into(), None, None, false, 0, 0, 0, String::new());
    };
    if conv_id != state.last_agent_conv {
        state.last_agent_conv = conv_id.clone();
        state.agent_had_work = false;
        state.agent_settled_polls = 0;
        state.agent_working_since = None;
        state.last_notified_step = 0;
    }

    let Some(a) = analyze_conversation(&conv_dir, false) else {
        return ("idle".into(), Some(conv_id), None, false, 0, 0, 0, String::new());
    };

    let mut just_finished = false;
    let mut final_state = a.state.clone();
    if a.pending_bg > 0 || !a.is_final_planner || a.age_s <= 2 {
        state.agent_settled_polls = 0;
        if a.age_s < 120 {
            state.agent_had_work = true;
            if state.agent_working_since.is_none() {
                state.agent_working_since = Some(Instant::now());
            }
        }
        final_state = if a.pending_bg > 0 { "waiting_bg".into() } else { "working".into() };
    } else {
        // Final PLANNER_RESPONSE settled with no pending background tasks or tool calls: debounce 2 polls (~3.6s)
        state.agent_settled_polls = state.agent_settled_polls.saturating_add(1);
        if state.agent_had_work && state.agent_settled_polls >= 2 && a.step != state.last_notified_step {
            state.last_notified_step = a.step;
            state.agent_had_work = false;
            state.agent_working_since = None;
            just_finished = true;
            state.last_agent_preview = a.preview.clone();
        } else if state.agent_had_work && state.agent_settled_polls < 2 {
            final_state = "working".into();
        }
    }
    let elapsed_s = state.agent_working_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    (
        final_state,
        Some(conv_id),
        Some(a.step),
        just_finished,
        a.turn_steps,
        a.pending_bg,
        elapsed_s,
        a.preview,
    )
}

pub fn focus_antigravity() -> bool {
    struct Ctx {
        best: Option<(i64, HWND)>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let pid = sys::pid_of(hwnd);
        let Some(exe) = sys::exe_of_pid(pid) else { return BOOL(1) };
        if !exe.to_ascii_lowercase().ends_with("antigravity.exe") {
            return BOOL(1);
        }
        let r = sys::rect_of(hwnd);
        let area = (r.right - r.left).max(0) as i64 * (r.bottom - r.top).max(0) as i64;
        if area < 40_000 {
            return BOOL(1);
        }
        let ctx = &mut *(lp.0 as *mut Ctx);
        if ctx.best.is_none_or(|(best_a, _)| area > best_a) {
            ctx.best = Some((area, hwnd));
        }
        BOOL(1)
    }
    unsafe {
        let mut ctx = Ctx { best: None };
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
        if let Some((_, hwnd)) = ctx.best {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            sys::force_foreground(hwnd);
            return true;
        }
    }
    false
}

pub fn agy_hub_data() -> Value {
    let Some(profile) = std::env::var_os("USERPROFILE") else {
        return json!({ "convs": [], "artifacts": [] });
    };
    let brain = PathBuf::from(profile).join(".gemini").join("antigravity").join("brain");
    let Ok(rd) = std::fs::read_dir(&brain) else {
        return json!({ "convs": [], "artifacts": [] });
    };
    let mut dirs: Vec<(SystemTime, PathBuf, String)> = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.len() < 12 {
            continue;
        }
        let log_p = ent.path().join(".system_generated").join("logs").join("transcript.jsonl");
        if let Ok(m) = std::fs::metadata(&log_p) {
            if let Ok(mt) = m.modified() {
                dirs.push((mt, ent.path(), name));
            }
        }
    }
    dirs.sort_by(|a, b| b.0.cmp(&a.0));

    let mut convs = Vec::new();
    let mut active_artifacts = Vec::new();
    let mut full_reply = String::new();
    let mut last_code = String::new();

    for (idx, (_, dir, id)) in dirs.into_iter().take(4).enumerate() {
        let is_first = idx == 0;
        if let Some(a) = analyze_conversation(&dir, is_first) {
            if is_first {
                full_reply = a.full_reply.clone();
                last_code = a.last_code.clone();
                // Collect generated files (artifacts & scratch scripts) excluding .system_generated
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for f in entries.flatten() {
                        let fnm = f.file_name().to_string_lossy().to_string();
                        if fnm.starts_with('.') {
                            continue;
                        }
                        let p = f.path();
                        if p.is_file() && !fnm.ends_with(".resolved") && !fnm.contains(".metadata.") {
                            active_artifacts.push(json!({ "name": fnm, "path": p.to_string_lossy() }));
                        } else if p.is_dir() && fnm == "scratch" {
                            if let Ok(sc) = std::fs::read_dir(&p) {
                                for sf in sc.flatten().take(8) {
                                    if sf.path().is_file() {
                                        active_artifacts.push(json!({
                                            "name": format!("scratch/{}", sf.file_name().to_string_lossy()),
                                            "path": sf.path().to_string_lossy(),
                                        }));
                                    }
                                }
                            }
                        }
                        if active_artifacts.len() >= 10 {
                            break;
                        }
                    }
                }
            }
            convs.push(json!({
                "id": id,
                "shortId": id.chars().take(8).collect::<String>(),
                "state": a.state,
                "step": a.step,
                "tools": a.turn_steps,
                "pendingBg": a.pending_bg,
                "ageSec": a.age_s,
                "preview": a.preview,
            }));
        }
    }
    json!({
        "convs": convs,
        "artifacts": active_artifacts,
        "fullReply": full_reply,
        "lastCode": last_code,
    })
}

pub fn agy_open_path(path: &str, reveal: bool) -> bool {
    let Some(profile) = std::env::var_os("USERPROFILE") else { return false };
    let base = PathBuf::from(profile).join(".gemini").join("antigravity").join("brain");
    let Ok(base_c) = std::fs::canonicalize(&base) else { return false };
    let Ok(target_c) = std::fs::canonicalize(path) else { return false };
    if !target_c.starts_with(&base_c) || !target_c.is_file() {
        return false;
    }
    let s = target_c.to_string_lossy().to_string();
    if reveal {
        sys::reveal_in_folder(&s);
    } else {
        sys::shell_open(&s, None);
    }
    true
}

// ---------------------------------------------------------------- Adaptive Chameleon Screen Edge Sampler
fn sample_backdrop_grid(state: &mut TelemetryState, cx: i32, top_y: i32, pill_w: i32, pill_h: i32) -> bool {
    use std::sync::{Mutex, OnceLock};
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_YVIRTUALSCREEN};
    static LAST_RGB: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();

    const GW: i32 = 32;
    const GH: i32 = 8;
    let w = pill_w.max(200);
    let h = pill_h.max(42);
    let left = cx - w / 2;
    let right = left + w;

    unsafe {
        let sdc = GetDC(None);
        if sdc.0.is_null() {
            return false;
        }
        let mdc = CreateCompatibleDC(Some(sdc));
        if mdc.0.is_null() {
            let _ = ReleaseDC(None, sdc);
            return false;
        }
        let bmp = CreateCompatibleBitmap(sdc, GW, GH);
        if bmp.0.is_null() {
            let _ = DeleteDC(mdc);
            let _ = ReleaseDC(None, sdc);
            return false;
        }
        let old = SelectObject(mdc, HGDIOBJ(bmp.0));
        let _ = SetStretchBltMode(mdc, HALFTONE);
        let _ = SetBrushOrgEx(mdc, 0, 0, None);

        let vtop = GetSystemMetrics(SM_YVIRTUALSCREEN);
        // Sample flanking vertical strips outside the pill (zero self-occlusion) + horizontal strips above/below
        let _ = StretchBlt(mdc, 0, 0, 4, GH, Some(sdc), left - 24, top_y, 22, h, SRCCOPY);
        let _ = StretchBlt(mdc, 28, 0, 4, GH, Some(sdc), right + 2, top_y, 22, h, SRCCOPY);
        if top_y - vtop > 24 {
            let _ = StretchBlt(mdc, 4, 0, 24, 3, Some(sdc), left, top_y - 18, w, 16, SRCCOPY);
        } else {
            let _ = StretchBlt(mdc, 4, 0, 24, 3, Some(sdc), left, top_y + h + 3, w, 12, SRCCOPY);
        }
        let _ = StretchBlt(mdc, 4, 3, 24, 5, Some(sdc), left, top_y + h + 4, w, 24, SRCCOPY);

        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: GW,
            biHeight: -GH,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bgra = [0u8; (GW * GH * 4) as usize];
        let lines = GetDIBits(
            mdc,
            bmp,
            0,
            GH as u32,
            Some(bgra.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        SelectObject(mdc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mdc);
        let _ = ReleaseDC(None, sdc);
        if lines <= 0 {
            return false;
        }

        let mut rgb = Vec::with_capacity((GW * GH * 3) as usize);
        let mut sum_r: u64 = 0;
        let mut sum_g: u64 = 0;
        let mut sum_b: u64 = 0;
        let count = (GW * GH) as f32;
        for px in bgra.chunks_exact(4) {
            let b = px[0];
            let g = px[1];
            let r = px[2];
            rgb.push(r);
            rgb.push(g);
            rgb.push(b);
            sum_r += r as u64;
            sum_g += g as u64;
            sum_b += b as u64;
        }

        let ur = (sum_r as f32 / count).round().clamp(0.0, 255.0) as u8;
        let ug = (sum_g as f32 / count).round().clamp(0.0, 255.0) as u8;
        let ub = (sum_b as f32 / count).round().clamp(0.0, 255.0) as u8;
        let avg_rgb = [ur, ug, ub];

        let changed = state.last_ambient.is_none_or(|p| {
            let dr = (avg_rgb[0] as i32 - p[0] as i32).abs();
            let dg = (avg_rgb[1] as i32 - p[1] as i32).abs();
            let db = (avg_rgb[2] as i32 - p[2] as i32).abs();
            dr + dg + db >= 18
        });
        if changed {
            state.last_ambient = Some(avg_rgb);
            let luma = (0.2126 * ur as f64 + 0.7152 * ug as f64 + 0.0722 * ub as f64) / 255.0;
            sys::helper(json!({
                "t": "ambient",
                "rgb": avg_rgb,
                "luma": (luma * 100.0).round() / 100.0,
            }));
        }

        if settings::flag("liquidGlass") {
            let lock = LAST_RGB.get_or_init(|| Mutex::new(Vec::new()));
            let mut prev = lock.lock().unwrap_or_else(|e| e.into_inner());
            let changed = prev.len() != rgb.len()
                || prev.iter().zip(rgb.iter()).any(|(&x, &y)| x.abs_diff(y) > 9);
            if changed {
                *prev = rgb.clone();
                drop(prev);
                sys::helper(json!({
                    "t": "backdrop",
                    "w": GW,
                    "h": GH,
                    "rgb": sys::b64(&rgb),
                }));
            }
        }
        true
    }
}

fn sample_ambient(state: &mut TelemetryState) {
    if !settings::flag("chameleonGlass") && !settings::flag("liquidGlass") {
        return;
    }
    let Some((cx, top_y, pill_w, pill_h, visible)) = hit::main_bounds_for_ambient() else { return };
    if !visible {
        return;
    }
    if sample_backdrop_grid(state, cx, top_y, pill_w, pill_h) {
        return;
    }
    let (_w, _h) = (pill_w, pill_h);
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
    let (agent_state, agent_conv, agent_step, agent_done, turn_steps, pending_bg, elapsed_s, agent_preview) = poll_antigravity(state);

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
            "turnSteps": turn_steps,
            "pendingBg": pending_bg,
            "elapsed": elapsed_s,
            "preview": agent_preview,
            "justFinished": agent_done,
        }
    });
    *LATEST_SNAPSHOT.get_or_init(|| Mutex::new(json!({}))).lock().unwrap() = payload.clone();
    sys::helper(payload);

    for (name, secs) in finished {
        sys::helper(json!({ "t": "heavy-done", "name": name, "secs": secs }));
    }
    if agent_done {
        sys::helper(json!({ "t": "agent-done", "step": agent_step, "preview": state.last_agent_preview }));
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
