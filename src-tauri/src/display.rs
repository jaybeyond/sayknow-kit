// Display control.
//
// Two different mechanisms, because macOS treats them differently:
//
// - External displays speak DDC/CI, so brightness (VCP 0x10) and power
//   (VCP 0xD6, the same code Lunar's BlackOut uses) go through ddc-hi.
//   That works on macOS and Windows over HDMI/DP/USB-C.
// - The built-in panel has no DDC. Its backlight is an IOKit parameter, set
//   through IODisplaySetFloatParameter on the IODisplayConnect service that
//   answers for it. ddc-hi deliberately excludes built-ins, so this is our
//   own small FFI layer, macOS only.
//
// DDC is slow (tens to hundreds of ms) and some monitors don't ack reads,
// so reads degrade to `None` instead of failing the listing, and every
// command is fire-and-forget from the UI's point of view.

use ddc_hi::Ddc as _;
use serde::Serialize;
use std::io;

const VCP_LUMINANCE: u8 = 0x10;
const VCP_POWER: u8 = 0xd6;
const POWER_ON: u16 = 0x01;
const POWER_OFF: u16 = 0x04;

/// The built-in display's stable id. EDID-based ids are used for externals,
/// and none of them can start with this prefix.
pub const BUILTIN_ID: &str = "builtin";

#[derive(Serialize, Clone, Debug)]
pub struct DisplayStatus {
    pub id: String,
    pub name: String,
    /// builtin | external
    pub kind: String,
    /// Carries the menu bar; only one display has this.
    pub is_main: bool,
    /// 0-100 when readable. Some monitors never ack brightness reads.
    pub brightness: Option<u8>,
    /// None when the monitor doesn't report power state over DDC.
    pub power: Option<bool>,
    /// False when this display can't be controlled from here at all.
    pub controllable: bool,
    /// How brightness is driven: backlight | ddc | gamma. The UI labels
    /// gamma honestly as software dimming — it scales the video signal, not
    /// the panel's backlight.
    pub method: String,
}

pub fn clamp_percent(v: i64) -> u8 {
    v.clamp(0, 100) as u8
}

/// DDC luminance is 0-100 by spec; the float for the built-in panel is 0.0-1.0.
pub fn percent_to_float(p: u8) -> f32 {
    p as f32 / 100.0
}

pub fn float_to_percent(f: f32) -> u8 {
    clamp_percent((f * 100.0).round() as i64)
}

// ─────────── built-in (macOS IOKit) ───────────

#[cfg(target_os = "macos")]
mod iokit_backlight {
    //! Built-in panel backlight.
    //!
    //! The classic IODisplayConnect + IODisplay{Get,Set}FloatParameter path
    //! still works on Intel Macs and older Apple Silicon, but on the
    //! "new-backlight-architecture" (AppleARMBacklight, this M4 included) it
    //! returns kIOReturnUnsupported — and the DisplayServices/CoreDisplay
    //! private setters are stubs without entitlements. Homebrew's `brightness`
    //! fails the same way here, so when the classic path is dead we report the
    //! panel as present-but-uncontrollable rather than faking a slider.

    use std::ffi::c_char;

