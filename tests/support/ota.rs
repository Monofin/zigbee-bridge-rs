use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
pub fn image(version: u32, dest: Option<u64>, hw: Option<(u16, u16)>) -> Vec<u8> {
    let mut data = vec![0; 56];
    data[0..4].copy_from_slice(&0x0beef11eu32.to_le_bytes());
    data[4..6].copy_from_slice(&0x0100u16.to_le_bytes());
    data[8] = 2 * u8::from(dest.is_some()) + 4 * u8::from(hw.is_some());
    data[10..14].copy_from_slice(&[0x34, 0x12, 0x56, 0x00]);
    data[14..18].copy_from_slice(&version.to_le_bytes());
    data[18] = 2;
    if let Some(addr) = dest {
        data.extend(addr.to_le_bytes());
    }
    if let Some((min, max)) = hw {
        data.extend(min.to_le_bytes());
        data.extend(max.to_le_bytes());
    }
    let header = data.len() as u16;
    data[6..8].copy_from_slice(&header.to_le_bytes());
    data.extend([0, 0, 8, 0, 0, 0]);
    data.extend(*b"TESTONLY");
    let n = data.len() as u32;
    data[52..56].copy_from_slice(&n.to_le_bytes());
    data
}
pub fn entry(dir: &Path, id: &str, bytes: &[u8]) -> Result<Value> {
    fs::write(dir.join(format!("{id}.ota")), bytes)?;
    Ok(
        json!({"id":id, "file":format!("{id}.ota"),"source":"https://example.invalid/firmware","sha256":format!("{:x}",Sha256::digest(bytes))}),
    )
}
pub fn manifest(dir: &Path, entries: Vec<Value>) -> Result<PathBuf> {
    let p = dir.join("manifest.json");
    fs::write(&p, serde_json::to_vec(&json!({"images":entries}))?)?;
    Ok(p)
}
pub fn query(version: u32, hw: Option<u16>) -> Vec<u8> {
    let mut msg = vec![1, 7, 1, u8::from(hw.is_some()), 0x34, 0x12, 0x56, 0];
    msg.extend(version.to_le_bytes());
    if let Some(hw) = hw {
        msg.extend(hw.to_le_bytes());
    }
    msg
}
pub fn block(version: u32, offset: u32, size: u8) -> Vec<u8> {
    let mut msg = vec![1, 9, 3, 0, 0x34, 0x12, 0x56, 0];
    msg.extend(version.to_le_bytes());
    msg.extend(offset.to_le_bytes());
    msg.push(size);
    msg
}
