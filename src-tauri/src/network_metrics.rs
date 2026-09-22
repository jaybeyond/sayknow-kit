//! Interface counters via `getifaddrs`, matching SystemInfoKit's
//! upload/download deltas. The first sample is a baseline, not a rate.

use std::time::Instant;

#[cfg(unix)]
use libc::{freeifaddrs, getifaddrs, if_data, ifaddrs, AF_INET, AF_LINK};

#[derive(Clone, Debug, PartialEq)]
pub enum NetworkStatus {
    WarmingUp {
        reason: String,
    },
    Available {
        interface: String,
        ip_address: Option<String>,
        upload_bytes_per_sec: u64,
        download_bytes_per_sec: u64,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug)]
struct Traffic {
    upload: u64,
    download: u64,
}

pub struct NetworkSampler {
    previous: Option<(Instant, Traffic)>,
}

impl NetworkSampler {
    pub fn new() -> Self {
        Self { previous: None }
    }

    pub fn sample(&mut self) -> NetworkStatus {
        let now = Instant::now();
        let (interface, ip_address, traffic) = match current_traffic() {
            Some(sample) => sample,
            None => {
                return NetworkStatus::Unavailable {
                    reason: "interface_unavailable".to_string(),
                }
            }
        };
        match self.previous.replace((now, traffic)) {
            None => NetworkStatus::WarmingUp {
                reason: "baseline_pending".to_string(),
            },
            Some((previous_at, previous)) => {
                let elapsed = now.duration_since(previous_at).as_secs_f64();
                if elapsed <= 0.0 {
                    return NetworkStatus::WarmingUp {
                        reason: "minimum_interval_pending".to_string(),
                    };
                }
                let upload = delta_per_sec(previous.upload, traffic.upload, elapsed);
                let download = delta_per_sec(previous.download, traffic.download, elapsed);
                NetworkStatus::Available {
                    interface,
                    ip_address,
                    upload_bytes_per_sec: upload,
                    download_bytes_per_sec: download,
                }
            }
        }
    }
}

fn delta_per_sec(previous: u64, current: u64, elapsed: f64) -> u64 {
    if current < previous {
        return 0;
    }
    ((current - previous) as f64 / elapsed).round() as u64
}

fn is_loopback(name: &str) -> bool {
    name == "lo0" || name == "lo"
}

fn is_link_local(octets: &[u8; 4]) -> bool {
    octets[0] == 169 && octets[1] == 254
}

fn interface_priority(name: &str) -> u8 {
    if name.starts_with("en") || name.starts_with("eth") || name.starts_with("wl") {
        0
    } else if name.starts_with("ap") {
        1
    } else if name.starts_with("utun") || name.starts_with("ipsec") {
        2
    } else {
        3
    }
}

#[cfg(unix)]
fn current_traffic() -> Option<(String, Option<String>, Traffic)> {
    unsafe {
        let mut ifap: *mut ifaddrs = std::ptr::null_mut();
        if getifaddrs(&mut ifap) != 0 || ifap.is_null() {
            return None;
        }
        let mut upload = 0u64;
        let mut download = 0u64;
        let mut chosen: Option<(u8, String, String)> = None;
        let mut cursor = ifap;
        while !cursor.is_null() {
            let entry = &*cursor;
            let name = if entry.ifa_name.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(entry.ifa_name)
                    .to_string_lossy()
                    .into_owned()
            };
            if !entry.ifa_addr.is_null() {
                let family = (*entry.ifa_addr).sa_family as i32;
                if family == AF_INET && !is_loopback(&name) {
                    let sin = &*(entry.ifa_addr as *const libc::sockaddr_in);
                    let octets = u32::from_be(sin.sin_addr.s_addr).to_be_bytes();
                    if !is_link_local(&octets) {
                        let ip = format!(
                            "{}.{}.{}.{}",
                            octets[0], octets[1], octets[2], octets[3]
                        );
                        let rank = interface_priority(&name);
                        if chosen.as_ref().map(|(r, _, _)| rank < *r).unwrap_or(true) {
                            chosen = Some((rank, name.clone(), ip));
                        }
                    }
                }
                if family == AF_LINK && !entry.ifa_data.is_null() {
                    let data = &*(entry.ifa_data as *const if_data);
                    upload = upload.saturating_add(data.ifi_obytes as u64);
                    download = download.saturating_add(data.ifi_ibytes as u64);
                }
            }
            cursor = entry.ifa_next;
        }
        freeifaddrs(ifap);
        let (interface, ip_address) = match chosen {
            Some((_, name, ip)) => (name, Some(ip)),
            None => ("unknown".to_string(), None),
        };
        Some((interface, ip_address, Traffic { upload, download }))
    }
}

#[cfg(not(unix))]
fn current_traffic() -> Option<(String, Option<String>, Traffic)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_is_a_baseline() {
        let mut sampler = NetworkSampler::new();
        match sampler.sample() {
            NetworkStatus::WarmingUp { reason } => assert_eq!(reason, "baseline_pending"),
            NetworkStatus::Unavailable { .. } => {}
            NetworkStatus::Available { .. } => panic!("first sample cannot be a rate"),
        }
    }
}