    type IoObject = u32;
    type IoIterator = u32;
    type IoReturn = i32;
    type CfAllocatorRef = *const ();
    type CfStringRef = *const ();
    type CfStringEncoding = u32;
    const KCF_STRING_ENCODING_UTF8: CfStringEncoding = 0x0800_0100;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceGetMatchingServices(
            main_port: u32,
            matching: CfStringRef,
            existing: *mut IoIterator,
        ) -> IoReturn;
        fn IOServiceMatching(name: *const c_char) -> CfStringRef;
        fn IOIteratorNext(iterator: IoIterator) -> IoObject;
        fn IOObjectRelease(object: IoObject) -> IoReturn;
        fn IODisplayGetFloatParameter(
            display: IoObject,
            options: u32,
            parameter_name: CfStringRef,
            value: *mut f32,
        ) -> IoReturn;
        fn IODisplaySetFloatParameter(
            display: IoObject,
            options: u32,
            parameter_name: CfStringRef,
            value: f32,
        ) -> IoReturn;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            alloc: CfAllocatorRef,
            c_str: *const c_char,
            encoding: CfStringEncoding,
        ) -> CfStringRef;
        fn CFRelease(cf: *const ());
    }

    const BACKLIGHT_KEY: &[u8] = b"brightness\0";

    struct BacklightService {
        service: IoObject,
        key: CfStringRef,
    }

    impl Drop for BacklightService {
        fn drop(&mut self) {
            unsafe {
                CFRelease(self.key as *const ());
                IOObjectRelease(self.service);
            }
        }
    }

    /// Probe the classes that own the brightness parameter, oldest first.
    /// Pre-new-architecture Macs expose it on IODisplayConnect; the new
    /// architecture exposes nothing user-settable.
    fn find_backlight_service() -> Option<BacklightService> {
        unsafe {
            let key = CFStringCreateWithCString(
                std::ptr::null(),
                BACKLIGHT_KEY.as_ptr() as *const c_char,
                KCF_STRING_ENCODING_UTF8,
            );
            if key.is_null() {
                return None;
            }
            let mut found: Option<BacklightService> = None;
            for class_name in [&b"IODisplayConnect\0"[..], &b"AppleARMBacklight\0"[..]] {
                let matching = IOServiceMatching(class_name.as_ptr() as *const c_char);
                if matching.is_null() {
                    continue;
                }
                let mut iterator: IoIterator = 0;
                // kIOMainPortDefault == 0
                if IOServiceGetMatchingServices(0, matching, &mut iterator) != 0 {
                    continue;
                }
                let mut probe: f32 = 0.0;
                loop {
                    let service = IOIteratorNext(iterator);
                    if service == 0 {
                        break;
                    }
                    if IODisplayGetFloatParameter(service, 0, key, &mut probe) == 0 {
                        // GetFloatParameter answering means Set will too.
                        found = Some(BacklightService { service, key });
                        break;
                    }
                    IOObjectRelease(service);
                }
                IOObjectRelease(iterator);
                if found.is_some() {
                    return found;
                }
            }
            CFRelease(key as *const ());
            None
        }
    }

    pub fn get() -> Option<u8> {
        let svc = find_backlight_service()?;
        let mut v: f32 = 0.0;
        let ok =
            unsafe { IODisplayGetFloatParameter(svc.service, 0, svc.key, &mut v) == 0 };
        ok.then(|| super::float_to_percent(v))
    }

    pub fn set(percent: u8) -> bool {
        let Some(svc) = find_backlight_service() else {
            return false;
        };
        unsafe {
            IODisplaySetFloatParameter(
                svc.service,
                0,
                svc.key,
                super::percent_to_float(percent),
            ) == 0
        }
    }

    /// A panel exists (CG says built-in display present) — independent of
    /// whether we can drive it.
    pub fn exists() -> bool {
        crate::display::cg_builtin_id().is_some()
    }

    /// Whether the classic path can actually drive this panel. False on the
    /// new backlight architecture; the UI then shows the panel without a
    /// working slider instead of pretending.
    pub fn controllable() -> bool {
        find_backlight_service().is_some()
    }
}

#[cfg(target_os = "macos")]
use iokit_backlight as builtin;

#[cfg(not(target_os = "macos"))]
mod builtin {
    // Built-in backlight control is macOS-only for now. Windows CI compiles
    // this module, and the UI shows externals only there.
    pub fn get() -> Option<u8> {
        None
    }
    pub fn set(_percent: u8) -> bool {
        false
    }
    pub fn exists() -> bool {
        false
    }
    pub fn controllable() -> bool {
        false
    }
}

#[cfg(target_os = "macos")]
mod core_graphics {
    pub type CgDirectDisplayId = u32;
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub fn CGGetActiveDisplayList(
            max: u32,
            displays: *mut CgDirectDisplayId,
            count: *mut u32,
        ) -> i32;
        pub fn CGDisplayIsBuiltin(id: CgDirectDisplayId) -> u32;
        pub fn CGMainDisplayID() -> CgDirectDisplayId;
    }
}

