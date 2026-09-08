// Display control.
//
// Two different mechanisms, because macOS treats them differently:
//
// - External displays speak DDC/CI, so brightness (VCP 0x10) and power
//   (VCP 0xD6, the same code Lunar's BlackOut uses) go through ddc-hi.
//   That works on macOS and Windows over HDMI/DP/USB-C.
// - The built-in panel has no DDC. Classic IOKit is tried first; on new
//   AppleARMBacklight Macs, real hardware brightness is driven locally through
//   Control Center's Accessibility UI, with gamma kept as a separate second-
//   stage dimmer. ddc-hi deliberately excludes built-ins.
//
// DDC is slow (tens to hundreds of ms) and some monitors don't ack reads,
// so reads degrade to `None` instead of failing the listing, and every
// command is fire-and-forget from the UI's point of view.

use ddc_hi::Ddc as _;
use serde::Serialize;
use std::io;
use std::sync::{mpsc, OnceLock};
#[cfg(target_os = "macos")]
use tauri::Manager as _;

const VCP_LUMINANCE: u8 = 0x10;
const VCP_POWER: u8 = 0xd6;
const POWER_ON: u16 = 0x01;
const POWER_OFF: u16 = 0x04;
/// MCCS defines both 0x04 and 0x05 as off. Monitors implement one, the other,
/// or both, so trying only 0x04 left whole models with a dead power button.
/// 0x05 is the dangerous one: monitors that implement it literally cut the
/// scaler, which takes DDC down with it, and then nothing but the button on the
/// bezel brings the panel back. It is a last resort, never a reflex.
const POWER_OFF_HARD: u16 = 0x05;

/// The value to send to turn `advertised` off, given the 0xD6 values it claims
/// on its capability string. `None` when it claims none of them.
///
/// The two failures here are not the same size. Withholding 0x05 costs a
/// monitor its off button; sending it costs the user a walk to the bezel,
/// because the panels that implement 0x05 take DDC down with the scaler and
/// nothing this app sends afterwards is heard. So 0x05 goes only to a monitor
/// that advertises it *instead of* 0x04, where it is the only off there is.
fn power_off_value(advertised: &[u8]) -> Option<u16> {
    if advertised.is_empty() || advertised.contains(&(POWER_OFF as u8)) {
        return Some(POWER_OFF);
    }
    advertised
        .contains(&(POWER_OFF_HARD as u8))
        .then_some(POWER_OFF_HARD)
}

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
    /// The system backlight level 0-100, readable from the tracked tap or
    /// the registry snapshot. The UI shows this separately from the gamma
    /// slider so the user can see which layer is dimming the screen.
    pub system_level: Option<u8>,
}

