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
    fn for_each_backlight_service<T>(
        mut f: impl FnMut(IoObject) -> Option<T>,
    ) -> Option<T> {
        unsafe {
            for class_name in
                [&b"IODisplayConnect\0"[..], &b"AppleARMBacklight\0"[..]]
            {
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
//
// The slider is an ABSOLUTE brightness that tracks the system: the real
// backlight level S is read live from IORegistry, the applied gamma offset g
// only changes when the user drags, and the screen's total is S x g. Pressing
// the system keys changes S, so the slider follows; we never rewrite g in
// response, which keeps auto-brightness and the keys behaving natively
// instead of being fought by a compensation loop.

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

    /// The system backlight level 0.0-1.0, straight from the registry.
    /// This is the value the keyboard keys change.
    pub fn system_level() -> Option<f64> {
        super::iokit_backlight::system_backlight_level()
    }

    /// Current total = system level x our gamma offset. The system level is
    /// the tap-tracked value (registry snapshot at boot + key-press deltas):
    /// the registry itself is static and never moves on this architecture.
    pub fn total_percent() -> Option<u8> {
        let g = *LAST.lock().unwrap();
        let s = super::brightness_tap::system_level();
        Some((s * g * 100.0).round().clamp(0.0, 100.0) as u8)
    }

    pub fn get_percent() -> u8 {
        (*LAST.lock().unwrap() * 100.0).round().clamp(0.0, 100.0) as u8
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

    /// Absolute-brightness entry: the slider's V is a TOTAL. The gamma offset
    /// becomes V / S where S is the live system backlight level, so the
    /// slider reads the same scale as the keyboard keys. Above the current
    /// backlight level the offset clamps to 1.0 — the backlight itself is
    /// the ceiling only the system keys can raise.
    pub fn set_absolute(display: CgDirectDisplayId, percent: u8) -> bool {
        let target = percent as f64 / 100.0;
        let system = super::brightness_tap::system_level().max(0.01);
        set_gamma_offset(display, (target / system).clamp(0.0, 1.0))
    }

    fn set_gamma_offset(display: CgDirectDisplayId, factor: f64) -> bool {
        // First touch: whatever is running now (Night Shift, True Tone)
        // becomes the base we scale and later restore.
        {
            let mut orig = ORIGINAL.lock().unwrap();
            if orig.is_none() {
                let Some((r, g, b)) = read_table(display) else {
                    return false;
                };
                *orig = Some(Original { display, red: r, green: g, blue: b });
            }
        }
        let factor = factor as f32;
        let (r, g, b) = {
            let orig = ORIGINAL.lock().unwrap();
            let o = orig.as_ref().unwrap();
            (
                scale_table(&o.red, factor),
                scale_table(&o.green, factor),
                scale_table(&o.blue, factor),
            )
        };
        let ok = write_table(display, &r, &g, &b);
        if ok {
            *LAST.lock().unwrap() = factor as f64;
        }
        ok
    }

    /// Put the captured original back. Called on app exit so the panel is
    /// never left dimmed with no obvious way back.
    pub(super) fn scale_table(src: &[f32], factor: f32) -> Vec<f32> {
        src.iter().map(|v| (v * factor).clamp(0.0, 1.0)).collect()
    }

    pub fn restore() {
        let orig = ORIGINAL.lock().unwrap().take();
        if let Some(o) = orig {
            if write_table(o.display, &o.red, &o.green, &o.blue) {
                *LAST.lock().unwrap() = 1.0;
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
        fn CFRunLoopAddSource(
            rl: CfRunLoopRef,
            source: CfRunLoopSourceRef,
            mode: CfStringRef,
        );
        fn CFRunLoopRun();
        static kCFRunLoopDefaultMode: CfStringRef;
    }

    /// Read NX data1 through NSEvent — CGEvent does not expose it.
    unsafe fn nx_data1(event: CGEventRef) -> Option<i64> {
        use objc2::runtime::{AnyObject, Sel};
        type MsgSendFn = unsafe extern "C" fn(*const AnyObject, Sel, ...) -> isize;
        extern "C" {
            fn objc_getClass(name: *const std::ffi::c_char) -> *mut AnyObject;
            fn objc_msgSend() -> MsgSendFn;
        }
        let send = objc_msgSend();
        let cls = objc_getClass(b"NSEvent\0".as_ptr() as *const std::ffi::c_char);
        if cls.is_null() {
            return None;
        }
        let ev: *const AnyObject = send(cls, Sel::register(c"eventWithCGEvent:"), event as isize)
            as *const AnyObject;
        if ev.is_null() {
            return None;
        }
        Some(send(ev, Sel::register(c"data1")) as i64)
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

    /// Create the tap ON the main runloop — the only place event taps are
    /// reliably created. A bare secondary thread's creation fails outright,
    /// and Tauri's setup() runs before the event loop services anything, so
    /// attempts are paced: main thread creates, a helper thread only sleeps
    /// and re-schedules the next attempt on main. Once created, the main
    /// runloop services the tap for the app's lifetime.
    pub fn start(app: &tauri::AppHandle) {
        use std::sync::atomic::{AtomicU32, Ordering as O2};
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
                    let source =
                        CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
                    if !source.is_null() {
                        CFRunLoopAddSource(
                            CFRunLoopGetCurrent(),
                            source,
                            kCFRunLoopDefaultMode,
                        );
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
    if let Some(f) = iokit_backlight::system_backlight_level() {
        brightness_tap::seed_sixteenths(f);
    }
    brightness_tap::start(app);
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
    gamma_dim::total_percent()
}

#[cfg(not(target_os = "macos"))]
fn builtin_total() -> Option<u8> {
    None
}

#[cfg(target_os = "macos")]
fn builtin_gamma_percent() -> u8 {
    gamma_dim::get_percent()
}

#[cfg(not(target_os = "macos"))]
fn builtin_gamma_percent() -> u8 {
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
        if builtin_gamma_set(percent) {
            return Ok(());
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
pub fn set_display_brightness(
    app: tauri::AppHandle,
    id: String,
    value: i64,
) -> Result<(), String> {
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

/// Cheap builtin-only refresh for polling: no DDC traffic, just the
/// registry read and our offset. The UI calls this about once a second while
/// the tools tab is visible so the slider follows the keyboard keys.
#[tauri::command]
pub fn sync_builtin_brightness() -> Option<u8> {
    builtin_total()
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
                        d: u32, c: u32, r: *mut f32, g: *mut f32, b: *mut f32, n: *mut u32,
                    ) -> i32;
                }
                CGGetDisplayTransferByTable(d, 512, r.as_mut_ptr(), g.as_mut_ptr(), b.as_mut_ptr(), &mut n);
            }
            r[n as usize / 2]
        };
        let sys = super::iokit_backlight::system_backlight_level();
        if let Some(f) = sys {
            super::brightness_tap::seed_sixteenths(f);
        }
        eprintln!("system backlight S={sys:?}");
        let base = mid();
        // Targets scaled to the live ceiling so the test holds at any S:
        // below-ceiling drags must land at V/S exactly, above-ceiling must
        // clamp at the backlight level (factor 1.0).
        let (s, cap) = match sys {
            Some(s) => (s.max(0.01), (s * 100.0) as u8),
            None => (1.0, 100),
        };
        let low = (cap as f64 * 0.6) as u8;
        let high = (cap as f64 * 0.9) as u8;
        assert!(set_brightness(BUILTIN_ID, low).is_ok(), "set low failed");
        let m_low = mid();
        assert!(set_brightness(BUILTIN_ID, high).is_ok(), "set high failed");
        let m_high = mid();
        set_brightness(BUILTIN_ID, 100).ok();
        let m100 = mid();
        eprintln!(
            "base={base:.4} cap={cap} low={low} high={high} m_low={m_low:.4} m_high={m_high:.4} m100={m100:.4}"
        );
        // Absolute model: table = base * V/S below the ceiling.
        let f_low = (low as f64 / 100.0 / s).clamp(0.0, 1.0);
        let expected_low = (base as f64 * f_low) as f32;
        assert!(
            (m_low - expected_low).abs() < 0.03,
            "low absolute mismatch: {m_low} vs {expected_low} (S={s})"
        );
        assert!(m_high > m_low, "high not brighter than low");
        // 100 is always >= ceiling -> factor clamps to 1 -> exact restore.
        assert!((m100 - base).abs() < 0.02, "100% did not restore: {m100} vs {base}");
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
