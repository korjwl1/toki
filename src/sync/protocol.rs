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
/// Values are hex-grouped by category: 0x01-0x03 auth, 0x10-0x11 cursor,
/// 0x20-0x22 batch, 0x30-0x31 keepalive.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Auth        = 0x01,
    AuthOk      = 0x02,
    AuthErr     = 0x03,
    GetLastTs   = 0x10,
    LastTs      = 0x11,
    SyncBatch     = 0x20,
    SyncAck       = 0x21,
    SyncErr       = 0x22,
    SyncBatchZstd = 0x23,
    Ping        = 0x30,
    Pong        = 0x31,
}

impl MsgType {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0x01 => Some(Self::Auth),
            0x02 => Some(Self::AuthOk),
            0x03 => Some(Self::AuthErr),
            0x10 => Some(Self::GetLastTs),
            0x11 => Some(Self::LastTs),
            0x20 => Some(Self::SyncBatch),
            0x21 => Some(Self::SyncAck),
            0x22 => Some(Self::SyncErr),
            0x23 => Some(Self::SyncBatchZstd),
            0x30 => Some(Self::Ping),
            0x31 => Some(Self::Pong),
            _    => None,
        }
    }
}

/// Current sync protocol version. Server rejects clients with unsupported versions.
pub const PROTOCOL_VERSION: u16 = 1;

// ─── Payloads ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthPayload {
    pub jwt: String,
    pub device_name: String,
    pub schema_version: u32,
    pub provider: String,
    /// Stable UUID generated at `toki sync enable`, uniquely identifies this device.
    pub device_key: String,
    /// Sync protocol version. Server rejects unsupported versions.
    pub protocol_version: u16,
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
    pub event: StoredEvent,
}

/// Payload for GET_LAST_TS: specifies which provider's cursor to query.
#[derive(Debug, Serialize, Deserialize)]
pub struct GetLastTsPayload {
    pub provider: String,
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