#[derive(Serialize, Clone, Debug)]
pub struct BuiltinBrightnessSync {
    pub brightness: u8,
    pub system_level: u8,
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
        let key = unsafe { make_key()? };
        for_each_backlight_service(|service| unsafe {
            let mut probe: f32 = 0.0;
            if IODisplayGetFloatParameter(service, 0, key, &mut probe) == 0 {
                // GetFloatParameter answering means Set will too.
                Some(BacklightService { service, key })
            } else {
                None
            }
        })
    }

    unsafe fn make_key() -> Option<CfStringRef> {
        let key = CFStringCreateWithCString(
            std::ptr::null(),
            BACKLIGHT_KEY.as_ptr() as *const c_char,
            KCF_STRING_ENCODING_UTF8,
        );
        (!key.is_null()).then_some(key)
    }

    /// Run `f` over the services of the display-related classes. Ownership of
    /// a returned service handle transfers to the caller; everything else is
    /// released here. The service is kept alive by its retain count from the
    /// iterator, exactly as before.
    fn for_each_backlight_service<T>(mut f: impl FnMut(IoObject) -> Option<T>) -> Option<T> {
        unsafe {
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
                loop {
                    let service = IOIteratorNext(iterator);
                    if service == 0 {
                        break;
                    }
                    if let Some(found) = f(service) {
                        IOObjectRelease(iterator);
                        return Some(found);
                    }
                    IOObjectRelease(service);
                }
                IOObjectRelease(iterator);
            }
            None
        }
    }

    /// The system backlight level 0.0-1.0, read from the registry the same
    /// way `ioreg` shows it: IODisplayParameters -> brightness -> value/max.
    /// This is what the keyboard keys change, so reading it is what makes the
    /// slider follow them.
    pub(super) fn system_backlight_level() -> Option<f64> {
        use core_foundation::base::TCFType;
        use core_foundation::dictionary::CFDictionary;
        use core_foundation::number::CFNumber;
        use core_foundation::string::CFString;

        #[link(name = "IOKit", kind = "framework")]
        extern "C" {
            fn IORegistryEntryCreateCFProperty(
                entry: IoObject,
                key: CfStringRef,
                allocator: *const (),
                options: u32,
            ) -> *const ();
        }

        unsafe {
            let params_key = CFString::new("IODisplayParameters");
            for_each_backlight_service(|service| {
                let dict = IORegistryEntryCreateCFProperty(
                    service,
                    params_key.as_concrete_TypeRef() as *const (),
                    std::ptr::null(),
                    0,
                );
                if dict.is_null() {
                    return None;
                }
                let params: CFDictionary =
                    <CFDictionary as TCFType>::wrap_under_create_rule(dict as *mut _);
                let bkey = CFString::new("brightness");
                let bref = core_foundation::dictionary::CFDictionaryGetValue(
                    params.as_concrete_TypeRef(),
                    bkey.as_concrete_TypeRef() as *const _,
                );
                if bref.is_null() {
                    return None;
                }
                // CFDictionaryGetValue is a GET-rule reference: wrapping it
                // create-rule would over-release on drop and crash.
                let bdict: CFDictionary =
                    <CFDictionary as TCFType>::wrap_under_get_rule(bref as *mut _);
                let num = |name: &str| -> Option<f64> {
                    let k = CFString::new(name);
                    let v = core_foundation::dictionary::CFDictionaryGetValue(
                        bdict.as_concrete_TypeRef(),
                        k.as_concrete_TypeRef() as *const _,
                    );
                    if v.is_null() {
                        return None;
                    }
                    let n: CFNumber =
                        core_foundation::number::CFNumber::wrap_under_get_rule(v as *mut _);
                    n.to_f64()
                };
                let (value, max) = (num("value")?, num("max")?);
                if max <= 0.0 {
                    return None;
                }
                Some((value / max).clamp(0.0, 1.0))
            })
        }
    }

    pub fn get() -> Option<u8> {
        let svc = find_backlight_service()?;
        let mut v: f32 = 0.0;
        let ok = unsafe { IODisplayGetFloatParameter(svc.service, 0, svc.key, &mut v) == 0 };
        ok.then(|| super::float_to_percent(v))
    }

    pub fn set(percent: u8) -> bool {
        let Some(svc) = find_backlight_service() else {
            return false;
        };
        unsafe {
            IODisplaySetFloatParameter(svc.service, 0, svc.key, super::percent_to_float(percent))
                == 0
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
    ///
    /// Cached: the answer is a property of the machine, and the probe walks the
    /// IOKit registry — which the 250ms brightness poll used to redo every tick.
    pub fn controllable() -> bool {
        static CONTROLLABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CONTROLLABLE.get_or_init(|| find_backlight_service().is_some())
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
//
// The slider is an ABSOLUTE brightness that tracks the system: the real
// backlight level S is read live from IORegistry, the applied gamma offset g
// only changes when the user drags, and the screen's total is S x g. Pressing
// the system keys changes S, so the slider follows; we never rewrite g in
// response, which keeps auto-brightness and the keys behaving natively
// instead of being fought by a compensation loop.

#[cfg(target_os = "macos")]
mod gamma_dim {
    use std::collections::BTreeMap;
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

    /// Per-display original tables captured before our first modification, plus
    /// the last factor applied to each (1.0 = untouched). This is keyed by
    /// display because the gamma path is also the fallback for external
    /// monitors that do not answer DDC; a single global slot would let one
    /// panel's original table be restored onto another.
    static ORIGINAL: Mutex<BTreeMap<CgDirectDisplayId, Original>> = Mutex::new(BTreeMap::new());
    static LAST: Mutex<BTreeMap<CgDirectDisplayId, f64>> = Mutex::new(BTreeMap::new());

    pub fn supported(display: CgDirectDisplayId) -> bool {
        read_table(display).is_some()
    }

    /// The system backlight level 0.0-1.0, straight from the registry.
    /// This is the value the keyboard keys change.
    pub fn system_level() -> Option<f64> {
        super::iokit_backlight::system_backlight_level()
    }

    /// Current slider value = the gamma offset alone. The slider is 0-100%
    /// of the AVAILABLE light: 100% means no dimming (whatever the backlight
    /// gives, we show all of it), 0% is fully dark, and the system backlight
    /// is a separate base layer the user adjusts with F1/F2. Pressing a key
    /// resets gamma to 1.0, so the slider always lands at 100% after a key
    /// press — no ceiling below 100%.
    pub fn total_percent(display: CgDirectDisplayId) -> Option<u8> {
        Some(get_percent(display))
    }

    pub fn get_percent(display: CgDirectDisplayId) -> u8 {
        let factor = LAST.lock().unwrap().get(&display).copied().unwrap_or(1.0);
        (factor * 100.0).round().clamp(0.0, 100.0) as u8
    }

    /// Scale the CAPTURED ORIGINAL by the factor — never the live table.
    ///
    /// The first version multiplied the current table on every set, so each
    /// drag compounded: 40% then 60% landed at orig×0.4×0.4×0.6… and the
    /// slider could only ever make the screen darker, no matter which way it
    /// moved. That is exactly the reported one-directional bug.
    pub fn set_percent(display: CgDirectDisplayId, percent: u8) -> bool {
        set_gamma_offset(display, percent as f64 / 100.0)
    }

    /// Slider entry: percent is how much of the available light to show.
    /// 100% = gamma 1.0 (no dimming), 0% = gamma 0.0 (fully dark). The
    /// system backlight is a base the user sets with F1/F2; our slider only
    /// dims below it. There is no sub-100% ceiling — 100% is always reachable.
    pub fn set_absolute(display: CgDirectDisplayId, percent: u8) -> bool {
        set_gamma_offset(display, percent as f64 / 100.0)
    }

    fn set_gamma_offset(display: CgDirectDisplayId, factor: f64) -> bool {
        let (r, g, b) = {
            // First touch of this panel: whatever is running now (Night Shift,
            // True Tone) becomes the base we scale and later restore.
            let mut originals = ORIGINAL.lock().unwrap();
            if !originals.contains_key(&display) {
                let Some((red, green, blue)) = read_table(display) else {
                    return false;
                };
                originals.insert(display, Original { red, green, blue });
            }
            let o = &originals[&display];
            let factor = factor as f32;
            (
                scale_table(&o.red, factor),
                scale_table(&o.green, factor),
                scale_table(&o.blue, factor),
            )
        };
        let ok = write_table(display, &r, &g, &b);
        if ok {
            LAST.lock().unwrap().insert(display, factor);
        }
        ok
    }

    /// Scale a captured table by a factor, clamped to the legal 0.0-1.0 range.
    pub(super) fn scale_table(src: &[f32], factor: f32) -> Vec<f32> {
        src.iter().map(|v| (v * factor).clamp(0.0, 1.0)).collect()
    }

    /// Reset one panel's gamma offset to 1.0 (system brightness keys take
    /// over). Keeps the captured original for future drags — no drop needed.
    pub fn reset_offset(display: CgDirectDisplayId) {
        let originals = ORIGINAL.lock().unwrap();
        let Some(o) = originals.get(&display) else {
            log::info!("reset_offset: no original captured — nothing to reset");
            return;
        };
        let (r, g, b) = (o.red.clone(), o.green.clone(), o.blue.clone());
        drop(originals);
        log::info!(
            "reset_offset: write_table disp={} len={} first={:.4}",
            display,
            r.len(),
            r[0]
        );
        let ok = write_table(display, &r, &g, &b);
        log::info!("reset_offset: write_table ok={}", ok);
        if ok {
            LAST.lock().unwrap().insert(display, 1.0);
        }
    }

    /// Put every captured original back. Called on app exit so no panel is
    /// left dimmed with no obvious way back.
    pub fn restore() {
        let originals = std::mem::take(&mut *ORIGINAL.lock().unwrap());
        for (display, o) in originals {
            if write_table(display, &o.red, &o.green, &o.blue) {
                LAST.lock().unwrap().insert(display, 1.0);
            }
        }
    }
}

// ─────────── brightness-key tap ───────────
//
// The registry's brightness dict is a static snapshot on this
// new-backlight-architecture Mac — it never moves when the keys are pressed
// (verified with a 10-minute poll), so it can't drive the sync. Instead we
// listen for the brightness NX_SYSDEFINED events themselves: a listen-only
// session event tap observes them fine (verified with a synthetic event),
// and each press steps our tracked system level by one macOS division.

/// A brightness key changes the built-in panel's own backlight, so only the
/// built-in offset is dropped; an external monitor dimmed in software must
/// keep its offset.
#[cfg(target_os = "macos")]
fn reset_gamma_offset() {
    if let Some(display) = cg_builtin_id() {
        gamma_dim::reset_offset(display);
    }
}

#[cfg(target_os = "macos")]
pub mod brightness_tap {
    use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};

    type CfAllocatorRef = *const ();
    type CfMachPortRef = *mut ();
    type CfRunLoopRef = *mut ();
    type CfRunLoopSourceRef = *mut ();
    type CfStringRef = *const ();
    type MachPort = u32;

    type CGEventTapProxy = *const ();
    type CGEventType = u32;
    type CGEventRef = *mut ();
    type CGEventMask = u64;
    type CGEventTapLocation = u32;
    type CGEventTapPlacement = u32;
    type CGEventTapOptions = u32;

    const NX_SYSDEFINED: u32 = 14;
    const K_CG_SESSION_EVENT_TAP: CGEventTapLocation = 1;
    const K_CG_HEAD_INSERT_EVENT_TAP: CGEventTapPlacement = 0;
    // kCGEventTapOptionListenOnly == 1 (Default is 0). Passing 2 is an
    // invalid option and CGEventTapCreate returns NULL — the whole
    // "unavailable after retries" chase was this one constant.
    const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: CGEventTapOptions = 1;

    /// System level in sixteenths (1..=16), i.e. the macOS brightness
    /// divisions. Seeded from the registry snapshot, then stepped by keys.
    pub static SYSTEM_SIXTEENTHS: AtomicU8 = AtomicU8::new(0);

    pub fn seed_sixteenths(fraction: f64) {
        let v = (fraction * 16.0).round().clamp(1.0, 16.0) as u8;
        SYSTEM_SIXTEENTHS.store(v, Ordering::Relaxed);
    }

    pub fn system_level() -> f64 {
        let s = SYSTEM_SIXTEENTHS.load(Ordering::Relaxed);
        if s == 0 {
            return 1.0;
        }
        s as f64 / 16.0
    }

    /// True until the registry seed has landed. The seed can fail during
    /// early setup (service not yet publishing), and without this the
    /// fallback level 1.0 shows the built-in at 100% when the system is at
    /// 50% — exactly what shipped. The 250ms sync poll retries the seed.
    pub fn unseeded() -> bool {
        SYSTEM_SIXTEENTHS.load(Ordering::Relaxed) == 0
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventTapCreate(
            tap: CGEventTapLocation,
            place: CGEventTapPlacement,
            options: CGEventTapOptions,
            events_of_interest: CGEventMask,
            callback: extern "C" fn(
                proxy: CGEventTapProxy,
                ty: CGEventType,
                event: CGEventRef,
                user_info: *mut (),
            ) -> *mut CGEventRef,
            user_info: *mut (),
        ) -> CfMachPortRef;
        fn CGEventTapEnable(tap: CfMachPortRef, enable: bool);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFMachPortCreateRunLoopSource(
            allocator: CfAllocatorRef,
            port: CfMachPortRef,
            order: isize,
        ) -> CfRunLoopSourceRef;
        fn CFRunLoopGetCurrent() -> CfRunLoopRef;
        fn CFRunLoopAddSource(rl: CfRunLoopRef, source: CfRunLoopSourceRef, mode: CfStringRef);
        fn CFRunLoopRun();
        static kCFRunLoopDefaultMode: CfStringRef;
    }

    /// Read NX data1 through NSEvent — CGEvent does not expose it.
    ///
    /// objc_msgSend is declared per-arity as NON-variadic externs. Casting it
    /// to a C-variadic fn pointer and calling that is UB on arm64 and crashed
    /// the tap thread (SIGSEGV in objc_msgSend, three overnight .ips reports)
    /// — arity-specific aliases are what the objc crates themselves use.
    unsafe fn nx_data1(event: CGEventRef) -> Option<i64> {
        use objc2::runtime::{AnyObject, Sel};
        extern "C" {
            fn objc_getClass(name: *const std::ffi::c_char) -> *mut AnyObject;
            #[link_name = "objc_msgSend"]
            fn msg_send_event(cls: *const AnyObject, sel: Sel, cg: CGEventRef) -> *const AnyObject;
            #[link_name = "objc_msgSend"]
            fn msg_send_d1(ev: *const AnyObject, sel: Sel) -> isize;
        }
        let cls = objc_getClass(b"NSEvent\0".as_ptr() as *const std::ffi::c_char);
        if cls.is_null() {
            return None;
        }
        let ev = msg_send_event(cls, Sel::register(c"eventWithCGEvent:"), event);
        if ev.is_null() {
            return None;
        }
        Some(msg_send_d1(ev, Sel::register(c"data1")) as i64)
    }

    extern "C" fn tap_callback(
        _proxy: CGEventTapProxy,
        ty: CGEventType,
        event: CGEventRef,
        _user_info: *mut (),
    ) -> *mut CGEventRef {
        if ty == NX_SYSDEFINED {
            let step = unsafe {
                nx_data1(event).map(|d1| {
                    let key = (d1 >> 16) & 0xffff;
                    match key {
                        3 => Some(1i32),  // brightness up
                        4 => Some(-1i32), // brightness down
                        _ => None,
                    }
                })
            };
            if let Some(Some(delta)) = step {
                let cur = SYSTEM_SIXTEENTHS.load(Ordering::Relaxed);
                let next = (cur as i32 + delta).clamp(1, 16) as u8;
                SYSTEM_SIXTEENTHS.store(next, Ordering::Relaxed);
                note_key(delta);
                log::info!(
                    "tap: brightness key delta={} cur={} -> {}",
                    delta,
                    cur,
                    next
                );
                // Schedule the gamma reset on the NEXT main-runloop pass.
                unsafe {
                    dispatch_async_f(
                        &_dispatch_main_q as *const () as *mut (),
                        std::ptr::null_mut(),
                        do_gamma_reset,
                    );
                }
            }
        }
        std::ptr::null_mut()
    }

    // ── key-press notification for the app layer ──
    //
    // The callback runs on the tap thread; the closure runs there too, so it
    // must be Send. Cloning the AppHandle out of the box each time would need
    // Sync, so instead the callback only flips an atomic and the app layer
    // reads it from its own poll — no cross-thread closure call at all.
    pub static KEY_STEPS: AtomicI32 = AtomicI32::new(0);

    fn note_key(delta: i32) {
        KEY_STEPS.fetch_add(delta, Ordering::Relaxed);
    }

    // Create the tap ON the main runloop — the only place event taps are
    // reliably created. A bare secondary thread's creation fails outright,
    // and Tauri's setup() runs before the event loop services anything, so
    // attempts are paced: main thread creates, a helper thread only sleeps
    // and re-schedules the next attempt on main. Once created, the main
    // runloop services the tap for the app's lifetime.
    // dispatch functions live in libSystem (linked by default on macOS).
    // dispatch_get_main_queue is a macro for the _dispatch_main_q global —
    // dlsym("dispatch_get_main_queue") returns NULL while the underlying
    // symbol resolves fine.
    extern "C" {
        static _dispatch_main_q: ();
        fn dispatch_async_f(queue: *mut (), context: *mut (), work: extern "C" fn(*mut ()));
    }

    /// Runs on the main queue (main runloop) AFTER the tap callback has
    /// returned — no WindowServer re-entrancy. This is the only place the
    /// gamma reset can safely happen in response to a brightness key.
    extern "C" fn do_gamma_reset(_ctx: *mut ()) {
        log::info!("do_gamma_reset: calling reset_offset");
        super::reset_gamma_offset();
        log::info!("do_gamma_reset: done");
    }

    pub fn start(app: &tauri::AppHandle) {
        use std::sync::atomic::AtomicU32;
        static ATTEMPTS: AtomicU32 = AtomicU32::new(0);
        attempt(app);
        fn attempt(app: &tauri::AppHandle) {
            let app = app.clone();
            let app_for_retry = app.clone();
            let spawned = app.run_on_main_thread(move || unsafe {
                let tap = CGEventTapCreate(
                    K_CG_SESSION_EVENT_TAP,
                    K_CG_HEAD_INSERT_EVENT_TAP,
                    K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
                    1u64 << NX_SYSDEFINED,
                    tap_callback,
                    std::ptr::null_mut(),
                );
                if !tap.is_null() {
                    let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
                    if !source.is_null() {
                        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
                        CGEventTapEnable(tap, true);
                        log::info!("brightness key tap running on main runloop");
                        return;
                    }
                    log::info!("brightness tap: source create failed");
                    return;
                }
                let n = ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n < 20 {
                    let app2 = app_for_retry.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        attempt(&app2);
                    });
                } else {
                    log::info!("brightness key tap unavailable after retries");
                }
            });
            if let Err(e) = spawned {
                log::info!("brightness tap: main dispatch failed: {e}");
            }
        }
    }
}

