//! Unprivileged Apple Silicon die-temperature adapter.
//!
//! macOS exposes SoC die sensors through the AppleVendor HID temperature page
//! (`PrimaryUsagePage = 0xff00`, `PrimaryUsage = 5`). Reading them needs no
//! privileged helper, no `powermetrics`, and no TCC prompt. The symbols are
//! private, so every entry point is resolved with `dlsym` and the whole adapter
//! fails closed to `None` when anything is missing, empty, or out of range.

use std::ffi::{c_void, CString};
use std::os::raw::c_char;

use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation::base::{kCFAllocatorDefault, CFAllocatorRef, CFRelease, CFTypeRef, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};

pub const ADAPTER_ID: &str = "macos.iohid.applevendor.die.v1";

const IOKIT_PATH: &str = "/System/Library/Frameworks/IOKit.framework/IOKit";
const RTLD_LAZY: i32 = 0x1;
const APPLE_VENDOR_USAGE_PAGE: i32 = 0xff00;
const TEMPERATURE_SENSOR_USAGE: i32 = 5;
const TEMPERATURE_EVENT_TYPE: i64 = 15;
const TEMPERATURE_EVENT_FIELD: u32 = (TEMPERATURE_EVENT_TYPE as u32) << 16;
/// Physically plausible band for an on-die sensor of a running Mac. Values
/// outside it indicate a disconnected or misinterpreted channel.
const MIN_PLAUSIBLE_CELSIUS: f64 = 5.0;
const MAX_PLAUSIBLE_CELSIUS: f64 = 125.0;
/// Apple Silicon exposes many `tdie` channels; a handful of readings is the
/// floor at which the maximum is trustworthy rather than a single stray probe.
const MIN_DIE_SENSORS: usize = 4;

