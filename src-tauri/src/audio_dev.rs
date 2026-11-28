// Active audio output enumerator and default endpoint switcher via Win32 MMDevice + IPolicyConfig.
use std::ffi::c_void;
use std::ptr::null_mut;

use serde_json::{json, Value};
use windows::core::{GUID, HSTRING, Interface, IUnknown, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eCommunications, eConsole, eMultimedia, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, STGM_READ};
use windows::Win32::System::Variant::VT_LPWSTR;

use crate::sys;

const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_IPOLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

fn classify_device(name: &str) -> &'static str {
    let low = name.to_lowercase();
    if low.contains("headphone")
        || low.contains("headset")
        || low.contains("airpod")
        || low.contains("buds")
        || low.contains("earphone")
        || low.contains("arctis")
        || low.contains("hyperx")
        || low.contains("hands-free")
    {
        "headphones"
    } else if low.contains("bluetooth") || low.contains("wireless") {
        "bluetooth"
    } else if low.contains("hdmi") || low.contains("displayport") || low.contains("nvidia") || low.contains("monitor") {
        "display"
    } else {
        "speaker"
    }
}

fn short_name(friendly: &str) -> String {
    let trimmed = friendly.trim();
    if let Some(idx) = trimmed.find(" (") {
        let head = trimmed[..idx].trim();
        let tail = trimmed[idx + 2..].trim_end_matches(')').trim();
        if head.eq_ignore_ascii_case("Speakers") || head.eq_ignore_ascii_case("Headphones") {
            return format!("{} · {}", head, tail);
        }
        return head.to_string();
    }
    trimmed.to_string()
}

pub fn list_outputs() -> Vec<Value> {
    sys::com_init();
    unsafe {
        let Ok(enumr) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return Vec::new();
        };
        let default_id = enumr
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .ok()
            .and_then(|d| d.GetId().ok())
            .map(|p| sys::pwstr_take(p))
            .unwrap_or_default();

        let Ok(coll) = enumr.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return Vec::new();
        };
        let count = coll.GetCount().unwrap_or(0);
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let Ok(dev) = coll.Item(i) else { continue };
            let Ok(id_pw) = dev.GetId() else { continue };
            let id = sys::pwstr_take(id_pw);
            if id.is_empty() {
                continue;
            }
            let friendly = dev
                .OpenPropertyStore(STGM_READ)
                .ok()
                .and_then(|store| {
                    let mut prop = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
                    let name = if prop.Anonymous.Anonymous.vt == VT_LPWSTR {
                        let p = prop.Anonymous.Anonymous.Anonymous.pwszVal;
                        if !p.is_null() {
                            p.to_string().unwrap_or_default()
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };
                    let _ = PropVariantClear(&mut prop);
                    Some(name)
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Audio Output".to_string());

            let is_default = id == default_id;
            let kind = classify_device(&friendly);
            out.push(json!({
                "id": id,
                "name": short_name(&friendly),
                "fullName": friendly,
                "kind": kind,
                "isDefault": is_default,
            }));
        }
        out
    }
}

pub fn set_default_output(id: &str) -> bool {
    let id_t = id.trim();
    if id_t.is_empty() {
        return false;
    }
    sys::com_init();
    unsafe {
        let Ok(unk) = CoCreateInstance::<_, IUnknown>(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL) else {
            return false;
        };
        let mut pc: *mut c_void = null_mut();
        if unk.query(&IID_IPOLICY_CONFIG, &mut pc).is_err() || pc.is_null() {
            return false;
        }
        let vtbl = *(pc as *const *const *const c_void);
        let set_default_ep: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> windows::core::HRESULT =
            std::mem::transmute(*vtbl.add(13));
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(2));

        let wide = HSTRING::from(id_t);
        let ptr = PCWSTR(wide.as_ptr());
        let ok0 = set_default_ep(pc, ptr, eConsole.0).is_ok();
        let ok1 = set_default_ep(pc, ptr, eMultimedia.0).is_ok();
        let _ = set_default_ep(pc, ptr, eCommunications.0);
        release(pc);
        ok0 || ok1
    }
}

/// Switches to the next active render device and emits the updated list to the frontend.
pub fn cycle_output() -> Value {
    let mut devs = list_outputs();
    if devs.len() < 2 {
        return json!({ "ok": false, "devices": devs });
    }
    let cur_idx = devs.iter().position(|d| d["isDefault"].as_bool() == Some(true)).unwrap_or(0);
    let next_idx = (cur_idx + 1) % devs.len();
    let next_id = devs[next_idx]["id"].as_str().unwrap_or("").to_string();
    let ok = set_default_output(&next_id);
    if ok {
        devs = list_outputs();
        sys::helper(json!({ "t": "audio-outputs", "devices": devs }));
    }
    json!({ "ok": ok, "devices": devs })
}
