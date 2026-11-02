//! Wire protocol between `omp-deck-host` daemons and clients.
//!
//! Frame: `u32 LE payload length` (≤ [`MAX_FRAME`]) + `u8 kind` + payload. Byte payloads
//! (`Input`, `Output`, `Snapshot`) are raw; control payloads are JSON (unknown fields ignored).
//! Both sides send `Hello` first. The `Hello` and `Kill` kinds and their payloads never change,
//! so any client can still kill a daemon of another protocol version.
//!
//! `Observe` is an additive read-only subscription: the daemon answers `Info`, `Resized`
//! (current grid), `Snapshot`… `SnapshotEnd`, then live `Output`/`Resized` in host order, then
//! `Exited` and end of stream. A daemon predating `Observe` closes the connection on the
//! unknown kind; clients report that, there is no fallback.
//!
//! `TspPeek` (additive, no payload) asks a session started with [`SessionSpec::tsp`] for its
//! TSP document: `Snapshot`… `SnapshotEnd` carrying one JSON document (see
//! `deck_tsp::document::Document`'s `Serialize`), or `Error` when the session is not a TSP one.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Protocol major version; a mismatch is [`Error::ProtocolMismatch`].
pub const PROTOCOL: u32 = 1;

/// Largest frame payload either side sends or accepts.
pub const MAX_FRAME: usize = 1 << 20;

/// Snapshot replay is split into chunks of this size.
pub(crate) const SNAPSHOT_CHUNK: usize = 256 * 1024;

/// Frame kinds. Values are part of the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    // Both directions.
    Hello = 0x01,
    // Client -> daemon.
    Attach = 0x02,
    Input = 0x03,
    Resize = 0x04,
    Palette = 0x05,
    Detach = 0x06,
    Kill = 0x07,
    Info = 0x08,
    Ping = 0x09,
    /// Non-displacing snapshot request (`dump`): `Peek{history_lines}`, answered with
    /// `Snapshot`… `SnapshotEnd` on the same connection.
    Peek = 0x0a,
    /// Read-only live subscription: `Observe{history_lines}` (see the module docs). The
    /// connection may afterwards send only `Ping` and `Detach`; anything else makes the daemon
    /// drop the subscription and close the connection immediately, without a reply.
    Observe = 0x0b,
    /// TSP document request: no payload, answered with `Snapshot`… `SnapshotEnd` (the JSON
    /// document) or `Error` for a session without TSP.
    TspPeek = 0x0c,
    // Daemon -> client (`Info` reuses 0x08 with a `HostInfo` payload).
    Snapshot = 0x21,
    SnapshotEnd = 0x22,
    Output = 0x23,
    Exited = 0x24,
    Displaced = 0x25,
    Error = 0x26,
    Pong = 0x27,
    /// Observers only: the session's grid (JSON [`Grid`]), before the snapshot and on every
    /// size change.
    Resized = 0x28,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Kind> {
        Some(match v {
            0x01 => Kind::Hello,
            0x02 => Kind::Attach,
            0x03 => Kind::Input,
            0x04 => Kind::Resize,
            0x05 => Kind::Palette,
            0x06 => Kind::Detach,
            0x07 => Kind::Kill,
            0x08 => Kind::Info,
            0x09 => Kind::Ping,
            0x0a => Kind::Peek,
            0x0b => Kind::Observe,
            0x0c => Kind::TspPeek,
            0x21 => Kind::Snapshot,
            0x22 => Kind::SnapshotEnd,
            0x23 => Kind::Output,
            0x24 => Kind::Exited,
            0x25 => Kind::Displaced,
            0x26 => Kind::Error,
            0x27 => Kind::Pong,
            0x28 => Kind::Resized,
            _ => return None,
        })
    }
}

const TRUNCATED_HEADER: &str = "truncated frame header";
const TRUNCATED_PAYLOAD: &str = "truncated frame payload";

/// Whether `e` is [`read_frame`]'s end of stream in the middle of a frame (e.g. the daemon
/// aborted a connection while writing).
pub(crate) fn is_truncated(e: &Error) -> bool {
    matches!(e, Error::Protocol(msg) if msg == TRUNCATED_HEADER || msg == TRUNCATED_PAYLOAD)
}