/// Seed the tap-tracked system level from the registry snapshot and start
/// listening for the brightness keys. Call from app setup.
#[cfg(target_os = "macos")]
pub fn start_brightness_sync(app: &tauri::AppHandle) {
    seed_system_level();
    brightness_tap::start(app);
}

#[cfg(target_os = "macos")]
fn seed_system_level() {
    if let Some(f) = iokit_backlight::system_backlight_level() {
        brightness_tap::seed_sixteenths(f);
    }
}

#[cfg(target_os = "macos")]
pub fn restore_builtin_gamma() {
    gamma_dim::restore();
}

#[cfg(not(target_os = "macos"))]
pub fn restore_builtin_gamma() {}

/// cfg!() is a runtime bool, so cfg-gated helper functions are how the
/// Windows build avoids resolving the macOS-only symbols at all.
#[cfg(target_os = "macos")]
fn builtin_gamma_supported() -> bool {
    cg_builtin_id()
        .map(|id| gamma_dim::supported(id))
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn builtin_gamma_supported() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn builtin_gamma_set(percent: u8) -> bool {
    cg_builtin_id()
        .map(|display| gamma_dim::set_absolute(display, percent))
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn builtin_gamma_set(_percent: u8) -> bool {
    false
}

/// Live total brightness of the built-in panel: system backlight level x our
/// gamma offset. This is what the slider displays, and what changes when the
/// user presses the keyboard keys — the sync the UI polls for.
#[cfg(target_os = "macos")]
fn builtin_total() -> Option<u8> {
    if builtin::controllable() {
        // Real backlight under our control: the native value is the total.
        return builtin::get();
    }
    gamma_dim::total_percent(cg_builtin_id()?)
}

#[cfg(not(target_os = "macos"))]
fn builtin_total() -> Option<u8> {
    None
}

#[cfg(target_os = "macos")]
fn builtin_gamma_percent() -> u8 {
    cg_builtin_id().map(gamma_dim::get_percent).unwrap_or(100)
}

#[cfg(not(target_os = "macos"))]
fn builtin_gamma_percent() -> u8 {
    100
}

#[cfg(target_os = "macos")]
fn accessibility_backlight_level() -> Option<u8> {
    // Cached: the live read is an inter-process accessibility round trip and
    // this runs on every list and every 250ms sync tick.
    crate::accessibility_backlight::level_cached()
}

#[cfg(not(target_os = "macos"))]
fn accessibility_backlight_level() -> Option<u8> {
    None
}

#[cfg(target_os = "macos")]
fn tracked_system_level_percent() -> u8 {
    (brightness_tap::system_level() * 100.0).round() as u8
}

#[cfg(not(target_os = "macos"))]
fn tracked_system_level_percent() -> u8 {
    100
}

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

/// macOS leaves the EDID identity fields empty for most external monitors:
/// `IODisplayEDIDOriginal` is absent on the USB-C/DisplayPort path, so ddc-hi
/// falls back to a name-only `DisplayInfo` and every monitor would share the id
/// `ddc:?:?:?`. Two of them then collide, every write lands on the first one,
/// and the second monitor's slider looks broken. The CoreGraphics display id is
/// unique per attached panel, so it is what keeps the card id unique.
fn ddc_id(info: &ddc_hi::DisplayInfo, cg_id: Option<u32>) -> String {
    let identity = format!(
        "{}:{}:{}",
        info.manufacturer_id.as_deref().unwrap_or("?"),
        info.model_id
            .map(|m| format!("{m:x}"))
            .unwrap_or_else(|| "?".into()),
        info.serial
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".into()),
    );
    match cg_id {
        Some(cg) => format!("ddc:{identity}:cg{cg}"),
        None => format!("ddc:{identity}"),
    }
}

/// The CoreGraphics display id behind a ddc-hi handle. Also what the gamma
/// fallback needs in order to address a single panel.
#[cfg(target_os = "macos")]
fn ddc_cg_id(display: &ddc_hi::Display) -> Option<u32> {
    match &display.handle {
        ddc_hi::Handle::MacOS(monitor) => Some(monitor.handle().id),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
fn ddc_cg_id(_display: &ddc_hi::Display) -> Option<u32> {
    None
}

/// Software gamma dimming is a macOS CoreGraphics facility; on other platforms
/// a monitor that refuses DDC simply has no second path.
#[cfg(target_os = "macos")]
fn panel_gamma_supported(display: u32) -> bool {
    gamma_dim::supported(display)
}

#[cfg(not(target_os = "macos"))]
fn panel_gamma_supported(_display: u32) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn panel_gamma_set(display: u32, percent: u8) -> bool {
    gamma_dim::set_absolute(display, percent)
}

#[cfg(not(target_os = "macos"))]
fn panel_gamma_set(_display: u32, _percent: u8) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn panel_gamma_percent(display: u32) -> u8 {
    gamma_dim::get_percent(display)
}

#[cfg(not(target_os = "macos"))]
fn panel_gamma_percent(_display: u32) -> u8 {
    100
}

/// What a monitor can actually be driven with, from what it answered.
///
/// A luminance reading proves DDC. Otherwise software gamma is the only
/// remaining path, and the level it reports is the offset we applied — a
/// gamma-dimmed card that reported `None` showed an em dash and an empty
/// slider even though the monitor was being dimmed.
fn classify_external(
    ddc_level: Option<u8>,
    gamma_level: Option<u8>,
) -> (bool, &'static str, Option<u8>) {
    match (ddc_level, gamma_level) {
        (Some(level), _) => (true, "ddc", Some(level)),
        (None, Some(level)) => (true, "gamma", Some(level)),
        (None, None) => (false, "none", None),
    }
}

/// Whether a monitor that just missed a luminance read is still a DDC monitor.
///
/// A panel that is waking or retraining its link drops a read or two. Demoting
/// it to software dimming on the first miss relabelled a working monitor as
/// "software dimmed" and made its slider jump to the gamma level.
fn ddc_survives_miss(previous_method: &str, misses: u8) -> bool {
    previous_method == "ddc" && misses <= 1
}

/// Whether a VCP 0xD6 reading means the panel is actually lit.
///
/// Only 0x01 is on: 0x02 standby, 0x03 suspend and 0x04/0x05 off are all dark.
/// Testing `!= 0x04` reported a monitor in standby — which is where 0x04 puts
/// most of them — as still on, so the card said ON for a black screen.
fn ddc_power_is_on(value: u16) -> bool {
    value == POWER_ON
}

/// Recovers the CoreGraphics display id this crate encoded into a card id.
/// Only the tests read it back out; production carries `CachedDisplay::cg_id`.
#[cfg(test)]
fn cg_id_from(id: &str) -> Option<u32> {
    id.rsplit(':').next()?.strip_prefix("cg")?.parse().ok()
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

mod ddc_worker {
    use super::*;

    struct CachedDisplay {
        display: ddc_hi::Display,
        status: DisplayStatus,
        /// CoreGraphics id of the same panel: the address the software gamma
        /// fallback needs when the monitor does not answer DDC.
        cg_id: Option<u32>,
        keep_when_missing: bool,
        /// When this process last told the monitor to turn off. Our own command
        /// outranks the wire only briefly, while the panel is entering standby.
        off_at: Option<std::time::Instant>,
        /// Consecutive luminance reads this monitor has failed to answer.
        ddc_misses: u8,
        /// The 0xD6 values this monitor advertises, read once from its
        /// capability string. `None` until the first power command; an empty
        /// list means the monitor would not hand its capabilities over.
        power_values: Option<Vec<u8>>,
    }

    enum Request {
        List {
            reply: mpsc::Sender<Vec<DisplayStatus>>,
            /// Reuse the previous scan when it is younger than this. A DDC read
            /// costs tens to hundreds of milliseconds per monitor, so rescanning
            /// on every popover open is what made opening the panel feel slow.
            max_age: Option<std::time::Duration>,
        },
        Brightness {
            id: String,
            value: u8,
            reply: mpsc::Sender<io::Result<()>>,
        },
        Power {
            id: String,
            on: bool,
            reply: mpsc::Sender<io::Result<()>>,
        },
    }

    fn sender() -> &'static mpsc::Sender<Request> {
        static SENDER: OnceLock<mpsc::Sender<Request>> = OnceLock::new();
        SENDER.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("sayknow-ddc".into())
                .spawn(move || run(rx))
                .expect("failed to start DDC worker");
            tx
        })
    }

    /// A scan is reusable only when the caller allows an age and we actually
    /// have a previous scan that is younger than it. `None` always rescans.
    pub(super) fn cache_is_fresh(
        max_age: Option<std::time::Duration>,
        scanned_at: Option<std::time::Instant>,
    ) -> bool {
        match (max_age, scanned_at) {
            (Some(max_age), Some(at)) => at.elapsed() < max_age,
            _ => false,
        }
    }

    fn run(rx: mpsc::Receiver<Request>) {
        let mut displays = Vec::new();
        let mut scanned_at: Option<std::time::Instant> = None;
        while let Ok(request) = rx.recv() {
            match request {
                Request::List { reply, max_age } => {
                    let fresh = cache_is_fresh(max_age, scanned_at);
                    if !fresh {
                        refresh(&mut displays);
                        scanned_at = Some(std::time::Instant::now());
                    }
                    let _ = reply.send(
                        displays
                            .iter()
                            .map(|cached| cached.status.clone())
                            .collect(),
                    );
                }
                Request::Brightness { id, value, reply } => {
                    refresh_if_missing(&mut displays, &id);
                    let result = displays
                        .iter_mut()
                        .find(|cached| cached.status.id == id)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "display not found"))
                        .and_then(|cached| set_cached_brightness(cached, value));
                    let _ = reply.send(result);
                }
                Request::Power { id, on, reply } => {
                    refresh_if_missing(&mut displays, &id);
                    let result = displays
                        .iter_mut()
                        .find(|cached| cached.status.id == id)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "display not found"))
                        .and_then(|cached| set_cached_power(cached, on));
                    let _ = reply.send(result);
                }
            }
        }
    }

    fn ddc_error(error: impl ToString) -> io::Error {
        io::Error::new(io::ErrorKind::Other, error.to_string())
    }

    fn refresh_if_missing(displays: &mut Vec<CachedDisplay>, id: &str) {
        if !displays.iter().any(|cached| cached.status.id == id) {
            refresh(displays);
        }
    }

    /// Refresh active handles without forgetting a monitor that this process
    /// put into DDC standby. macOS may remove a sleeping display from
    /// CoreGraphics, but its retained IOAVService handle is still the only path
    /// that can wake it.
    fn refresh(displays: &mut Vec<CachedDisplay>) {
        let main = main_display_identity();
        let fresh = ddc_hi::Display::enumerate()
            .into_iter()
            .enumerate()
            .map(|(index, mut display)| {
                let brightness = display
                    .handle
                    .get_vcp_feature(VCP_LUMINANCE)
                    .ok()
                    .map(|v| clamp_percent(v.value() as i64));
                let power = display
                    .handle
                    .get_vcp_feature(VCP_POWER)
                    .ok()
                    .map(|v| ddc_power_is_on(v.value()));
                let cg_id = ddc_cg_id(&display);
                let id = ddc_id(&display.info, cg_id);
                // A monitor that answers a luminance read speaks DDC. One that
                // does not is common on USB-C hubs and DisplayLink, and used to
                // be advertised as controllable anyway: the slider moved and
                // the panel did not. Software gamma dimming still works on any
                // CoreGraphics display, so it is the honest fallback, labelled
                // as software in the UI.
                let gamma = cg_id
                    .filter(|id| panel_gamma_supported(*id))
                    .map(panel_gamma_percent);
                let (controllable, method, level) = classify_external(brightness, gamma);
                let is_main = main
                    .map(|(vendor, model)| {
                        let model = model & 0xffff;
                        display.info.model_id == Some(model as u16) || vendor == 0 && model == 0
                    })
                    .unwrap_or(false);
                CachedDisplay {
                    status: DisplayStatus {
                        id,
                        name: display_name(&display, index),
                        kind: "external".into(),
                        is_main,
                        brightness: level,
                        power,
                        controllable,
                        method: method.into(),
                        system_level: level,
                    },
                    display,
                    cg_id,
                    keep_when_missing: false,
                    off_at: None,
                    ddc_misses: 0,
                    power_values: None,
                }
            })
            .collect::<Vec<_>>();

        let mut previous = std::mem::take(displays);
        let mut merged = Vec::with_capacity(fresh.len() + previous.len());
        for mut current in fresh {
            if let Some(index) = previous
                .iter()
                .position(|old| old.status.id == current.status.id)
            {
                let old = previous.swap_remove(index);
                // A 0xD6 read can still report ON for a moment after the off
                // command lands, so our own command wins — but only for that
                // moment. Holding it forever made a monitor switched on at its
                // own button keep showing as off until the app was restarted.
                let entering_standby = old
                    .off_at
                    .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(3));
                if entering_standby {
                    current.status.power = Some(false);
                    current.off_at = old.off_at;
                } else {
                    if current.status.brightness.is_none() {
                        current.status.brightness = old.status.brightness;
                        current.status.system_level = old.status.system_level;
                    }
                    if current.status.power.is_none() {
                        current.status.power = old.status.power;
                    }
                }
                current.ddc_misses = if current.status.method == "ddc" {
                    0
                } else {
                    old.ddc_misses.saturating_add(1)
                };
                if current.status.method != "ddc"
                    && ddc_survives_miss(&old.status.method, old.ddc_misses)
                {
                    current.status.method = "ddc".into();
                    current.status.controllable = true;
                    current.status.brightness = old.status.brightness;
                    current.status.system_level = old.status.system_level;
                }
                // Keep a sleeping monitor's card and handle: macOS may drop it
                // from CoreGraphics while it is off.
                current.keep_when_missing = current.status.power == Some(false);
                // A monitor's capability string does not change between scans,
                // and re-reading it costs over a second on some panels.
                current.power_values = old.power_values;
            }
            merged.push(current);
        }
        merged.extend(previous.into_iter().filter(|old| old.keep_when_missing));
        *displays = merged;
    }

    /// DDC first, software gamma second. A monitor on a USB-C hub often
    /// enumerates but never answers DDC, and before this the write failed and
    /// the slider silently did nothing; dimming its gamma table is the only
    /// remaining way to actually change what the user sees.
    fn set_cached_brightness(cached: &mut CachedDisplay, value: u8) -> io::Result<()> {
        let mut ddc = cached
            .display
            .handle
            .set_vcp_feature(VCP_LUMINANCE, value as u16)
            .map_err(ddc_error);
        if ddc.is_err() && reopen(cached) {
            // The handle can die on its own after the monitor sleeps or
            // retrains its link. A monitor that does speak DDC must not be
            // quietly demoted to software dimming because of that.
            ddc = cached
                .display
                .handle
                .set_vcp_feature(VCP_LUMINANCE, value as u16)
                .map_err(ddc_error);
        }
        let method = match &ddc {
            Ok(()) => "ddc",
            Err(error) => {
                let Some(cg_id) = cached.cg_id.filter(|id| panel_gamma_supported(*id)) else {
                    return Err(ddc_error(error.to_string()));
                };
                if !panel_gamma_set(cg_id, value) {
                    return Err(ddc_error(error.to_string()));
                }
                log::info!(
                    "display {} refused DDC ({}); dimmed in software instead",
                    cached.status.id,
                    error
                );
                "gamma"
            }
        };
        cached.status.method = method.into();
        cached.status.controllable = true;
        cached.status.brightness = Some(value);
        cached.status.system_level = Some(value);
        Ok(())
    }

    /// Waking a monitor makes it retrain its link, and that invalidates the
    /// IOAVService handle the wake commands are being sent on: the retained
    /// handle starts returning `MacOS kernel I/O error: 268435459` mid-sequence
    /// and stays dead. Re-enumerating is the only way to get a live one, and
    /// without it every later ON press hit the same corpse — the monitor went
    /// off and could not be turned back on.
    fn reopen(cached: &mut CachedDisplay) -> bool {
        let Some(fresh) = ddc_hi::Display::enumerate()
            .into_iter()
            .find(|display| ddc_id(&display.info, ddc_cg_id(display)) == cached.status.id)
        else {
            log::info!(
                "{} is no longer in DDC enumeration; its handle cannot be replaced",
                cached.status.id
            );
            return false;
        };
        cached.cg_id = ddc_cg_id(&fresh);
        cached.display = fresh;
        true
    }

    /// Reads the power state, replacing a handle that has gone stale. `None`
    /// means the monitor did not answer at all.
    fn read_power(cached: &mut CachedDisplay) -> Option<bool> {
        if let Ok(value) = cached.display.handle.get_vcp_feature(VCP_POWER) {
            return Some(ddc_power_is_on(value.value()));
        }
        if !reopen(cached) {
            return None;
        }
        cached
            .display
            .handle
            .get_vcp_feature(VCP_POWER)
            .ok()
            .map(|value| ddc_power_is_on(value.value()))
    }

    /// The 0xD6 values this monitor advertises. Read once and cached: a
    /// capability string is a long, chatty read that some panels answer in over
    /// a second, and it must not sit in front of every power press.
    fn power_values(cached: &mut CachedDisplay) -> Vec<u8> {
        if let Some(values) = &cached.power_values {
            return values.clone();
        }
        let values = cached
            .display
            .handle
            .capabilities()
            .ok()
            .and_then(|caps| {
                caps.vcp_features
                    .get(&VCP_POWER)
                    .map(|feature| feature.values().copied().collect::<Vec<u8>>())
            })
            .unwrap_or_default();
        log::info!(
            "{} advertises 0xD6 values {:02x?}",
            cached.status.id,
            values
        );
        cached.power_values = Some(values.clone());
        values
    }

    fn set_cached_power(cached: &mut CachedDisplay, on: bool) -> io::Result<()> {
        if !on {
            let Some(value) = power_off_value(&power_values(cached)) else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "this monitor does not offer an off state over DDC",
                ));
            };
            cached
                .display
                .handle
                .set_vcp_feature(VCP_POWER, value)
                .map_err(ddc_error)?;
            cached.status.power = Some(false);
            cached.off_at = Some(std::time::Instant::now());
            cached.keep_when_missing = true;
            return Ok(());
        }

        // Stop as soon as the panel confirms it is lit. Every write sent after
        // that races the link retraining, and those were the ones killing the
        // handle this sequence needs.
        let mut accepted = false;
        let mut last_error = None;
        for attempt in 0..6 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(450));
            }
            match cached.display.handle.set_vcp_feature(VCP_POWER, POWER_ON) {
                Ok(()) => accepted = true,
                Err(error) => {
                    last_error = Some(error.to_string());
                    log::info!("power-on attempt {} failed: {}", attempt + 1, error);
                    if reopen(cached) {
                        match cached.display.handle.set_vcp_feature(VCP_POWER, POWER_ON) {
                            Ok(()) => accepted = true,
                            Err(error) => last_error = Some(error.to_string()),
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
            if read_power(cached) == Some(true) {
                wake_succeeded(cached);
                return Ok(());
            }
        }

        // A monitor that took the write but never answers 0xD6 is almost
        // certainly awake: an accepted write means the link is live.
        if accepted {
            wake_succeeded(cached);
            return Ok(());
        }
        log::info!(
            "{} refused the whole wake sequence; advertised 0xD6 values were {:02x?}",
            cached.status.id,
            cached.power_values.clone().unwrap_or_default()
        );
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "{} will not wake over DDC — use the button on the monitor ({})",
                cached.status.name,
                last_error.unwrap_or_default()
            ),
        ))
    }

    fn wake_succeeded(cached: &mut CachedDisplay) {
        cached.status.power = Some(true);
        cached.off_at = None;
        cached.keep_when_missing = false;
        // Some monitors come back at their own default level, not the one the
        // card is showing.
        if let Some(brightness) = cached.status.brightness {
            let _ = cached
                .display
                .handle
                .set_vcp_feature(VCP_LUMINANCE, brightness as u16);
        }
    }

    pub fn list(max_age: Option<std::time::Duration>) -> Vec<DisplayStatus> {
        let (reply, response) = mpsc::channel();
        if sender().send(Request::List { reply, max_age }).is_err() {
            return Vec::new();
        }
        response
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_default()
    }

    pub fn set_brightness(id: &str, value: u8) -> io::Result<()> {
        let (reply, response) = mpsc::channel();
        sender()
            .send(Request::Brightness {
                id: id.into(),
                value,
                reply,
            })
            .map_err(ddc_error)?;
        response
            .recv_timeout(std::time::Duration::from_secs(10))
            .map_err(ddc_error)?
    }

    pub fn set_power(id: &str, on: bool) -> io::Result<()> {
        let (reply, response) = mpsc::channel();
        sender()
            .send(Request::Power {
                id: id.into(),
                on,
                reply,
            })
            .map_err(ddc_error)?;
        response
            .recv_timeout(std::time::Duration::from_secs(12))
            .map_err(ddc_error)?
    }
}