// ─────────── built-in fallback: gamma dimming ───────────
//
// When the backlight can't be driven (new-backlight-architecture blocks
// every API, permissioned or not), the honest remaining lever is the display
// transfer table: scale the video signal the panel receives. It is NOT the
// backlight — no battery saving, and deep dims crush blacks — so the UI says
// "software dimming" outright. The original table is captured before the
// first change and restored on app exit so the screen is never left dimmed
// with no obvious way back.

#[cfg(target_os = "macos")]
mod gamma_dim {
    use std::sync::Mutex;

    type CgDirectDisplayId = u32;
    type CgError = i32;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGGetDisplayTransferByTable(
            display: CgDirectDisplayId,
            capacity: u32,
            red: *mut f32,
            green: *mut f32,
            blue: *mut f32,
            sample_count: *mut u32,
        ) -> CgError;
        // The public CGDisplaySetDisplayTransferByTable is gone on current
        // macOS; the CGS-prefixed private export in the same image does the
        // identical job with the same argument order. Verified against the
        // live table: set 0.4 -> read-back 0.5010*0.4 exactly, restore exact.
        fn CGSSetDisplayTransferByTable(
            display: CgDirectDisplayId,
            table_size: u32,
            red: *const f32,
            green: *const f32,
            blue: *const f32,
        ) -> CgError;
    }

    const CAPACITY: u32 = 512;

    struct Original {
        display: CgDirectDisplayId,
        red: Vec<f32>,
        green: Vec<f32>,
        blue: Vec<f32>,
    }

    fn read_table(display: CgDirectDisplayId) -> Option<(Vec<f32>, Vec<f32>, Vec<f32>)> {
        unsafe {
            let mut red = vec![0f32; CAPACITY as usize];
            let mut green = vec![0f32; CAPACITY as usize];
            let mut blue = vec![0f32; CAPACITY as usize];
            let mut n: u32 = 0;
            let r = CGGetDisplayTransferByTable(
                display,
                CAPACITY,
                red.as_mut_ptr(),
                green.as_mut_ptr(),
                blue.as_mut_ptr(),
                &mut n,
            );
            if r != 0 || n == 0 {
                return None;
            }
            red.truncate(n as usize);
            green.truncate(n as usize);
            blue.truncate(n as usize);
            Some((red, green, blue))
        }
    }

    fn write_table(display: CgDirectDisplayId, r: &[f32], g: &[f32], b: &[f32]) -> bool {
        debug_assert_eq!(r.len(), g.len());
        debug_assert_eq!(g.len(), b.len());
        unsafe {
            CGSSetDisplayTransferByTable(
                display,
                r.len() as u32,
                r.as_ptr(),
                g.as_ptr(),
                b.as_ptr(),
            ) == 0
        }
    }

    /// Original table captured before our first modification, plus the last
    /// factor we applied (1.0 = untouched).
    static ORIGINAL: Mutex<Option<Original>> = Mutex::new(None);
    static LAST: Mutex<f64> = Mutex::new(1.0);

    pub fn supported(display: CgDirectDisplayId) -> bool {
        read_table(display).is_some()
    }

    pub fn get_percent() -> u8 {
        (*LAST.lock().unwrap() * 100.0).round().clamp(0.0, 100.0) as u8
    }

    pub fn set_percent(display: CgDirectDisplayId, percent: u8) -> bool {
        let Some((mut r, mut g, mut b)) = read_table(display) else {
            return false;
        };
        {
            let mut orig = ORIGINAL.lock().unwrap();
            if orig.is_none() {
                // First touch: whatever is running now (Night Shift, True
                // Tone) becomes the base we scale and later restore.
                *orig = Some(Original {
                    display,
                    red: r.clone(),
                    green: g.clone(),
                    blue: b.clone(),
                });
            }
        }
        let factor = percent as f32 / 100.0;
        for ch in [&mut r, &mut g, &mut b] {
            for v in ch.iter_mut() {
                *v = (*v * factor).clamp(0.0, 1.0);
            }
        }
        let ok = write_table(display, &r, &g, &b);
        if ok {
            *LAST.lock().unwrap() = percent as f64 / 100.0;
        }
        ok
    }

    /// Put the captured original back. Called on app exit so the panel is
    /// never left dimmed with no obvious way back.
    pub fn restore() {
        let orig = ORIGINAL.lock().unwrap().take();
        if let Some(o) = orig {
            if write_table(o.display, &o.red, &o.green, &o.blue) {
                *LAST.lock().unwrap() = 1.0;
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub fn restore_builtin_gamma() {
    gamma_dim::restore();
}

#[cfg(not(target_os = "macos"))]
pub fn restore_builtin_gamma() {}

#[cfg(target_os = "macos")]
pub(crate) fn cg_builtin_id() -> Option<u32> {
    use core_graphics::*;
    unsafe {
        let mut ids = [0u32; 8];
        let mut n: u32 = 0;
        if CGGetActiveDisplayList(8, ids.as_mut_ptr(), &mut n) != 0 {
            return None;
        }
        ids[..n as usize]
            .iter()
            .copied()
            .find(|id| CGDisplayIsBuiltin(*id) != 0)
    }
}

#[cfg(target_os = "macos")]
fn builtin_is_main() -> bool {
    use core_graphics::*;
    cg_builtin_id()
        .map(|id| unsafe { CGMainDisplayID() } == id)
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn builtin_is_main() -> bool {
    false
}

// ─────────── externals (DDC/CI via ddc-hi) ───────────

/// Stable id derived from EDID identity, not enumeration order.
fn ddc_id(info: &ddc_hi::DisplayInfo) -> String {
    format!(
        "ddc:{}:{}:{}",
        info.manufacturer_id.as_deref().unwrap_or("?"),
        info.model_id.map(|m| format!("{m:x}")).unwrap_or_else(|| "?".into()),
        info.serial.map(|s| s.to_string()).unwrap_or_else(|| "?".into()),
    )
}

fn display_name(d: &ddc_hi::Display, index: usize) -> String {
    // Every source can legitimately be empty — some monitors ship EDID
    // without a product name and ddc-macos' description() then yields "".
    let mfr_model = |name: String| {
        let model = d
            .info
            .model_id
            .map(|m| format!("{m:x}"))
            .unwrap_or_default();
        match (name.is_empty(), model.is_empty()) {
            (false, false) => format!("{name} ({model})"),
            (false, true) => name,
            (true, false) => format!("Display {model}"),
            (true, true) => format!("Display {}", index + 1),
        }
    };
    d.info
        .model_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| {
            d.info
                .manufacturer_id
                .clone()
                .filter(|m| !m.trim().is_empty())
        })
        .map(mfr_model)
        .unwrap_or_else(|| format!("Display {}", index + 1))
}

#[cfg(target_os = "macos")]
fn main_display_identity() -> Option<(u32, u32)> {
    // CGDisplay vendor/model numbers match EDID vendor/product well enough
    // to tell which external carries the menu bar.
    type CgDirectDisplayId = u32;
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGMainDisplayID() -> CgDirectDisplayId;
        fn CGDisplayIsBuiltin(id: CgDirectDisplayId) -> u32;
        fn CGDisplayVendorNumber(id: CgDirectDisplayId) -> u32;
        fn CGDisplayModelNumber(id: CgDirectDisplayId) -> u32;
    }
    unsafe {
        let main = CGMainDisplayID();
        if CGDisplayIsBuiltin(main) != 0 {
            return None; // main is the built-in; no external gets the badge
        }
        Some((CGDisplayVendorNumber(main), CGDisplayModelNumber(main)))
    }
}

#[cfg(not(target_os = "macos"))]
fn main_display_identity() -> Option<(u32, u32)> {
    None
}

pub fn list() -> Vec<DisplayStatus> {
    let mut out = Vec::new();

    if builtin::exists() {
        // Preference order: real backlight first, gamma dimming as the
        // fallback. Gamma always works but is software — labelled as such.
        let backlight_ok = builtin::controllable();
        let gamma_ok = cfg!(target_os = "macos")
            && cg_builtin_id()
                .map(|id| gamma_dim::supported(id))
                .unwrap_or(false);
        out.push(DisplayStatus {
            id: BUILTIN_ID.into(),
            name: String::new(), // the UI labels the built-in by kind
            kind: "builtin".into(),
            is_main: builtin_is_main(),
            brightness: if backlight_ok {
                builtin::get()
            } else if gamma_ok {
                Some(gamma_dim::get_percent())
            } else {
                None
            },
            power: None,
            controllable: backlight_ok || gamma_ok,
            method: if backlight_ok {
                "backlight".into()
            } else if gamma_ok {
                "gamma".into()
            } else {
                "none".into()
            },
        });
    }

    for (index, d) in ddc_hi::Display::enumerate().into_iter().enumerate() {
        let mut d = d;
        let brightness = d
            .handle
            .get_vcp_feature(VCP_LUMINANCE)
            .ok()
            .map(|v| clamp_percent(v.value() as i64));
        let power = d.handle.get_vcp_feature(VCP_POWER).ok().map(|v| {
            // Some monitors report transient values (2/3/5); only 4 means off.
            v.value() != POWER_OFF as u16
        });
        let id = ddc_id(&d.info);
        let is_main = main_display_identity().map(|(vendor, model)| {
            // CG model number packs vendor in high bits on some systems;
            // compare low 16 bits against the EDID product id.
            let m = model & 0xffff;
            let v_ok = d
                .info
                .manufacturer_id
                .as_deref()
                .map(|_| true)
                .unwrap_or(false);
            let _ = v_ok;
            d.info.model_id == Some(m as u16) || vendor == 0 && m == 0
        });
        out.push(DisplayStatus {
            id,
            name: display_name(&d, index),
            kind: "external".into(),
            is_main: is_main.unwrap_or(false),
            brightness,
            power,
            controllable: true,
            method: "ddc".into(),
        });
    }

    out
}

pub fn set_brightness(id: &str, percent: u8) -> io::Result<()> {
    let percent = clamp_percent(percent as i64);
    if id == BUILTIN_ID {
        if builtin::set(percent) {
            return Ok(());
        }
        if cfg!(target_os = "macos") {
            if let Some(display) = cg_builtin_id() {
                if gamma_dim::set_percent(display, percent) {
                    return Ok(());
                }
            }
        }
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "built-in display not controllable on this machine",
        ));
    }
    for mut d in ddc_hi::Display::enumerate() {
        if ddc_id(&d.info) == id {
            d.handle
                .set_vcp_feature(VCP_LUMINANCE, percent as u16)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            return Ok(());
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "display not found"))
}

