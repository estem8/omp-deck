//! An owned, incrementally maintained copy of the inline `omp.session` surface.
//!
//! The terminal's TSP mirror lives behind a mutex the output thread also takes, so the renderer
//! never draws from it directly: [`Snap::sync`] walks the document once per revision (inside one
//! short [`deck_term::TspHandle::with`]) and re-copies only nodes whose props changed (their
//! `changed_at` stamp moved). Every node carries a local generation and a subtree fingerprint, the
//! keys of the renderer's caches (parsed markdown, measured heights) so nothing is recomputed for
//! nodes omp did not touch.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use deck_tsp::document::{Document, NodeRef, Surface};
use serde_json::{Map, Value};

/// The props of one node.
pub type Props = Map<String, Value>;

/// Role of the surface the chat view draws.
pub const SESSION_ROLE: &str = "omp.session";

/// One node of the copy.
#[derive(Debug, Clone)]
pub struct SNode {
    pub id: String,
    pub kind: String,
    pub props: Props,
    pub children: Vec<String>,
    /// The parent node (`None` for the surface root).
    pub parent: Option<String>,
    /// Receive time of the last prop/text change (drives local timers: `age`, `took`).
    pub changed: Instant,
    /// Local counter, bumped whenever this node's own kind/props changed.
    pub gen_: u64,
    /// Fingerprint of the whole subtree (every `gen_`, the order of every child list).
    pub sub: u64,
    seen: u64,
}

impl SNode {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.props.get(key)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.props.get(key).and_then(Value::as_str)
    }

    /// `true` only for a JSON `true`.
    pub fn flag(&self, key: &str) -> bool {
        self.props.get(key).and_then(Value::as_bool) == Some(true)
    }

    pub fn num(&self, key: &str) -> Option<f64> {
        self.props.get(key).and_then(Value::as_f64)
    }

    pub fn arr(&self, key: &str) -> &[Value] {
        self.props
            .get(key)
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    pub fn obj(&self, key: &str) -> Option<&Props> {
        self.props.get(key).and_then(Value::as_object)
    }

    /// The primary text (`text` prop).
    pub fn text(&self) -> &str {
        self.str("text").unwrap_or_default()
    }

    /// omp's own role name (`omp.user`, `omp.tool.read`, …); empty when none.
    pub fn role(&self) -> &str {
        self.str("role").unwrap_or_default()
    }

    pub fn tone(&self) -> Option<&str> {
        self.str("tone")
    }

    pub fn hidden(&self) -> bool {
        self.flag("hidden")
    }

    /// Role equals `role` or is below it (`omp.tool.bash.output` is below `omp.tool`).
    pub fn role_under(&self, role: &str) -> bool {
        let r = self.role();
        r == role
            || (r.len() > role.len() && r.starts_with(role) && r.as_bytes()[role.len()] == b'.')
    }

    /// The `actions.click` name, if any.
    pub fn click(&self) -> Option<&str> {
        self.obj("actions")?.get("click")?.as_str()
    }

    /// The `actions.dblclick` name, if any.
    pub fn dblclick(&self) -> Option<&str> {
        self.obj("actions")?.get("dblclick")?.as_str()
    }
}

/// Immutable bytes of one blob.
#[derive(Debug, Clone)]
pub struct BlobBytes {
    pub bytes: Arc<[u8]>,
}

/// The copy of one surface.
#[derive(Default)]
pub struct Snap {
    /// Id of the surface (`sf` of every event).
    pub sf: String,
    nodes: HashMap<String, SNode>,
    blobs: HashMap<String, BlobBytes>,
    next_gen: u64,
    /// Counts sync passes; a node not touched by the latest one is gone.
    pass: u64,
    /// Bumped whenever the copy starts over: the first surface, another one replacing it, or the
    /// terminal replacing the whole document (a restart or a full replay of the session). The
    /// transcript follows the end again.
    pub resets: u64,
    /// The terminal's `(revision, epoch)` this copy reflects (see `TspHandle::with_stamp`).
    pub stamp: Option<(u64, u64)>,
    /// The surface's focused node id (omp's `focus` op).
    pub focus: Option<String>,
    /// What omp's hello said it applies: `send`, `edit`.
    pub can_send: bool,
    pub can_edit: bool,
}

