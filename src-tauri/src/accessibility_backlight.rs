//! Real built-in backlight control through macOS Control Center's local UI.
//!
//! New AppleARMBacklight Macs reject every public/private direct setter, but
//! Control Center still owns the real backlight. With Accessibility permission
//! we can open its Display menu extra and physically drag the built-in slider.
//! This is deliberately local UI automation: no shell, AppleScript, or helper
//! binary, and the value we read back is Control Center's live value.

#![cfg(target_os = "macos")]

use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::thread;
use std::time::Duration;

type CfTypeRef = *const ();
type CfStringRef = *const ();
type CfArrayRef = *const ();
type CfDictionaryRef = *const ();
type AxElement = *mut c_void;
type CgEvent = *mut c_void;
type CgEventSource = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Size {
    width: f64,
    height: f64,
}

const UTF8: u32 = 0x0800_0100;
const PROC_ALL_PIDS: u32 = 1;
const CF_NUMBER_DOUBLE: isize = 13;
const AX_VALUE_POINT: i32 = 1;
const AX_VALUE_SIZE: i32 = 2;
const HID_EVENT_TAP: u32 = 0;
const HID_SYSTEM_STATE: i32 = 1;
const LEFT_MOUSE_DOWN: u32 = 1;
const LEFT_MOUSE_UP: u32 = 2;
const LEFT_MOUSE_DRAGGED: u32 = 6;
const KEY_ESCAPE: u16 = 53;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CfDictionaryRef) -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> AxElement;
    fn AXUIElementCopyAttributeValue(
        element: AxElement,
        attribute: CfStringRef,
        value: *mut CfTypeRef,
    ) -> i32;
    fn AXValueGetValue(value: CfTypeRef, value_type: i32, out: *mut c_void) -> bool;

}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: CfTypeRef);
    fn CFRetain(value: CfTypeRef) -> CfTypeRef;
    fn CFArrayGetCount(array: CfArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfArrayRef, index: isize) -> CfTypeRef;
    fn CFStringCreateWithCString(
        allocator: CfTypeRef,
        string: *const c_char,
        encoding: u32,
    ) -> CfStringRef;
    fn CFStringGetCString(
        string: CfStringRef,
        buffer: *mut c_char,
        size: isize,
        encoding: u32,
    ) -> bool;
    fn CFNumberGetValue(number: CfTypeRef, number_type: isize, value: *mut c_void) -> bool;
    fn CFDictionaryCreate(
        allocator: CfTypeRef,
        keys: *const CfTypeRef,
        values: *const CfTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CfDictionaryRef;
    static kCFBooleanTrue: CfTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceCreate(state: i32) -> CgEventSource;
    fn CGEventCreate(source: CgEventSource) -> CgEvent;
    fn CGEventGetLocation(event: CgEvent) -> Point;
    fn CGEventCreateMouseEvent(
        source: CgEventSource,
        event_type: u32,
        position: Point,
        button: u32,
    ) -> CgEvent;
    fn CGEventCreateKeyboardEvent(
        source: CgEventSource,
        virtual_key: u16,
        key_down: bool,
    ) -> CgEvent;
    fn CGEventPost(tap: u32, event: CgEvent);
    fn CGWarpMouseCursorPosition(position: Point) -> i32;
}

extern "C" {
    fn proc_listpids(kind: u32, type_info: u32, buffer: *mut c_void, size: i32) -> i32;
    fn proc_name(pid: i32, buffer: *mut c_void, size: u32) -> i32;
}

struct OwnedCf(CfTypeRef);
impl Drop for OwnedCf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

unsafe fn cf_string(value: &str) -> Option<OwnedCf> {
    let c = std::ffi::CString::new(value).ok()?;
    let string = CFStringCreateWithCString(ptr::null(), c.as_ptr(), UTF8);
    (!string.is_null()).then_some(OwnedCf(string))
}

