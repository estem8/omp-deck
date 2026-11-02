//! Passive incremental scanner for TSP APC strings in a PTY output stream.
//!
//! [`Splitter::feed`] never alters or consumes the stream: the caller still feeds the same
//! bytes to its terminal emulator (alacritty/vte swallows APC silently). The scanner is a
//! byte state machine that keeps its state across calls, so a sequence may be split at any byte.
//!
//! Semantics (spec `transport.md`):
//! - A TSP message is `ESC _ tsp ; <verb> [; key=value]* ; <body> ESC \`. Other APC strings
//!   (kitty graphics `G…`) are skipped without being buffered.
//! - An ESC inside an APC that is not followed by `\` aborts it (as a terminal would end the
//!   string) and starts a fresh escape sequence.
//! - A body split with `c=<id>` (`m=1` on all chunks but the last) is joined byte for byte. A
//!   sequence is dropped when another TSP message other than its next chunk arrives
//!   (interrupted), after [`CHUNK_IDLE`] without a chunk, or when the joined body would pass
//!   [`MAX_BODY`]; the rest of a timed-out or oversized sequence is skipped up to its last chunk.
//! - One APC string (everything after `ESC _`) is capped at [`MAX_APC`] bytes; a longer one
//!   is dropped and logged.
//!
//! Idle expiry is evaluated when [`Splitter::feed`] is called, with or without bytes (this
//! crate has no clock or timer): a caller that must not wait for output that may never come
//! feeds an empty slice at the current time.

use std::time::{Duration, Instant};

const ESC: u8 = 0x1b;

/// Largest APC string accepted (spec: Tern abandons a longer one).
pub const MAX_APC: usize = 262_144;
/// Largest reassembled body: a 16 MiB blob in base64 plus slack for parameters.
pub const MAX_BODY: usize = (16usize << 20).div_ceil(3) * 4 + 4096;
/// A chunk sequence idle this long is dropped.
pub const CHUNK_IDLE: Duration = Duration::from_secs(1);

/// Body bytes per chunk of [`Splitter::resume_bytes`].
const RESUME_CHUNK: usize = 65_536;
/// Parameter of a chunk written by [`Splitter::resume_bytes`]: how many milliseconds old the
/// sequence's last chunk was. Never part of a [`Message`].
const RESUME_AGE: &str = "x-resume-age";
/// Parameter of the marker chunk written by [`Splitter::resume_bytes`] for a sequence whose
/// remaining chunks are skipped. Never part of a [`Message`].
const RESUME_SKIP: &str = "x-resume-skip";

/// A message verb (the letter after `tsp;`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// `q` query (program → terminal).
    Query,
    /// `o` open a surface.
    Open,
    /// `f` frame.
    Frame,
    /// `b` blob.
    Blob,
    /// `s` stylesheet.
    Style,
    /// `x` close a surface.
    Close,
    /// `t` palette (obsolete, ignored).
    Palette,
    /// `r` reply (terminal → program).
    Reply,
    /// `e` event (terminal → program).
    Event,
    /// Any other byte.
    Unknown(u8),
}

impl Verb {
    pub fn from_byte(b: u8) -> Verb {
        match b {
            b'q' => Verb::Query,
            b'o' => Verb::Open,
            b'f' => Verb::Frame,
            b'b' => Verb::Blob,
            b's' => Verb::Style,
            b'x' => Verb::Close,
            b't' => Verb::Palette,
            b'r' => Verb::Reply,
            b'e' => Verb::Event,
            other => Verb::Unknown(other),
        }
    }

    pub fn as_byte(self) -> u8 {
        match self {
            Verb::Query => b'q',
            Verb::Open => b'o',
            Verb::Frame => b'f',
            Verb::Blob => b'b',
            Verb::Style => b's',
            Verb::Close => b'x',
            Verb::Palette => b't',
            Verb::Reply => b'r',
            Verb::Event => b'e',
            Verb::Unknown(b) => b,
        }
    }
}

/// One complete (reassembled) TSP message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub verb: Verb,
    /// `key=value` parameters in order; for a chunked message those of the first chunk
    /// without `c` and `m`.
    pub params: Vec<(String, String)>,
    /// The body bytes (JSON, or base64 for blobs), chunks joined.
    pub body: Vec<u8>,
}

impl Message {
    /// The value of parameter `key`.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// Saw ESC outside a string.
    Esc,
    /// Inside an APC string.
    Apc,
    /// Saw ESC inside an APC string.
    ApcEsc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Fewer than 4 bytes seen: still a prefix of `tsp;`.
    Undecided,
    Tsp,
    /// Some other APC (kitty graphics …): skipped, not buffered.
    Other,
    /// A TSP string past [`MAX_APC`]: skipped to its terminator.
    Overflow,
}

struct Sequence {
    id: String,
    verb: Verb,
    params: Vec<(String, String)>,
    body: Vec<u8>,
    last: Instant,
}

