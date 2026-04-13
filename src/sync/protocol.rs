/// Toki sync binary protocol.
///
/// Wire types are defined in the shared `toki-sync-protocol` crate.
/// This module provides synchronous (std::io) frame I/O.

use std::io::{self, Read, Write};

// Re-export shared wire types so existing imports continue to work.
// Note: StoredEvent is NOT re-exported here — the local fjall StoredEvent is in
// crate::common::types. The wire protocol StoredEvent (with Vec<u64> tokens) is
// accessed as toki_sync_protocol::StoredEvent when building sync batches.
pub use toki_sync_protocol::{
    MsgType, AuthPayload, AuthOkPayload, AuthErrPayload,
    GetLastTsPayload, LastTsPayload,
    SyncItem, SyncBatchPayload, SyncAckPayload, SyncErrPayload,
    PROTOCOL_VERSION, MAX_PAYLOAD_SIZE,
};

// ─── Frame read/write (synchronous) ────────────────────────────────────────

pub fn write_frame<W: Write>(w: &mut W, msg_type: MsgType, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD_SIZE as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("payload too large: {} bytes (max {MAX_PAYLOAD_SIZE})", payload.len()),
        ));
    }
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