/// Reads one frame into `payload` (cleared first). `Ok(None)` on a clean EOF at a frame boundary.
pub fn read_frame(r: &mut impl Read, payload: &mut Vec<u8>) -> Result<Option<Kind>> {
    let mut header = [0u8; 5];
    let mut got = 0;
    while got < header.len() {
        match r.read(&mut header[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(Error::Protocol(TRUNCATED_HEADER.into())),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if len > MAX_FRAME {
        return Err(Error::FrameTooLarge(len));
    }
    let kind = Kind::from_u8(header[4])
        .ok_or_else(|| Error::Protocol(format!("unknown frame kind {:#04x}", header[4])))?;
    payload.clear();
    payload.resize(len, 0);
    r.read_exact(payload).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => Error::Protocol(TRUNCATED_PAYLOAD.into()),
        _ => Error::Io(e),
    })?;
    Ok(Some(kind))
}

/// Writes one frame (header and payload in one buffer, so a single `write_all`).
pub fn write_frame(
    w: &mut impl Write,
    buf: &mut Vec<u8>,
    kind: Kind,
    payload: &[u8],
) -> Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(Error::FrameTooLarge(payload.len()));
    }
    buf.clear();
    // Length fits: checked against MAX_FRAME (1 MiB) above.
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.push(kind as u8);
    buf.extend_from_slice(payload);
    w.write_all(buf)?;
    w.flush()?;
    Ok(())
}

/// Writes a JSON control frame.
pub fn write_json<T: Serialize>(
    w: &mut impl Write,
    buf: &mut Vec<u8>,
    kind: Kind,
    value: &T,
) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    write_frame(w, buf, kind, &payload)
}

/// Parses a JSON control payload.
pub fn parse<'a, T: Deserialize<'a>>(payload: &'a [u8]) -> Result<T> {
    serde_json::from_slice(payload)
        .map_err(|e| Error::Protocol(format!("bad control payload: {e}")))
}

/// First frame in both directions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
}

impl Hello {
    pub fn ours() -> Self {
        Hello {
            protocol: PROTOCOL,
            version: crate::HOST_VERSION.to_owned(),
        }
    }
}

/// Colors the daemon's terminal uses to answer OSC 4/10/11/12 queries: indices 0..16 are the
/// ANSI colors, 16 = foreground, 17 = background, 18 = cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Palette(pub [[u8; 3]; 19]);

impl Palette {
    pub const FOREGROUND: usize = 16;
    pub const BACKGROUND: usize = 17;
    pub const CURSOR: usize = 18;
}

impl Default for Palette {
    /// xterm's default colors on a dark background.
    fn default() -> Self {
        Palette([
            [0x00, 0x00, 0x00],
            [0xcd, 0x00, 0x00],
            [0x00, 0xcd, 0x00],
            [0xcd, 0xcd, 0x00],
            [0x00, 0x00, 0xee],
            [0xcd, 0x00, 0xcd],
            [0x00, 0xcd, 0xcd],
            [0xe5, 0xe5, 0xe5],
            [0x7f, 0x7f, 0x7f],
            [0xff, 0x00, 0x00],
            [0x00, 0xff, 0x00],
            [0xff, 0xff, 0x00],
            [0x5c, 0x5c, 0xff],
            [0xff, 0x00, 0xff],
            [0x00, 0xff, 0xff],
            [0xff, 0xff, 0xff],
            [0xe5, 0xe5, 0xe5],
            [0x00, 0x00, 0x00],
            [0xe5, 0xe5, 0xe5],
        ])
    }
}

/// `Attach` payload. `cols`/`rows` of 0 keep the session's current size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachParams {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
    /// Scrollback rows to include in the snapshot (capped by the daemon's history).
    pub history_lines: u32,
    pub palette: Palette,
}

/// `Resize` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeParams {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

/// `Peek` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeekParams {
    pub history_lines: u32,
}

/// `Observe` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveParams {
    /// Scrollback rows to include in the snapshot (capped by the daemon's history).
    pub history_lines: u32,
}

/// A session's terminal grid as observers see it (`Resized` payload): cells plus cell pixels
/// (0 when no controller reported them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

