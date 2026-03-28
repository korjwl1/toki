use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter};
use std::net::TcpStream;
use std::time::Duration;

use crate::db::SCHEMA_VERSION;
use super::protocol::{
    AuthErrPayload, AuthOkPayload, AuthPayload, LastTsPayload, MsgType,
    SyncAckPayload, SyncBatchPayload, SyncErrPayload, SyncItem,
    read_frame, write_empty_frame, write_frame,
};

const READ_TIMEOUT: Duration = Duration::from_secs(90);  // PING every 60s; allow margin
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const BATCH_SIZE: usize = 1000;

pub struct SyncClient {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
}

impl SyncClient {
    /// Connect to server and perform TCP handshake (no auth yet).
    pub fn connect(addr: &str) -> io::Result<Self> {
        // Resolve hostname (handles both "host:port" and "1.2.3.4:port").
        use std::net::ToSocketAddrs;
        let socket_addr = addr
            .to_socket_addrs()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses resolved"))?;

        let stream = TcpStream::connect_timeout(&socket_addr, CONNECT_TIMEOUT)?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_nodelay(true)?;

        let reader = BufReader::new(stream.try_clone()?);
        let writer = BufWriter::new(stream);
        Ok(Self { reader, writer })
    }

    /// Send AUTH and wait for AUTH_OK / AUTH_ERR.
    /// Returns device_id on success.
    pub fn auth(
        &mut self,
        jwt: &str,
        device_name: &str,
        provider: &str,
    ) -> Result<String, AuthError> {
        let payload = AuthPayload {
            jwt: jwt.to_string(),
            device_name: device_name.to_string(),
            schema_version: SCHEMA_VERSION,
            provider: provider.to_string(),
        };
        let bytes = bincode::serialize(&payload).map_err(|e| AuthError::Protocol(e.to_string()))?;
        write_frame(&mut self.writer, MsgType::Auth, &bytes)
            .map_err(AuthError::Io)?;

        let (msg_type, payload) = read_frame(&mut self.reader).map_err(AuthError::Io)?;
        match msg_type {
            MsgType::AuthOk => {
                let ok: AuthOkPayload = bincode::deserialize(&payload)
                    .map_err(|e| AuthError::Protocol(e.to_string()))?;
                Ok(ok.device_id)
            }
            MsgType::AuthErr => {
                let err: AuthErrPayload = bincode::deserialize(&payload)
                    .map_err(|e| AuthError::Protocol(e.to_string()))?;
                Err(AuthError::Rejected { reason: err.reason, reset_required: err.reset_required })
            }
            other => Err(AuthError::Protocol(format!("unexpected response to AUTH: {other:?}"))),
        }
    }

    /// Send GET_LAST_TS and receive LAST_TS.
    pub fn get_last_ts(&mut self) -> io::Result<i64> {
        write_empty_frame(&mut self.writer, MsgType::GetLastTs)?;
        let (msg_type, payload) = read_frame(&mut self.reader)?;
        if msg_type != MsgType::LastTs {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected LAST_TS, got {msg_type:?}"),
            ));
        }
        let p: LastTsPayload = bincode::deserialize(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(p.ts_ms)
    }

    /// Send a SYNC_BATCH and wait for SYNC_ACK / SYNC_ERR.
    /// Returns the ack'd last_ts_ms on success.
    pub fn sync_batch(
        &mut self,
        items: Vec<SyncItem>,
        dict: &HashMap<u32, String>,
        provider: &str,
    ) -> Result<i64, SyncError> {
        // Only include dict entries referenced by this batch
        let used_ids: std::collections::HashSet<u32> = items.iter().flat_map(|item| {
            [
                item.event.model_id,
                item.event.session_id,
                item.event.source_file_id,
                item.event.project_name_id,
            ]
        }).collect();
        let batch_dict: HashMap<u32, String> = dict
            .iter()
            .filter(|(id, _)| used_ids.contains(id))
            .map(|(&id, v)| (id, v.clone()))
            .collect();

        let payload = SyncBatchPayload {
            items,
            dict: batch_dict,
            provider: provider.to_string(),
        };
        let bytes = bincode::serialize(&payload)
            .map_err(|e| SyncError::Protocol(e.to_string()))?;
        write_frame(&mut self.writer, MsgType::SyncBatch, &bytes)
            .map_err(SyncError::Io)?;

        let (msg_type, resp_payload) = read_frame(&mut self.reader).map_err(SyncError::Io)?;
        match msg_type {
            MsgType::SyncAck => {
                let ack: SyncAckPayload = bincode::deserialize(&resp_payload)
                    .map_err(|e| SyncError::Protocol(e.to_string()))?;
                Ok(ack.last_ts_ms)
            }
            MsgType::SyncErr => {
                let err: SyncErrPayload = bincode::deserialize(&resp_payload)
                    .map_err(|e| SyncError::Protocol(e.to_string()))?;
                Err(SyncError::ServerError(err.reason))
            }
            other => Err(SyncError::Protocol(format!("expected SYNC_ACK, got {other:?}"))),
        }
    }

    /// Send PING and expect PONG.
    pub fn ping(&mut self) -> io::Result<()> {
        write_empty_frame(&mut self.writer, MsgType::Ping)?;
        let (msg_type, _) = read_frame(&mut self.reader)?;
        if msg_type != MsgType::Pong {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected PONG, got {msg_type:?}"),
            ));
        }
        Ok(())
    }
}

// ─── Error types ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum AuthError {
    Io(io::Error),
    Protocol(String),
    Rejected { reason: String, reset_required: bool },
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::Protocol(s) => write!(f, "protocol error: {s}"),
            Self::Rejected { reason, .. } => write!(f, "auth rejected: {reason}"),
        }
    }
}

#[derive(Debug)]
pub enum SyncError {
    Io(io::Error),
    Protocol(String),
    ServerError(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::Protocol(s) => write!(f, "protocol error: {s}"),
            Self::ServerError(s) => write!(f, "server error: {s}"),
        }
    }
}
