//! Amlogic ADNL USB burning protocol, U-Boot (TPL) stage only.
//!
//! Every Amlogic Android 11+ U-Boot runs `adnl` for ~1.2 s on a cold boot (`try_auto_burn`)
//! and stays in burning mode when a host talks to it. Commands are plain ASCII written to
//! the bulk OUT endpoint; replies (≤512 bytes) start with OKAY / FAIL / INFO / DATA.
//!
//! Partition write sequence:
//!   oem mwrite 0x<size> <normal|sparse> store <part>          -> OKAY
//!   loop: mwrite:verify=addsum -> DATAOUT0x<len>:0x<offset>   (or OKAY when finished)
//!         <len bytes of image data> then 4-byte LE additive checksum -> OKAY
//!   oem verify sha1sum <hex>                                   -> INFO… then OKAY
//!
//! Protocol notes come from pyamlboot (github.com/superna9999/pyamlboot, GPL-2.0 OR MIT)
//! and from reading the device's U-Boot.

use crate::image::{Item, ItemReader};
use anyhow::{Context, Result, anyhow, bail};
use rusb::{DeviceHandle, Direction, GlobalContext, TransferType};
use std::time::{Duration, Instant};

pub const VID: u16 = 0x1B8E;
pub const PID: u16 = 0xC004;
pub const STAGE_TPL: u8 = 0x10;

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const DATA_TIMEOUT: Duration = Duration::from_secs(60);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const BULK_CHUNK: usize = 16 * 1024;
const READ_LEN: usize = 512;

const BURNSTEPS: [u32; 3] = [0xC004_1030, 0xC004_1031, 0xC004_1032];