/// `Exited` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitedPayload {
    pub code: Option<i32>,
}

/// What a session runs. Passed to the daemon through a private spec file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpec {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Applied after `env_remove` on top of the daemon's inherited environment.
    pub env_set: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    /// Act as a TSP terminal for the program (see `docs/notes/tsp.md`): answer its `hello`
    /// query, apply its surface messages to a document and ack frames, even detached.
    #[serde(default)]
    pub tsp: bool,
}

/// The spec file a client writes for `serve --spec`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SpawnRequest {
    pub spec: SessionSpec,
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
    pub palette: Palette,
}

/// `Info` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub name: String,
    pub protocol: u32,
    pub host_version: String,
    pub daemon_pid: u32,
    pub child_pid: u32,
    pub child_started_unix_ms: u64,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    /// Unix: PTY slave path (`/dev/pts/N`).
    pub tty: Option<String>,
    /// `TERM_SESSION_ID` given to the child (set on Windows for omp's breadcrumb).
    pub term_session_id: Option<String>,
    pub attached: bool,
    pub cols: u16,
    pub rows: u16,
    /// Current terminal title (OSC 0/2); `None` when none was set or from an older daemon.
    #[serde(default)]
    pub title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_and_exact_header() {
        let mut wire = Vec::new();
        let mut buf = Vec::new();
        write_frame(&mut wire, &mut buf, Kind::Input, b"ab").unwrap();
        assert_eq!(wire, [2, 0, 0, 0, 0x03, b'a', b'b']);
        write_frame(&mut wire, &mut buf, Kind::Detach, b"").unwrap();
        let mut r = &wire[..];
        let mut payload = Vec::new();
        assert_eq!(read_frame(&mut r, &mut payload).unwrap(), Some(Kind::Input));
        assert_eq!(payload, b"ab");
        assert_eq!(
            read_frame(&mut r, &mut payload).unwrap(),
            Some(Kind::Detach)
        );
        assert!(payload.is_empty());
        assert_eq!(read_frame(&mut r, &mut payload).unwrap(), None);
    }

    #[test]
    fn oversized_and_truncated_frames_are_errors() {
        let mut payload = Vec::new();
        let mut big: &[u8] = &[0x01, 0x00, 0x10, 0x00, 0x03];
        assert!(matches!(
            read_frame(&mut big, &mut payload),
            Err(Error::FrameTooLarge(0x0010_0001))
        ));
        let mut short: &[u8] = &[0x02, 0, 0];
        assert!(matches!(
            read_frame(&mut short, &mut payload),
            Err(Error::Protocol(_))
        ));
        let mut body: &[u8] = &[0x02, 0, 0, 0, 0x03, b'a'];
        assert!(matches!(
            read_frame(&mut body, &mut payload),
            Err(Error::Protocol(_))
        ));
        let mut unknown: &[u8] = &[0, 0, 0, 0, 0x7f];
        assert!(matches!(
            read_frame(&mut unknown, &mut payload),
            Err(Error::Protocol(_))
        ));
        let mut exact = vec![0u8; MAX_FRAME];
        let mut wire = Vec::new();
        write_frame(&mut wire, &mut Vec::new(), Kind::Output, &exact).unwrap();
        exact.push(0);
        assert!(matches!(
            write_frame(&mut wire, &mut Vec::new(), Kind::Output, &exact),
            Err(Error::FrameTooLarge(_))
        ));
    }

    #[test]
    fn hello_and_kill_wire_values_are_frozen() {
        assert_eq!(Kind::Hello as u8, 0x01);
        assert_eq!(Kind::Kill as u8, 0x07);
        let hello: Hello = parse(br#"{"protocol":7,"version":"9.9.9","future":true}"#).unwrap();
        assert_eq!(hello.protocol, 7);
    }

    #[test]
    fn info_without_title_from_an_old_daemon_parses() {
        let old = br#"{"name":"n","protocol":1,"host_version":"0","daemon_pid":1,"child_pid":2,
            "child_started_unix_ms":3,"cwd":"/","argv":[],"tty":null,"term_session_id":null,
            "attached":false,"cols":80,"rows":24}"#;
        let info: HostInfo = parse(old).unwrap();
        assert_eq!(info.title, None);
    }
}
