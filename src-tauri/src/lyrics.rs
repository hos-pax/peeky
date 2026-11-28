// Synced song lyrics client (LRCLIB) using native Win32 Urlmon + local JSON cache.
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use windows::core::HSTRING;
use windows::Win32::System::Com::Urlmon::URLDownloadToFileW;

use crate::{settings, sys};

static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

const CACHE_TTL_HIT_MS: u64 = 14 * 24 * 3600 * 1000;
const CACHE_TTL_MISS_MS: u64 = 6 * 3600 * 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LyricLine {
    pub ms: u64,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedLyrics {
    pub key: String,
    pub track: String,
    pub artist: String,
    #[serde(default)]
    pub synced: Vec<LyricLine>,
    #[serde(default)]
    pub plain: String,
    #[serde(default)]
    pub instrumental: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub offset_ms: i64,
    pub fetched_at: u64,
}

fn flight_set() -> &'static Mutex<HashSet<String>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

fn cache_dir() -> PathBuf {
    let dir = sys::data_dir().join("lyrics");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn cache_path(key: &str) -> PathBuf {
    cache_dir().join(format!("{:016x}.json", fnv1a_64(key)))
}

fn read_cache(key: &str) -> Option<CachedLyrics> {
    let raw = fs::read_to_string(cache_path(key)).ok()?;
    serde_json::from_str::<CachedLyrics>(&raw).ok()
}

fn write_cache(entry: &CachedLyrics) {
    if let Ok(s) = serde_json::to_string(entry) {
        let _ = fs::write(cache_path(&entry.key), s);
    }
}

fn emit_lyrics(entry: &CachedLyrics, status: &str) {
    sys::helper(json!({
        "t": "lyrics",
        "key": entry.key,
        "status": status,
        "track": entry.track,
        "artist": entry.artist,
        "synced": entry.synced,
        "plain": entry.plain,
        "instrumental": entry.instrumental,
        "source": entry.source,
        "offsetMs": entry.offset_ms,
    }));
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push_str("%20"),
            v => {
                out.push('%');
                let hi = v >> 4;
                let lo = v & 0xf;
                out.push(char::from(if hi < 10 { b'0' + hi } else { b'A' + hi - 10 }));
                out.push(char::from(if lo < 10 { b'0' + lo } else { b'A' + lo - 10 }));
            }
        }
    }
    out
}

fn http_get_json(url: &str) -> Option<Value> {
    sys::com_init();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("peeky_lyr_{}_{}.json", std::process::id(), seq));
    let _ = fs::remove_file(&tmp);
    let ok = unsafe {
        URLDownloadToFileW(
            None,
            &HSTRING::from(url),
            &HSTRING::from(tmp.to_string_lossy().as_ref()),
            0,
            None,
        )
        .is_ok()
    };
    let bytes = if ok { fs::read(&tmp).ok() } else { None };
    let _ = fs::remove_file(&tmp);
    let bytes = bytes?;
    if bytes.is_empty() {
        return None;
    }
    serde_json::from_slice::<Value>(&bytes).ok()
}

fn is_noise_bracket(inner: &str) -> bool {
    let low = inner.trim().to_lowercase();
    const KEYS: &[&str] = &[
        "official", "video", "audio", "lyric", "lyrics", "visualizer", "hd", "4k", "8k",
        "mv", "m/v", "feat", "ft.", "prod", "topic", "explicit", "clean", "radio edit",
        "music video", "full", "hq",
    ];
    KEYS.iter().any(|k| low.contains(k))
}