extern "C" {
    fn dlopen(filename: *const c_char, flag: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

type IOHIDEventSystemClientCreate = unsafe extern "C" fn(CFAllocatorRef) -> *mut c_void;
type IOHIDEventSystemClientSetMatching = unsafe extern "C" fn(*mut c_void, CFDictionaryRef);
type IOHIDEventSystemClientCopyServices = unsafe extern "C" fn(*mut c_void) -> CFArrayRef;
type IOHIDServiceClientCopyProperty = unsafe extern "C" fn(*mut c_void, CFStringRef) -> CFTypeRef;
type IOHIDServiceClientCopyEvent = unsafe extern "C" fn(*mut c_void, i64, i32, i64) -> *mut c_void;
type IOHIDEventGetFloatValue = unsafe extern "C" fn(*mut c_void, u32) -> f64;

struct Symbols {
    client_create: IOHIDEventSystemClientCreate,
    client_set_matching: IOHIDEventSystemClientSetMatching,
    client_copy_services: IOHIDEventSystemClientCopyServices,
    service_copy_property: IOHIDServiceClientCopyProperty,
    service_copy_event: IOHIDServiceClientCopyEvent,
    event_get_float: IOHIDEventGetFloatValue,
}

unsafe fn lookup(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
    let symbol = CString::new(name).ok()?;
    let address = dlsym(handle, symbol.as_ptr());
    if address.is_null() {
        None
    } else {
        Some(address)
    }
}

fn symbols() -> Option<Symbols> {
    let path = CString::new(IOKIT_PATH).ok()?;
    unsafe {
        let handle = dlopen(path.as_ptr(), RTLD_LAZY);
        if handle.is_null() {
            return None;
        }
        Some(Symbols {
            client_create: std::mem::transmute::<*mut c_void, IOHIDEventSystemClientCreate>(
                lookup(handle, "IOHIDEventSystemClientCreate")?,
            ),
            client_set_matching: std::mem::transmute::<
                *mut c_void,
                IOHIDEventSystemClientSetMatching,
            >(lookup(
                handle,
                "IOHIDEventSystemClientSetMatching",
            )?),
            client_copy_services: std::mem::transmute::<
                *mut c_void,
                IOHIDEventSystemClientCopyServices,
            >(lookup(
                handle,
                "IOHIDEventSystemClientCopyServices",
            )?),
            service_copy_property: std::mem::transmute::<*mut c_void, IOHIDServiceClientCopyProperty>(
                lookup(handle, "IOHIDServiceClientCopyProperty")?,
            ),
            service_copy_event: std::mem::transmute::<*mut c_void, IOHIDServiceClientCopyEvent>(
                lookup(handle, "IOHIDServiceClientCopyEvent")?,
            ),
            event_get_float: std::mem::transmute::<*mut c_void, IOHIDEventGetFloatValue>(lookup(
                handle,
                "IOHIDEventGetFloatValue",
            )?),
        })
    }
}

/// True for the SoC die channels (`PMU tdie4`, `PMU2 tdie1`, ...). `tdev`
/// channels are board/device probes and `gas gauge battery` or `NAND` channels
/// are unrelated components, so they never contribute.
fn is_die_sensor(name: &str) -> bool {
    let name = name.trim();
    name.starts_with("PMU") && name.contains("tdie")
}

/// Pure selection step: highest plausible die reading, or `None` when the
/// evidence is too thin to report a number truthfully.
pub(crate) fn select_die_celsius<I>(samples: I) -> Option<f32>
where
    I: IntoIterator<Item = (String, f64)>,
{
    let mut count = 0usize;
    let mut hottest = f64::MIN;
    for (name, celsius) in samples {
        if !is_die_sensor(&name) {
            continue;
        }
        if !celsius.is_finite()
            || celsius < MIN_PLAUSIBLE_CELSIUS
            || celsius > MAX_PLAUSIBLE_CELSIUS
        {
            continue;
        }
        count += 1;
        if celsius > hottest {
            hottest = celsius;
        }
    }
    if count < MIN_DIE_SENSORS {
        return None;
    }
    Some(hottest as f32)
}

fn matching_dictionary() -> CFDictionary<CFString, CFNumber> {
    CFDictionary::from_CFType_pairs(&[
        (
            CFString::from_static_string("PrimaryUsagePage"),
            CFNumber::from(APPLE_VENDOR_USAGE_PAGE),
        ),
        (
            CFString::from_static_string("PrimaryUsage"),
            CFNumber::from(TEMPERATURE_SENSOR_USAGE),
        ),
    ])
}

unsafe fn sensor_name(symbols: &Symbols, service: *mut c_void) -> Option<String> {
    let key = CFString::from_static_string("Product");
    let value = (symbols.service_copy_property)(service, key.as_concrete_TypeRef());
    if value.is_null() {
        return None;
    }
    let name = CFString::wrap_under_create_rule(value as CFStringRef).to_string();
    Some(name)
}

unsafe fn sensor_celsius(symbols: &Symbols, service: *mut c_void) -> Option<f64> {
    let event = (symbols.service_copy_event)(service, TEMPERATURE_EVENT_TYPE, 0, 0);
    if event.is_null() {
        return None;
    }
    let celsius = (symbols.event_get_float)(event, TEMPERATURE_EVENT_FIELD);
    CFRelease(event as CFTypeRef);
    Some(celsius)
}

/// Reads the hottest SoC die sensor in degrees Celsius, or `None` when no
/// trustworthy unprivileged reading is available on this machine.
pub fn read_die_celsius() -> Option<f32> {
    let symbols = symbols()?;
    unsafe {
        let client = (symbols.client_create)(kCFAllocatorDefault);
        if client.is_null() {
            return None;
        }
        let filter = matching_dictionary();
        (symbols.client_set_matching)(client, filter.as_concrete_TypeRef());

        let services = (symbols.client_copy_services)(client);
        if services.is_null() {
            CFRelease(client as CFTypeRef);
            return None;
        }

        let mut samples = Vec::new();
        let count = CFArrayGetCount(services);
        for index in 0..count {
            let service = CFArrayGetValueAtIndex(services, index) as *mut c_void;
            if service.is_null() {
                continue;
            }
            let Some(name) = sensor_name(&symbols, service) else {
                continue;
            };
            if !is_die_sensor(&name) {
                continue;
            }
            let Some(celsius) = sensor_celsius(&symbols, service) else {
                continue;
            };
            samples.push((name, celsius));
        }

        CFRelease(services as CFTypeRef);
        CFRelease(client as CFTypeRef);
        select_die_celsius(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, celsius: f64) -> (String, f64) {
        (name.to_string(), celsius)
    }

    #[test]
    fn selects_hottest_die_sensor() {
        let selected = select_die_celsius(vec![
            sample("PMU tdie1", 36.5),
            sample("PMU tdie2", 41.25),
            sample("PMU2 tdie1", 33.0),
            sample("PMU2 tdie2", 34.5),
        ]);
        assert_eq!(selected, Some(41.25));
    }

    #[test]
    fn ignores_non_die_channels() {
        let selected = select_die_celsius(vec![
            sample("gas gauge battery", 96.0),
            sample("NAND CH0 temp", 88.0),
            sample("PMU tdev1", 99.0),
            sample("PMU tcal", 51.8),
            sample("PMU tdie1", 36.0),
            sample("PMU tdie2", 37.0),
            sample("PMU tdie3", 38.0),
            sample("PMU tdie4", 39.0),
        ]);
        assert_eq!(selected, Some(39.0));
    }

    #[test]
    fn drops_implausible_and_non_finite_readings() {
        let selected = select_die_celsius(vec![
            sample("PMU tdie1", -22.0),
            sample("PMU tdie2", f64::NAN),
            sample("PMU tdie3", 900.0),
            sample("PMU tdie4", 36.0),
            sample("PMU tdie5", 37.0),
            sample("PMU tdie6", 38.0),
            sample("PMU tdie7", 39.5),
        ]);
        assert_eq!(selected, Some(39.5));
    }

    #[test]
    fn requires_enough_die_evidence() {
        assert_eq!(
            select_die_celsius(vec![
                sample("PMU tdie1", 36.0),
                sample("PMU tdie2", 37.0),
                sample("PMU tdie3", 38.0),
            ]),
            None
        );
        assert_eq!(select_die_celsius(Vec::new()), None);
    }

    #[test]
    fn live_adapter_is_plausible_or_absent() {
        match read_die_celsius() {
            Some(celsius) => {
                eprintln!("live die sensor: {celsius:.2} C");
                assert!(celsius as f64 >= MIN_PLAUSIBLE_CELSIUS);
                assert!(celsius as f64 <= MAX_PLAUSIBLE_CELSIUS);
            }
            None => eprintln!("live die sensor: unavailable"),
        }
    }
}
