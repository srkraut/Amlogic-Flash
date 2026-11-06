//! The flashing workflow shared by the GUI and the CLI.

use crate::adnl::{self, Adnl};
use crate::image::{AmlImage, SPARSE_MAGIC, human};
use anyhow::{Result, bail};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Event {
    Log(String),
    /// label, fraction 0..=1 (None = indeterminate)
    Progress(String, Option<f32>),
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

fn log(s: &Sink, m: impl Into<String>) {
    s(Event::Log(m.into()));
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
            let head = img.reader(item)?.read_all_prefix(4)?;
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

pub fn run(img: Arc<AmlImage>, opts: FlashOptions, cancel: Arc<AtomicBool>, sink: &Sink) -> Result<()> {
    if !opts.probe_only {
        plan(&img, &opts.partitions)?;
        log(sink, format!("Image: {} (v{}, {} items, {})", img.path.display(), img.version, img.items.len(), human(img.file_size)));
    }

    if opts.precheck && !opts.probe_only {
        sink(Event::Progress("Checking image CRC".into(), Some(0.0)));
        let ok = img.check_crc(|d, t| sink(Event::Progress("Checking image CRC".into(), Some(d as f32 / t as f32))))?;
        if !ok {
            bail!("image header CRC/size mismatch: the file is corrupt or was modified without repacking");
        }
        log(sink, "Image CRC OK");
        for p in &opts.partitions {
            let item = img.partition(p).unwrap();
            let want = img.expected_sha1(p)?.unwrap();
            let label = format!("Checking {p} SHA-1");
            let got = img.sha1_of(item, |d, t| sink(Event::Progress(label.clone(), Some(d as f32 / t as f32))))?;
            if got != want {
                bail!("{p}: SHA-1 in image ({want}) does not match data ({got})");
            }
            log(sink, format!("{p}: SHA-1 OK ({want})"));
        }
    }

    log(sink, "Waiting for the box in USB burning mode (1b8e:c004)…");
    log(sink, "→ Connect the USB cable, then unplug and re-plug the box's POWER (no reset button).");
    let start = Instant::now();
    while !adnl::device_present() {
        if cancel.load(Ordering::SeqCst) {
            bail!("cancelled before anything was written");
        }
        if start.elapsed() > opts.wait_for_device {
            bail!("no device appeared within {}s", opts.wait_for_device.as_secs());
        }
        sink(Event::Progress(format!("Waiting for device… {}s", start.elapsed().as_secs()), None));
        std::thread::sleep(Duration::from_millis(50));
    }

    let dev = Adnl::open()?;
    let id = dev.identify()?;
    log(sink, format!("Device: ADNL protocol {} (minor {}), stage {} [{:02x?}]", id.protocol, id.minor, id.stage_name(), &id.raw[..id.raw.len().min(12)]));
    if !id.is_supported() {
        bail!("unsupported device state: need ADNL protocol 5/6 in U-Boot stage, got protocol {} stage {}. Nothing was written.", id.protocol, id.stage_name());
    }
    if opts.probe_only {
        log(sink, "Probe only: no changes made.");
        return Ok(());
    }
    if cancel.load(Ordering::SeqCst) {
        bail!("cancelled before anything was written");
    }

    dev.prepare()?;
    log(sink, "Burn session started (disk_initial 0: nothing erased)");
    log(sink, "⚠ Do not unplug the box or the cable until it finishes.");

    for p in &opts.partitions {
        let item = img.partition(p).unwrap().clone();
        let want = img.expected_sha1(p)?.unwrap();
        let mut reader = img.reader(&item)?;
        log(sink, format!("Writing {p}: {} as {}", human(item.size), item.format()));
        let t0 = Instant::now();
        let label = format!("Writing {p}");
        dev.burn(&item, &mut reader, |d, t| {
            let secs = t0.elapsed().as_secs_f32().max(0.001);
            let rate = d as f32 / secs / (1024.0 * 1024.0);
            sink(Event::Progress(format!("{label} — {} / {} ({rate:.1} MiB/s)", human(d), human(t)), Some(d as f32 / t as f32)));
        })?;
        log(sink, format!("{p}: transfer complete in {:.0}s", t0.elapsed().as_secs_f32()));
        let vlabel = format!("Device verifying {p} SHA-1");
        sink(Event::Progress(vlabel.clone(), None));
        dev.verify_sha1(&want, |e| sink(Event::Progress(format!("{vlabel}… {}s", e.as_secs()), None)))?;
        log(sink, format!("{p}: verified on device ✓"));
    }

    if opts.reboot {
        log(sink, "Rebooting the box");
        dev.reboot()?;
    }
    log(sink, "Done. If Android shows \"Can't load Android system\", choose Factory data reset in recovery.");
    Ok(())
}

trait PrefixRead {
    fn read_all_prefix(&mut self, n: usize) -> Result<Vec<u8>>;
}
impl PrefixRead for crate::image::ItemReader {
    fn read_all_prefix(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut v = vec![0u8; n];
        self.read_at(0, &mut v)?;
        Ok(v)
    }
}