fn strip_brackets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '(' || ch == '[' {
            let close = if ch == '(' { ')' } else { ']' };
            let mut j = i + 1;
            while j < chars.len() && chars[j] != close {
                j += 1;
            }
            if j < chars.len() {
                let inner: String = chars[i + 1..j].iter().collect();
                if is_noise_bracket(&inner) {
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Cleans noisy browser/YouTube titles into crisp `(track, artist)` hints for LRCLIB.
pub fn normalize_meta(raw_title: &str, raw_artist: &str) -> (String, String) {
    let mut artist = raw_artist.trim().to_string();
    for suffix in [" - Topic", " – Topic", " — Topic", " VEVO", " Official"] {
        if let Some(pos) = artist.rfind(suffix) {
            if pos + suffix.len() == artist.len() {
                artist = artist[..pos].trim().to_string();
            }
        }
    }

    let mut title = raw_title.trim().to_string();
    for suffix in [" - YouTube", " - YouTube Music", " | Spotify", " - SoundCloud"] {
        if let Some(pos) = title.rfind(suffix) {
            if pos + suffix.len() == title.len() {
                title = title[..pos].trim().to_string();
            }
        }
    }

    title = strip_brackets(&title);
    for noise in [
        "feat.", "ft.", "feat ", "ft ", "with ", "prod. by", "official video",
        "official audio", "lyric video", "lyrics", "visualizer",
    ] {
        let lower = title.to_lowercase();
        if let Some(pos) = lower.find(noise) {
            if pos > 2 {
                let prefix = title[..pos].trim_end_matches(|c: char| c == ' ' || c == '-' || c == '|' || c == ',');
                if !prefix.is_empty() {
                    title = prefix.to_string();
                }
            }
        }
    }

    title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    artist = artist.split_whitespace().collect::<Vec<_>>().join(" ");

    let artist_low = artist.to_lowercase();
    let weak_artist = artist.is_empty()
        || artist_low == "youtube"
        || artist_low == "youtube music"
        || artist_low == "unknown artist"
        || artist_low == "chrome"
        || artist_low == "edge";

    for sep in [" - ", " – ", " — "] {
        if let Some(idx) = title.find(sep) {
            let left = title[..idx].trim();
            let right = title[idx + sep.len()..].trim();
            if !left.is_empty() && !right.is_empty() && (weak_artist || artist.eq_ignore_ascii_case(left)) {
                artist = left.to_string();
                title = right.to_string();
                break;
            }
        }
    }

    if weak_artist && artist.eq_ignore_ascii_case("youtube") {
        artist.clear();
    }
    (title, artist)
}

/// Parses `[mm:ss.xx]` or `[mm:ss.xxx]` LRC lines (including multi-tag lines) sorted by timestamp.
pub fn parse_lrc(raw: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for src in raw.lines() {
        let mut rest = src.trim();
        let mut stamps = Vec::new();
        while rest.starts_with('[') {
            let Some(end) = rest.find(']') else { break };
            let tag = &rest[1..end];
            if let Some(ms) = parse_lrc_stamp(tag) {
                stamps.push(ms);
                rest = &rest[end + 1..];
            } else {
                // metadata tag like [ar:...] or [ti:...]
                rest = "";
                break;
            }
        }
        if stamps.is_empty() {
            continue;
        }
        let cleaned = strip_inline_word_tags(rest).trim().to_string();
        for ms in stamps {
            lines.push(LyricLine {
                ms,
                text: cleaned.clone(),
            });
        }
    }
    lines.sort_by_key(|l| l.ms);
    lines
}

fn parse_lrc_stamp(tag: &str) -> Option<u64> {
    let (mm_s, sec_s) = tag.split_once(':')?;
    let mm: u64 = mm_s.trim().parse().ok()?;
    let (ss_s, frac_s) = match sec_s.split_once('.') {
        Some((a, b)) => (a, b),
        None => (sec_s, ""),
    };
    let ss: u64 = ss_s.trim().parse().ok()?;
    if ss >= 60 {
        return None;
    }
    let frac_ms: u64 = match frac_s.len() {
        0 => 0,
        1 => frac_s.parse::<u64>().ok()? * 100,
        2 => frac_s.parse::<u64>().ok()? * 10,
        _ => frac_s[..3].parse::<u64>().ok()?,
    };
    Some((mm * 60 + ss) * 1000 + frac_ms)
}

fn strip_inline_word_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_angle = false;
    for ch in s.chars() {
        if ch == '<' {
            in_angle = true;
        } else if ch == '>' && in_angle {
            in_angle = false;
        } else if !in_angle {
            out.push(ch);
        }
    }
    out
}

fn entry_from_lrclib(v: &Value, key: &str, fallback_track: &str, fallback_artist: &str, offset_ms: i64) -> Option<CachedLyrics> {
    if v.get("statusCode").and_then(|c| c.as_u64()).is_some() {
        return None;
    }
    let synced_raw = v.get("syncedLyrics").and_then(|s| s.as_str()).unwrap_or("");
    let plain = v.get("plainLyrics").and_then(|s| s.as_str()).unwrap_or("").trim().to_string();
    let instrumental = v.get("instrumental").and_then(|b| b.as_bool()).unwrap_or(false);
    let synced = parse_lrc(synced_raw);
    if synced.is_empty() && plain.is_empty() && !instrumental {
        return None;
    }
    let track = v
        .get("trackName")
        .and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback_track)
        .to_string();
    let artist = v
        .get("artistName")
        .and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback_artist)
        .to_string();
    Some(CachedLyrics {
        key: key.to_string(),
        track,
        artist,
        synced,
        plain,
        instrumental,
        source: "lrclib".to_string(),
        offset_ms,
        fetched_at: sys::now_ms(),
    })
}

