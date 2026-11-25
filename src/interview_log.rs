//! Per-attempt observations. A historical response does not confer membership
//! or establish the device's present state.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    pub profile: u16,
    pub device: u16,
    /// Device version nibble, or 0xff when absent from an imported descriptor.
    pub version: u8,
    pub servers: Vec<u16>,
    pub clients: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum IdentitySource { Announcement, AdapterLookup, DeviceReply, LastKnownAddress, Z2mDatabase }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag="result",rename_all="snake_case")]
pub enum AttributeResult {
    Value { data_type:u8, bytes:Vec<u8> },
    Error { status:u8 },
    Omitted,
}

impl AttributeResult {
    /// R8 §7.3.2.13.5 defaults the optional SecurityLevel to network security.
    pub(crate) fn allows_lock_network_security(&self)->bool {
        matches!(self, Self::Value{data_type:0x30,bytes} if bytes.as_slice()==[0])
            || matches!(self,Self::Error{status:0x86})
    }

    pub(crate) fn character_string(&self) -> Option<&str> {
        let Self::Value { data_type: 0x42, bytes } = self else { return None };
        let (&len, text) = bytes.split_first()?;
        if len == 0xff || usize::from(len) != text.len() { return None; }
        std::str::from_utf8(text).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag="event", rename_all="snake_case")]
pub enum Observation {
    Target { ieee:String, short:u16, identity_source:IdentitySource },
    Request { sequence:u8, profile:u16, endpoint:u8, cluster:u16, payload:Vec<u8> },
    ReceiverOnWhenIdle { enabled: bool },
    Timeout { sequence:u8 },
    ZdoError { sequence:u8, status:u8 },
    ActiveEndpoints { sequence:u8, endpoints:Vec<u8> },
    SimpleDescriptor { sequence:u8, endpoint:u8, descriptor:Descriptor },
    Attribute { sequence:u8, endpoint:u8, cluster:u16, attribute:u16, #[serde(flatten)] result:AttributeResult },
    /// Reporting configuration from a previous coordinator, without a sampled value.
    ReportingConfigured { endpoint:u8, cluster:u16, attribute:u16 },
    ReportingReadback { endpoint:u8, cluster:u16, attribute:u16, data_type:u8, minimum:u16, maximum:u16, change:Option<Vec<u8>> },
    AttributeAccess { endpoint:u8, cluster:u16, attribute:u16, data_type:u8, access:u8 },
    CoverCommandsReviewed { endpoint:u8, commands:Vec<u8> },
    CommandsReceived { endpoint:u8, cluster:u16, commands:Vec<u8> },
    DefaultResponse { sequence:u8, command:u8, status:u8 },
    Finished,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    /// Zero denotes an unknown observation time in imported historical records.
    pub unix_ms:u64,
    #[serde(flatten)]
    pub observation:Observation,
}

/// A fresh private file per attempt prevents a partial or stale run from
/// overwriting an earlier interview. A newline commits one complete record.
pub struct InterviewLog { file: File, path:PathBuf }
impl InterviewLog {
    pub(crate) fn create(root:&Path) -> Result<Self> {
        let (file,path)=tempfile::Builder::new().prefix("interview-v1-").suffix(".jsonl").tempfile_in(root)?.keep()?;
        File::open(root)?.sync_all()?;
        Ok(Self{file,path})
    }
    pub fn path(&self)->&Path { &self.path }
    pub fn record(&mut self, observation:Observation)->Result<()> {
        let entry=Entry{unix_ms:SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis().try_into()?, observation};
        self.record_entry(&entry)
    }
    /// Preserve the supplied observation time when recording an existing entry.
    pub fn record_entry(&mut self, entry: &Entry) -> Result<()> {
        let mut bytes=serde_json::to_vec(entry)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.file.sync_data()?;
        Ok(())
    }
}

#[derive(Serialize)]
pub struct InterviewReport { pub finished:bool, pub entries:Vec<Entry> }
impl InterviewReport {
    /// A final unterminated record can be left by process or power loss. Earlier
    /// committed records remain readable. Corruption inside the log is an error.
    pub fn open(path:&Path)->Result<Self> {
        let file=crate::store::open_regular(path,File::options().read(true))?;
        let mut reader=BufReader::new(file);
        let mut bytes=Vec::new();
        let mut entries=Vec::new();
        let mut finished=false;
        loop {
            bytes.clear();
            reader.read_until(b'\n',&mut bytes)?;
            if bytes.last()!=Some(&b'\n') { break; }
            ensure!(!finished,"entry after interview finished");
            let entry:Entry=serde_json::from_slice(&bytes).with_context(||format!("invalid interview entry {}",entries.len()+1))?;
            finished=matches!(entry.observation,Observation::Finished);
            entries.push(entry);
        }
        Ok(Self{finished,entries})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    #[test]
    fn reopening_preserves_completed_facts_and_an_unanswered_request() -> Result<()> {
        let dir=tempfile::tempdir()?;
        let request=Observation::Request{sequence:2,profile:0x0104,endpoint:1,cluster:6,payload:vec![0,2,0,0,0]};
        let list=Observation::ActiveEndpoints{sequence:1,endpoints:vec![1,242]};
        let path={
            let store=Store::open(dir.path())?;
            let mut attempt=store.begin_interview()?;
            attempt.record(list.clone())?;
            attempt.record(request.clone())?;
            attempt.file.write_all(br#"{"unix_ms":"#)?;
            attempt.path().to_owned()
        };
        let store=Store::open(dir.path())?;
        let mut second=store.begin_interview()?;
        second.record(Observation::Finished)?;
        assert!(InterviewReport::open(second.path())?.finished);
        let old=InterviewReport::open(&path)?;
        assert!(!old.finished);
        assert_eq!(old.entries.into_iter().map(|e|e.observation).collect::<Vec<_>>(),[list,request]);
        File::options().append(true).open(&path)?.write_all(b"bad}\n")?;
        assert!(InterviewReport::open(&path).is_err());
        Ok(())
    }
}