/// `max_age` reuses the previous DDC scan when it is that fresh; `None` forces
/// a rescan. Opening the popover must not pay for a full DDC round trip.
/// True when the caller is on the macOS main thread — the UI thread. A Tauri
/// command without `(async)` runs there, so any blocking work inside one
/// freezes the window: that is what made granting Accessibility permission look
/// like the app had stopped opening.
#[cfg(target_os = "macos")]
pub fn on_main_thread() -> bool {
    extern "C" {
        fn pthread_main_np() -> i32;
    }
    unsafe { pthread_main_np() == 1 }
}

#[cfg(not(target_os = "macos"))]
pub fn on_main_thread() -> bool {
    false
}

/// Report a command that was slow, or that ran somewhere it must never run.
/// Silent on the healthy path, so it can stay in the shipped build.
fn probe(tag: &str, started: std::time::Instant) {
    let elapsed = started.elapsed();
    let on_main = on_main_thread();
    if on_main || elapsed > std::time::Duration::from_millis(150) {
        log::info!(
            "command {} took {}ms{}",
            tag,
            elapsed.as_millis(),
            if on_main {
                " ON THE MAIN THREAD (this blocks the UI)"
            } else {
                ""
            },
        );
    }
}

pub fn list(max_age: Option<std::time::Duration>) -> Vec<DisplayStatus> {
    let mut out = Vec::new();

    if builtin::exists() {
        // Preference order: real backlight first, gamma dimming as the
        // fallback. Gamma always works but is software — labelled as such.
        let backlight_ok = builtin::controllable();
        let gamma_ok = builtin_gamma_supported();
        out.push(DisplayStatus {
            id: BUILTIN_ID.into(),
            name: String::new(), // the UI labels the built-in by kind
            kind: "builtin".into(),
            is_main: builtin_is_main(),
            brightness: if backlight_ok {
                builtin::get()
            } else if gamma_ok {
                builtin_total().or_else(|| Some(builtin_gamma_percent()))
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
            system_level: if backlight_ok {
                builtin::get()
            } else {
                accessibility_backlight_level().or_else(|| Some(tracked_system_level_percent()))
            },
        });
    }

    out.extend(ddc_worker::list(max_age));

    out
}

pub fn set_brightness(id: &str, percent: u8) -> io::Result<()> {
    let percent = clamp_percent(percent as i64);
    if id == BUILTIN_ID {
        if builtin::set(percent) {
            return Ok(());
        }
        if builtin_gamma_set(percent) {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "built-in display not controllable on this machine",
        ));
    }
    ddc_worker::set_brightness(id, percent)
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
    ddc_worker::set_power(id, on)
}