/// Incremental TSP APC scanner; see the module docs.
pub struct Splitter {
    state: State,
    kind: Kind,
    /// Payload (after `ESC _`) of the APC being read, while [`Kind::Undecided`]/[`Kind::Tsp`].
    buf: Vec<u8>,
    /// Payload bytes seen so far in the current APC.
    seen: usize,
    sequence: Option<Sequence>,
    /// Chunk id of a dropped sequence whose remaining chunks are skipped.
    skipping: Option<String>,
}

impl Default for Splitter {
    fn default() -> Self {
        Splitter::new()
    }
}

impl Splitter {
    pub fn new() -> Splitter {
        Splitter {
            state: State::Ground,
            kind: Kind::Other,
            buf: Vec::new(),
            seen: 0,
            sequence: None,
            skipping: None,
        }
    }

    /// No APC string is being read and no chunked message is waiting for its next chunk: the
    /// stream is at a point where a fresh scanner would see the same messages from here on.
    /// A lone ESC does not count as in progress. The idle expiry of a pending chunk sequence
    /// is only evaluated when bytes arrive, so a sequence abandoned by the program keeps this
    /// `false` until the next output.
    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Ground | State::Esc) && self.sequence.is_none()
    }

    /// Appends bytes that, fed to a fresh splitter, leave it in this one's state: a sequence
    /// being skipped as a `x-resume-skip` chunk, a pending chunk sequence as `m=1` chunks of
    /// what has arrived (each carrying its age at `now` as `x-resume-age`, so it expires when
    /// this one does), then the APC string being read (TSP only; other APC strings are not
    /// buffered). A late joiner given the document's replay and then these bytes sees the
    /// messages this splitter completes from here on. A lone ESC is resumed as that ESC; an
    /// idle splitter otherwise appends nothing.
    pub fn resume_bytes(&self, now: Instant, out: &mut Vec<u8>) {
        if let Some(id) = &self.skipping {
            out.extend_from_slice(&encode_message(
                Verb::Frame,
                &[("c", id.as_str()), ("m", "1"), (RESUME_SKIP, "1")],
                b"",
            ));
        }
        if let Some(seq) = &self.sequence {
            let age = now
                .saturating_duration_since(seq.last)
                .as_millis()
                .to_string();
            let mut start = 0;
            let mut first = true;
            loop {
                let end = chunk_end(&seq.body, start, RESUME_CHUNK);
                let mut params: Vec<(&str, &str)> = Vec::new();
                if first {
                    params.extend(seq.params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
                }
                params.push(("c", seq.id.as_str()));
                params.push(("m", "1"));
                params.push((RESUME_AGE, age.as_str()));
                out.extend_from_slice(&encode_message(seq.verb, &params, &seq.body[start..end]));
                first = false;
                start = end;
                if start >= seq.body.len() {
                    break;
                }
            }
        }
        match (self.state, self.kind) {
            (State::Esc, _) => out.push(ESC),
            (State::Apc | State::ApcEsc, Kind::Undecided | Kind::Tsp) => {
                out.extend_from_slice(b"\x1b_");
                out.extend_from_slice(&self.buf);
                if self.state == State::ApcEsc {
                    out.push(ESC);
                }
            }
            _ => {}
        }
    }

    /// Scans `bytes` and returns the TSP messages completed by them. Allocates nothing when
    /// no message completes (the non-TSP fast path is one scan for ESC).
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> Vec<Message> {
        let mut out = Vec::new();
        self.feed_into(bytes, now, &mut out);
        out
    }

    /// [`Splitter::feed`] appending to `out`.
    pub fn feed_into(&mut self, bytes: &[u8], now: Instant, out: &mut Vec<Message>) {
        self.expire(now);
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => match find_esc(&bytes[i..]) {
                    None => return,
                    Some(p) => {
                        i += p + 1;
                        self.state = State::Esc;
                    }
                },
                State::Esc => {
                    let b = bytes[i];
                    i += 1;
                    match b {
                        b'_' => self.begin_apc(),
                        ESC => {}
                        _ => self.state = State::Ground,
                    }
                }
                State::Apc => {
                    let rest = &bytes[i..];
                    match find_esc(rest) {
                        None => {
                            self.push(rest);
                            return;
                        }
                        Some(p) => {
                            self.push(&rest[..p]);
                            i += p + 1;
                            self.state = State::ApcEsc;
                        }
                    }
                }
                State::ApcEsc => {
                    if bytes[i] == b'\\' {
                        i += 1;
                        self.finish_apc(now, out);
                        self.state = State::Ground;
                    } else {
                        // Not a string terminator: the APC is aborted and this byte follows
                        // the ESC as a new escape sequence (reprocessed, not consumed).
                        self.buf.clear();
                        self.state = State::Esc;
                    }
                }
            }
        }
    }

    fn begin_apc(&mut self) {
        self.buf.clear();
        self.seen = 0;
        self.kind = Kind::Undecided;
        self.state = State::Apc;
    }

    fn push(&mut self, bytes: &[u8]) {
        self.seen += bytes.len();
        if matches!(self.kind, Kind::Other | Kind::Overflow) {
            return;
        }
        if self.seen > MAX_APC {
            log::warn!("TSP APC string over {MAX_APC} bytes dropped");
            self.kind = Kind::Overflow;
            self.buf.clear();
            return;
        }
        self.buf.extend_from_slice(bytes);
        if self.kind == Kind::Undecided {
            const PREFIX: &[u8] = b"tsp;";
            if self.buf.len() >= PREFIX.len() {
                if self.buf.starts_with(PREFIX) {
                    self.kind = Kind::Tsp;
                } else {
                    self.kind = Kind::Other;
                    self.buf.clear();
                }
            } else if !PREFIX.starts_with(&self.buf) {
                self.kind = Kind::Other;
                self.buf.clear();
            }
        }
    }

    fn finish_apc(&mut self, now: Instant, out: &mut Vec<Message>) {
        if self.kind != Kind::Tsp {
            self.buf.clear();
            return;
        }
        let payload = std::mem::take(&mut self.buf);
        // `Kind::Tsp` is only set once the buffer holds at least the 4-byte `tsp;` prefix.
        if let Some(raw) = parse_payload(&payload[4..]) {
            self.accept(raw, now, out);
        } else {
            log::debug!("ignoring a TSP string without a verb");
        }
        // Reuse the allocation for the next string.
        self.buf = payload;
        self.buf.clear();
    }

    fn expire(&mut self, now: Instant) {
        if let Some(seq) = &self.sequence
            && now.saturating_duration_since(seq.last) > CHUNK_IDLE
        {
            log::warn!(
                "chunked TSP message c={} dropped: timed out",
                seq.id.as_str()
            );
            let id = seq.id.clone();
            self.sequence = None;
            self.skipping = Some(id);
        }
    }

    fn accept(&mut self, raw: Raw, now: Instant, out: &mut Vec<Message>) {
        self.expire(now);
        let chunk = raw
            .params
            .iter()
            .find(|(k, _)| k == "c")
            .map(|(_, v)| v.clone());
        let more = raw.params.iter().any(|(k, v)| k == "m" && v == "1");
        let Some(id) = chunk else {
            // An ordinary message interrupts a running sequence and is handled on its own.
            self.interrupt();
            self.skipping = None;
            out.push(raw.into_message());
            return;
        };
        if raw.params.iter().any(|(k, _)| k == RESUME_SKIP) {
            // The state of a splitter that was skipping this sequence (see `resume_bytes`).
            self.sequence = None;
            self.skipping = Some(id);
            return;
        }
        // A resumed chunk is as old as it was at the splitter it resumes.
        let last = raw
            .params
            .iter()
            .find(|(k, _)| k == RESUME_AGE)
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .and_then(|ms| now.checked_sub(Duration::from_millis(ms)))
            .unwrap_or(now);
        if let Some(skip) = &self.skipping {
            if *skip == id {
                if !more {
                    self.skipping = None;
                }
                return;
            }
            self.skipping = None;
        }
        match self.sequence.take() {
            Some(mut seq) if seq.id == id && seq.verb == raw.verb => {
                seq.body.extend_from_slice(&raw.body);
                seq.last = last;
                if seq.body.len() > MAX_BODY {
                    log::warn!("chunked TSP message c={id} dropped: over {MAX_BODY} bytes");
                    if more {
                        self.skipping = Some(id);
                    }
                } else if more {
                    self.sequence = Some(seq);
                } else {
                    out.push(Message {
                        verb: seq.verb,
                        params: seq.params,
                        body: seq.body,
                    });
                }
            }
            previous => {
                if previous.is_some() {
                    log::warn!("chunked TSP message dropped: interrupted");
                }
                let verb = raw.verb;
                let mut params = raw.params;
                params.retain(|(k, _)| !is_framing(k));
                if raw.body.len() > MAX_BODY {
                    log::warn!("chunked TSP message c={id} dropped: over {MAX_BODY} bytes");
                    if more {
                        self.skipping = Some(id);
                    }
                } else if more {
                    self.sequence = Some(Sequence {
                        id,
                        verb,
                        params,
                        body: raw.body,
                        last,
                    });
                } else {
                    out.push(Message {
                        verb,
                        params,
                        body: raw.body,
                    });
                }
            }
        }
    }

    fn interrupt(&mut self) {
        if let Some(seq) = self.sequence.take() {
            log::warn!("chunked TSP message c={} dropped: interrupted", seq.id);
        }
    }
}

