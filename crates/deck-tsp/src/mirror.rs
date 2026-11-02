//! A passive copy of a program's surfaces: [`Splitter`] + [`Document`], nothing else.
//!
//! The host terminal is the only answerer of TSP (hello reply, acks, errors); every other
//! reader of the same PTY output (the gateway's owner terminal, the GUI's frontend terminal)
//! keeps a [`Mirror`], which never produces bytes. A mirror started from a
//! [`Document::replay`] followed by the live stream ends up with the document the host holds.
//!
//! A mirror can only start from the stream at a point where [`Mirror::is_idle`]: a chunked
//! message joined half way would lose its first chunks.

use std::time::Instant;

use crate::document::Document;
use crate::stream::Splitter;

/// See the module docs.
#[derive(Default)]
pub struct Mirror {
    splitter: Splitter,
    document: Document,
}

impl Mirror {
    pub fn new() -> Mirror {
        Mirror::default()
    }

    /// Scans `bytes` of program output (a replay included) and applies the messages they
    /// complete. Returns whether the document changed ([`Document::revision`] moved).
    /// Allocation-free when the bytes hold no TSP message.
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> bool {
        let before = self.document.revision();
        for msg in self.splitter.feed(bytes, now) {
            // Acks and errors are the host terminal's business; a mirror drops them.
            self.document.apply(&msg, now);
        }
        self.document.revision() != before
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    /// No APC string is half read and no chunked message is waiting for its next chunk.
    pub fn is_idle(&self) -> bool {
        self.splitter.is_idle()
    }

    /// An open inline surface with role `omp.session` exists: omp speaks TSP here and a chat
    /// view can draw it.
    pub fn live(&self) -> bool {
        self.document
            .inline()
            .is_some_and(|s| s.role() == Some("omp.session"))
    }

    /// Forgets everything (a new snapshot is about to be fed).
    pub fn reset(&mut self) {
        *self = Mirror::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Verb, encode_message};

    fn o(id: &str, role: Option<&str>) -> Vec<u8> {
        let mut body = serde_json::json!({"id": id, "mode": "inline"});
        if let Some(r) = role {
            body["role"] = r.into();
        }
        encode_message(Verb::Open, &[], body.to_string().as_bytes())
    }

    #[test]
    fn feed_reports_document_changes_only_and_live_needs_the_session_role() {
        let t = Instant::now();
        let mut m = Mirror::new();
        assert!(!m.live() && m.is_idle());
        assert!(
            !m.feed(b"plain \x1b[1mtext\x1b[0m\r\n", t),
            "no TSP, no change"
        );
        assert!(m.feed(&o("s1", None), t));
        assert!(!m.live(), "an inline surface without the omp.session role");
        assert!(
            m.feed(&o("s2", Some("omp.session")), t),
            "opening closes s1"
        );
        assert!(m.live());
        // A reply or event travelling the other way, and a malformed frame, change nothing.
        let ev = encode_message(Verb::Event, &[], br#"{"ev":"ack","sf":"s2","s":1}"#);
        assert!(!m.feed(&ev, t));
        let bad = encode_message(Verb::Frame, &[], b"{nope");
        assert!(!m.feed(&bad, t));
        // Half an APC: no change yet, not idle, then complete.
        let f = encode_message(Verb::Frame, &[], br#"{"sf":"s2","s":1,"ops":[]}"#);
        assert!(!m.feed(&f[..10], t));
        assert!(!m.is_idle());
        assert!(m.feed(&f[10..], t));
        assert!(m.is_idle());
        m.reset();
        assert!(!m.live() && m.document().surfaces().is_empty());
    }
}