fn score_candidate(v: &Value, norm_track: &str, norm_artist: &str, dur_sec: Option<i64>) -> i64 {
    let has_synced = !v.get("syncedLyrics").and_then(|s| s.as_str()).unwrap_or("").trim().is_empty();
    let has_plain = !v.get("plainLyrics").and_then(|s| s.as_str()).unwrap_or("").trim().is_empty();
    let instrumental = v.get("instrumental").and_then(|b| b.as_bool()).unwrap_or(false);
    if !has_synced && !has_plain && !instrumental {
        return -10_000;
    }
    let mut score: i64 = if has_synced {
        1000
    } else if has_plain {
        400
    } else {
        100
    };
    if let Some(d_target) = dur_sec {
        if let Some(d_cand) = v.get("duration").and_then(|n| n.as_f64()) {
            let diff = (d_cand.round() as i64 - d_target).abs();
            if diff <= 3 {
                score += 600 - diff * 30;
            } else if diff <= 10 {
                score += 250 - diff * 15;
            } else if diff > 30 {
                score -= 350;
            }
        }
    }
    let cand_t = v.get("trackName").and_then(|s| s.as_str()).unwrap_or("").to_lowercase();
    let cand_a = v.get("artistName").and_then(|s| s.as_str()).unwrap_or("").to_lowercase();
    let nt = norm_track.to_lowercase();
    let na = norm_artist.to_lowercase();
    if !nt.is_empty() && (cand_t == nt || cand_t.contains(&nt) || nt.contains(&cand_t)) {
        score += 300;
    }
    if !na.is_empty() && (cand_a == na || cand_a.contains(&na) || na.contains(&cand_a)) {
        score += 220;
    }
    score
}

fn lookup_remote(key: &str, raw_title: &str, raw_artist: &str, dur_ms: u64, offset_ms: i64) -> Option<CachedLyrics> {
    let (track, artist) = normalize_meta(raw_title, raw_artist);
    if track.is_empty() {
        return None;
    }
    let dur_sec = if dur_ms >= 15_000 { Some((dur_ms / 1000) as i64) } else { None };
    let stamp = sys::now_ms();

    // 1) Exact match endpoint when artist + track are present
    if !artist.is_empty() {
        let mut url = format!(
            "https://lrclib.net/api/get?artist_name={}&track_name={}&_t={}",
            url_encode(&artist),
            url_encode(&track),
            stamp
        );
        if let Some(d) = dur_sec {
            url.push_str(&format!("&duration={}", d));
        }
        if let Some(v) = http_get_json(&url) {
            if let Some(entry) = entry_from_lrclib(&v, key, &track, &artist, offset_ms) {
                return Some(entry);
            }
        }
    }

    // 2) Structured search fallback (handles slight remixes, live, non-standard durations, or missing artist)
    let search_url = if !artist.is_empty() {
        format!(
            "https://lrclib.net/api/search?track_name={}&artist_name={}&_t={}",
            url_encode(&track),
            url_encode(&artist),
            stamp
        )
    } else {
        format!("https://lrclib.net/api/search?q={}&_t={}", url_encode(&track), stamp)
    };
    if let Some(Value::Array(arr)) = http_get_json(&search_url) {
        if let Some(best) = arr
            .iter()
            .max_by_key(|v| score_candidate(v, &track, &artist, dur_sec))
        {
            if score_candidate(best, &track, &artist, dur_sec) > 0 {
                if let Some(entry) = entry_from_lrclib(best, key, &track, &artist, offset_ms) {
                    return Some(entry);
                }
            }
        }
    }

    // 3) Broad single-query fallback if track_name + artist_name returned empty (e.g., flipped artist/title on YouTube)
    if !artist.is_empty() {
        let q = format!("{} {}", artist, track);
        let q_url = format!("https://lrclib.net/api/search?q={}&_t={}", url_encode(&q), stamp);
        if let Some(Value::Array(arr)) = http_get_json(&q_url) {
            if let Some(best) = arr
                .iter()
                .max_by_key(|v| score_candidate(v, &track, &artist, dur_sec))
            {
                if score_candidate(best, &track, &artist, dur_sec) > 0 {
                    if let Some(entry) = entry_from_lrclib(best, key, &track, &artist, offset_ms) {
                        return Some(entry);
                    }
                }
            }
        }
    }
    None
}