// ─────────── Tauri commands ───────────

/// `force` is the explicit refresh button and post-power-toggle rescan; every
/// other caller accepts a scan from the last few seconds.
#[tauri::command(async)]
pub fn list_displays(force: Option<bool>) -> Vec<DisplayStatus> {
    let max_age = (!force.unwrap_or(false)).then(|| std::time::Duration::from_secs(5));
    let started = std::time::Instant::now();
    let displays = list(max_age);
    probe("list_displays", started);
    displays
}

#[tauri::command(async)]
pub fn set_display_brightness(app: tauri::AppHandle, id: String, value: i64) -> Result<(), String> {
    if id == BUILTIN_ID {
        // CGS gamma writes only take effect from the main thread's WindowServer
        // connection. The identical call returns success from a worker thread
        // and then silently does nothing — which is why the shipped build
        // moved the slider (and reported the new percent) without the screen
        // ever changing. Dispatch the whole built-in path to main.
        let percent = clamp_percent(value);
        let (tx, rx) = std::sync::mpsc::channel();
        app.run_on_main_thread(move || {
            let _ = tx.send(set_brightness(BUILTIN_ID, percent));
        })
        .map_err(|e| e.to_string())?;
        return rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string());
    }
    set_brightness(&id, clamp_percent(value)).map_err(|e| e.to_string())
}