pub fn set_power(id: &str, on: bool) -> io::Result<()> {
    if id == BUILTIN_ID {
        // Turning the built-in panel "off" via backlight 0 is not a power
        // state and would be a lie; macOS sleep is the honest equivalent and
        // not ours to trigger from a slider. Externals only, like Lunar.
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "power control is DDC-only; the built-in display has no DDC",
        ));
    }
    let value = if on { POWER_ON } else { POWER_OFF };
    for mut d in ddc_hi::Display::enumerate() {
        if ddc_id(&d.info) == id {
            d.handle
                .set_vcp_feature(VCP_POWER, value)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            return Ok(());
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "display not found"))
}

// ─────────── Tauri commands ───────────

#[tauri::command]
pub fn list_displays() -> Vec<DisplayStatus> {
    list()
}

#[tauri::command]
pub fn set_display_brightness(id: String, value: i64) -> Result<(), String> {
    set_brightness(&id, clamp_percent(value)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_display_power(id: String, on: bool) -> Result<(), String> {
    set_power(&id, on).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_clamps_to_ddc_range() {
        assert_eq!(clamp_percent(-5), 0);
        assert_eq!(clamp_percent(0), 0);
        assert_eq!(clamp_percent(64), 64);
        assert_eq!(clamp_percent(100), 100);
        assert_eq!(clamp_percent(300), 100);
    }

    #[test]
    fn builtin_float_roundtrips_through_percent() {
        assert!((percent_to_float(50) - 0.5).abs() < 1e-6);
        assert_eq!(float_to_percent(0.0), 0);
        assert_eq!(float_to_percent(1.0), 100);
        assert_eq!(float_to_percent(0.555), 56);
    }

    #[test]
    fn builtin_id_cannot_collide_with_ddc_ids() {
        assert!(ddc_id(&ddc_hi::DisplayInfo::new(
            ddc_hi::Backend::MacOS,
            "builtin".into()
        ))
        .starts_with("ddc:"));
        assert_ne!(BUILTIN_ID, "ddc:");
    }
}
