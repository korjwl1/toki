/// Toki sync binary protocol.
///
/// Frame format (all integers little-endian):
///   [4B msg_type: u32][4B payload_len: u32][payload bytes]
///
/// Max payload: 16 MiB (MAX_PAYLOAD_SIZE)

use std::collections::HashMap;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::common::types::StoredEvent;

pub const MAX_PAYLOAD_SIZE: u32 = 16 * 1024 * 1024; // 16 MiB

/// Message type discriminants.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Auth        = 1,
    AuthOk      = 2,
    AuthErr     = 3,
    GetLastTs   = 4,
    LastTs      = 5,
    SyncBatch   = 6,
    SyncAck     = 7,
    SyncErr     = 8,
    Ping        = 9,
    Pong        = 10,
}

impl MsgType {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            1  => Some(Self::Auth),
            2  => Some(Self::AuthOk),
            3  => Some(Self::AuthErr),
            4  => Some(Self::GetLastTs),
            5  => Some(Self::LastTs),
            6  => Some(Self::SyncBatch),
            7  => Some(Self::SyncAck),
            8  => Some(Self::SyncErr),
            9  => Some(Self::Ping),
            10 => Some(Self::Pong),
            _  => None,
        }
    }
}

// ─── Payloads ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthPayload {
    pub jwt: String,
    pub device_name: String,
    pub schema_version: u32,
    pub provider: String,
    /// Stable UUID generated at `toki sync enable`, uniquely identifies this device.
    pub device_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthOkPayload {
    pub device_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthErrPayload {
    pub reason: String,
    /// True if the client should delete its local sync cursor and re-sync from scratch.
    pub reset_required: bool,
}

/// A single event item in a sync batch.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SyncItem {
    pub ts_ms: i64,
    pub message_id: String,
    pub event: StoredEvent,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncBatchPayload {
    pub items: Vec<SyncItem>,
    /// Dictionary snapshot: all dict IDs referenced by items in this batch.
    pub dict: HashMap<u32, String>,
    pub provider: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncAckPayload {
    /// Last successfully processed event timestamp (ms). Client advances cursor to this.
    pub last_ts_ms: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncErrPayload {
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LastTsPayload {
    pub ts_ms: i64,
}

// ─── Frame read/write ────────────────────────────────────────────────────────

pub fn write_frame<W: Write>(w: &mut W, msg_type: MsgType, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() as u32;
    w.write_all(&(msg_type as u32).to_le_bytes())?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

pub fn write_empty_frame<W: Write>(w: &mut W, msg_type: MsgType) -> io::Result<()> {
    write_frame(w, msg_type, &[])
}

pub fn read_frame<R: Read>(r: &mut R) -> io::Result<(MsgType, Vec<u8>)> {
    let mut header = [0u8; 8];
    r.read_exact(&mut header)?;

    let type_u32 = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(header[4..8].try_into().unwrap());

    let msg_type = MsgType::from_u32(type_u32).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("unknown msg_type: {type_u32}"))
    })?;

    if len > MAX_PAYLOAD_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload too large: {len} bytes (max {MAX_PAYLOAD_SIZE})"),
        ));
    }

    let mut payload = vec![0u8; len as usize];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }

    Ok((msg_type, payload))
}