/// Set the real built-in backlight through macOS Control Center's local
/// Accessibility UI. This is separate from `set_display_brightness`, which
/// remains the second-stage gamma dimmer.
#[tauri::command(async)]
pub fn set_builtin_backlight(app: tauri::AppHandle, value: i64) -> Result<u8, String> {
    let _probe_started = std::time::Instant::now();
    let percent = clamp_percent(value);
    #[cfg(target_os = "macos")]
    {
        let actual = crate::accessibility_backlight::set(percent)?;
        probe("set_builtin_backlight", _probe_started);
        brightness_tap::seed_sixteenths(actual as f64 / 100.0);
        crate::accessibility_backlight::publish_level(actual);
        // Opening Control Center steals focus and hides our popover. Put the
        // user back where the drag started after the local UI transaction.
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }
        return Ok(actual);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        let _ = percent;
        Err("built-in backlight control is macOS-only".into())
    }
}

#[tauri::command(async)]
pub fn request_accessibility_permission() -> bool {
    let _probe_started = std::time::Instant::now();
    #[cfg(target_os = "macos")]
    {
        let trusted = crate::accessibility_backlight::is_trusted(true);
        probe("request_accessibility_permission", _probe_started);
        return trusted;
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Clear our stale Accessibility entry and immediately ask again, so the fix
/// does not require the user to open a terminal.
#[tauri::command(async)]
pub fn reset_accessibility_permission() -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        crate::accessibility_backlight::reset_grant()?;
        return Ok(crate::accessibility_backlight::is_trusted(true));
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("Accessibility permission is macOS-only".into())
    }
}

/// Why the built-in backlight is or is not controllable, so the UI can say it
/// once instead of macOS re-prompting on every visit to the Tools tab.
#[derive(serde::Serialize)]
pub struct AccessibilityStatus {
    pub trusted: bool,
    /// Running from a randomized read-only App Translocation copy, where a
    /// granted Accessibility permission never survives the next launch.
    pub translocated: bool,
    /// Ad-hoc signed build: macOS pins the grant to this binary's hash, so an
    /// update leaves a stale entry that still reads as enabled but is denied.
    pub adhoc: bool,
}

