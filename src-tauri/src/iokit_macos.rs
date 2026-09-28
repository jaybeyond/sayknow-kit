//! Unprivileged IORegistry property reads shared by the battery and GPU
//! readouts: find a service by class, copy its properties, and pull typed
//! values out of the dictionary.

use core_foundation::base::TCFType;
use core_foundation::dictionary::{CFDictionary, CFDictionaryGetValue, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use std::ffi::CString;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const i8) -> CFDictionaryRef;
    fn IOServiceGetMatchingService(main_port: u32, matching: CFDictionaryRef) -> u32;
    fn IORegistryEntryCreateCFProperties(
        entry: u32,
        properties: *mut CFDictionaryRef,
        allocator: *const std::ffi::c_void,
        options: u32,
    ) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
}

const KERN_SUCCESS: i32 = 0;

/// The properties of the first service of class `name` (or a subclass).
pub(crate) fn service_properties(name: &str) -> Option<CFDictionary> {
    let c_name = CString::new(name).ok()?;
    unsafe {
        let matching = IOServiceMatching(c_name.as_ptr());
        if matching.is_null() {
            return None;
        }
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return None;
        }
        let mut properties: CFDictionaryRef = std::ptr::null();
        let rc = IORegistryEntryCreateCFProperties(service, &mut properties, std::ptr::null(), 0);
        IOObjectRelease(service);
        if rc != KERN_SUCCESS || properties.is_null() {
            return None;
        }
        Some(CFDictionary::wrap_under_create_rule(properties))
    }
}

fn lookup(dict: &CFDictionary, key: &str) -> *const std::ffi::c_void {
    let key = CFString::new(key);
    unsafe {
        CFDictionaryGetValue(
            dict.as_concrete_TypeRef(),
            key.as_concrete_TypeRef() as *const _,
        )
    }
}

pub(crate) fn dict_value(dict: &CFDictionary, key: &str) -> Option<CFDictionary> {
    let value = lookup(dict, key);
    if value.is_null() {
        return None;
    }
    unsafe { Some(CFDictionary::wrap_under_get_rule(value as *mut _)) }
}

pub(crate) fn int_value(dict: &CFDictionary, key: &str) -> Option<i64> {
    number_value(dict, key)?.to_i64()
}

pub(crate) fn float_value(dict: &CFDictionary, key: &str) -> Option<f64> {
    number_value(dict, key)?.to_f64()
}

fn number_value(dict: &CFDictionary, key: &str) -> Option<CFNumber> {
    let value = lookup(dict, key);
    if value.is_null() {
        return None;
    }
    unsafe { Some(CFNumber::wrap_under_get_rule(value as *mut _)) }
}

pub(crate) fn string_value(dict: &CFDictionary, key: &str) -> Option<String> {
    let value = lookup(dict, key);
    if value.is_null() {
        return None;
    }
    unsafe { Some(CFString::wrap_under_get_rule(value as *mut _).to_string()) }
}

/// How busy the GPU is, 0–100, as the Apple Silicon accelerator driver reports
/// it in its own performance statistics — the figure Activity Monitor's GPU
/// history draws. No privileges and no private framework involved.
pub(crate) fn gpu_utilization_percent() -> Option<f32> {
    let properties = service_properties("IOAccelerator")?;
    let stats = dict_value(&properties, "PerformanceStatistics")?;
    let percent = float_value(&stats, "Device Utilization %")?;
    percent
        .is_finite()
        .then_some(percent.clamp(0.0, 100.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_gpu_utilization_is_a_percentage() {
        match gpu_utilization_percent() {
            None => eprintln!("live gpu: no accelerator statistics"),
            Some(percent) => {
                eprintln!("live gpu: {percent:.0}%");
                assert!((0.0..=100.0).contains(&percent));
            }
        }
    }
}