unsafe fn copy_attribute(element: AxElement, attribute: &str) -> Option<OwnedCf> {
    let attribute = cf_string(attribute)?;
    let mut value: CfTypeRef = ptr::null();
    (AXUIElementCopyAttributeValue(element, attribute.0, &mut value) == 0 && !value.is_null())
        .then_some(OwnedCf(value))
}

unsafe fn string_attribute(element: AxElement, attribute: &str) -> Option<String> {
    let value = copy_attribute(element, attribute)?;
    let mut buffer = [0i8; 512];
    if !CFStringGetCString(value.0, buffer.as_mut_ptr(), buffer.len() as isize, UTF8) {
        return None;
    }
    Some(
        CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned(),
    )
}

unsafe fn children(element: AxElement) -> Vec<AxElement> {
    let Some(array) = copy_attribute(element, "AXChildren") else {
        return Vec::new();
    };
    let count = CFArrayGetCount(array.0);
    (0..count)
        .map(|i| CFRetain(CFArrayGetValueAtIndex(array.0, i)) as AxElement)
        .collect()
}

unsafe fn find(
    element: AxElement,
    depth: u8,
    predicate: &dyn Fn(&str, &str) -> bool,
) -> Option<AxElement> {
    if depth > 14 {
        return None;
    }
    let id = string_attribute(element, "AXIdentifier").unwrap_or_default();
    let role = string_attribute(element, "AXRole").unwrap_or_default();
    if predicate(&id, &role) {
        return Some(CFRetain(element as CfTypeRef) as AxElement);
    }
    for child in children(element) {
        let found = find(child, depth + 1, predicate);
        CFRelease(child as CfTypeRef);
        if found.is_some() {
            return found;
        }
    }
    None
}

unsafe fn value_number(element: AxElement) -> Option<f64> {
    let value = copy_attribute(element, "AXValue")?;
    let mut out = 0.0f64;
    CFNumberGetValue(value.0, CF_NUMBER_DOUBLE, &mut out as *mut _ as *mut c_void).then_some(out)
}

unsafe fn point_attribute(element: AxElement, attribute: &str) -> Option<Point> {
    let value = copy_attribute(element, attribute)?;
    let mut out = Point::default();
    AXValueGetValue(value.0, AX_VALUE_POINT, &mut out as *mut _ as *mut c_void).then_some(out)
}

unsafe fn size_attribute(element: AxElement, attribute: &str) -> Option<Size> {
    let value = copy_attribute(element, attribute)?;
    let mut out = Size::default();
    AXValueGetValue(value.0, AX_VALUE_SIZE, &mut out as *mut _ as *mut c_void).then_some(out)
}

fn control_center_pid() -> Option<i32> {
    unsafe {
        let bytes = proc_listpids(PROC_ALL_PIDS, 0, ptr::null_mut(), 0);
        if bytes <= 0 {
            return None;
        }
        let mut pids = vec![0i32; bytes as usize / std::mem::size_of::<i32>() + 16];
        let written = proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut c_void,
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        );
        for pid in pids.into_iter().take((written.max(0) as usize) / 4) {
            if pid <= 0 {
                continue;
            }
            let mut name = [0i8; 128];
            if proc_name(pid, name.as_mut_ptr() as *mut c_void, name.len() as u32) > 0
                && CStr::from_ptr(name.as_ptr()).to_bytes() == b"ControlCenter"
            {
                return Some(pid);
            }
        }
        None
    }
}

pub fn is_trusted(prompt: bool) -> bool {
    unsafe {
        if AXIsProcessTrusted() || !prompt {
            return AXIsProcessTrusted();
        }
        let Some(key) = cf_string("AXTrustedCheckOptionPrompt") else {
            return false;
        };
        let key_ref = key.0;
        let value = kCFBooleanTrue;
        let options =
            CFDictionaryCreate(ptr::null(), &key_ref, &value, 1, ptr::null(), ptr::null());
        if options.is_null() {
            return false;
        }
        let trusted = AXIsProcessTrustedWithOptions(options);
        CFRelease(options);
        trusted
    }
}