/// Is an Amlogic device in USB burning mode on the bus right now?
pub fn device_present() -> bool {
    rusb::devices()
        .map(|list| {
            list.iter().any(|d| d.device_descriptor().is_ok_and(|dd| dd.vendor_id() == VID && dd.product_id() == PID))
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct Identify {
    pub protocol: u8,
    pub minor: u8,
    pub stage: u8,
    pub raw: Vec<u8>,
}

impl Identify {
    pub fn stage_name(&self) -> &'static str {
        match self.stage {
            0x00 => "BootROM",
            0x08 => "BL2",
            STAGE_TPL => "U-Boot",
            _ => "unknown",
        }
    }
    pub fn is_supported(&self) -> bool {
        matches!(self.protocol, 5 | 6) && self.stage == STAGE_TPL
    }
}

pub struct Adnl {
    h: DeviceHandle<GlobalContext>,
    ep_out: u8,
    ep_in: u8,
    iface: u8,
}

fn reply_text(r: &[u8]) -> String {
    let n = r.iter().position(|&c| c == 0).unwrap_or(r.len());
    String::from_utf8_lossy(&r[..n]).into_owned()
}

/// Additive checksum over little-endian u32 words (last partial word zero-padded).
pub fn addsum(buf: &[u8]) -> u32 {
    buf.chunks(4).fold(0u32, |acc, c| {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        acc.wrapping_add(u32::from_le_bytes(w))
    })
}

fn parse_num(s: &str) -> Result<u64> {
    let s = s.trim();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u64::from_str_radix(s, 16).map_err(|e| anyhow!("bad hex number {s:?}: {e}"))
}

/// Parse `DATAOUT0x<len>:0x<offset>`.
pub fn parse_dataout(text: &str) -> Result<(u64, u64)> {
    let rest = text.strip_prefix("DATAOUT").ok_or_else(|| anyhow!("not a DATAOUT reply: {text:?}"))?;
    let (len, off) = rest.split_once(':').ok_or_else(|| anyhow!("malformed DATAOUT reply: {text:?}"))?;
    Ok((parse_num(len)?, parse_num(off)?))
}

impl Adnl {
    pub fn open() -> Result<Self> {
        let h = rusb::open_device_with_vid_pid(VID, PID).ok_or_else(|| anyhow!("no Amlogic device in burning mode (1b8e:c004) found"))?;
        let dev = h.device();
        let cfg = dev.active_config_descriptor().context("reading USB configuration")?;
        for iface in cfg.interfaces() {
            for alt in iface.descriptors() {
                let mut ep_out = None;
                let mut ep_in = None;
                for ep in alt.endpoint_descriptors() {
                    if ep.transfer_type() != TransferType::Bulk {
                        continue;
                    }
                    match ep.direction() {
                        Direction::Out if ep_out.is_none() => ep_out = Some(ep.address()),
                        Direction::In if ep_in.is_none() => ep_in = Some(ep.address()),
                        _ => {}
                    }
                }
                if let (Some(o), Some(i)) = (ep_out, ep_in) {
                    let n = alt.interface_number();
                    h.claim_interface(n).context("claiming USB interface (is another flasher running?)")?;
                    return Ok(Self { h, ep_out: o, ep_in: i, iface: n });
                }
            }
        }
        bail!("device has no bulk IN/OUT endpoint pair")
    }

    fn write(&self, data: &[u8], timeout: Duration) -> Result<()> {
        let mut off = 0;
        while off < data.len() {
            let n = self.h.write_bulk(self.ep_out, &data[off..], timeout).context("USB write")?;
            if n == 0 {
                bail!("USB write stalled");
            }
            off += n;
        }
        Ok(())
    }

    fn read(&self, timeout: Duration) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; READ_LEN];
        let n = self.h.read_bulk(self.ep_in, &mut buf, timeout).context("USB read")?;
        buf.truncate(n);
        if n < 4 {
            bail!("reply too short ({n} bytes)");
        }
        Ok(buf)
    }

    /// Send a command and require an OKAY reply.
    pub fn cmd(&self, c: &[u8]) -> Result<Vec<u8>> {
        self.write(c, IO_TIMEOUT)?;
        let r = self.read(IO_TIMEOUT)?;
        if !r.starts_with(b"OKAY") {
            bail!("device rejected {:?}: {}", String::from_utf8_lossy(c), reply_text(&r));
        }
        Ok(r)
    }

    pub fn identify(&self) -> Result<Identify> {
        let r = self.cmd(b"getvar:identify")?;
        if r.len() < 8 {
            bail!("identify reply too short: {r:02x?}");
        }
        Ok(Identify { protocol: r[4], minor: r[5], stage: r[7], raw: r })
    }

    /// Enter partition-burning state without erasing anything (`disk_initial 0`).
    pub fn prepare(&self) -> Result<()> {
        self.cmd(format!("oem setvar burnsteps {:#x}", BURNSTEPS[0]).as_bytes())?;
        self.cmd(format!("oem setvar burnsteps {:#x}", BURNSTEPS[1]).as_bytes())?;
        self.cmd(b"oem disk_initial 0")?;
        self.cmd(format!("oem setvar burnsteps {:#x}", BURNSTEPS[2]).as_bytes())?;
        Ok(())
    }

    pub fn burn(&self, item: &Item, reader: &mut ItemReader, mut progress: impl FnMut(u64, u64)) -> Result<()> {
        self.cmd(format!("oem mwrite {:#x} {} store {}", item.size, item.format(), item.sub_type).as_bytes())?;
        loop {
            self.write(b"mwrite:verify=addsum", IO_TIMEOUT)?;
            let r = self.read(DATA_TIMEOUT)?;
            let text = reply_text(&r);
            if text.starts_with("OKAY") {
                return Ok(());
            }
            let (len, off) = parse_dataout(&text).with_context(|| format!("while writing {}", item.sub_type))?;
            if off + len > item.size || len > 512 << 20 {
                bail!("device requested an invalid range {off:#x}+{len:#x} (item size {:#x})", item.size);
            }
            let mut buf = vec![0u8; len as usize];
            reader.read_at(off, &mut buf)?;
            for chunk in buf.chunks(BULK_CHUNK) {
                self.write(chunk, DATA_TIMEOUT)?;
            }
            let sum = addsum(&buf).to_le_bytes();
            self.write(&sum, IO_TIMEOUT)?;
            let r = self.read(DATA_TIMEOUT)?;
            if !r.starts_with(b"OKAY") {
                bail!("checksum rejected at offset {off:#x}: {}", reply_text(&r));
            }
            progress(off + len, item.size);
        }
    }

    /// Ask the device to SHA-1 what it wrote. `tick` is called while it is busy.
    pub fn verify_sha1(&self, sha1: &str, mut tick: impl FnMut(Duration)) -> Result<()> {
        self.write(format!("oem verify sha1sum {sha1}").as_bytes(), IO_TIMEOUT)?;
        let start = Instant::now();
        loop {
            if start.elapsed() > VERIFY_TIMEOUT {
                bail!("device did not finish verification in time");
            }
            let r = match self.read(Duration::from_secs(30)) {
                Ok(r) => r,
                Err(e) if e.downcast_ref::<rusb::Error>() == Some(&rusb::Error::Timeout) => {
                    tick(start.elapsed());
                    continue;
                }
                Err(e) => return Err(e),
            };
            match &r[..4] {
                b"OKAY" => return Ok(()),
                b"INFO" => {
                    tick(start.elapsed());
                    std::thread::sleep(Duration::from_millis(500));
                }
                _ => bail!("verification FAILED: {}", reply_text(&r)),
            }
        }
    }

    pub fn reboot(&self) -> Result<()> {
        // The device usually drops off the bus before answering; that is fine.
        let _ = self.write(b"reboot", IO_TIMEOUT);
        let _ = self.read(Duration::from_secs(2));
        Ok(())
    }
}

impl Drop for Adnl {
    fn drop(&mut self) {
        let _ = self.h.release_interface(self.iface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addsum_matches_pyamlboot() {
        // python: sum(int.from_bytes(buf[i:i+4],'little') for i in range(0,len(buf),4)) & 0xffffffff
        assert_eq!(addsum(&[1, 0, 0, 0, 2, 0, 0, 0]), 3);
        assert_eq!(addsum(&[0xff, 0xff, 0xff, 0xff, 2, 0, 0, 0]), 1);
        assert_eq!(addsum(&[1, 2, 3]), 0x0003_0201);
    }

    #[test]
    fn dataout_parsing() {
        assert_eq!(parse_dataout("DATAOUT0x4000:0x1f000").unwrap(), (0x4000, 0x1f000));
        assert_eq!(parse_dataout("DATAOUT4000:1f000").unwrap(), (0x4000, 0x1f000));
        assert!(parse_dataout("OKAY").is_err());
    }
}
