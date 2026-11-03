//! The terminal's passive copy of its program's TSP surfaces (the chat view's data).
//!
//! The output thread feeds one [`Mirror`] with every chunk the `Term` sees ([`TspShared::feed`],
//! called by `OutputScan`); the UI reads it through a cloneable [`TspHandle`] without the
//! engine lock. Nothing here answers the program: the session host is the only TSP terminal
//! that replies, acks or says hello.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use deck_tsp::document::Document;
use deck_tsp::mirror::Mirror;

const ESC: u8 = 0x1b;

/// The mirror and what the UI reads about it without taking its lock.
pub(crate) struct TspShared {
    mirror: Mutex<Mirror>,
    /// Monotonic over the terminal's life, also across [`TspShared::reset`] (a new snapshot
    /// replaces the document, but a watcher comparing revisions must still see a change).
    revision: AtomicU64,
    /// Counts [`TspShared::reset`]s: a copy of the document made before one is of another
    /// document, even when both happen to carry the same document revision.
    epoch: AtomicU64,
    live: AtomicBool,
    /// Some node refers to a blob the document lacks (see `Document::missing_blobs`); read
    /// without the lock so the output thread can skip asking on every chunk.
    wants_blobs: AtomicBool,
}

impl TspShared {
    pub(crate) fn new() -> TspShared {
        TspShared {
            mirror: Mutex::new(Mirror::new()),
            revision: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            live: AtomicBool::new(false),
            wants_blobs: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Mirror> {
        self.mirror.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Scans a chunk of child output. Only the output thread (under the engine lock) calls
    /// this. Returns whether the mirror is idle afterwards.
    pub(crate) fn feed(&self, bytes: &[u8], now: Instant) -> bool {
        let mut mirror = self.lock();
        let before = mirror.document().revision();
        if mirror.feed(bytes, now) {
            // The published counter moves by the document's own step; `reset` keeps it ahead.
            let step = mirror.document().revision().wrapping_sub(before);
            self.revision.fetch_add(step.max(1), Ordering::Release);
            self.live.store(mirror.live(), Ordering::Release);
            self.wants_blobs
                .store(mirror.document().wants_blobs(), Ordering::Release);
        }
        mirror.is_idle()
    }

    /// Forgets the document (a new snapshot is about to be fed). Counts as a change.
    pub(crate) fn reset(&self) {
        // Both counters move under the lock, so a reader holding it sees them agree with the
        // document.
        let mut mirror = self.lock();
        mirror.reset();
        self.epoch.fetch_add(1, Ordering::Release);
        self.revision.fetch_add(1, Ordering::Release);
        drop(mirror);
        self.live.store(false, Ordering::Release);
        self.wants_blobs.store(false, Ordering::Release);
    }

    /// Appends the replay of the document (see `Document::replay`).
    pub(crate) fn replay(&self, now: Instant, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.lock().document().replay(now));
    }
    /// Blob ids some node refers to that the document does not hold (cheap when none).
    pub(crate) fn missing_blobs(&self) -> Vec<String> {
        if !self.wants_blobs.load(Ordering::Acquire) {
            return Vec::new();
        }
        self.lock().document().missing_blobs()
    }
    /// Stores a blob the session host supplied, its bytes already hashed to `id` by the caller
    /// (outside any lock). Counts as a change.
    pub(crate) fn put_verified_blob(
        &self,
        id: &str,
        mime: Option<String>,
        bytes: std::sync::Arc<[u8]>,
    ) -> bool {
        let mut mirror = self.lock();
        let stored = mirror.put_verified_blob(id, mime, bytes);
        if stored {
            self.revision.fetch_add(1, Ordering::Release);
            self.wants_blobs
                .store(mirror.document().wants_blobs(), Ordering::Release);
        }
        stored
    }

    /// Stops wanting `id`: the host holds no such blob.
    pub(crate) fn forget_blob(&self, id: &str) {
        let mut mirror = self.lock();
        mirror.forget_blob(id);
        self.wants_blobs
            .store(mirror.document().wants_blobs(), Ordering::Release);
    }

    /// Bumped by every change of the mirror.
    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
}

/// Cheap clone of the access to a terminal's TSP mirror ([`crate::Terminal::tsp`]).
#[derive(Clone)]
pub struct TspHandle(pub(crate) Arc<TspShared>);

impl TspHandle {
    /// Changes so far: moves whenever a message changed the document or a new snapshot
    /// replaced it. Compare with the value seen last to know whether to redraw.
    pub fn revision(&self) -> u64 {
        self.0.revision.load(Ordering::Acquire)
    }

    /// Resets so far: moves when a new snapshot replaced the document, after which copies of the
    /// old one are stale whatever the document revisions say.
    pub fn epoch(&self) -> u64 {
        self.0.epoch.load(Ordering::Acquire)
    }

    /// An open inline surface with role `omp.session` exists: the program speaks TSP here and
    /// a chat view can draw it.
    pub fn live(&self) -> bool {
        self.0.live.load(Ordering::Acquire)
    }

    /// [`TspHandle::with`], also handing over the [`revision`](Self::revision) and
    /// [`epoch`](Self::epoch) the document is at (read under its lock, so they match it).
    pub fn with_stamp<R>(&self, f: impl FnOnce(&Document, u64, u64) -> R) -> R {
        let mirror = self.0.lock();
        let rev = self.0.revision.load(Ordering::Acquire);
        let epoch = self.0.epoch.load(Ordering::Acquire);
        f(mirror.document(), rev, epoch)
    }

    /// Runs `f` on the document. The output thread waits meanwhile (it holds the same lock
    /// only while applying one chunk), so keep `f` short and do not call back into the terminal.
    pub fn with<R>(&self, f: impl FnOnce(&Document) -> R) -> R {
        f(self.0.lock().document())
    }
}

/// Why [`crate::Terminal::send_tsp`] did not hand an event to the child's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TspRefused {
    /// The program has not opened an `omp.session` surface (not a TSP session, or not yet).
    #[error("this session does not speak TSP")]
    NotLive,
    /// An injected sequence owns the input (see `Terminal::hold_input_until`).
    #[error("input is held")]
    Held,
    /// An observer connection: it never writes.
    #[error("this connection is read-only")]
    ReadOnly,
    /// The child exited.
    #[error("the session has exited")]
    Exited,
    /// The connection to the child ended or is not current (displaced, shut down, a
    /// frontend whose link is down).
    #[error("the connection is not attached")]
    Detached,
}

/// Whether a chunk can change the mirror: bytes inside a TSP string or an ESC that may start one.
pub(crate) fn may_touch_mirror(idle: bool, bytes: &[u8]) -> bool {
    !idle || bytes.contains(&ESC)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    use deck_tsp::stream::{Verb, encode_chunked, encode_message};
    use deck_tsp::wire::Event;

    use super::*;
    use crate::Terminal;
    use crate::engine::OutputEvent;
    use crate::remote::RemoteEvent;
    use crate::remote::tests::{Log, Sent, connect};

    const TIMEOUT: Duration = Duration::from_secs(10);

    pub(crate) fn hello() -> Vec<u8> {
        encode_message(
            Verb::Query,
            &[],
            br#"{"q":"hello","v":[1],"app":"fake","ver":"0","features":["send"]}"#,
        )
    }

    pub(crate) fn open() -> Vec<u8> {
        encode_message(
            Verb::Open,
            &[],
            br#"{"id":"s1","mode":"inline","role":"omp.session"}"#,
        )
    }

    pub(crate) fn frame1() -> Vec<u8> {
        encode_message(
            Verb::Frame,
            &[],
            br#"{"sf":"s1","s":1,"ops":[["add","main","s1",null,{"id":"main","k":"col","c":[{"id":"t","k":"md","p":{"text":"hello"}}]}]]}"#,
        )
    }

    /// An append to `t`, in chunks of at most 24 body bytes: several APC strings.
    pub(crate) fn frame2_chunks() -> Vec<Vec<u8>> {
        let wire = encode_chunked(
            Verb::Frame,
            &[],
            br#"{"sf":"s1","s":2,"ops":[["text","t","append"," and more of it"]]}"#,
            24,
            "c1",
        );
        // Every chunk ends with ST; split after each.
        let mut chunks = Vec::new();
        let mut start = 0;
        for i in 0..wire.len() - 1 {
            if wire[i] == 0x1b && wire[i + 1] == b'\\' {
                chunks.push(wire[start..i + 2].to_vec());
                start = i + 2;
            }
        }
        assert!(chunks.len() >= 3, "{} chunks", chunks.len());
        chunks
    }

    pub(crate) fn send(host: &Sender<RemoteEvent>, bytes: &[u8]) {
        host.send(RemoteEvent::Output(bytes.to_vec()))
            .expect("feeder alive");
    }

    fn until(t: &mut Terminal, what: &str, mut done: impl FnMut(&Terminal) -> bool) {
        let deadline = std::time::Instant::now() + TIMEOUT;
        while !done(t) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            t.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(crate) fn text(t: &Terminal) -> Option<String> {
        t.tsp().with(|d| {
            d.surface("s1")
                .and_then(|s| s.node("t"))
                .and_then(|n| n.text().map(str::to_owned))
        })
    }

    pub(crate) fn doc_json(t: &Terminal) -> serde_json::Value {
        t.tsp()
            .with(|d| serde_json::to_value(d).expect("document JSON"))
    }

    pub(crate) fn host_inputs(log: &Log) -> Vec<u8> {
        log.lock()
            .expect("log")
            .iter()
            .filter_map(|s| match s {
                Sent::Input(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    pub(crate) fn event() -> Event {
        Event::Send {
            sf: "s1".into(),
            id: "e".into(),
            text: "hi \u{e9}".into(),
        }
    }

    #[test]
    fn the_mirror_follows_output_and_stays_passive() {
        let (mut t, host, log) = connect();
        let handle = t.tsp();
        assert!(!handle.live());
        let r0 = handle.revision();

        // Plain output never touches the mirror.
        send(&host, b"plain \x1b[1mtext\x1b[0m\r\n");
        until(&mut t, "plain text", |t| {
            t.screen_text().lines.join("\n").contains("plain")
        });
        assert_eq!((handle.revision(), handle.live()), (r0, false));

        // A TSP stream split at awkward places, a lone ESC across reads included.
        let mut wire = hello();
        wire.extend(open());
        wire.extend(frame1());
        let (a, b) = wire.split_at(wire.len() - 7);
        send(&host, a);
        send(&host, b);
        send(&host, b"\x1b");
        send(&host, b"[0m");
        until(&mut t, "the first frame", |t| {
            text(t).as_deref() == Some("hello")
        });
        assert!(handle.live());
        assert!(handle.revision() > r0);
        assert!(t.tsp().live(), "clones share the state");

        // Not one byte towards the program: no hello reply, no acks, no errors.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(host_inputs(&log), b"", "a mirror never answers");
    }

    #[test]
    fn a_join_inside_a_chunked_message_waits_for_its_end_and_replays_the_document() {
        let (mut owner, host, log) = connect();
        let mut head = hello();
        head.extend(open());
        head.extend(frame1());
        let chunks = frame2_chunks();
        send(&host, &head);
        send(&host, &chunks[0]);
        send(&host, &chunks[1]);
        until(&mut owner, "the first frame", |t| {
            text(t).as_deref() == Some("hello")
        });

        let sub = owner.subscribe_output(std::sync::Arc::new(|| {}));
        // The sequence is half way: no replay may be taken (it would lose the frame).
        std::thread::sleep(Duration::from_millis(250));
        owner.poll();
        assert!(sub.try_recv().is_none(), "the join waits");
        for chunk in &chunks[2..chunks.len() - 1] {
            send(&host, chunk);
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            sub.try_recv().is_none(),
            "still waiting before the last chunk"
        );
        send(&host, &chunks[chunks.len() - 1]);
        until(&mut owner, "the whole frame", |t| {
            text(t).as_deref() == Some("hello and more of it")
        });

        let mut snapshot = None;
        let deadline = std::time::Instant::now() + TIMEOUT;
        while snapshot.is_none() {
            assert!(std::time::Instant::now() < deadline, "no snapshot");
            if let Some(OutputEvent::Snapshot(bytes)) = sub.recv_timeout(Duration::from_millis(50))
            {
                snapshot = Some(bytes);
            }
        }
        let mut mirror = deck_tsp::mirror::Mirror::new();
        assert!(mirror.feed(&snapshot.expect("snapshot"), std::time::Instant::now()));
        assert!(mirror.is_idle() && mirror.live());
        assert_eq!(
            serde_json::to_value(mirror.document()).expect("document JSON"),
            doc_json(&owner),
            "the joiner holds the owner's document"
        );
        assert_eq!(host_inputs(&log), b"");
    }

    #[test]
    fn a_host_snapshot_rebuilds_the_mirror_and_counts_as_a_change() {
        let (mut t, host, _log) = connect();
        let mut wire = hello();
        wire.extend(open());
        wire.extend(frame1());
        let mut source = deck_tsp::mirror::Mirror::new();
        source.feed(&wire, std::time::Instant::now());
        let mut snapshot = b"grid".to_vec();
        snapshot.extend(source.document().replay(std::time::Instant::now()));

        host.send(RemoteEvent::Snapshot(snapshot)).expect("send");
        until(&mut t, "the replayed document", |t| {
            text(t).as_deref() == Some("hello")
        });
        assert!(t.tsp().live());
        assert_eq!(
            t.tsp().with(|d| d.program().and_then(|p| p.app.clone())),
            Some("fake".to_owned())
        );
        let before = t.tsp().revision();
        let epoch = t.tsp().epoch();

        // The next snapshot has none of it: a different document, a changed revision.
        host.send(RemoteEvent::Snapshot(b"plain".to_vec()))
            .expect("send");
        until(&mut t, "the cleared document", |t| !t.tsp().live());
        assert!(t.tsp().revision() > before);
        assert!(
            t.tsp().epoch() > epoch,
            "a reset is told apart from streaming"
        );
        assert_eq!(
            t.tsp().with_stamp(|_, r, e| (r, e)),
            (t.tsp().revision(), t.tsp().epoch())
        );
        assert_eq!(text(&t), None);
        assert!(t.tsp().with(|d| d.surfaces().is_empty()));
    }

    #[test]
    fn send_tsp_writes_the_exact_event_bytes_or_says_why_not() {
        let (mut t, host, log) = connect();
        assert_eq!(t.send_tsp(&event()), Err(TspRefused::NotLive));
        assert_eq!(host_inputs(&log), b"");

        let mut wire = hello();
        wire.extend(open());
        wire.extend(frame1());
        send(&host, &wire);
        until(&mut t, "live", |t| t.tsp().live());
        t.send_tsp(&event()).expect("accepted");
        let expected =
            deck_tsp::terminal::encode_event(&event(), deck_tsp::terminal::InputEncoding::native());
        assert!(expected.starts_with(b"\x1b_tsp;e;{\"ev\":\"send\"") || cfg!(windows));
        until(&mut t, "the bytes at the host", |_| {
            host_inputs(&log) == expected
        });

        t.hold_input_until(std::time::Instant::now() + Duration::from_secs(3600));
        assert_eq!(t.send_tsp(&event()), Err(TspRefused::Held));
        t.hold_input_until(std::time::Instant::now());
        t.send_tsp(&event()).expect("hold over");

        host.send(RemoteEvent::Exited(Some(0))).expect("send");
        until(&mut t, "the exit", |t| t.is_exited());
        assert_eq!(t.send_tsp(&event()), Err(TspRefused::Exited));
        assert_eq!(
            host_inputs(&log).len(),
            expected.len() * 2,
            "refusals wrote nothing"
        );
    }

    #[test]
    fn a_displaced_connection_is_detached() {
        let (mut t, host, log) = connect();
        send(&host, &[hello(), open(), frame1()].concat());
        until(&mut t, "live", |t| t.tsp().live());
        host.send(RemoteEvent::Displaced).expect("send");
        until(&mut t, "displaced", |t| t.is_displaced());
        assert_eq!(t.send_tsp(&event()), Err(TspRefused::Detached));
        assert_eq!(host_inputs(&log), b"");
    }
    const PIXEL: &[u8] = b"pixel-a";
    const PIXEL_ID: &str = "6fe2d5846e515bc9f48577f0d0a5c72e8935655af5e138b26face383b3d2174b";

    /// A frame adding an image node that refers to `PIXEL_ID`.
    fn image_frame() -> Vec<u8> {
        encode_message(
            Verb::Frame,
            &[],
            format!(
                r#"{{"sf":"s1","s":2,"ops":[["add","main","s1",null,{{"id":"img","k":"image","p":{{"blob":"{PIXEL_ID}"}}}}]]}}"#
            )
            .as_bytes(),
        )
    }

    fn blob_requests(log: &Log) -> Vec<String> {
        log.lock()
            .expect("log")
            .iter()
            .filter_map(|s| match s {
                Sent::Blob(id) => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    fn has_pixel(t: &Terminal) -> bool {
        t.tsp().with(|d| d.blob(PIXEL_ID).is_some())
    }

    /// The blob went to the host before this client attached, so the snapshot replay lacks it
    /// and only the later frame refers to it: the terminal must ask the host and show it.
    #[test]
    fn a_blob_sent_before_attaching_is_fetched_from_the_host_when_a_node_refers_to_it() {
        let (mut t, host, log) = connect();
        let mut wire = hello();
        wire.extend(open());
        wire.extend(frame1());
        host.send(RemoteEvent::Snapshot(wire)).expect("send");
        until(&mut t, "live", |t| t.tsp().live());
        assert_eq!(blob_requests(&log), Vec::<String>::new(), "nothing to ask");

        send(&host, &image_frame());
        until(&mut t, "the request", |_| blob_requests(&log) == [PIXEL_ID]);
        assert!(!has_pixel(&t));
        let before = t.tsp().revision();

        host.send(RemoteEvent::Blob {
            id: PIXEL_ID.into(),
            mime: Some("image/png".into()),
            bytes: Some(PIXEL.to_vec()),
        })
        .expect("send");
        until(&mut t, "the blob in the mirror", has_pixel);
        assert!(t.tsp().revision() > before, "a repaint is due");
        assert_eq!(
            t.tsp()
                .with(|d| d.blob(PIXEL_ID).and_then(|b| b.mime.clone())),
            Some("image/png".to_owned())
        );

        // Once held, a later change asks for nothing more.
        send(&host, b"more");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(blob_requests(&log), [PIXEL_ID]);
    }

    #[test]
    fn an_absent_or_forged_blob_is_asked_once_and_never_stored() {
        let (mut t, host, log) = connect();
        let mut wire = hello();
        wire.extend(open());
        wire.extend(frame1());
        wire.extend(image_frame());
        host.send(RemoteEvent::Snapshot(wire)).expect("send");
        until(&mut t, "the request", |_| blob_requests(&log) == [PIXEL_ID]);

        host.send(RemoteEvent::Blob {
            id: PIXEL_ID.into(),
            mime: None,
            bytes: Some(b"forged".to_vec()),
        })
        .expect("send");
        // A second answer for the same request is ignored; so is absence.
        host.send(RemoteEvent::Blob {
            id: PIXEL_ID.into(),
            mime: None,
            bytes: None,
        })
        .expect("send");
        send(&host, b"tick");
        until(&mut t, "the tick", |t| {
            t.screen_text().lines.join("\n").contains("tick")
        });
        assert!(!has_pixel(&t));
        assert_eq!(blob_requests(&log), [PIXEL_ID], "asked once, not again");
        assert!(
            t.tsp().0.missing_blobs().is_empty(),
            "the host has none: the mirror stops wanting it"
        );
    }

    /// The owner of a host-backed session passes a fetched blob on to its subscribers as the
    /// `b` message the program would have sent, so a frontend mirror gets it too.
    #[test]
    fn a_fetched_blob_reaches_a_subscriber_mirror() {
        let (mut owner, host, log) = connect();
        let mut wire = hello();
        wire.extend(open());
        wire.extend(frame1());
        host.send(RemoteEvent::Snapshot(wire)).expect("send");
        until(&mut owner, "live", |t| t.tsp().live());

        let sub = owner.subscribe_output(std::sync::Arc::new(|| {}));
        let mut mirror = deck_tsp::mirror::Mirror::new();
        let deadline = std::time::Instant::now() + TIMEOUT;
        let feed = |mirror: &mut deck_tsp::mirror::Mirror, ev: Option<OutputEvent>| {
            if let Some(OutputEvent::Snapshot(b) | OutputEvent::Output(b)) = ev {
                mirror.feed(&b, std::time::Instant::now());
            }
        };
        while !mirror.live() {
            assert!(std::time::Instant::now() < deadline, "no snapshot");
            feed(&mut mirror, sub.recv_timeout(Duration::from_millis(50)));
        }

        send(&host, &image_frame());
        until(&mut owner, "the request", |_| {
            blob_requests(&log) == [PIXEL_ID]
        });
        host.send(RemoteEvent::Blob {
            id: PIXEL_ID.into(),
            mime: Some("image/png".into()),
            bytes: Some(PIXEL.to_vec()),
        })
        .expect("send");
        while mirror.document().blob(PIXEL_ID).is_none() {
            assert!(std::time::Instant::now() < deadline, "blob never reached");
            feed(&mut mirror, sub.recv_timeout(Duration::from_millis(50)));
        }
        assert!(mirror.is_idle());
        assert_eq!(
            serde_json::to_value(mirror.document()).expect("document JSON"),
            doc_json(&owner)
        );
        assert_eq!(host_inputs(&log), b"");
    }

    /// A blob arriving while the program is half way through a sequence waits for its end.
    #[test]
    fn a_blob_for_subscribers_waits_until_the_stream_is_between_messages() {
        let (mut owner, host, log) = connect();
        let mut wire = hello();
        wire.extend(open());
        wire.extend(frame1());
        host.send(RemoteEvent::Snapshot(wire)).expect("send");
        until(&mut owner, "live", |t| t.tsp().live());
        let sub = owner.subscribe_output(std::sync::Arc::new(|| {}));
        let deadline = std::time::Instant::now() + TIMEOUT;
        let mut mirror = deck_tsp::mirror::Mirror::new();
        let mut stream = Vec::new();
        let pump = |mirror: &mut deck_tsp::mirror::Mirror, stream: &mut Vec<u8>| {
            while let Some(ev) = sub.recv_timeout(Duration::from_millis(60)) {
                match ev {
                    OutputEvent::Snapshot(b) => {
                        mirror.feed(&b, std::time::Instant::now());
                    }
                    OutputEvent::Output(b) => {
                        stream.extend_from_slice(&b);
                        mirror.feed(&b, std::time::Instant::now());
                    }
                    _ => {}
                }
            }
        };
        while !mirror.live() {
            assert!(std::time::Instant::now() < deadline, "no snapshot");
            pump(&mut mirror, &mut stream);
        }

        send(&host, &image_frame());
        until(&mut owner, "the request", |_| {
            blob_requests(&log) == [PIXEL_ID]
        });
        // A third frame in chunks; the blob arrives between its first two and its end.
        let chunks = encode_chunked(
            Verb::Frame,
            &[],
            br#"{"sf":"s1","s":3,"ops":[["text","t","append"," and more of it"]]}"#,
            24,
            "c2",
        );
        let mut parts = Vec::new();
        let mut start = 0;
        for i in 0..chunks.len() - 1 {
            if chunks[i] == 0x1b && chunks[i + 1] == b'\\' {
                parts.push(chunks[start..i + 2].to_vec());
                start = i + 2;
            }
        }
        assert!(parts.len() >= 3);
        send(&host, &parts[0]);
        send(&host, &parts[1]);
        host.send(RemoteEvent::Blob {
            id: PIXEL_ID.into(),
            mime: None,
            bytes: Some(PIXEL.to_vec()),
        })
        .expect("send");
        until(&mut owner, "the blob in the owner mirror", has_pixel);
        pump(&mut mirror, &mut stream);
        assert!(
            !String::from_utf8_lossy(&stream).contains("\x1b_tsp;b;"),
            "the blob message must not split the chunked frame: {stream:?}"
        );
        assert!(mirror.document().blob(PIXEL_ID).is_none());

        for part in &parts[2..] {
            send(&host, part);
        }
        while mirror.document().blob(PIXEL_ID).is_none() {
            assert!(std::time::Instant::now() < deadline, "blob never reached");
            pump(&mut mirror, &mut stream);
        }
        assert!(mirror.is_idle());
        assert_eq!(
            serde_json::to_value(mirror.document()).expect("document JSON"),
            doc_json(&owner),
            "the frame completed and the blob followed it"
        );
    }
}