fn mix(h: u64, x: u64) -> u64 {
    (h ^ x).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(17)
}

fn hash_str(s: &str) -> u64 {
    s.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| mix(h, u64::from(b)))
}

/// The inline `omp.session` surface of `doc`.
pub fn session_surface(doc: &Document) -> Option<&Surface> {
    doc.surfaces()
        .iter()
        .find(|s| s.is_open() && s.role() == Some(SESSION_ROLE))
}

impl Snap {
    /// Whether the copy is as new as the terminal's mirror (`rev`, `epoch`: [`TspHandle::revision`]
    /// and [`TspHandle::epoch`]), so nothing needs the mirror's lock.
    pub fn is_current(&self, rev: u64, epoch: u64) -> bool {
        self.stamp == Some((rev, epoch))
    }

    /// Brings the copy up to `doc`, which stands at `rev` in reset epoch `epoch` (both from
    /// `TspHandle::with_stamp`). Returns whether anything may have changed.
    pub fn sync(&mut self, doc: &Document, rev: u64, epoch: u64) -> bool {
        if self.is_current(rev, epoch) {
            return false;
        }
        // Another epoch is another document: nothing of the old copy may be told from the new
        // one by revision or id alone.
        let replaced = self.stamp.is_none_or(|(_, e)| e != epoch);
        self.stamp = Some((rev, epoch));
        let prog = doc.program();
        self.can_send = prog.is_some_and(|p| p.supports("send"));
        self.can_edit = prog.is_some_and(|p| p.supports("edit"));
        let Some(surface) = session_surface(doc) else {
            let had = !self.nodes.is_empty();
            self.nodes.clear();
            self.blobs.clear();
            self.sf.clear();
            if replaced {
                self.resets += 1;
            }
            return had || replaced;
        };
        if self.sf != surface.id() || replaced {
            self.nodes.clear();
            self.sf = surface.id().to_owned();
            self.resets += 1;
        }
        self.focus = surface.focus().map(str::to_owned);
        self.pass += 1;
        let root = surface.root();
        self.sync_node(doc, root, None);
        let pass = self.pass;
        self.nodes.retain(|_, n| n.seen == pass);
        // Blobs of images that are gone go with them.
        if !self.blobs.is_empty() {
            let used: std::collections::HashSet<&str> = self
                .nodes
                .values()
                .filter(|n| n.kind == "image")
                .filter_map(|n| n.str("blob"))
                .collect();
            self.blobs.retain(|id, _| used.contains(id.as_str()));
        }
        true
    }

    fn sync_node(&mut self, doc: &Document, n: NodeRef<'_>, parent: Option<&str>) -> u64 {
        let id = n.id();
        let unchanged = self.nodes.get(id).is_some_and(|old| {
            old.changed == n.changed_at()
                && old.kind == n.kind()
                && old.props.len() == n.props().len()
        });
        if !unchanged {
            self.next_gen += 1;
            let children = self
                .nodes
                .get_mut(id)
                .map(|old| std::mem::take(&mut old.children))
                .unwrap_or_default();
            self.nodes.insert(
                id.to_owned(),
                SNode {
                    id: id.to_owned(),
                    kind: n.kind().to_owned(),
                    props: n.props().clone(),
                    children,
                    parent: None,
                    changed: n.changed_at(),
                    gen_: self.next_gen,
                    sub: 0,
                    seen: 0,
                },
            );
        }
        let pass = self.pass;
        let mut sub = 0;
        let mut blob = None;
        if let Some(node) = self.nodes.get_mut(id) {
            node.seen = pass;
            if node.parent.as_deref() != parent {
                node.parent = parent.map(str::to_owned);
            }
            if node.children.as_slice() != n.child_ids() {
                node.children = n.child_ids().to_vec();
            }
            if node.kind == "image" {
                blob = node.str("blob").map(str::to_owned);
            }
            sub = mix(node.gen_, node.children.len() as u64);
        }
        if let Some(blob) = blob
            && !self.blobs.contains_key(&blob)
            && let Some(b) = doc.blob(&blob)
        {
            self.blobs.insert(
                blob,
                BlobBytes {
                    bytes: b.bytes.as_slice().into(),
                },
            );
        }
        for c in n.children() {
            let cs = self.sync_node(doc, c, Some(id));
            sub = mix(mix(sub, hash_str(c.id())), cs);
        }
        if let Some(node) = self.nodes.get_mut(id) {
            node.sub = sub;
        }
        sub
    }

