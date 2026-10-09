//! Unprivileged Apple Smart Battery readout, matching SystemInfoKit /
//! RunCat Neo: IOKit `AppleSmartBattery` (and pack temperature on macOS 27).

use crate::iokit_macos::{dict_value, float_value, int_value, service_properties, string_value};

#[derive(Clone, Debug, PartialEq)]
pub enum BatteryReading {
    NotInstalled,
    Installed {
        percent: f32,
        is_charging: bool,
        /// Wall power is attached. Separate from `is_charging`: macOS holds a
        /// full or optimised battery on the adapter without charging it.
        external_connected: bool,
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
    let external_connected = int_value(&dict, "ExternalConnected").unwrap_or(0) == 1;
    let adapter_name = if external_connected {
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
        external_connected,
        adapter_name,
        max_capacity_percent,
        cycle_count,
        temperature_celsius,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_battery_is_installed_or_absent() {
        match read_battery() {
            None => eprintln!("live battery: iokit unavailable"),
            Some(BatteryReading::NotInstalled) => eprintln!("live battery: not installed"),
            Some(BatteryReading::Installed {
                percent,
                is_charging,
                external_connected,
                ..
            }) => {
                eprintln!(
                    "live battery: {percent:.1}% charging={is_charging} external={external_connected}"
                );
                assert!((0.0..=100.0).contains(&percent));
            }
        }
    }
}