#[tauri::command(async)]
pub fn accessibility_status() -> AccessibilityStatus {
    #[cfg(target_os = "macos")]
    {
        let _probe_started = std::time::Instant::now();
        let status = AccessibilityStatus {
            trusted: crate::accessibility_backlight::is_trusted(false),
            translocated: crate::accessibility_backlight::bundle_is_translocated(),
            adhoc: crate::accessibility_backlight::is_adhoc_signed(),
        };
        probe("accessibility_status", _probe_started);
        // One line per launch, so an "I allowed it and it still asks" report is
        // answered from the log instead of guesswork.
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            log::info!(
                "accessibility: trusted={} translocated={} adhoc={} exe={:?}",
                status.trusted,
                status.translocated,
                status.adhoc,
                std::env::current_exe().ok(),
            );
        });
        return status;
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Nothing to consent to; the UI must not show a macOS-only notice.
        AccessibilityStatus {
            trusted: true,
            translocated: false,
            adhoc: false,
        }
    }
}

/// Cheap built-in refresh for polling. Once Accessibility permission exists,
/// the retained Control Center slider is the authoritative live backlight
/// value, so changes made in Control Center also flow back into SayKnow Kit.
#[tauri::command(async)]
pub fn sync_builtin_brightness() -> Option<BuiltinBrightnessSync> {
    let started = std::time::Instant::now();
    #[cfg(target_os = "macos")]
    if brightness_tap::unseeded() {
        seed_system_level();
    }
    let out = Some(BuiltinBrightnessSync {
        brightness: builtin_total()?,
        system_level: accessibility_backlight_level().unwrap_or_else(tracked_system_level_percent),
    });
    probe("sync_builtin_brightness", started);
    out
}

