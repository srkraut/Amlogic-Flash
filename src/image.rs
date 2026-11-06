//! Amlogic USB Burning Tool image (`aml_upgrade_package.img`) reader.
//!
//! Layout (little endian):
//!   header (64 bytes): crc32 | version | magic 0x27B51956 | size u64 | align u32 | item_count u32 | reserved
//!   item table: item_count entries (v1: 128 bytes, v2: 576 bytes)
//!   item data
//! The header CRC is `!crc32(file[4..])` (zlib CRC32 with the final XOR inverted).
//! Every `PARTITION` item normally has a matching `VERIFY` item containing `sha1sum <hex>`.

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const IMAGE_MAGIC: u32 = 0x27B5_1956;
pub const SPARSE_MAGIC: [u8; 4] = [0x3A, 0xFF, 0x26, 0xED];
const FILE_TYPE_SPARSE: u32 = 0xFE;

/// Partitions this tool refuses to write: they need the full ROM/BL2 burn flow
/// and a mistake there can hard-brick the box.
pub const PROTECTED_PARTITIONS: &[&str] = &["bootloader", "_aml_dtb"];

#[derive(Clone, Debug)]
pub struct Item {
    pub main_type: String,
    pub sub_type: String,
    pub file_type: u32,
    pub offset: u64,
    pub size: u64,
}

impl Item {
    pub fn is_partition(&self) -> bool {
        self.main_type == "PARTITION"
    }
    pub fn is_sparse(&self) -> bool {
        self.file_type == FILE_TYPE_SPARSE
    }
    /// Image format string the ADNL `mwrite` command expects.
    pub fn format(&self) -> &'static str {
        if self.is_sparse() { "sparse" } else { "normal" }
    }
    pub fn is_protected(&self) -> bool {
        PROTECTED_PARTITIONS.contains(&self.sub_type.as_str())
    }
}

pub struct AmlImage {
    pub path: PathBuf,
    pub version: u32,
    pub stored_crc: u32,
    pub declared_size: u64,
    pub file_size: u64,
    pub items: Vec<Item>,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn cstr(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

impl AmlImage {
    pub fn open(path: &Path) -> Result<Self> {
        let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let file_size = f.metadata()?.len();
        let mut h = [0u8; 64];
        f.read_exact(&mut h).context("file too small for an Amlogic image header")?;

        let stored_crc = u32_at(&h, 0);
        let version = u32_at(&h, 4);
        let magic = u32_at(&h, 8);
        if magic != IMAGE_MAGIC {
            bail!("not an Amlogic burn image (magic {magic:#010x}, expected {IMAGE_MAGIC:#010x})");
        }
        let declared_size = u64_at(&h, 12);
        let count = u32_at(&h, 24) as usize;
        let (entry_size, name_len) = match version {
            1 => (128usize, 32usize),
            2 => (576, 256),
            v => bail!("unsupported Amlogic image version {v}"),
        };
        if count == 0 || count > 1024 {
            bail!("implausible item count {count}");
        }

        let mut items = Vec::with_capacity(count);
        let mut e = vec![0u8; entry_size];
        for _ in 0..count {
            f.read_exact(&mut e).context("truncated item table")?;
            let item = Item {
                file_type: u32_at(&e, 4),
                offset: u64_at(&e, 16),
                size: u64_at(&e, 24),
                main_type: cstr(&e[32..32 + name_len]),
                sub_type: cstr(&e[32 + name_len..32 + 2 * name_len]),
            };
            if item.offset.checked_add(item.size).is_none_or(|end| end > file_size) {
                bail!("item {}/{} extends past the end of the file (truncated download?)", item.main_type, item.sub_type);
            }
            items.push(item);
        }
        Ok(Self { path: path.to_path_buf(), version, stored_crc, declared_size, file_size, items })
    }

    pub fn find(&self, main: &str, sub: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.main_type == main && i.sub_type == sub)
    }

    pub fn partitions(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.is_partition())
    }

    pub fn partition(&self, name: &str) -> Option<&Item> {
        self.find("PARTITION", name)
    }

    /// Expected SHA-1 (lowercase hex) for a partition, taken from its VERIFY item.
    pub fn expected_sha1(&self, part: &str) -> Result<Option<String>> {
        let Some(v) = self.find("VERIFY", part) else { return Ok(None) };
        if v.size > 4096 {
            bail!("VERIFY item for {part} is unexpectedly large");
        }
        let text = String::from_utf8_lossy(&self.reader(v)?.read_all()?).trim_matches(char::from(0)).trim().to_string();
        match text.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["sha1sum", hex] if hex.len() == 40 && hex.chars().all(|c| c.is_ascii_hexdigit()) => Ok(Some(hex.to_lowercase())),
            _ => bail!("unrecognised VERIFY entry for {part}: {text:?}"),
        }
    }

    pub fn reader(&self, item: &Item) -> Result<ItemReader> {
        Ok(ItemReader { f: File::open(&self.path)?, base: item.offset, size: item.size })
    }

    /// `true` when the header CRC and size field match the file.
    pub fn check_crc(&self, mut progress: impl FnMut(u64, u64)) -> Result<bool> {
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(4))?;
        let mut hasher = crc32fast::Hasher::new();
        let mut buf = vec![0u8; 8 << 20];
        let total = self.file_size - 4;
        let mut done = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            done += n as u64;
            progress(done, total);
        }
        Ok(!hasher.finalize() == self.stored_crc && self.declared_size == self.file_size)
    }

    pub fn sha1_of(&self, item: &Item, mut progress: impl FnMut(u64, u64)) -> Result<String> {
        let mut r = self.reader(item)?;
        let mut h = Sha1::new();
        let mut buf = vec![0u8; 8 << 20];
        let mut off = 0u64;
        while off < item.size {
            let n = (item.size - off).min(buf.len() as u64) as usize;
            r.read_at(off, &mut buf[..n])?;
            h.update(&buf[..n]);
            off += n as u64;
            progress(off, item.size);
        }
        Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }
}

pub struct ItemReader {
    f: File,
    base: u64,
    size: u64,
}

impl ItemReader {
    pub fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        if off + buf.len() as u64 > self.size {
            bail!("read past end of item (offset {off:#x}, len {:#x}, size {:#x})", buf.len(), self.size);
        }
        self.f.seek(SeekFrom::Start(self.base + off))?;
        self.f.read_exact(buf)?;
        Ok(())
    }
    pub fn read_all(&mut self) -> Result<Vec<u8>> {
        let mut v = vec![0u8; self.size as usize];
        self.read_at(0, &mut v)?;
        Ok(v)
    }
}

pub fn human(bytes: u64) -> String {
    const U: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", U[i]) }
}