/// Fetches lyrics for the active track on a background thread (hitting disk cache first).
pub fn fetch_async(title: &str, artist: &str, key: &str, dur_ms: u64, force: bool) {
    if !settings::flag("lyrics") || key.is_empty() || title.trim().is_empty() {
        return;
    }

    let existing_offset = read_cache(key).map(|c| c.offset_ms).unwrap_or(0);
    if !force {
        if let Some(cached) = read_cache(key) {
            let age = sys::now_ms().saturating_sub(cached.fetched_at);
            let ttl = if cached.synced.is_empty() && cached.plain.is_empty() && !cached.instrumental {
                CACHE_TTL_MISS_MS
            } else {
                CACHE_TTL_HIT_MS
            };
            if age < ttl {
                let status = if cached.synced.is_empty() && cached.plain.is_empty() && !cached.instrumental {
                    "none"
                } else {
                    "ready"
                };
                emit_lyrics(&cached, status);
                return;
            }
        }
    }

    {
        let mut set = flight_set().lock().unwrap();
        if set.contains(key) {
            return;
        }
        set.insert(key.to_string());
    }

    let key_s = key.to_string();
    let title_s = title.to_string();
    let artist_s = artist.to_string();

    sys::helper(json!({
        "t": "lyrics",
        "key": key_s,
        "status": "loading",
        "track": title_s,
        "artist": artist_s,
        "synced": [],
        "plain": "",
        "instrumental": false,
        "source": "",
        "offsetMs": existing_offset,
    }));

    std::thread::spawn(move || {
        let result = lookup_remote(&key_s, &title_s, &artist_s, dur_ms, existing_offset);
        let entry = result.unwrap_or_else(|| {
            let (t, a) = normalize_meta(&title_s, &artist_s);
            CachedLyrics {
                key: key_s.clone(),
                track: t,
                artist: a,
                synced: Vec::new(),
                plain: String::new(),
                instrumental: false,
                source: String::new(),
                offset_ms: existing_offset,
                fetched_at: sys::now_ms(),
            }
        });
        write_cache(&entry);
        let status = if entry.synced.is_empty() && entry.plain.is_empty() && !entry.instrumental {
            "none"
        } else {
            "ready"
        };
        emit_lyrics(&entry, status);

        let mut set = flight_set().lock().unwrap();
        set.remove(&key_s);
    });
}

/// Searches LRCLIB interactively and returns lightweight candidate summaries for manual pick.
pub fn search_candidates(query: &str, dur_ms: u64) -> Vec<Value> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let dur_sec = if dur_ms >= 15_000 { Some((dur_ms / 1000) as i64) } else { None };
    let url = format!("https://lrclib.net/api/search?q={}&_t={}", url_encode(q), sys::now_ms());
    let Some(Value::Array(arr)) = http_get_json(&url) else {
        return Vec::new();
    };
    arr.into_iter()
        .filter(|v| {
            !v.get("syncedLyrics").and_then(|s| s.as_str()).unwrap_or("").trim().is_empty()
                || !v.get("plainLyrics").and_then(|s| s.as_str()).unwrap_or("").trim().is_empty()
        })
        .take(8)
        .map(|v| {
            let id = v.get("id").and_then(|n| n.as_u64()).unwrap_or(0);
            let track = v.get("trackName").and_then(|s| s.as_str()).unwrap_or("");
            let artist = v.get("artistName").and_then(|s| s.as_str()).unwrap_or("");
            let album = v.get("albumName").and_then(|s| s.as_str()).unwrap_or("");
            let dur = v.get("duration").and_then(|n| n.as_f64()).unwrap_or(0.0).round() as u64;
            let has_synced = !v.get("syncedLyrics").and_then(|s| s.as_str()).unwrap_or("").trim().is_empty();
            let diff = dur_sec.map(|t| (dur as i64 - t).abs()).unwrap_or(0);
            json!({
                "id": id,
                "track": track,
                "artist": artist,
                "album": album,
                "durSec": dur,
                "hasSynced": has_synced,
                "durDiff": diff,
            })
        })
        .collect()
}

/// Pins a specific LRCLIB record ID to `key` and emits the updated lyrics immediately.
pub fn apply_candidate_id(key: &str, id: u64) -> bool {
    if key.is_empty() || id == 0 {
        return false;
    }
    let url = format!("https://lrclib.net/api/get/{}?_t={}", id, sys::now_ms());
    let Some(v) = http_get_json(&url) else {
        return false;
    };
    let existing_offset = read_cache(key).map(|c| c.offset_ms).unwrap_or(0);
    let Some(entry) = entry_from_lrclib(&v, key, "", "", existing_offset) else {
        return false;
    };
    write_cache(&entry);
    emit_lyrics(&entry, "ready");
    true
}

/// Persists per-track timestamp offset in milliseconds (useful for MV intros/outros).
pub fn set_offset(key: &str, offset_ms: i64) -> bool {
    let Some(mut entry) = read_cache(key) else {
        return false;
    };
    entry.offset_ms = offset_ms.clamp(-30_000, 30_000);
    write_cache(&entry);
    let status = if entry.synced.is_empty() && entry.plain.is_empty() && !entry.instrumental {
        "none"
    } else {
        "ready"
    };
    emit_lyrics(&entry, status);
    true
}
