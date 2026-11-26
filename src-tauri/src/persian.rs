// 1-keystroke En <-> Fa keyboard-layout rescue, Persian ZWNJ/orthography polisher, and safe in-place selection replacement.
use std::time::{Duration, Instant};

use serde_json::json;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ActivateKeyboardLayout, LoadKeyboardLayoutW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD,
    KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KLF_ACTIVATE, VIRTUAL_KEY, VK_CONTROL,
    VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, PostMessageW, WM_INPUTLANGCHANGEREQUEST};

use crate::{clip, sys};

const PAIRS: &[(char, char)] = &[
    ('q', 'ض'), ('w', 'ص'), ('e', 'ث'), ('r', 'ق'), ('t', 'ف'), ('y', 'غ'), ('u', 'ع'), ('i', 'ه'), ('o', 'خ'), ('p', 'ح'), ('[', 'ج'), (']', 'چ'),
    ('a', 'ش'), ('s', 'س'), ('d', 'ی'), ('f', 'ب'), ('g', 'ل'), ('h', 'ا'), ('j', 'ت'), ('k', 'ن'), ('l', 'م'), (';', 'ک'), ('\'', 'گ'),
    ('z', 'ظ'), ('x', 'ط'), ('c', 'ز'), ('v', 'ر'), ('b', 'ذ'), ('n', 'د'), ('m', 'پ'), (',', 'و'),
    ('Q', 'ْ'), ('W', 'ٌ'), ('E', 'ٍ'), ('R', 'ً'), ('T', 'ُ'), ('Y', 'ِ'), ('U', 'َ'), ('I', 'ّ'), ('O', ']'), ('P', '['), ('{', '}'), ('}', '{'),
    ('A', 'ؤ'), ('S', 'ئ'), ('D', 'ي'), ('F', 'إ'), ('G', 'أ'), ('H', 'آ'), ('J', 'ة'), ('K', '»'), ('L', '«'), (':', ':'), ('"', '؛'),
    ('Z', 'ك'), ('X', 'ٓ'), ('C', 'ژ'), ('V', 'ٰ'), ('B', '\u{200C}'), ('N', 'ٔ'), ('M', 'ء'), ('?', '؟'), ('<', '>'), ('>', '<'),
];

const SUFFIXES: &[&str] = &[
    "هایشان", "هایمان", "هایتان", "هایم", "هایت", "هایش", "ها", "های", "ترین", "تر", "ام", "ات", "اش", "ایم", "اید", "اند", "ای",
];

fn is_persian_letter(c: char) -> bool {
    matches!(c,
        '\u{0621}'..='\u{063A}' | '\u{0641}'..='\u{064A}' | '\u{067E}' | '\u{0686}' | '\u{0698}' | '\u{06A9}' | '\u{06AF}' | '\u{06CC}'
    )
}

fn en_to_fa_char(c: char) -> char {
    PAIRS.iter().find(|(en, _)| *en == c).map(|(_, fa)| *fa).unwrap_or(c)
}

fn fa_to_en_char(c: char) -> char {
    let norm = match c {
        'ي' => 'ی',
        'ك' => 'ک',
        other => other,
    };
    PAIRS.iter().find(|(_, fa)| *fa == norm).map(|(en, _)| *en).unwrap_or(c)
}

/// Converts text between English QWERTY and Windows Standard Persian automatically based on character balance.
/// Returns `(converted_text, target_lang)` where `target_lang` is `"fa"` or `"en"`.
pub fn fix_layout_auto(input: &str) -> (String, &'static str) {
    let latin = input.chars().filter(|c| c.is_ascii_alphabetic()).count();
    let persian = input.chars().filter(|&c| is_persian_letter(c)).count();
    if latin >= persian {
        (input.chars().map(en_to_fa_char).collect(), "fa")
    } else {
        (input.chars().map(fa_to_en_char).collect(), "en")
    }
}