/// Whether parameter `key` belongs to the chunk framing (and is not part of the message).
fn is_framing(key: &str) -> bool {
    matches!(key, "c" | "m" | RESUME_AGE | RESUME_SKIP)
}

struct Raw {
    verb: Verb,
    params: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Raw {
    fn into_message(mut self) -> Message {
        self.params.retain(|(k, _)| !is_framing(k));
        Message {
            verb: self.verb,
            params: self.params,
            body: self.body,
        }
    }
}

/// Position of the next ESC.
fn find_esc(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&b| b == ESC)
}

/// Whether `segment` has the form `key=value` (key `[A-Za-z0-9_-]+`, value printable ASCII
/// without spaces; `;` cannot occur, the caller cut at it).
fn param_of(segment: &[u8]) -> Option<(&str, &str)> {
    let eq = segment.iter().position(|&b| b == b'=')?;
    let (key, value) = (&segment[..eq], &segment[eq + 1..]);
    let key_ok = !key.is_empty()
        && key
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let value_ok = value.iter().all(|&b| (0x21..=0x7e).contains(&b));
    if !(key_ok && value_ok) {
        return None;
    }
    // Both slices are ASCII, checked above.
    Some((
        std::str::from_utf8(key).ok()?,
        std::str::from_utf8(value).ok()?,
    ))
}

