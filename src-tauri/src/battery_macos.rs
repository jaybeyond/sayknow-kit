//! Unprivileged Apple Smart Battery readout, matching SystemInfoKit /
//! RunCat Neo: IOKit `AppleSmartBattery` (and pack temperature on macOS 27).

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

#[derive(Clone, Debug, PartialEq)]
pub enum BatteryReading {
    NotInstalled,
    Installed {
        percent: f32,
        is_charging: bool,
        adapter_name: Option<String>,
        max_capacity_percent: Option<f32>,
        cycle_count: Option<u32>,
        temperature_celsius: Option<f32>,
    },
}

pub fn read_battery() -> Option<BatteryReading> {
    let dict = service_properties("AppleSmartBattery")?;
    let installed = int_value(&dict, "BatteryInstalled").unwrap_or(0) == 1;
    if !installed {
        return Some(BatteryReading::NotInstalled);
    }

    let (percent, max_capacity_percent, nested_temp) = if let Some(data) = dict_value(&dict, "BatteryData")
    {
        let percent = float_value(&data, "CurrentCapacity").map(|v| (v as f32).clamp(0.0, 100.0));
        let max_capacity_percent = match (
            float_value(&data, "FullChargeCapacity"),
            float_value(&data, "DesignCapacity"),
        ) {
            (Some(full), Some(design)) if design > 0.0 => {
                Some(((full / design) * 100.0).clamp(0.0, 100.0) as f32)
            }
            _ => None,
        };
        (percent, max_capacity_percent, float_value(&data, "Temperature"))
    } else {
        let percent = match (
            float_value(&dict, "CurrentCapacity"),
            float_value(&dict, "MaxCapacity"),
        ) {
            (Some(current), Some(max)) if max > 0.0 => {
                Some(((current / max) * 100.0).clamp(0.0, 100.0) as f32)
            }
            _ => None,
        };
        let max_capacity_percent = match (
            float_value(&dict, "AppleRawMaxCapacity"),
            float_value(&dict, "DesignCapacity"),
        ) {
            (Some(raw), Some(design)) if design > 0.0 => {
                Some(((raw / design) * 100.0).clamp(0.0, 100.0) as f32)
            }
            _ => None,
        };
        (percent, max_capacity_percent, float_value(&dict, "Temperature"))
    };

    let pack_temp = service_properties("AppleSmartBatteryPack")
        .and_then(|pack| dict_value(&pack, "BatteryData"))
        .and_then(|data| float_value(&data, "Temperature"));
    let temperature_celsius = pack_temp
        .or(nested_temp)
        .map(|raw| (raw / 100.0) as f32)
        .filter(|c| (-40.0..=90.0).contains(c));

    let percent = percent?;
    let is_charging = int_value(&dict, "IsCharging").unwrap_or(0) == 1;
    let adapter_name = if int_value(&dict, "ExternalConnected").unwrap_or(0) == 1 {
        dict_value(&dict, "AdapterDetails").and_then(|adapter| {
            string_value(&adapter, "Name").or_else(|| {
                int_value(&adapter, "Watts").filter(|w| *w > 0).map(|w| format!("{w}W"))
            })
        })
    } else {
        None
    };
    let cycle_count = int_value(&dict, "CycleCount").map(|n| n as u32);

    Some(BatteryReading::Installed {
        percent,
        is_charging,
        adapter_name,
        max_capacity_percent,
        cycle_count,
        temperature_celsius,
    })
}

fn service_properties(name: &str) -> Option<CFDictionary> {
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

fn dict_value(dict: &CFDictionary, key: &str) -> Option<CFDictionary> {
    let value = lookup(dict, key);
    if value.is_null() {
        return None;
    }
    unsafe { Some(CFDictionary::wrap_under_get_rule(value as *mut _)) }
}

fn int_value(dict: &CFDictionary, key: &str) -> Option<i64> {
    number_value(dict, key)?.to_i64()
}

fn float_value(dict: &CFDictionary, key: &str) -> Option<f64> {
    number_value(dict, key)?.to_f64()
}

fn number_value(dict: &CFDictionary, key: &str) -> Option<CFNumber> {
    let value = lookup(dict, key);
    if value.is_null() {
        return None;
    }
    unsafe { Some(CFNumber::wrap_under_get_rule(value as *mut _)) }
}

fn string_value(dict: &CFDictionary, key: &str) -> Option<String> {
    let value = lookup(dict, key);
    if value.is_null() {
        return None;
    }
    unsafe { Some(CFString::wrap_under_get_rule(value as *mut _).to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_battery_is_installed_or_absent() {
        match read_battery() {
            None => eprintln!("live battery: iokit unavailable"),
            Some(BatteryReading::NotInstalled) => eprintln!("live battery: not installed"),
            Some(BatteryReading::Installed { percent, is_charging, .. }) => {
                eprintln!("live battery: {percent:.1}% charging={is_charging}");
                assert!((0.0..=100.0).contains(&percent));
            }
        }
    }
}