/// Normalizes Arabic yeh/kaf, fixes common Persian ZWNJ (half-space) prefixes & suffixes, and tidies spacing.
pub fn polish_persian(input: &str) -> String {
    let norm: String = input
        .chars()
        .map(|c| match c {
            '\u{064A}' | '\u{0649}' => '\u{06CC}', // ي / ى -> ی
            '\u{0643}' => '\u{06A9}',              // ك -> ک
            '\u{0629}' => '\u{0647}',              // ة -> ه
            c => c,
        })
        .collect();

    let mut lines_out = Vec::new();
    for line in norm.split('\n') {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.is_empty() {
            lines_out.push(String::new());
            continue;
        }
        let mut rebuilt: Vec<String> = Vec::with_capacity(tokens.len());
        let mut i = 0;
        while i < tokens.len() {
            let cur = tokens[i];
            // Prefix join: می / نمی + Persian word (>= 2 letters)
            if (cur == "می" || cur == "نمی") && i + 1 < tokens.len() {
                let next = tokens[i + 1];
                let pchars = next.chars().filter(|&c| is_persian_letter(c)).count();
                if pchars >= 2 {
                    rebuilt.push(format!("{cur}\u{200C}{next}"));
                    i += 2;
                    continue;
                }
            }
            // Suffix join: Persian base (>= 2 letters, not ending in already-detached punct) + common suffix
            if let Some(prev) = rebuilt.last_mut() {
                let prev_persian = prev.chars().filter(|&c| is_persian_letter(c)).count();
                let prev_ends_letter = prev.chars().last().is_some_and(is_persian_letter);
                if prev_persian >= 2 && prev_ends_letter && SUFFIXES.contains(&cur) {
                    // Exceptions: short conjunctions or standalone words like "ای" only join after vowels ه/ا/و/ی cleanly
                    let should_join = if cur == "ای" {
                        prev.ends_with('ه') || prev.ends_with('ا') || prev.ends_with('و')
                    } else if matches!(cur, "ام" | "ات" | "اش" | "اید" | "ایم" | "اند") {
                        prev.ends_with('ه')
                    } else {
                        true
                    };
                    if should_join {
                        prev.push('\u{200C}');
                        prev.push_str(cur);
                        i += 1;
                        continue;
                    }
                }
            }
            rebuilt.push(cur.to_string());
            i += 1;
        }
        let joined = rebuilt.join(" ");
        // Collapse repeated ZWNJs
        let mut clean = String::with_capacity(joined.len());
        let mut prev_zwnj = false;
        for c in joined.chars() {
            if c == '\u{200C}' {
                if !prev_zwnj {
                    clean.push(c);
                }
                prev_zwnj = true;
            } else {
                prev_zwnj = false;
                clean.push(c);
            }
        }
        lines_out.push(clean);
    }
    lines_out.join("\n")
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

unsafe fn release_modifiers() {
    let keys = [
        VK_MENU.0, VK_LMENU.0, VK_RMENU.0,
        VK_CONTROL.0, VK_LCONTROL.0, VK_RCONTROL.0,
        VK_SHIFT.0, VK_LSHIFT.0, VK_RSHIFT.0,
    ];
    let ups: Vec<INPUT> = keys.into_iter().map(|k| key_input(k, true)).collect();
    SendInput(&ups, std::mem::size_of::<INPUT>() as i32);
}

unsafe fn send_ctrl_key(vk_ascii: u16) {
    let seq = [
        key_input(VK_CONTROL.0, false),
        key_input(vk_ascii, false),
        key_input(vk_ascii, true),
        key_input(VK_CONTROL.0, true),
    ];
    SendInput(&seq, std::mem::size_of::<INPUT>() as i32);
}

unsafe fn switch_fg_keyboard(lang: &str) {
    let fg: HWND = GetForegroundWindow();
    if fg.0.is_null() {
        return;
    }
    let id = if lang == "fa" { w!("00000429") } else { w!("00000409") };
    if let Ok(hkl) = LoadKeyboardLayoutW(id, KLF_ACTIVATE) {
        let _ = ActivateKeyboardLayout(hkl, windows::Win32::UI::Input::KeyboardAndMouse::ACTIVATE_KEYBOARD_LAYOUT_FLAGS(0));
        let _ = PostMessageW(Some(fg), WM_INPUTLANGCHANGEREQUEST, WPARAM(0), LPARAM(hkl.0 as isize));
    }
}

/// Copies highlighted selection in active app, transforms it (`"layout"` or `"zwnj"`), pastes in place,
/// restores previous clipboard text, and notifies UI cleanly.
pub fn rescue_selection(mode: &str) {
    let mode = mode.to_string();
    std::thread::spawn(move || unsafe {
        let backup = clip::read_text();
        let before_seq = clip::sequence();
        clip::QUIET_UNTIL.store(sys::now_ms() + 1800, std::sync::atomic::Ordering::SeqCst);
        release_modifiers();
        std::thread::sleep(Duration::from_millis(35));
        send_ctrl_key(0x43); // Ctrl+C

        let start = Instant::now();
        while clip::sequence() == before_seq && start.elapsed() < Duration::from_millis(240) {
            std::thread::sleep(Duration::from_millis(15));
        }

        let got = if clip::sequence() != before_seq { clip::read_text() } else { None };
        let Some(src) = got.filter(|s| !s.trim().is_empty()) else {
            if let Some(prev) = backup {
                let _ = clip::write_text(&prev);
            }
            sys::helper(json!({
                "t": "persian-res",
                "ok": false,
                "mode": mode,
                "why": "ابتدا متن را انتخاب کنید"
            }));
            return;
        };

        let (out, lang) = if mode == "zwnj" {
            (polish_persian(&src), "")
        } else {
            fix_layout_auto(&src)
        };

        if out != src {
            clip::QUIET_UNTIL.store(sys::now_ms() + 1800, std::sync::atomic::Ordering::SeqCst);
            let _ = clip::write_text(&out);
            std::thread::sleep(Duration::from_millis(25));
            send_ctrl_key(0x56); // Ctrl+V
            std::thread::sleep(Duration::from_millis(140));
        }
        if !lang.is_empty() {
            switch_fg_keyboard(lang);
        }
        if let Some(prev) = backup {
            let _ = clip::write_text(&prev);
        }

        let preview: String = out.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(48).collect();
        sys::helper(json!({
            "t": "persian-res",
            "ok": true,
            "mode": mode,
            "lang": lang,
            "preview": preview
        }));
    });
}