/// Splits `<verb>[;key=value]*;<body>` (the payload after `tsp;`). A segment is a parameter
/// only when it has the `key=value` form *and* another `;` follows it; the first segment
/// that is not starts the body, which then stays whole.
fn parse_payload(payload: &[u8]) -> Option<Raw> {
    let (&verb, rest) = payload.split_first()?;
    let mut rest = rest.strip_prefix(b";")?;
    let mut params = Vec::new();
    while let Some(semi) = rest.iter().position(|&b| b == b';') {
        let Some((k, v)) = param_of(&rest[..semi]) else {
            break;
        };
        params.push((k.to_owned(), v.to_owned()));
        rest = &rest[semi + 1..];
    }
    Some(Raw {
        verb: Verb::from_byte(verb),
        params,
        body: rest.to_vec(),
    })
}

/// One APC string `ESC _ tsp;<verb>[;k=v]*;<body> ESC \`, never chunked (events and replies
/// to the child, and bodies within the limit).
pub fn encode_message(verb: Verb, params: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend_from_slice(b"\x1b_tsp;");
    out.push(verb.as_byte());
    for (k, v) in params {
        out.push(b';');
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
    }
    out.push(b';');
    out.extend_from_slice(body);
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Like [`encode_message`], split into `c=<chunk_id>` chunks of at most `limit` body bytes
/// when `body` is longer, cut as omp's encoder does: on a UTF-8 boundary where the rest does
/// not lead with a parameter-shaped segment. `params` ride on the first chunk.
pub fn encode_chunked(
    verb: Verb,
    params: &[(&str, &str)],
    body: &[u8],
    limit: usize,
    chunk_id: &str,
) -> Vec<u8> {
    let limit = limit.max(1);
    if body.len() <= limit {
        return encode_message(verb, params, body);
    }
    let mut out = Vec::with_capacity(body.len() + 64);
    let mut start = 0;
    let mut first = true;
    while start < body.len() {
        let end = chunk_end(body, start, limit);
        let last = end >= body.len();
        let mut p: Vec<(&str, &str)> = Vec::new();
        if first {
            p.extend_from_slice(params);
        }
        p.push(("c", chunk_id));
        if !last {
            p.push(("m", "1"));
        }
        out.extend_from_slice(&encode_message(verb, &p, &body[start..end]));
        start = end;
        first = false;
    }
    out
}

/// Whether `bytes` from `at` lead with a `key=value;` segment.
fn leads_with_param(bytes: &[u8], at: usize) -> bool {
    let mut i = at;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
    {
        i += 1;
    }
    if i == at || bytes.get(i) != Some(&b'=') {
        return false;
    }
    i += 1;
    while i < bytes.len() && (0x21..=0x7e).contains(&bytes[i]) && bytes[i] != b';' {
        i += 1;
    }
    bytes.get(i) == Some(&b';')
}

fn chunk_end(bytes: &[u8], start: usize, limit: usize) -> usize {
    let target = start + limit;
    if target >= bytes.len() {
        return bytes.len();
    }
    let safe = |at: usize| (bytes[at] & 0xc0) != 0x80 && !leads_with_param(bytes, at);
    if let Some(at) = (start + 1..=target).rev().find(|&at| safe(at)) {
        return at;
    }
    (target + 1..bytes.len())
        .find(|&at| safe(at))
        .unwrap_or(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn feed_all(s: &mut Splitter, bytes: &[u8]) -> Vec<Message> {
        s.feed(bytes, t0())
    }

    #[test]
    fn parses_verb_params_and_whole_json_body() {
        let wire = b"before\x1b_tsp;f;{\"sf\":\"a;b=c\",\"s\":1,\"ops\":[]}\x1b\\after";
        let mut s = Splitter::new();
        let got = feed_all(&mut s, wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].verb, Verb::Frame);
        assert!(got[0].params.is_empty());
        assert_eq!(got[0].body, br#"{"sf":"a;b=c","s":1,"ops":[]}"#);

        let blob = encode_message(Verb::Blob, &[("id", "ab"), ("mime", "image/png")], b"QUJD");
        let got = feed_all(&mut s, &blob);
        assert_eq!(got[0].verb, Verb::Blob);
        assert_eq!(got[0].param("mime"), Some("image/png"));
        assert_eq!(got[0].param("id"), Some("ab"));
        assert_eq!(got[0].body, b"QUJD");
    }

    #[test]
    fn a_key_value_segment_is_a_parameter_only_when_another_semicolon_follows() {
        let got = feed_all(&mut Splitter::new(), b"\x1b_tsp;b;mime=x/y;k=v\x1b\\");
        assert_eq!(got[0].param("mime"), Some("x/y"));
        assert_eq!(got[0].body, b"k=v");
        let got = feed_all(&mut Splitter::new(), b"\x1b_tsp;b;k=v\x1b\\");
        assert!(got[0].params.is_empty());
        assert_eq!(got[0].body, b"k=v");
        // The first non-parameter segment starts the body, even if parameter-shaped ones follow.
        let got = feed_all(&mut Splitter::new(), b"\x1b_tsp;f;{\"a\":1};x=1;y\x1b\\");
        assert!(got[0].params.is_empty());
        assert_eq!(got[0].body, b"{\"a\":1};x=1;y");
    }

    #[test]
    fn every_split_offset_yields_the_same_messages() {
        let mut wire = Vec::new();
        wire.extend_from_slice(b"text\x1b[31m");
        wire.extend_from_slice(&encode_message(Verb::Open, &[], br#"{"id":"s1"}"#));
        wire.extend_from_slice(b"\x1b_Gi=1,a=T;AAAA\x1b\\");
        wire.extend_from_slice(&encode_chunked(
            Verb::Frame,
            &[],
            "{\"x\":\"é€😀;k=v;z\"}".as_bytes(),
            5,
            "k1",
        ));
        wire.extend_from_slice(b"\x1b]0;title\x07tail");
        let expect = feed_all(&mut Splitter::new(), &wire);
        assert_eq!(expect.len(), 2);
        assert_eq!(expect[1].body, "{\"x\":\"é€😀;k=v;z\"}".as_bytes());
        for at in 0..=wire.len() {
            let mut s = Splitter::new();
            let mut got = s.feed(&wire[..at], t0());
            got.extend(s.feed(&wire[at..], t0()));
            assert_eq!(got, expect, "split at {at}");
        }
        // Byte by byte.
        let mut s = Splitter::new();
        let mut got = Vec::new();
        for b in &wire {
            got.extend(s.feed(std::slice::from_ref(b), t0()));
        }
        assert_eq!(got, expect);
    }

    #[test]
    fn chunks_split_inside_a_utf8_character_join_byte_for_byte() {
        let body = "{\"t\":\"é😀\"}".as_bytes();
        let mut wire = Vec::new();
        // First chunk ends inside 'é' (2 bytes).
        let cut = body.iter().position(|&b| b == 0xc3).map(|p| p + 1);
        let cut = cut.unwrap_or(0);
        wire.extend_from_slice(&encode_message(
            Verb::Frame,
            &[("c", "u"), ("m", "1")],
            &body[..cut],
        ));
        wire.extend_from_slice(&encode_message(Verb::Frame, &[("c", "u")], &body[cut..]));
        let got = feed_all(&mut Splitter::new(), &wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, body);
        assert!(got[0].params.is_empty());
    }

    #[test]
    fn first_chunk_params_survive_and_c_m_are_stripped() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_message(
            Verb::Blob,
            &[("mime", "image/png"), ("c", "z"), ("m", "1")],
            b"AAAA",
        ));
        wire.extend_from_slice(&encode_message(
            Verb::Blob,
            &[("mime", "text/plain"), ("c", "z")],
            b"BBBB",
        ));
        let got = feed_all(&mut Splitter::new(), &wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].params, [("mime".to_owned(), "image/png".to_owned())]);
        assert_eq!(got[0].body, b"AAAABBBB");
    }

    #[test]
    fn a_message_interrupting_a_sequence_drops_it_and_is_still_handled() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_message(
            Verb::Frame,
            &[("c", "a"), ("m", "1")],
            b"{\"half",
        ));
        wire.extend_from_slice(&encode_message(Verb::Open, &[], br#"{"id":"s"}"#));
        // The orphaned tail of the dropped sequence starts a new one-chunk message.
        wire.extend_from_slice(&encode_message(Verb::Frame, &[("c", "a")], b"tail\"}"));
        let got = feed_all(&mut Splitter::new(), &wire);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].verb, Verb::Open);
        assert_eq!(got[1].body, b"tail\"}");
        // A different chunk id also interrupts.
        let mut wire = Vec::new();
        wire.extend_from_slice(&encode_message(
            Verb::Frame,
            &[("c", "a"), ("m", "1")],
            b"{\"x",
        ));
        wire.extend_from_slice(&encode_message(Verb::Frame, &[("c", "b")], b"whole"));
        let got = feed_all(&mut Splitter::new(), &wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, b"whole");
    }

    #[test]
    fn an_idle_sequence_is_dropped_after_one_second_and_its_rest_is_skipped() {
        let mut s = Splitter::new();
        let start = t0();
        let first = encode_message(Verb::Frame, &[("c", "k"), ("m", "1")], b"{\"a\":");
        assert!(s.feed(&first, start).is_empty());
        // Within the idle limit the next chunk joins.
        let mid = encode_message(Verb::Frame, &[("c", "k"), ("m", "1")], b"1,");
        assert!(s.feed(&mid, start + Duration::from_millis(900)).is_empty());
        // More than a second after the last chunk: the sequence is gone, the remainder skipped.
        let rest = encode_message(Verb::Frame, &[("c", "k")], b"\"b\":2}");
        let late = start + Duration::from_millis(900) + CHUNK_IDLE + Duration::from_millis(1);
        assert!(s.feed(&rest, late).is_empty());
        // The skip ended with the last chunk: the id is usable again.
        let again = encode_message(Verb::Frame, &[("c", "k")], b"{}");
        assert_eq!(s.feed(&again, late).len(), 1);
    }

    #[test]
    fn is_idle_follows_apc_and_chunk_sequence_state_at_every_byte() {
        let start = t0();
        let mut s = Splitter::new();
        assert!(s.is_idle());

        // Inside an APC string (any APC, kitty graphics too) it is busy at every byte but
        // the last, and idle again after the terminator.
        for wire in [
            encode_message(Verb::Frame, &[], br#"{"sf":"a","s":1,"ops":[]}"#),
            b"\x1b_Gf=24,s=1,v=1;AAAA\x1b\\".to_vec(),
        ] {
            for end in 1..=wire.len() {
                let mut s = Splitter::new();
                s.feed(&wire[..end], start);
                let inside = (2..wire.len()).contains(&end);
                assert_eq!(s.is_idle(), !inside, "{end} of {} bytes", wire.len());
            }
        }

        // A chunk sequence is pending from its first chunk to its last, however the chunks'
        // bytes arrive, and plain output in between does not end it.
        let first = encode_message(Verb::Frame, &[("c", "q"), ("m", "1")], br#"{"sf":"a","#);
        let last = encode_message(Verb::Frame, &[("c", "q")], br#""s":1,"ops":[]}"#);
        assert_eq!(s.feed(&first, start).len(), 0);
        assert!(!s.is_idle());
        s.feed(b"plain output\r\n\x1b[31mred\x1b[0m", start);
        assert!(!s.is_idle(), "non-TSP output leaves the sequence pending");
        assert_eq!(s.feed(&last, start).len(), 1);
        assert!(s.is_idle());

        // Interrupted by another TSP message: the sequence is dropped, the stream idle.
        s.feed(&first, start);
        assert!(!s.is_idle());
        let other = encode_message(Verb::Open, &[], b"{}");
        assert_eq!(s.feed(&other, start).len(), 1);
        assert!(s.is_idle());

        // Expired: evaluated when bytes arrive, so it stays pending until then.
        s.feed(&first, start);
        let late = start + CHUNK_IDLE + Duration::from_millis(1);
        assert!(!s.is_idle(), "no clock of its own");
        s.feed(b"x", late);
        assert!(s.is_idle());

        // A lone ESC between sequences is not an APC in progress.
        s.feed(b"\x1b", late);
        assert!(s.is_idle());
    }

    #[test]
    fn resume_bytes_bring_a_fresh_splitter_to_the_same_state_at_every_offset() {
        let start = t0();
        let body = format!(
            r#"{{"sf":"a","s":1,"ops":[["add","n","a",null,{{"id":"n","k":"text","p":{{"text":"{}"}}}}]]}}"#,
            "é€;k=v;😀".repeat(40)
        );
        let mut wire = encode_message(Verb::Open, &[], br#"{"id":"a"}"#);
        wire.extend_from_slice(b"\x1b_Gf=24;AAAA\x1b\\plain\x1b[1m");
        wire.extend(encode_chunked(
            Verb::Frame,
            &[("k", "v")],
            body.as_bytes(),
            37,
            "seq1",
        ));
        wire.extend(encode_message(Verb::Blob, &[("mime", "x/y")], b"QUJD"));
        wire.extend(encode_chunked(
            Verb::Frame,
            &[],
            body.as_bytes(),
            61,
            "seq2",
        ));

        let mut checked_pending = 0;
        for k in 0..=wire.len() {
            let mut a = Splitter::new();
            a.feed(&wire[..k], start);
            let mut b = Splitter::new();
            let mut resume = Vec::new();
            a.resume_bytes(start, &mut resume);
            assert!(
                b.feed(&resume, start).is_empty(),
                "offset {k}: nothing completes in the resume bytes"
            );
            if !resume.is_empty() {
                checked_pending += 1;
            }
            if a.is_idle() {
                assert!(
                    resume.is_empty() || resume == [ESC],
                    "offset {k}: an idle splitter resumes at most a lone ESC"
                );
            }
            let rest_a = a.feed(&wire[k..], start);
            let rest_b = b.feed(&wire[k..], start);
            assert_eq!(rest_a, rest_b, "offset {k}");
        }
        assert!(checked_pending > 100, "{checked_pending}");

        // A body past one chunk of the resume encoding: re-encoded in pieces, joined again.
        let big = "x".repeat(RESUME_CHUNK * 2 + 10);
        let mut a = Splitter::new();
        a.feed(
            &encode_message(Verb::Blob, &[("c", "big"), ("m", "1")], big.as_bytes()),
            start,
        );
        let mut resume = Vec::new();
        a.resume_bytes(start, &mut resume);
        let mut b = Splitter::new();
        assert!(b.feed(&resume, start).is_empty());
        let end = encode_message(Verb::Blob, &[("c", "big")], b"END");
        let got = b.feed(&end, start);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body.len(), big.len() + 3);
        assert_eq!(a.feed(&end, start), got);
    }

    #[test]
    fn resume_bytes_carry_a_skipped_sequence_at_every_offset() {
        let start = t0();
        let late = start + CHUNK_IDLE + Duration::from_millis(1);
        let chunk = |more: bool, body: &[u8]| {
            let mut p = vec![("c", "k")];
            if more {
                p.push(("m", "1"));
            }
            encode_message(Verb::Frame, &p, body)
        };
        // The rest of a sequence that timed out, then a message and a whole sequence.
        let mut wire = chunk(true, b"\"b\":");
        wire.extend(chunk(true, b"2,"));
        wire.extend(chunk(false, b"\"c\":3}"));
        wire.extend(encode_message(Verb::Open, &[], br#"{"id":"a"}"#));
        wire.extend(encode_chunked(
            Verb::Frame,
            &[],
            b"{\"whole\":true}",
            5,
            "k2",
        ));

        let mut with_marker = 0;
        for k in 0..=wire.len() {
            let mut a = Splitter::new();
            assert!(a.feed(&chunk(true, b"{\"a\":1,"), start).is_empty());
            let mut got_a = a.feed(&wire[..k], late);
            let mut resume = Vec::new();
            a.resume_bytes(late, &mut resume);
            let mut b = Splitter::new();
            assert!(b.feed(&resume, late).is_empty(), "offset {k}");
            if resume
                .windows(RESUME_SKIP.len())
                .any(|w| w == RESUME_SKIP.as_bytes())
            {
                with_marker += 1;
            }
            let got_b = b.feed(&wire[k..], late);
            got_a.extend(a.feed(&wire[k..], late));
            assert_eq!(got_b, got_a[got_a.len() - got_b.len()..], "offset {k}");
            assert_eq!(
                got_a.len(),
                2,
                "offset {k}: the skipped tail is not a message"
            );
            assert_eq!(got_a[0].verb, Verb::Open);
            assert_eq!(got_a[1].body, b"{\"whole\":true}");
        }
        assert!(with_marker > 10, "{with_marker}");
    }

    #[test]
    fn resume_bytes_carry_an_oversized_sequence_being_skipped() {
        let start = t0();
        let piece = vec![b'x'; 200_000];
        let n = MAX_BODY / piece.len() + 2;
        let chunk = |more: bool| {
            let mut p = vec![("c", "big")];
            if more {
                p.push(("m", "1"));
            }
            encode_message(Verb::Blob, &p, &piece)
        };
        let mut a = Splitter::new();
        for _ in 0..n - 1 {
            assert!(a.feed(&chunk(true), start).is_empty());
        }
        assert!(a.is_idle(), "the oversized sequence was dropped");
        let mut resume = Vec::new();
        a.resume_bytes(start, &mut resume);
        assert!(!resume.is_empty(), "its remaining chunks are still skipped");
        let mut b = Splitter::new();
        assert!(b.feed(&resume, start).is_empty());
        let rest = [chunk(true), chunk(false)].concat();
        let open = encode_message(Verb::Open, &[], b"{}");
        assert!(b.feed(&rest, start).is_empty(), "the tail is not a message");
        assert_eq!(b.feed(&open, start).len(), 1);
        assert!(a.feed(&rest, start).is_empty());
        assert_eq!(a.feed(&open, start).len(), 1);
        // Once the skip ended, the id is free again on the resumed splitter too.
        let again = encode_message(Verb::Blob, &[("c", "big")], b"QUJD");
        assert_eq!(b.feed(&again, start).len(), 1);
    }

    #[test]
    fn a_resumed_sequence_expires_when_the_original_does() {
        let start = t0();
        let first = encode_message(Verb::Frame, &[("c", "k"), ("m", "1")], b"{\"a\":");
        let rest = encode_message(Verb::Frame, &[("c", "k")], b"1}");
        let mut a = Splitter::new();
        a.feed(&first, start);

        // Resumed 900 ms into its life (any later wall time on the resumed side).
        let at = start + Duration::from_millis(900);
        let mut resume = Vec::new();
        a.resume_bytes(at, &mut resume);
        let base = start + Duration::from_secs(60);

        // 100 ms later it is not yet old enough to drop, at both ends.
        let mut b = Splitter::new();
        assert!(b.feed(&resume, base).is_empty());
        let got = b.feed(&rest, base + Duration::from_millis(99));
        assert_eq!(got.len(), 1);
        assert_eq!(got, a.feed(&rest, at + Duration::from_millis(99)));

        // 200 ms later both dropped it (the resumed one does not start its second over).
        let mut a = Splitter::new();
        a.feed(&first, start);
        let mut b = Splitter::new();
        assert!(b.feed(&resume, base).is_empty());
        assert!(b.feed(&rest, base + Duration::from_millis(200)).is_empty());
        assert!(a.feed(&rest, at + Duration::from_millis(200)).is_empty());
        assert!(a.is_idle() && b.is_idle());
    }

    #[test]
    fn caps_oversized_apc_and_reassembled_bodies() {
        let mut s = Splitter::new();
        let mut big = b"\x1b_tsp;f;".to_vec();
        big.extend(std::iter::repeat_n(b'a', MAX_APC));
        big.extend_from_slice(b"\x1b\\");
        let ok = encode_message(Verb::Open, &[], b"{}");
        big.extend_from_slice(&ok);
        let got = s.feed(&big, t0());
        assert_eq!(
            got.len(),
            1,
            "the oversized string is dropped, the next kept"
        );
        assert_eq!(got[0].verb, Verb::Open);

        // Exactly at the cap is accepted ("tsp;f;" + body = MAX_APC).
        let mut edge = b"\x1b_tsp;f;".to_vec();
        edge.extend(std::iter::repeat_n(b'a', MAX_APC - 6));
        edge.extend_from_slice(b"\x1b\\");
        assert_eq!(s.feed(&edge, t0()).len(), 1);

        // A reassembled body over MAX_BODY is dropped, later chunks skipped, next message fine.
        let mut s = Splitter::new();
        let piece = vec![b'x'; 200_000];
        let mut wire = Vec::new();
        let n = MAX_BODY / piece.len() + 2;
        for i in 0..n {
            let mut p = vec![("c", "big")];
            if i + 1 < n {
                p.push(("m", "1"));
            }
            wire.extend_from_slice(&encode_message(Verb::Blob, &p, &piece));
        }
        wire.extend_from_slice(&ok);
        let got = s.feed(&wire, t0());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].verb, Verb::Open);
    }

    #[test]
    fn non_tsp_apc_and_other_sequences_are_ignored_without_buffering() {
        let mut s = Splitter::new();
        let mut wire = Vec::new();
        wire.extend_from_slice(b"\x1b_Gf=100,a=T,m=1;");
        wire.extend(std::iter::repeat_n(b'A', MAX_APC * 3));
        wire.extend_from_slice(b"\x1b\\\x1b_ts\x1b\\\x1b_tspx;f;{}\x1b\\");
        wire.extend_from_slice(b"\x1b[?2004h\x1b]7;file:///x\x1b\\\x1bP1$r\x1b\\");
        assert!(s.feed(&wire, t0()).is_empty());
        assert!(s.buf.capacity() < 64, "kitty payload must not be buffered");
        let got = s.feed(&encode_message(Verb::Query, &[], b"{}"), t0());
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn a_stray_esc_inside_a_string_aborts_it_and_a_new_apc_can_start_there() {
        let mut s = Splitter::new();
        let mut wire = b"\x1b_tsp;f;{\"cut".to_vec();
        wire.extend_from_slice(&encode_message(Verb::Open, &[], br#"{"id":"s"}"#));
        let got = s.feed(&wire, t0());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].verb, Verb::Open);
        // ESC ESC \ : the first ESC aborts the string; the second starts a fresh escape.
        let got = s.feed(b"\x1b_tsp;f;{}\x1b\x1b\\", t0());
        assert!(got.is_empty());
        assert_eq!(
            s.feed(&encode_message(Verb::Open, &[], b"{}"), t0()).len(),
            1
        );
    }

    #[test]
    fn a_tsp_string_without_a_verb_separator_is_ignored() {
        let mut s = Splitter::new();
        assert!(s.feed(b"\x1b_tsp;\x1b\\\x1b_tsp;fx\x1b\\", t0()).is_empty());
        assert_eq!(
            s.feed(&encode_message(Verb::Open, &[], b"{}"), t0()).len(),
            1
        );
    }

    #[test]
    fn encode_chunked_cuts_on_character_and_parameter_boundaries() {
        let body = b"aaaa x=1;bbbb".to_vec();
        for limit in 1..body.len() + 2 {
            let wire = encode_chunked(Verb::Frame, &[], &body, limit, "c1");
            let got = feed_all(&mut Splitter::new(), &wire);
            assert_eq!(got.len(), 1, "limit {limit}");
            assert_eq!(got[0].body, body, "limit {limit}");
            assert!(got[0].params.is_empty());
        }
        let text = "é€😀é€😀".as_bytes();
        for limit in 1..text.len() + 2 {
            let wire = encode_chunked(Verb::Frame, &[], text, limit, "c2");
            let got = feed_all(&mut Splitter::new(), &wire);
            assert_eq!(got[0].body, text, "limit {limit}");
        }
    }
}