    pub fn get(&self, id: &str) -> Option<&SNode> {
        self.nodes.get(id)
    }

    pub fn parent_of(&self, id: &str) -> Option<&SNode> {
        self.nodes.get(self.nodes.get(id)?.parent.as_deref()?)
    }

    /// One of the surface's regions: `main`, `dock` or `layer`.
    pub fn region(&self, name: &str) -> Option<&SNode> {
        self.get(name)
    }

    /// The children of `n`, in document order.
    pub fn kids<'a>(&'a self, n: &'a SNode) -> impl Iterator<Item = &'a SNode> + 'a {
        n.children.iter().filter_map(|c| self.nodes.get(c))
    }

    /// Depth-first search below `n` (excluding `n`).
    pub fn find<'a>(&'a self, n: &'a SNode, pred: &dyn Fn(&SNode) -> bool) -> Option<&'a SNode> {
        for c in self.kids(n) {
            if pred(c) {
                return Some(c);
            }
            if let Some(f) = self.find(c, pred) {
                return Some(f);
            }
        }
        None
    }

    /// The first descendant of `n` whose `role` is exactly `role`.
    pub fn by_role<'a>(&'a self, n: &'a SNode, role: &str) -> Option<&'a SNode> {
        self.find(n, &|c| c.role() == role)
    }

    pub fn blob(&self, id: &str) -> Option<&BlobBytes> {
        self.blobs.get(id)
    }

    /// The composer: the `editor` with `sendable` set, else any non-readonly `editor` in the dock.
    pub fn composer(&self) -> Option<&SNode> {
        let dock = self.region("dock")?;
        self.find(dock, &|n| n.kind == "editor" && n.flag("sendable"))
            .or_else(|| self.find(dock, &|n| n.kind == "editor" && !n.flag("readonly")))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use deck_tsp::mirror::Mirror;
    use deck_tsp::stream::{Verb, encode_message};

    const FIXTURE: &str =
        include_str!("../../../deck-tsp/tests/fixtures/omp-18.15.0-session.jsonl");

    /// A mirror fed the first `n` program messages of the recorded 18.15.0 session.
    /// Syncs `s` to `m` as a terminal at its document's revision in reset epoch 0 would.
    pub(crate) fn sync_doc(s: &mut Snap, m: &Mirror) -> bool {
        s.sync(m.document(), m.document().revision(), 0)
    }

    pub(crate) fn fixture_mirror(n: usize) -> Mirror {
        let mut m = Mirror::new();
        feed_fixture(&mut m, 0..n);
        m
    }

    /// Feeds program messages `range` (counted over the fixture's `out` lines) into `m`.
    pub(crate) fn feed_fixture(m: &mut Mirror, range: std::ops::Range<usize>) {
        let mut taken = 0;
        for line in FIXTURE.lines() {
            let v: Value = serde_json::from_str(line).expect("fixture line is JSON");
            if v["dir"] != "out" {
                continue;
            }
            if taken >= range.end {
                break;
            }
            taken += 1;
            if taken <= range.start {
                continue;
            }
            let verb = match v["verb"].as_str() {
                Some("o") => Verb::Open,
                Some("f") => Verb::Frame,
                Some("x") => Verb::Close,
                Some("s") => Verb::Style,
                Some("q") => Verb::Query,
                _ => continue,
            };
            let params: Vec<(String, String)> = v["params"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                        .collect()
                })
                .unwrap_or_default();
            let refs: Vec<(&str, &str)> = params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let body = serde_json::to_vec(&v["body"]).expect("body JSON");
            m.feed(&encode_message(verb, &refs, &body), Instant::now());
        }
    }

    #[test]
    fn the_fixture_session_is_copied_with_its_regions() {
        let m = fixture_mirror(usize::MAX);
        let mut s = Snap::default();
        assert!(sync_doc(&mut s, &m));
        assert_eq!(s.sf, "s:1");
        let main = s.region("main").expect("main region");
        assert!(s.kids(main).any(|n| n.role() == "omp.user"));
        assert!(s.kids(main).any(|n| n.kind == "tool"));
        assert!(s.composer().is_some_and(|c| c.flag("sendable")));
        // Nothing moved: nothing to do.
        assert!(!sync_doc(&mut s, &m));
    }

    #[test]
    fn a_first_or_replayed_surface_counts_as_a_reset_and_streaming_does_not() {
        let mut mirror = fixture_mirror(12);
        let mut s = Snap::default();
        assert_eq!(s.resets, 0);
        sync_doc(&mut s, &mirror);
        assert_eq!(s.resets, 1, "the first surface");
        feed_fixture(&mut mirror, 12..usize::MAX);
        sync_doc(&mut s, &mirror);
        assert_eq!(s.resets, 1, "later frames of the same surface");
        // The terminal replaced its document: the same surface again, at whatever revision
        // (here exactly the one the copy stands at), in the next epoch.
        let rev = mirror.document().revision();
        assert!(
            !s.sync(mirror.document(), rev, 0),
            "same stamp: nothing to do"
        );
        assert!(s.sync(mirror.document(), rev, 1));
        assert_eq!(s.resets, 2, "a reset that kept the document revision");
        assert!(s.region("main").is_some());
        assert!(s.is_current(rev, 1));
    }

    #[test]
    fn images_are_dropped_with_their_nodes_and_unchanged_syncs_keep_generations() {
        let m = fixture_mirror(usize::MAX);
        let mut s = Snap::default();
        sync_doc(&mut s, &m);
        let gens: HashMap<String, u64> = s.nodes.iter().map(|(k, n)| (k.clone(), n.gen_)).collect();
        // The same document at a later revision: every node is as it was.
        s.sync(m.document(), m.document().revision() + 1, 0);
        assert!(s.nodes.iter().all(|(k, n)| gens[k] == n.gen_));
        s.blobs.insert(
            "ghost".into(),
            BlobBytes {
                bytes: Arc::from(vec![1u8]),
            },
        );
        s.sync(m.document(), m.document().revision() + 2, 0);
        assert!(
            s.blob("ghost").is_none(),
            "a blob no image refers to is pruned"
        );
    }

    #[test]
    fn untouched_nodes_keep_their_generation_and_a_touched_one_moves_its_ancestors() {
        let mut mirror = fixture_mirror(12);
        let mut s = Snap::default();
        sync_doc(&mut s, &mirror);
        let before: HashMap<String, (u64, u64)> = s
            .nodes
            .iter()
            .map(|(k, n)| (k.clone(), (n.gen_, n.sub)))
            .collect();
        feed_fixture(&mut mirror, 12..usize::MAX);
        sync_doc(&mut s, &mirror);
        let welcome = s.by_role(s.region("main").expect("main"), "omp.welcome");
        let welcome = welcome.expect("welcome card");
        if let Some((g, sub)) = before.get(&welcome.id) {
            assert_eq!(*g, welcome.gen_, "an untouched node keeps its generation");
            assert_eq!(*sub, welcome.sub, "and its subtree fingerprint");
        }
        let main = s.region("main").expect("main");
        assert_ne!(before["main"].1, main.sub, "the region's subtree changed");
    }
}