unsafe fn post_event(event: CgEvent) {
    if !event.is_null() {
        CGEventPost(HID_EVENT_TAP, event);
        CFRelease(event as CfTypeRef);
    }
}

unsafe fn press_escape(source: CgEventSource) {
    post_event(CGEventCreateKeyboardEvent(source, KEY_ESCAPE, true));
    post_event(CGEventCreateKeyboardEvent(source, KEY_ESCAPE, false));
}

unsafe fn click(source: CgEventSource, position: Point) {
    let _ = CGWarpMouseCursorPosition(position);
    thread::sleep(Duration::from_millis(35));
    post_event(CGEventCreateMouseEvent(
        source,
        LEFT_MOUSE_DOWN,
        position,
        0,
    ));
    thread::sleep(Duration::from_millis(45));
    post_event(CGEventCreateMouseEvent(source, LEFT_MOUSE_UP, position, 0));
}

unsafe fn application_windows(app: AxElement) -> Vec<AxElement> {
    let Some(array) = copy_attribute(app, "AXWindows") else {
        return Vec::new();
    };
    let count = CFArrayGetCount(array.0);
    (0..count)
        .map(|i| CFRetain(CFArrayGetValueAtIndex(array.0, i)) as AxElement)
        .collect()
}

unsafe fn builtin_slider(app: AxElement) -> Option<AxElement> {
    let windows = application_windows(app);
    let mut fallback: Option<AxElement> = None;
    for window in windows {
        if let Some(group) = find(window, 0, &|id, _| {
            id.starts_with("controlcenter-display-")
                && id != "controlcenter-display-brightness-slider"
                && id.contains("Retina")
        }) {
            let slider = find(group, 0, &|_, role| role == "AXSlider");
            CFRelease(group as CfTypeRef);
            CFRelease(window as CfTypeRef);
            if let Some(old) = fallback.take() {
                CFRelease(old as CfTypeRef);
            }
            return slider;
        }
        // On non-Retina/localized names, Control Center lists the built-in
        // after external displays. Retain the last display group's slider.
        if let Some(group) = find(window, 0, &|id, _| {
            id.starts_with("controlcenter-display-")
                && id != "controlcenter-display-brightness-slider"
        }) {
            if let Some(slider) = find(group, 0, &|_, role| role == "AXSlider") {
                if let Some(old) = fallback.replace(slider) {
                    CFRelease(old as CfTypeRef);
                }
            }
            CFRelease(group as CfTypeRef);
        }
        CFRelease(window as CfTypeRef);
    }
    fallback
}

unsafe fn ensure_slider(app: AxElement, source: CgEventSource) -> Result<AxElement, String> {
    // ControlCenter retains hidden AXWindows after their popover closes. Never
    // trust a pre-existing slider: close any popup, physically open Display,
    // then use the freshly visible tree.
    let menu = find(app, 0, &|id, _| id == "com.apple.menuextra.display")
        .ok_or_else(|| "macOS Display menu item was not found".to_string())?;
    let position = point_attribute(menu, "AXPosition")
        .ok_or_else(|| "Display menu position is unavailable".to_string())?;
    let size = size_attribute(menu, "AXSize")
        .ok_or_else(|| "Display menu size is unavailable".to_string())?;
    CFRelease(menu as CfTypeRef);

    press_escape(source);
    thread::sleep(Duration::from_millis(180));
    click(
        source,
        Point {
            x: position.x + size.width / 2.0,
            y: position.y + size.height / 2.0,
        },
    );
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(60));
        if let Some(slider) = builtin_slider(app) {
            return Ok(slider);
        }
    }
    Err("macOS Display brightness slider did not open".into())
}

