//! Background jobs shared by the GUI and the CLI: image verification, probing and flashing.

use crate::adnl::{self, Adnl};
use crate::image::{AmlImage, SPARSE_MAGIC, human};
use anyhow::{Result, bail};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub enum Event {
    Log(Level, String),
    /// `total == 0` means indeterminate. `bytes` tells the UI to show speed / ETA.
    Progress { phase: String, done: u64, total: u64, bytes: bool },
    /// Result of a SHA-1 check for one partition (name, ok).
    PartCheck(String, bool),
    CrcCheck(bool),
    Identified(String),
    Done(Result<(), String>),
}

#[derive(Debug, Clone)]
pub struct FlashOptions {
    pub partitions: Vec<String>,
    pub precheck: bool,
    pub reboot: bool,
    pub wait_for_device: Duration,
    pub probe_only: bool,
}

pub type Sink = dyn Fn(Event) + Send + Sync;

fn log(s: &Sink, l: Level, m: impl Into<String>) {
    s(Event::Log(l, m.into()));
}

/// Validate that a set of partitions can be flashed from this image.
pub fn plan(img: &AmlImage, parts: &[String]) -> Result<()> {
    if parts.is_empty() {
        bail!("no partitions selected");
    }
    for p in parts {
        let Some(item) = img.partition(p) else { bail!("partition {p:?} is not in the image") };
        if item.is_protected() {
            bail!("refusing to write {p:?}: boot loader partitions need the full ROM-stage burn (use Amlogic USB Burning Tool)");
        }
        if item.is_sparse() {
            let mut head = [0u8; 4];
            img.reader(item)?.read_at(0, &mut head)?;
            if head != SPARSE_MAGIC {
                bail!("{p:?} is marked sparse but does not start with the Android sparse magic");
            }
        }
        if img.expected_sha1(p)?.is_none() {
            bail!("{p:?} has no VERIFY (sha1) entry in the image, refusing to flash unverifiable data");
        }
    }
    Ok(())
}

/// Check header CRC and the SHA-1 of the given partitions on this Mac.
pub fn verify_image(img: &AmlImage, parts: &[String], cancel: &AtomicBool, sink: &Sink) -> Result<()> {
    let crc_ok = img.check_crc(|d, t| sink(Event::Progress { phase: "Checking image CRC".into(), done: d, total: t, bytes: true }))?;
    sink(Event::CrcCheck(crc_ok));
    if !crc_ok {
        bail!("image header CRC/size mismatch: the file is corrupt or was modified without repacking");
    }
    log(sink, Level::Ok, "Image CRC OK");
    let mut bad = vec![];
    for p in parts {
        if cancel.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        let Some(item) = img.partition(p) else { continue };
        let Some(want) = img.expected_sha1(p)? else {
            log(sink, Level::Warn, format!("{p}: no checksum in image"));
            continue;
        };
        let phase = format!("Checking {p} SHA-1");
        let got = img.sha1_of(item, |d, t| sink(Event::Progress { phase: phase.clone(), done: d, total: t, bytes: true }))?;
        let ok = got == want;
        sink(Event::PartCheck(p.clone(), ok));
        if ok {
            log(sink, Level::Ok, format!("{p}: SHA-1 OK"));
        } else {
            log(sink, Level::Error, format!("{p}: SHA-1 MISMATCH (image says {want}, data is {got})"));
            bad.push(p.clone());
        }
    }
    if !bad.is_empty() {
        bail!("checksum mismatch in: {}", bad.join(", "));
    }
    Ok(())
}

pub fn run(img: Arc<AmlImage>, opts: FlashOptions, cancel: Arc<AtomicBool>, sink: &Sink) -> Result<()> {
    if !opts.probe_only {
        plan(&img, &opts.partitions)?;
        log(sink, Level::Info, format!("Image: {} (v{}, {} items, {})", img.path.display(), img.version, img.items.len(), human(img.file_size)));
        if opts.precheck {
            verify_image(&img, &opts.partitions, &cancel, sink)?;
        }
    }

    if !adnl::device_present() {
        log(sink, Level::Info, "Waiting for the box in USB burning mode (1b8e:c004)…");
        log(sink, Level::Info, "→ With the USB cable connected, unplug and re-plug the box's POWER (no reset button).");
    }
    let start = Instant::now();
    while !adnl::device_present() {
        if cancel.load(Ordering::SeqCst) {
            bail!("cancelled before anything was written");
        }
        if start.elapsed() > opts.wait_for_device {
            bail!("no box appeared within {} min", opts.wait_for_device.as_secs() / 60);
        }
        sink(Event::Progress { phase: format!("Waiting for box… {}s", start.elapsed().as_secs()), done: 0, total: 0, bytes: false });
        std::thread::sleep(Duration::from_millis(30));
    }

    let dev = Adnl::open()?;
    let id = dev.identify()?;
    let desc = format!("ADNL protocol {}.{} · stage {}", id.protocol, id.minor, id.stage_name());
    sink(Event::Identified(desc.clone()));
    log(sink, Level::Info, format!("Device: {desc} [{:02x?}]", &id.raw[..id.raw.len().min(12)]));
    if !id.is_supported() {
        bail!("unsupported device state: need ADNL protocol 5/6 in U-Boot stage, got protocol {} stage {}. Nothing was written.", id.protocol, id.stage_name());
    }
    if opts.probe_only {
        log(sink, Level::Ok, "Probe OK: box is ready. No changes were made.");
        return Ok(());
    }
    if cancel.load(Ordering::SeqCst) {
        bail!("cancelled before anything was written");
    }

    dev.prepare()?;
    log(sink, Level::Info, "Burn session started (disk_initial 0 — nothing erased)");
    log(sink, Level::Warn, "Do not unplug the box or the cable until it finishes.");

    for p in &opts.partitions {
        let item = img.partition(p).unwrap().clone();
        let want = img.expected_sha1(p)?.unwrap();
        let mut reader = img.reader(&item)?;
        log(sink, Level::Info, format!("Writing {p}: {} as {}", human(item.size), item.format()));
        let t0 = Instant::now();
        let phase = format!("Writing {p}");
        dev.burn(&item, &mut reader, |d, t| sink(Event::Progress { phase: phase.clone(), done: d, total: t, bytes: true }))?;
        log(sink, Level::Info, format!("{p}: transfer complete in {:.0}s", t0.elapsed().as_secs_f32()));
        let vphase = format!("Box is verifying {p}");
        dev.verify_sha1(&want, |e| sink(Event::Progress { phase: format!("{vphase}… {}s", e.as_secs()), done: 0, total: 0, bytes: false }))?;
        log(sink, Level::Ok, format!("{p}: verified on the box"));
    }

    if opts.reboot {
        log(sink, Level::Info, "Rebooting the box");
        dev.reboot()?;
    }
    log(sink, Level::Info, "If Android shows \"Can't load Android system\", choose Factory data reset in recovery.");
    Ok(())
}