#[tauri::command(async)]
pub fn set_display_power(id: String, on: bool) -> Result<(), String> {
    set_power(&id, on).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `#[tauri::command]` without `(async)` runs on the macOS main thread,
    /// so every blocking call inside one freezes the window. These commands all
    /// block — on DDC round trips, on Control Center accessibility automation,
    /// or on TCC — and dropping the marker is how the app stopped opening once
    /// Accessibility permission was granted.
    #[test]
    fn every_blocking_display_command_runs_off_the_main_thread() {
        let source = include_str!("display.rs");
        for name in [
            "list_displays",
            "set_display_brightness",
            "set_builtin_backlight",
            "request_accessibility_permission",
            "reset_accessibility_permission",
            "accessibility_status",
            "sync_builtin_brightness",
            "set_display_power",
        ] {
            let at = source
                .find(&format!("pub fn {name}("))
                .unwrap_or_else(|| panic!("{name} is gone; update this test"));
            let attribute = source[..at]
                .rsplit("#[tauri::command")
                .next()
                .expect("a command attribute above the function");
            assert!(
                attribute.starts_with("(async)"),
                "{name} must be #[tauri::command(async)] or it blocks the UI thread"
            );
        }
    }

    #[test]
    fn a_forced_scan_never_reuses_the_cache() {
        use std::time::{Duration, Instant};
        // The refresh button and the post-power-toggle rescan must hit the wire.
        assert!(!ddc_worker::cache_is_fresh(None, Some(Instant::now())));
        // Nothing scanned yet: there is nothing to reuse.
        assert!(!ddc_worker::cache_is_fresh(
            Some(Duration::from_secs(5)),
            None
        ));
    }

    #[test]
    fn a_recent_scan_is_reused_and_an_old_one_is_not() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        assert!(ddc_worker::cache_is_fresh(
            Some(Duration::from_secs(5)),
            Some(now)
        ));
        let stale = now
            .checked_sub(Duration::from_secs(6))
            .expect("clock is far enough from the epoch");
        assert!(!ddc_worker::cache_is_fresh(
            Some(Duration::from_secs(5)),
            Some(stale)
        ));
    }

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
    #[cfg(target_os = "macos")]
    fn gamma_scaling_is_reversible_from_the_original() {
        // The one-directional bug: scaling the live table compounds.
        let orig = [0.0f32, 0.25, 0.5, 0.75, 1.0];
        let down = gamma_dim::scale_table(&orig, 0.4);
        // Now drag UP to 80 — must be brighter than 40, derived from the
        // ORIGINAL, not from the already-dimmed table.
        let up = gamma_dim::scale_table(&orig, 0.8);
        assert!((down[2] - 0.5 * 0.4).abs() < 1e-6);
        assert!((up[2] - 0.5 * 0.8).abs() < 1e-6);
        assert!(up[2] > down[2]);
        // And the compounding behaviour this test guards against:
        let compounded = gamma_dim::scale_table(&down, 0.8);
        assert!(compounded[2] < down[2]);
    }

    /// Live hardware check, runs only when SAYKNOW_LIVE_GAMMA=1 so CI never
    /// touches a real display. Exercises the real read/capture/scale/write
    /// chain on the built-in panel and asserts both directions.
    #[test]
    #[cfg(target_os = "macos")]
    fn live_gamma_is_bidirectional() {
        if std::env::var("SAYKNOW_LIVE_GAMMA").ok().as_deref() != Some("1") {
            return;
        }
        let Some(d) = cg_builtin_id() else {
            eprintln!("no builtin; skip");
            return;
        };
        let mid = || -> f32 {
            let mut r = vec![0f32; 512];
            let mut g = vec![0f32; 512];
            let mut b = vec![0f32; 512];
            let mut n = 0u32;
            unsafe {
                extern "C" {
                    fn CGGetDisplayTransferByTable(
                        d: u32,
                        c: u32,
                        r: *mut f32,
                        g: *mut f32,
                        b: *mut f32,
                        n: *mut u32,
                    ) -> i32;
                }
                CGGetDisplayTransferByTable(
                    d,
                    512,
                    r.as_mut_ptr(),
                    g.as_mut_ptr(),
                    b.as_mut_ptr(),
                    &mut n,
                );
            }
            r[n as usize / 2]
        };
        eprintln!("slider-as-gamma-offset model");
        let base = mid();
        assert!(set_brightness(BUILTIN_ID, 40).is_ok(), "set 40 failed");
        let m40 = mid();
        assert!(set_brightness(BUILTIN_ID, 80).is_ok(), "set 80 failed");
        let m80 = mid();
        set_brightness(BUILTIN_ID, 100).ok();
        let m100 = mid();
        eprintln!("base={base:.4} m40={m40:.4} m80={m80:.4} m100={m100:.4}");
        // Slider percent IS the gamma factor: 40% → base×0.4, 80% → base×0.8.
        assert!(
            (m40 - base * 0.4).abs() < 0.03,
            "40% mismatch: {m40} vs {}",
            base * 0.4
        );
        assert!(
            (m80 - base * 0.8).abs() < 0.03,
            "80% mismatch: {m80} vs {}",
            base * 0.8
        );
        assert!(m80 > m40, "80% not brighter than 40%");
        assert!(
            (m100 - base).abs() < 0.02,
            "100% did not restore: {m100} vs {base}"
        );
    }

    /// Live enumeration through the exact list() path; CI-safe (empty on
    /// machines with no DDC displays).
    #[test]
    #[cfg(target_os = "macos")]
    fn live_ddc_enumerate() {
        if std::env::var("SAYKNOW_LIVE_GAMMA").ok().as_deref() != Some("1") {
            return;
        }
        for d in list(None) {
            eprintln!(
                "{} kind={} main={} bright={:?} power={:?} ctrl={} method={}",
                d.id, d.kind, d.is_main, d.brightness, d.power, d.controllable, d.method
            );
        }
    }

    /// A hub or DisplayLink monitor enumerates but never answers DDC. It used
    /// to be advertised as `controllable` with method `ddc` anyway, so the
    /// slider moved and nothing happened.
    #[test]
    fn a_monitor_that_refuses_ddc_is_driven_by_gamma_not_advertised_as_ddc() {
        assert_eq!(
            classify_external(Some(30), Some(100)),
            (true, "ddc", Some(30))
        );
        assert_eq!(classify_external(None, Some(80)), (true, "gamma", Some(80)));
        assert_eq!(classify_external(None, None), (false, "none", None));
    }

    /// The level of a software-dimmed monitor is the offset we applied; without
    /// it the card shows an em dash and a slider stuck at zero.
    #[test]
    fn a_gamma_driven_card_reports_a_level() {
        let (_, _, level) = classify_external(None, Some(45));
        assert_eq!(level, Some(45));
    }

    /// The fallback for monitors that never answer DDC: software gamma on the
    /// external panel itself. Opt-in only — it visibly dims the monitor.
    #[test]
    #[cfg(target_os = "macos")]
    fn live_external_gamma_fallback() {
        if std::env::var("SAYKNOW_LIVE_GAMMA").ok().as_deref() != Some("1") {
            return;
        }
        let Some(external) = ddc_worker::list(None)
            .into_iter()
            .find(|display| display.kind == "external")
        else {
            return;
        };
        let cg = cg_id_from(&external.id).expect("external card carries no CoreGraphics id");
        assert!(
            gamma_dim::supported(cg),
            "gamma fallback unavailable on external display {cg}"
        );
        assert!(gamma_dim::set_absolute(cg, 50), "gamma dim rejected");
        assert_eq!(gamma_dim::get_percent(cg), 50);
        // The built-in must keep its own offset: one global slot used to mean
        // dimming one panel rewrote the other panel's captured original.
        if let Some(builtin) = cg_builtin_id() {
            assert_eq!(gamma_dim::get_percent(builtin), 100);
        }
        gamma_dim::reset_offset(cg);
        assert_eq!(gamma_dim::get_percent(cg), 100);
    }

    /// The capability read is the only thing standing between a monitor and a
    /// 0x05 it cannot come back from, so it has to work against real hardware,
    /// not just against a vector of bytes. Prints what each panel advertises:
    /// that line is the evidence for any monitor that will not wake.
    #[test]
    #[cfg(target_os = "macos")]
    fn live_ddc_advertises_its_power_values() {
        if std::env::var("SAYKNOW_LIVE_GAMMA").ok().as_deref() != Some("1") {
            return;
        }
        for mut display in ddc_hi::Display::enumerate() {
            let id = ddc_id(&display.info, ddc_cg_id(&display));
            let values = display
                .handle
                .capabilities()
                .ok()
                .and_then(|caps| {
                    caps.vcp_features
                        .get(&VCP_POWER)
                        .map(|feature| feature.values().copied().collect::<Vec<u8>>())
                })
                .unwrap_or_default();
            eprintln!(
                "{id} advertises 0xD6 {values:02x?} -> sends {:02x?}",
                power_off_value(&values)
            );
            if values.contains(&(POWER_OFF as u8)) {
                assert_eq!(
                    power_off_value(&values),
                    Some(POWER_OFF),
                    "{id} takes the soft off and must never be sent 0x05"
                );
            }
        }
    }

    /// Physical OFF→ON cycle through the same retained worker handle used by
    /// the app. Opt-in only: this visibly blanks the external monitor.
    #[test]
    #[cfg(target_os = "macos")]
    fn live_external_power_cycle() {
        if std::env::var("SAYKNOW_LIVE_DDC_POWER").ok().as_deref() != Some("1") {
            return;
        }
        let external = ddc_worker::list(None)
            .into_iter()
            .find(|display| display.kind == "external")
            .expect("no external DDC display");
        let id = external.id;

        struct Restore(String);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = ddc_worker::set_power(&self.0, true);
            }
        }
        let restore = Restore(id.clone());

        ddc_worker::set_power(&id, false).expect("DDC power off failed");
        std::thread::sleep(std::time::Duration::from_secs(2));
        let off = ddc_worker::list(None)
            .into_iter()
            .find(|display| display.id == id)
            .expect("powered-off display card was lost");
        assert_eq!(
            off.power,
            Some(false),
            "display did not report cached off state"
        );

        ddc_worker::set_power(&id, true).expect("DDC wake sequence failed");
        std::thread::sleep(std::time::Duration::from_secs(3));
        let on = ddc_worker::list(None)
            .into_iter()
            .find(|display| display.id == id)
            .expect("woken display card was lost");
        assert_eq!(on.power, Some(true), "display did not return to on state");
        // The wake used to leave a dead handle behind, so the monitor was lit
        // but nothing could be sent to it any more.
        ddc_worker::set_brightness(&id, on.brightness.unwrap_or(50))
            .expect("handle was dead after the wake sequence");
        let after = ddc_worker::list(None)
            .into_iter()
            .find(|display| display.id == id)
            .expect("display card was lost after the wake");
        assert_eq!(
            after.method, "ddc",
            "monitor was demoted off DDC by the wake"
        );
        std::mem::forget(restore);
    }

    /// A monitor that just woke misses a read; before this it was relabelled
    /// as software dimmed and its slider jumped to the gamma level.
    #[test]
    fn a_waking_monitor_is_not_demoted_off_ddc_by_one_missed_read() {
        assert!(ddc_survives_miss("ddc", 0));
        assert!(ddc_survives_miss("ddc", 1));
        assert!(
            !ddc_survives_miss("ddc", 2),
            "a monitor that keeps missing is not on DDC"
        );
        assert!(!ddc_survives_miss("gamma", 0));
        assert!(!ddc_survives_miss("none", 0));
    }

    /// 0x04 puts most monitors into standby, which reads back as 0x02. Testing
    /// `!= 0x04` therefore called a black screen "on".
    #[test]
    fn only_0x01_counts_as_a_lit_panel() {
        assert!(ddc_power_is_on(0x01));
        for dark in [0x02, 0x03, 0x04, 0x05] {
            assert!(!ddc_power_is_on(dark), "0x{dark:02x} is not a lit panel");
        }
    }

    /// The monitor on this desk advertises 0x04 *and* 0x05, and 0x04 is enough
    /// to blank it. The old code sent 0x05 the moment a 0x04 write reported an
    /// error — and a DDC write reports errors it did not really suffer. On the
    /// panels that implement 0x05 literally, that is a one-way trip: the scaler
    /// goes and DDC goes with it, so the monitor that went off on a click can
    /// only be brought back by hand at the bezel.
    #[test]
    fn a_monitor_that_takes_the_soft_off_is_never_sent_the_hard_one() {
        assert_eq!(power_off_value(&[0x01, 0x04]), Some(POWER_OFF));
        assert_eq!(power_off_value(&[0x01, 0x04, 0x05]), Some(POWER_OFF));
    }

    /// 0x05 is only ever right where it is the only off the monitor claims.
    #[test]
    fn the_hard_off_goes_only_to_a_monitor_with_no_other_off() {
        assert_eq!(power_off_value(&[0x01, 0x05]), Some(POWER_OFF_HARD));
    }

    /// No capability string is no information, not permission to guess 0x05.
    #[test]
    fn an_unreadable_capability_string_gets_the_soft_off() {
        assert_eq!(power_off_value(&[]), Some(POWER_OFF));
    }

    /// Advertising 0xD6 without an off value means the feature is read-only on
    /// this panel; writing a guess at it is how monitors end up wedged.
    #[test]
    fn a_monitor_with_no_off_value_gets_no_write() {
        assert_eq!(power_off_value(&[0x01]), None);
    }

    /// macOS gives most external monitors no EDID identity at all, so two
    /// monitors used to share the id `ddc:?:?:?`. Every write then landed on
    /// whichever one enumerated first and the other slider looked dead.
    #[test]
    fn anonymous_monitors_still_get_distinct_ids() {
        let anonymous = || ddc_hi::DisplayInfo::new(ddc_hi::Backend::MacOS, "ARZOPA".into());
        let first = ddc_id(&anonymous(), Some(3));
        let second = ddc_id(&anonymous(), Some(7));
        assert_ne!(first, second, "two EDID-less monitors collided: {first}");
        assert_eq!(cg_id_from(&first), Some(3));
        assert_eq!(cg_id_from(&second), Some(7));
    }

    #[test]
    fn ids_without_a_coregraphics_suffix_yield_no_display() {
        assert_eq!(cg_id_from("ddc:?:?:?"), None);
        assert_eq!(cg_id_from(BUILTIN_ID), None);
    }

    #[test]
    fn builtin_id_cannot_collide_with_ddc_ids() {
        assert!(ddc_id(
            &ddc_hi::DisplayInfo::new(ddc_hi::Backend::MacOS, "builtin".into()),
            Some(1)
        )
        .starts_with("ddc:"));
        assert_ne!(BUILTIN_ID, "ddc:");
    }
}