/// Read Control Center's retained built-in slider without opening its popover.
/// ControlCenter keeps the AXWindow alive while hidden, so once Accessibility
/// permission is granted this is the authoritative live backlight value.
pub fn get() -> Option<u8> {
    if !is_trusted(false) {
        return None;
    }
    let pid = control_center_pid()?;
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let value = builtin_slider(app).and_then(|slider| {
            let value = value_number(slider).map(|v| (v * 100.0).round().clamp(0.0, 100.0) as u8);
            CFRelease(slider as CfTypeRef);
            value
        });
        CFRelease(app as CfTypeRef);
        value
    }
}
pub fn set(percent: u8) -> Result<u8, String> {
    if !is_trusted(true) {
        return Err("Accessibility permission is required; allow SayKnow Kit and try again".into());
    }
    let pid = control_center_pid().ok_or_else(|| "ControlCenter is not running".to_string())?;
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Err("ControlCenter accessibility connection failed".into());
        }
        let source = CGEventSourceCreate(HID_SYSTEM_STATE);
        if source.is_null() {
            CFRelease(app as CfTypeRef);
            return Err("macOS input source creation failed".into());
        }
        let cursor_event = CGEventCreate(ptr::null_mut());
        let original_cursor = if cursor_event.is_null() {
            Point::default()
        } else {
            let p = CGEventGetLocation(cursor_event);
            CFRelease(cursor_event as CfTypeRef);
            p
        };

        let result = (|| {
            let slider = ensure_slider(app, source)?;
            let current = value_number(slider).unwrap_or(1.0).clamp(0.0, 1.0);
            let position = point_attribute(slider, "AXPosition")
                .ok_or_else(|| "Brightness slider position is unavailable".to_string())?;
            let size = size_attribute(slider, "AXSize")
                .ok_or_else(|| "Brightness slider size is unavailable".to_string())?;
            let target = (percent as f64 / 100.0).clamp(0.0, 1.0);
            let y = position.y + size.height / 2.0;
            // AX reports the track bounds, while the thumb centre stops just
            // inside them. Exact 0/1 coordinates miss the thumb hit target.
            let thumb_x = |value: f64| position.x + size.width * value.clamp(0.02, 0.98);
            let start = Point {
                x: thumb_x(current),
                y,
            };
            let end = Point {
                x: thumb_x(target),
                y,
            };
            let _ = CGWarpMouseCursorPosition(start);
            thread::sleep(Duration::from_millis(45));
            post_event(CGEventCreateMouseEvent(source, LEFT_MOUSE_DOWN, start, 0));
            for step in 1..=10 {
                let t = step as f64 / 10.0;
                let point = Point {
                    x: start.x + (end.x - start.x) * t,
                    y,
                };
                post_event(CGEventCreateMouseEvent(
                    source,
                    LEFT_MOUSE_DRAGGED,
                    point,
                    0,
                ));
                thread::sleep(Duration::from_millis(18));
            }
            post_event(CGEventCreateMouseEvent(source, LEFT_MOUSE_UP, end, 0));
            thread::sleep(Duration::from_millis(220));
            let actual = value_number(slider)
                .map(|v| (v * 100.0).round().clamp(0.0, 100.0) as u8)
                .unwrap_or(percent);
            CFRelease(slider as CfTypeRef);
            Ok(actual)
        })();

        press_escape(source);
        let _ = CGWarpMouseCursorPosition(original_cursor);
        CFRelease(source as CfTypeRef);
        CFRelease(app as CfTypeRef);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_control_center_backlight() {
        if std::env::var("SAYKNOW_LIVE_BACKLIGHT").ok().as_deref() != Some("1") {
            return;
        }
        let mid = set(60).expect("Control Center 60% write failed");
        assert!((55..=65).contains(&mid), "expected about 60%, got {mid}%");
        let full = set(100).expect("Control Center 100% restore failed");
        assert!(full >= 98, "expected full restore, got {full}%");
        let reread = get().expect("hidden Control Center slider read failed");
        assert!(reread >= 98, "expected full readback, got {reread}%");
    }
}
