//! Zigbee Cluster Library R8 §§11.4–11.5. All integers are little endian.
use crate::store::open_regular;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Component, Path},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    images: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    file: String,
    source: String,
    sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Key {
    pub manufacturer: u16,
    pub image_type: u16,
    pub version: u32,
}

pub(super) struct Image {
    pub bytes: Vec<u8>,
    pub key: Key,
    pub hardware: Option<(u16, u16)>,
    pub destination: Option<u64>,
    pub digest: String,
}

impl Image {
    fn parse(bytes: Vec<u8>, digest: String) -> Result<Self> {
        ensure!(bytes.len() >= 56, "truncated OTA header");
        let word = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let long = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        ensure!(
            long(0) == 0x0beef11e && word(4) == 0x0100,
            "unsupported OTA file header"
        );
        ensure!(
            usize::try_from(long(52))? == bytes.len(),
            "OTA total length differs from file length"
        );
        let flags = word(8);
        ensure!(flags & !7 == 0, "unknown OTA header flags");
        let header_length = 56
            + usize::from(flags & 1 != 0)
            + 8 * usize::from(flags & 2 != 0)
            + 4 * usize::from(flags & 4 != 0);
        ensure!(
            usize::from(word(6)) == header_length && bytes.len() >= header_length,
            "invalid OTA header length"
        );
        let key = Key {
            manufacturer: word(10),
            image_type: word(12),
            version: long(14),
        };
        ensure!(
            key.manufacturer != u16::MAX && key.image_type <= 0xffbf && key.version != u32::MAX,
            "expected a manufacturer firmware image"
        );
        let dest_start = 56 + usize::from(flags & 1 != 0);
        let destination = (flags & 2 != 0)
            .then(|| u64::from_le_bytes(bytes[dest_start..dest_start + 8].try_into().unwrap()));
        let hardware = (flags & 4 != 0).then(|| (word(header_length - 4), word(header_length - 2)));
        ensure!(
            hardware.is_none_or(|(min, max)| min <= max),
            "reversed OTA hardware range"
        );
        let mut elements = &bytes[header_length..];
        let mut firmware = false;
        while !elements.is_empty() {
            let (h, tail) = elements
                .split_first_chunk::<6>()
                .context("incomplete OTA subelement header")?;
            let id = u16::from_le_bytes([h[0], h[1]]);
            let size = usize::try_from(u32::from_le_bytes(h[2..6].try_into().unwrap()))?;
            if id == 0 && size != 0 {
                firmware = true;
            }
            elements = tail.get(size..).context("truncated OTA subelement data")?;
        }
        ensure!(firmware, "no firmware upgrade subelement");
        Ok(Self {
            bytes,
            key,
            destination,
            hardware,
            digest,
        })
    }
}

fn read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = open_regular(path, File::options().read(true))?;
    ensure!(
        file.metadata()?.len() <= limit as u64,
        "file exceeds catalogue memory budget"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "file exceeds catalogue memory budget");
    Ok(bytes)
}

/// Loaded bytes are pinned in memory. A replaced on-disk image cannot affect
/// a download already in progress.
pub(super) fn load(path: &Path) -> Result<BTreeMap<String, Image>> {
    let entries: Manifest = serde_json::from_slice(&read(path, 4 * 1024 * 1024)?)?;
    let dir = path.parent().context("manifest directory")?;
    let mut images = BTreeMap::new();
    let mut budget = 128 * 1024 * 1024;
    for e in entries.images {
        ensure!(
            !e.id.is_empty() && !images.contains_key(&e.id),
            "empty or duplicate catalogue ID"
        );
        ensure!(
            e.source
                .strip_prefix("https://")
                .and_then(|rest| rest.split('/').next())
                .is_some_and(|host| !host.is_empty() && !host.contains('@')),
            "expected publisher HTTPS URL"
        );
        ensure!(
            matches!(
                (
                    Path::new(&e.file).components().next(),
                    Path::new(&e.file).components().count()
                ),
                (Some(Component::Normal(_)), 1)
            ),
            "image file must be a local catalogue basename"
        );
        ensure!(
            e.sha256.len() == 64 && e.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid catalogue SHA-256"
        );
        let bytes =
            read(&dir.join(e.file), budget).with_context(|| format!("read image {}", e.id))?;
        budget -= bytes.len();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        ensure!(
            digest.eq_ignore_ascii_case(&e.sha256),
            "checksum mismatch for {}",
            e.id
        );
        images.insert(e.id, Image::parse(bytes, digest)?);
    }
    Ok(images)
}
