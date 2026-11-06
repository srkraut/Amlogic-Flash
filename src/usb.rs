//! USB device detection: recognises an Amlogic box in each of its USB personalities
//! (ADNL burning mode, older Optimus burning mode, fastboot, adb) without opening it.

use rusb::{Device, GlobalContext, Speed};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// 1b8e:c004 — U-Boot / ROM in ADNL burning mode. Ready to flash.
    Adnl,
    /// 1b8e:c003 — older "Optimus/WorldCup" burning protocol (pre-SC2 SoCs). Not supported.
    Optimus,
    /// Android fastboot interface (U-Boot fastboot or fastbootd).
    Fastboot,
    /// Android Debug Bridge (box is running Android or recovery with adb).
    Adb,
    /// Any other device (only listed when "show all" is on).
    Other,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Adnl => "Burning mode — ready to flash",
            Kind::Optimus => "Amlogic burning mode (old protocol)",
            Kind::Fastboot => "Fastboot mode",
            Kind::Adb => "Android (adb)",
            Kind::Other => "USB device",
        }
    }
    pub fn hint(self) -> &'static str {
        match self {
            Kind::Adnl => "The box is waiting for this app. Press Flash or Probe.",
            Kind::Optimus => "This SoC uses the older Optimus protocol, which this app does not speak yet. Use Amlogic USB Burning Tool.",
            Kind::Fastboot => "Fastboot cannot write 'super' on these boxes. Unplug and re-plug the box's power to enter burning mode.",
            Kind::Adb => "Android is running. Use \"Reboot to burning mode\" (adb reboot update) or power-cycle the box.",
            Kind::Other => "Not an Amlogic flashing interface.",
        }
    }
    pub fn rank(self) -> u8 {
        match self {
            Kind::Adnl => 0,
            Kind::Optimus => 1,
            Kind::Fastboot => 2,
            Kind::Adb => 3,
            Kind::Other => 9,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDev {
    pub kind: Kind,
    pub vid: u16,
    pub pid: u16,
    pub bus: u8,
    pub addr: u8,
    pub port_path: String,
    pub speed: &'static str,
}

impl UsbDev {
    pub fn key(&self) -> String {
        format!("{}-{}", self.bus, self.port_path)
    }
    pub fn id(&self) -> String {
        format!("{:04x}:{:04x}", self.vid, self.pid)
    }
    pub fn describe(&self) -> String {
        format!("{} [{}] bus {} port {} · {}", self.kind.title(), self.id(), self.bus, self.port_path, self.speed)
    }
}

fn speed_str(s: Speed) -> &'static str {
    match s {
        Speed::Low => "USB 1.0 low speed",
        Speed::Full => "USB 1.1 full speed",
        Speed::High => "USB 2.0 high speed",
        Speed::Super => "USB 3.0 SuperSpeed",
        Speed::SuperPlus => "USB 3.1 SuperSpeed+",
        _ => "unknown speed",
    }
}

fn classify(d: &Device<GlobalContext>, vid: u16, pid: u16) -> Kind {
    if vid == 0x1B8E {
        return match pid {
            0xC004 => Kind::Adnl,
            0xC003 => Kind::Optimus,
            _ => Kind::Other,
        };
    }
    if let Ok(cfg) = d.config_descriptor(0) {
        for iface in cfg.interfaces() {
            for alt in iface.descriptors() {
                if alt.class_code() == 0xFF && alt.sub_class_code() == 0x42 {
                    match alt.protocol_code() {
                        0x03 => return Kind::Fastboot,
                        0x01 => return Kind::Adb,
                        _ => {}
                    }
                }
            }
        }
    }
    Kind::Other
}

/// Snapshot of relevant USB devices, best candidate first.
pub fn scan(include_other: bool) -> Vec<UsbDev> {
    let Ok(list) = rusb::devices() else { return vec![] };
    let mut out: BTreeMap<(u8, String), UsbDev> = BTreeMap::new();
    for d in list.iter() {
        let Ok(dd) = d.device_descriptor() else { continue };
        let (vid, pid) = (dd.vendor_id(), dd.product_id());
        // skip hubs
        if dd.class_code() == 0x09 {
            continue;
        }
        let kind = classify(&d, vid, pid);
        if kind == Kind::Other && !include_other {
            continue;
        }
        let port_path = d.port_numbers().map(|p| p.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(".")).unwrap_or_default();
        let dev = UsbDev { kind, vid, pid, bus: d.bus_number(), addr: d.address(), port_path, speed: speed_str(d.speed()) };
        out.insert((kind.rank(), dev.key()), dev);
    }
    out.into_values().collect()
}

pub fn libusb_version() -> String {
    let v = rusb::version();
    format!("libusb {}.{}.{}", v.major(), v.minor(), v.micro())
}

/// Locate an `adb` binary (GUI apps don't inherit the shell PATH).
pub fn find_adb() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        "/opt/homebrew/bin/adb".to_string(),
        "/usr/local/bin/adb".to_string(),
        format!("{home}/Library/Android/sdk/platform-tools/adb"),
    ];
    candidates.into_iter().map(std::path::PathBuf::from).find(|p| p.is_file()).or_else(|| {
        std::process::Command::new("/bin/sh")
            .args(["-lc", "command -v adb"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    })
}
