//! Pure layout of the session tree view: which nodes are shown for a filter, how hidden nodes
//! lift their visible descendants, which lane each row draws in (git-graph style) and the row
//! model the view paints. No egui here; the view caches one [`Layout`] per (tree revision,
//! leaf, filter).

mod fuzzy;

use std::ops::Range;

use deck_core::tree::{NodeKind, SessionTree, TreeNode};

pub use fuzzy::{FuzzySearch, FuzzyText};

/// Which nodes are listed. Mirrors omp's `/tree` filters (default, no-tools, user-only, labeled,
/// all), plus the GUI's conversation view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Filter {
    /// User prompts, assistant replies with text, compactions and branch summaries.
    #[default]
    Conversation,
    /// omp's `/tree` default: everything except settings-like entries and empty assistant steps.
    Default,
    /// The default view minus tool results.
    NoTools,
    /// Only user requests: prompts and user-invoked skill / collab prompts.
    UserOnly,
    /// Only nodes that carry a label.
    Labeled,
    /// Everything except empty assistant steps (omp's base rule applies in every mode).
    All,
}

impl Filter {
    /// In the order Ctrl+O cycles through them (omp's order, with the GUI's view first).
    pub const ALL: [Filter; 6] = [
        Filter::Conversation,
        Filter::Default,
        Filter::NoTools,
        Filter::UserOnly,
        Filter::Labeled,
        Filter::All,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Filter::Conversation => "Conversation",
            Filter::Default => "Default",
            Filter::NoTools => "No tools",
            Filter::UserOnly => "User only",
            Filter::Labeled => "Labeled",
            Filter::All => "All",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Filter::Conversation => "Your prompts and the assistant's replies, tool calls hidden",
            Filter::Default => "What omp's /tree shows by default",
            Filter::NoTools => "The default view without tool results",
            Filter::UserOnly => "Only your prompts",
            Filter::Labeled => "Only entries with a label",
            Filter::All => "Every entry, settings included; empty assistant steps stay hidden",
        }
    }

    /// The letter of the Alt+letter shortcut that picks this filter.
    pub fn shortcut(self) -> char {
        match self {
            Filter::Conversation => 'C',
            Filter::Default => 'D',
            Filter::NoTools => 'T',
            Filter::UserOnly => 'U',
            Filter::Labeled => 'L',
            Filter::All => 'A',
        }
    }

    /// The filter `delta` steps away in [`Filter::ALL`], wrapping (Ctrl+O / Ctrl+Shift+O).
    pub fn cycled(self, delta: isize) -> Filter {
        let n = Filter::ALL.len() as isize;
        let at = Filter::ALL.iter().position(|&f| f == self).unwrap_or(0) as isize;
        Filter::ALL[(at + delta).rem_euclid(n) as usize]
    }
}

/// Visual class of a row (glyph and accent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowClass {
    User,
    Assistant,
    Tool,
    Compaction,
    Summary,
    Other,
}

impl RowClass {
    pub fn of(kind: &NodeKind) -> RowClass {
        match kind {
            NodeKind::User => RowClass::User,
            NodeKind::Assistant => RowClass::Assistant,
            NodeKind::ToolResult | NodeKind::BashExecution => RowClass::Tool,
            NodeKind::Compaction => RowClass::Compaction,
            NodeKind::BranchSummary => RowClass::Summary,
            _ => RowClass::Other,
        }
    }
}

/// One visible node.
#[derive(Clone, Debug)]
pub struct Row {
    /// Index into `SessionTree::nodes`.
    pub node: usize,
    pub lane: u16,
    /// On the root → current-leaf path.
    pub on_path: bool,
    /// The session's current leaf ("you are here").
    pub is_leaf: bool,
    /// Visible (lifted) children; more than one makes this a branch point.
    pub children: u16,
    /// Row of the nearest visible ancestor.
    pub parent: Option<u32>,
    /// Epoch seconds of the node's timestamp.
    pub ts: Option<i64>,
    pub class: RowClass,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Entries in the session file.
    pub entries: usize,
    /// Rows shown under the filter.
    pub shown: usize,
    /// Visible nodes with no visible child: the tips of the branches.
    pub tips: usize,
    /// Visible nodes with more than one visible child.
    pub forks: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub rows: Vec<Row>,
    /// Number of lanes used (at least 1 when there are rows).
    pub lanes: u16,
    /// Rows whose edge to the parent spans more than the adjacent row (drawn when their span
    /// crosses the viewport, even if neither end is on screen).
    pub long_edges: Vec<u32>,
    pub leaf_row: Option<usize>,
    /// The session's actual leaf node, listed or not.
    pub leaf_node: Option<usize>,
    pub stats: Stats,
    /// `node index → row` (`u32::MAX` = not shown).
    row_of: Vec<u32>,
}

impl Layout {
    pub fn row_of_node(&self, node: usize) -> Option<usize> {
        self.row_of
            .get(node)
            .copied()
            .filter(|&r| r != u32::MAX)
            .map(|r| r as usize)
    }

    /// `node` when it is listed, else its nearest listed ancestor: where a selection goes when a
    /// filter hides it (omp keeps the selection on the nearest visible row).
    pub fn nearest_listed(&self, tree: &SessionTree, node: usize) -> Option<usize> {
        let mut cur = Some(node);
        for _ in 0..=tree.nodes.len() {
            let i = cur?;
            if self.row_of_node(i).is_some() {
                return Some(i);
            }
            cur = tree.parent_index(i);
        }
        None // parent cycle
    }
}

/// Whether node `i` is listed for `filter`. A leaf that the filter hides is not forced in: "you
/// are here" moves to its nearest listed ancestor (except omp's own rule that an empty
/// assistant step is shown while it is the leaf).
pub fn is_visible(tree: &SessionTree, i: usize, filter: Filter, leaf: Option<usize>) -> bool {
    let n = &tree.nodes[i];
    if filter == Filter::Conversation {
        return match n.kind {
            NodeKind::User | NodeKind::Compaction | NodeKind::BranchSummary => true,
            NodeKind::Assistant => n.has_text,
            _ => false,
        };
    }
    // omp's other modes all start from the same rule (tree-selector.ts `#buildFilter`).
    if !assistant_listed(n, Some(i) == leaf) {
        return false;
    }
    match filter {
        Filter::All => true,
        Filter::Default => !is_settings(n),
        Filter::NoTools => !is_settings(n) && n.kind != NodeKind::ToolResult,
        Filter::UserOnly => is_user_request(n),
        Filter::Labeled => tree.labels.contains_key(&n.id),
        Filter::Conversation => false, // handled above
    }
}

/// omp's base rule: an assistant message without text is hidden unless it is the current leaf or
/// stopped for a reason other than `stop` / `toolUse` (error, aborted, length, …).
fn assistant_listed(n: &TreeNode, is_leaf: bool) -> bool {
    if n.kind != NodeKind::Assistant || is_leaf || n.has_text {
        return true;
    }
    n.stop_reason
        .as_deref()
        .is_some_and(|r| !r.is_empty() && r != "stop" && r != "toolUse")
}

/// omp's `isSettingsEntry`: bookkeeping entries that only the "all" mode lists.
fn is_settings(n: &TreeNode) -> bool {
    match &n.kind {
        NodeKind::Label
        | NodeKind::Custom(_)
        | NodeKind::ModelChange
        | NodeKind::ModelUsage
        | NodeKind::ThinkingChange => true,
        // Unknown entry types are `Setting` too, but omp lists those like any other entry.
        NodeKind::Setting(ty) => matches!(
            ty.as_str(),
            "service_tier_change"
                | "title_change"
                | "mode_change"
                | "credential_pin"
                | "ttsr_injection"
                | "session_init"
                | "reset_boundary"
        ),
        _ => false,
    }
}

/// omp's `isUserRequestEntry`, computed by deck-core while reading the file.
fn is_user_request(n: &TreeNode) -> bool {
    n.user_request
}

/// A result of omp's `ask` tool, the entries "Re-answer…" applies to.
pub fn is_ask_result(n: &TreeNode) -> bool {
    matches!(n.kind, NodeKind::ToolResult) && n.tool_name.as_deref() == Some("ask")
}

/// Lay the tree out for `filter`. Hidden nodes drop out and their visible descendants attach to
/// the nearest visible ancestor (omp's lift rule). Siblings order: the child on the path to
/// `leaf` first, then by time. Lane 0 carries the active path; every other sibling opens a new
/// lane (the lowest one free for its whole span) that closes after its subtree.
pub fn build(tree: &SessionTree, leaf: Option<usize>, filter: Filter) -> Layout {
    let n = tree.nodes.len();
    let leaf = leaf.filter(|&l| l < n);
    let on_path = {
        let mut p = tree.active_path(leaf);
        p.resize(n, false);
        p
    };

    // Lift: visible children per nearest visible ancestor, in file/time order.
    let mut vkids: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut vroots: Vec<u32> = Vec::new();
    let mut stack: Vec<(usize, Option<usize>)> =
        tree.roots.iter().rev().map(|&r| (r, None)).collect();
    while let Some((i, anc)) = stack.pop() {
        let mut next_anc = anc;
        if is_visible(tree, i, filter, leaf) {
            match anc {
                Some(a) => vkids[a].push(i as u32),
                None => vroots.push(i as u32),
            }
            next_anc = Some(i);
        }
        if let Some(kids) = tree.children.get(i) {
            stack.extend(kids.iter().rev().map(|&c| (c, next_anc)));
        }
    }
    // The branch holding the leaf leads (stable: time order is kept within each group).
    let lead = |c: &u32| !on_path[*c as usize];
    vroots.sort_by_key(lead);
    for k in vkids.iter_mut().filter(|k| k.len() > 1) {
        k.sort_by_key(lead);
    }

    // Pre-order rows: (node, parent row, index among the parent's visible children).
    let mut order: Vec<(u32, Option<u32>, u32)> = Vec::new();
    let mut dfs: Vec<(u32, Option<u32>, u32)> = vroots
        .iter()
        .enumerate()
        .rev()
        .map(|(i, &r)| (r, None, i as u32))
        .collect();
    while let Some((node, parent, idx)) = dfs.pop() {
        let row = order.len() as u32;
        order.push((node, parent, idx));
        dfs.extend(
            vkids[node as usize]
                .iter()
                .enumerate()
                .rev()
                .map(|(i, &c)| (c, Some(row), i as u32)),
        );
    }
    let count = order.len();
    let mut size = vec![1u32; count];
    for r in (0..count).rev() {
        if let Some(p) = order[r].1 {
            size[p as usize] += size[r];
        }
    }

    // Lanes: busy row intervals per lane.
    let mut busy: Vec<Vec<(u32, u32)>> = Vec::new();
    let mut lane_of = vec![0u16; count];
    for r in 0..count {
        let (_, parent, idx) = order[r];
        let end = r as u32 + size[r] - 1;
        let lane = match parent {
            Some(p) if idx == 0 => lane_of[p as usize],
            _ => {
                let (from, start) = match parent {
                    Some(p) => (lane_of[p as usize] as usize + 1, p + 1),
                    None => (0, r as u32),
                };
                let mut l = from;
                while busy
                    .get(l)
                    .is_some_and(|b| b.iter().any(|&(a, z)| a <= end && z >= start))
                {
                    l += 1;
                }
                if l == busy.len() {
                    busy.push(Vec::new());
                }
                busy[l].push((start, end));
                l as u16
            }
        };
        lane_of[r] = lane;
    }

    let mut row_of = vec![u32::MAX; n];
    let mut rows = Vec::with_capacity(count);
    let mut long_edges = Vec::new();
    let mut stats = Stats {
        entries: n,
        shown: count,
        ..Stats::default()
    };
    for (r, &(node, parent, _)) in order.iter().enumerate() {
        let i = node as usize;
        let kids = vkids[i].len();
        match kids {
            0 => stats.tips += 1,
            1 => {}
            _ => stats.forks += 1,
        }
        if parent.is_some_and(|p| p as usize + 1 != r) {
            long_edges.push(r as u32);
        }
        row_of[i] = r as u32;
        rows.push(Row {
            node: i,
            lane: lane_of[r],
            on_path: on_path[i],
            is_leaf: false,
            children: kids.min(u16::MAX as usize) as u16,
            parent,
            ts: tree.nodes[i].timestamp.as_deref().and_then(parse_iso),
            class: RowClass::of(&tree.nodes[i].kind),
        });
    }
    // "You are here": the leaf's row, or its nearest listed ancestor when the filter hides it.
    let mut here = leaf;
    while let Some(i) = here
        && row_of[i] == u32::MAX
    {
        here = tree.parent_index(i);
    }
    let leaf_row = here.map(|i| row_of[i] as usize);
    if let Some(r) = leaf_row {
        rows[r].is_leaf = true;
    }
    Layout {
        lanes: busy.len().max(usize::from(count > 0)) as u16,
        rows,
        long_edges,
        leaf_row,
        leaf_node: leaf,
        stats,
        row_of,
    }
}

// ------------------------------------------------------------------ keyboard jumps

/// Alt+Up / Alt+Down: the nearest row after (`forward`) or before row `from` that holds a user or
/// assistant message (omp `moveSelectionWhere`; no wrapping, no move when there is none).
pub fn turn_jump(tree: &SessionTree, layout: &Layout, from: usize, forward: bool) -> Option<usize> {
    let is_turn = |r: &Row| {
        matches!(
            tree.nodes[r.node].kind,
            NodeKind::User | NodeKind::Assistant
        )
    };
    if forward {
        let after = layout.rows.get(from + 1..)?;
        after.iter().position(is_turn).map(|p| from + 1 + p)
    } else {
        layout.rows.get(..from)?.iter().rposition(is_turn)
    }
}

/// Children of a node in omp's order: the branch holding the leaf first, the rest as stored.
fn ordered<'a>(kids: &'a [usize], on_path: &'a [bool]) -> impl Iterator<Item = usize> + 'a {
    let lead = kids.iter().copied().filter(|&c| on_path[c]);
    lead.chain(kids.iter().copied().filter(|&c| !on_path[c]))
}

/// `[` / `]`: the row of the previous / next visible branch (omp `#moveBranch`).
///
/// A branch is one child of a fork (a node, or the roots, with at least two children, counted over
/// the unfiltered tree). Its head is promoted to the first listed node at or below it, so a
/// filtered-out head still has a target. Targets are visited in row order and the walk wraps; from
/// a row outside every branch, forward starts at the first target and backward at the last.
pub fn branch_jump(
    tree: &SessionTree,
    layout: &Layout,
    from: usize,
    forward: bool,
) -> Option<usize> {
    let selected = layout.rows.get(from)?.node;
    let n = tree.nodes.len();
    let on_path = tree.active_path(layout.leaf_node);

    // Pre-order over the whole tree, the leaf's branch first at every fork.
    let roots: Vec<usize> = ordered(&tree.roots, &on_path).collect();
    let mut preorder: Vec<usize> = Vec::with_capacity(n);
    let mut stack: Vec<usize> = roots.iter().rev().copied().collect();
    while let Some(i) = stack.pop() {
        preorder.push(i);
        let kids: Vec<usize> = ordered(&tree.children[i], &on_path).collect();
        stack.extend(kids.iter().rev());
    }

    // First listed node of each subtree, children before parents.
    let mut first_listed = vec![usize::MAX; n];
    for &i in preorder.iter().rev() {
        first_listed[i] = if layout.row_of_node(i).is_some() {
            i
        } else {
            ordered(&tree.children[i], &on_path)
                .map(|c| first_listed[c])
                .find(|&t| t != usize::MAX)
                .unwrap_or(usize::MAX)
        };
    }

    // (branch head, its target) for every branch of every fork.
    let mut heads: Vec<(usize, usize)> = Vec::new();
    let mut add_fork = |branches: &mut dyn Iterator<Item = usize>, count: usize| {
        if count < 2 {
            return;
        }
        heads.extend(
            branches
                .map(|b| (b, first_listed[b]))
                .filter(|&(_, t)| t != usize::MAX),
        );
    };
    add_fork(&mut roots.iter().copied(), roots.len());
    for &i in &preorder {
        add_fork(
            &mut ordered(&tree.children[i], &on_path),
            tree.children[i].len(),
        );
    }
    let row_of = |node: usize| layout.row_of_node(node).unwrap_or(usize::MAX);
    let mut targets: Vec<usize> = heads.iter().map(|&(_, t)| t).collect();
    targets.sort_by_key(|&t| row_of(t));
    targets.dedup();
    if targets.is_empty() {
        return None;
    }

    // The branch that holds the selection, at its nearest ancestor fork.
    let mut branch = selected;
    let mut current = None;
    for _ in 0..=n {
        let parent = tree.parent_index(branch);
        let siblings = parent.map_or(tree.roots.len(), |p| tree.children[p].len());
        if siblings > 1 {
            current = Some(branch);
            break;
        }
        match parent {
            Some(p) => branch = p,
            None => break,
        }
    }
    let current = current
        .and_then(|b| heads.iter().find(|&&(head, _)| head == b))
        .and_then(|&(_, t)| targets.iter().position(|&x| x == t));
    let count = targets.len();
    let next = match current {
        None if forward => 0,
        None => count - 1,
        Some(c) if forward => (c + 1) % count,
        Some(c) => (c + count - 1) % count,
    };
    layout.row_of_node(targets[next])
}

// ------------------------------------------------------------------ text, search, time

/// Lowercased whitespace-separated search words.
pub fn search_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The text omp's tree search matches a node against (`#getSearchableText`): the label, then
/// deck-core's `search_text` (role / type keyword, content, tool output, bash command, …).
fn search_text(tree: &SessionTree, node: usize) -> String {
    let n = &tree.nodes[node];
    let label = tree.labels.get(&n.id).filter(|l| !l.is_empty());
    let mut s = String::with_capacity(n.search_text.len() + label.map_or(0, |l| l.len() + 1));
    if let Some(l) = label {
        s.push_str(l);
        s.push(' ');
    }
    s.push_str(&n.search_text);
    s
}

/// Prepared (normalized) search texts, one per node, built on first use. Valid for one tree
/// revision: the owner clears it when the tree or its labels change.
#[derive(Default)]
pub struct SearchIndex {
    texts: Vec<Option<FuzzyText>>,
}

impl SearchIndex {
    pub fn clear(&mut self) {
        self.texts.clear();
    }
}

/// Per row: every search word fuzzy-matches the node's search text (omp's `FuzzyQuery`).
pub fn search_matches(
    tree: &SessionTree,
    layout: &Layout,
    search: &FuzzySearch,
    index: &mut SearchIndex,
) -> Vec<bool> {
    if index.texts.len() != tree.nodes.len() {
        index.texts.clear();
        index.texts.resize_with(tree.nodes.len(), || None);
    }
    layout
        .rows
        .iter()
        .map(|r| {
            let text = index.texts[r.node]
                .get_or_insert_with(|| FuzzyText::new(&search_text(tree, r.node)));
            search.matches(text)
        })
        .collect()
}

/// Byte ranges of `text` the search words occur in, merged and sorted.
pub fn highlight_spans(text: &str, tokens: &[String]) -> Vec<Range<usize>> {
    let lower = text.to_ascii_lowercase();
    let mut spans: Vec<Range<usize>> = Vec::new();
    for t in tokens.iter().filter(|t| !t.is_empty()) {
        let mut from = 0;
        while let Some(at) = lower[from..].find(t.as_str()) {
            let start = from + at;
            spans.push(start..start + t.len());
            from = start + t.len();
        }
    }
    spans.sort_by_key(|s| s.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    for s in spans {
        match merged.last_mut() {
            Some(last) if s.start <= last.end => last.end = last.end.max(s.end),
            _ => merged.push(s),
        }
    }
    merged
}

pub fn kind_name(kind: &NodeKind) -> &str {
    match kind {
        NodeKind::User => "user",
        NodeKind::Assistant => "assistant",
        NodeKind::ToolResult => "tool",
        NodeKind::Developer => "developer",
        NodeKind::BashExecution => "bash",
        NodeKind::OtherRole(r) => r,
        NodeKind::CustomMessage(c) => c,
        NodeKind::Compaction => "compaction",
        NodeKind::BranchSummary => "summary",
        NodeKind::ModelChange => "model",
        NodeKind::ModelUsage => "model usage",
        NodeKind::ThinkingChange => "thinking",
        NodeKind::Label => "label",
        NodeKind::Custom(c) => c,
        NodeKind::Setting(s) => s,
    }
}

/// `2026-10-21T12:34:56.789Z` (any offset is ignored beyond `Z`/none) → epoch seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = b.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0i64, |a, d| a * 10 + i64::from(d - b'0')))
    };
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') || !matches!(b.get(10), Some(b'T' | b' '))
    {
        return None;
    }
    let (y, m, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, sec) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + mi * 60 + sec)
}

/// Compact relative age for the right-hand column: `now`, `5m`, `3h`, `2d`, `6w`, `8mo`, `2y`.
pub fn age(now: i64, ts: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=44 => "now".to_owned(),
        45..=3_599 => format!("{}m", (d + 30) / 60),
        3_600..=86_399 => format!("{}h", d / 3_600),
        86_400..=1_209_599 => format!("{}d", d / 86_400),
        1_209_600..=5_183_999 => format!("{}w", d / 604_800),
        5_184_000..=31_535_999 => format!("{}mo", d / 2_592_000),
        _ => format!("{}y", d / 31_536_000),
    }
}

/// `2026-10-21 12:34:56 UTC` for tooltips.
pub fn clock(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

/// Tooltip of the fork button for a node.
pub fn fork_hint(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::User => "Fork before this prompt",
        NodeKind::Assistant => "Fork after this reply",
        _ => "Fork after this message",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;

    /// (id, parent, kind, has_text)
    type Spec<'a> = (&'a str, Option<&'a str>, NodeKind, bool);

    fn user<'a>(id: &'a str, parent: Option<&'a str>) -> Spec<'a> {
        (id, parent, NodeKind::User, true)
    }
    fn asst<'a>(id: &'a str, parent: Option<&'a str>) -> Spec<'a> {
        (id, parent, NodeKind::Assistant, true)
    }
    fn tool<'a>(id: &'a str, parent: Option<&'a str>) -> Spec<'a> {
        (id, parent, NodeKind::ToolResult, false)
    }

    fn tree(spec: &[Spec<'_>]) -> SessionTree {
        let nodes: Vec<Arc<TreeNode>> = spec
            .iter()
            .enumerate()
            .map(|(i, (id, parent, kind, has_text))| {
                Arc::new(TreeNode {
                    id: (*id).to_owned(),
                    parent: parent.map(str::to_owned),
                    kind: kind.clone(),
                    timestamp: Some(format!("2026-01-01T00:{:02}:{:02}Z", i / 60, i % 60)),
                    text: format!("{id} text"),
                    has_text: *has_text,
                    stop_reason: None,
                    active_leaf: None,
                    cost_before: Default::default(),
                    search_text: format!("{} {id} text", kind_name(kind)),
                    attribution: None,
                    user_request: matches!(kind, NodeKind::User),
                    tool_name: None,
                })
            })
            .collect();
        let index: HashMap<Arc<str>, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (Arc::from(n.id.as_str()), i))
            .collect();
        let mut children = vec![Vec::new(); nodes.len()];
        let mut roots = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            match n.parent.as_deref().and_then(|p| index.get(p)) {
                Some(&p) if p != i => children[p].push(i),
                _ => roots.push(i),
            }
        }
        SessionTree {
            nodes,
            index,
            children: children.into_iter().map(Arc::new).collect(),
            roots,
            labels: HashMap::new(),
            file_leaf: None,
        }
    }

    fn ids<'a>(t: &'a SessionTree, l: &Layout) -> Vec<&'a str> {
        l.rows.iter().map(|r| t.nodes[r.node].id.as_str()).collect()
    }
    fn lane_of(t: &SessionTree, l: &Layout, id: &str) -> u16 {
        l.rows[l.row_of_node(t.index[id]).unwrap()].lane
    }

    #[test]
    fn linear_chain_is_one_lane_in_file_order() {
        let t = tree(&[user("a", None), asst("b", Some("a")), user("c", Some("b"))]);
        let l = build(&t, Some(2), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b", "c"]);
        assert_eq!(l.lanes, 1);
        assert!(l.long_edges.is_empty());
        assert!(l.rows.iter().all(|r| r.lane == 0 && r.on_path));
        assert_eq!(l.leaf_row, Some(2));
        assert_eq!(l.stats.forks, 0);
        assert_eq!(l.stats.tips, 1);
    }

    #[test]
    fn hidden_nodes_lift_their_visible_descendants() {
        // a → t1(tool) → t2(tool) → b ; Conversation hides the tools, b attaches to a.
        let t = tree(&[
            user("a", None),
            tool("t1", Some("a")),
            tool("t2", Some("t1")),
            asst("b", Some("t2")),
        ]);
        let l = build(&t, Some(3), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b"]);
        assert_eq!(l.rows[1].parent, Some(0));
        let all = build(&t, Some(3), Filter::All);
        assert_eq!(ids(&t, &all), ["a", "t1", "t2", "b"]);
    }

    #[test]
    fn lifting_through_a_hidden_fork_creates_branches_on_the_visible_ancestor() {
        // a → tool → {b, c}: tool is hidden but forks; b and c both lift to a.
        let t = tree(&[
            user("a", None),
            tool("t", Some("a")),
            asst("b", Some("t")),
            asst("c", Some("t")),
        ]);
        let l = build(&t, Some(3), Filter::Conversation);
        assert_eq!(l.rows[0].children, 2);
        assert_eq!(l.stats.forks, 1);
        // The leaf's branch (c) leads and keeps the lane; b branches off.
        assert_eq!(ids(&t, &l), ["a", "c", "b"]);
        assert_eq!(lane_of(&t, &l, "c"), 0);
        assert_eq!(lane_of(&t, &l, "b"), 1);
    }

    #[test]
    fn nested_branches_get_separate_lanes_and_reuse_closed_ones() {
        //        a
        //        └ b ─┬ c ─┬ d        (leaf path: a b c d)
        //             │    └ e
        //             └ f ── g
        //   a2 (second root, own graph)
        let t = tree(&[
            user("a", None),
            asst("b", Some("a")),
            user("c", Some("b")),
            asst("d", Some("c")),
            asst("e", Some("c")),
            user("f", Some("b")),
            asst("g", Some("f")),
            user("a2", None),
        ]);
        let l = build(&t, Some(3), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b", "c", "d", "e", "f", "g", "a2"]);
        for id in ["a", "b", "c", "d"] {
            assert_eq!(lane_of(&t, &l, id), 0, "{id}");
        }
        // DFS order: e (processed first) takes lane 1 for rows 3..=4; f's line, which spans
        // rows 2..=6, then has to route around it.
        assert_eq!(lane_of(&t, &l, "e"), 1);
        assert_eq!(lane_of(&t, &l, "f"), 2);
        assert_eq!(lane_of(&t, &l, "g"), 2);
        assert_eq!(l.lanes, 3);
        // The second root starts a fresh graph in lane 0.
        assert_eq!(lane_of(&t, &l, "a2"), 0);
        assert_eq!(l.rows[7].parent, None);
        // Edges that skip rows or leave the parent's lane are listed as long.
        let e = l.row_of_node(t.index["e"]).unwrap() as u32;
        let f = l.row_of_node(t.index["f"]).unwrap() as u32;
        assert!(l.long_edges.contains(&e) && l.long_edges.contains(&f));
        assert_eq!(l.stats.forks, 2);
        assert_eq!(l.stats.tips, 4);
    }

    #[test]
    fn later_branches_route_around_lanes_claimed_by_nested_forks() {
        let t = tree(&[
            user("a", None),
            asst("b", Some("a")),
            asst("c", Some("a")),
            user("d", Some("b")),
            asst("e", Some("d")),
            asst("f", Some("d")),
        ]);
        // Leaf e: path a b d e. Rows: a b d e f c.
        let l = build(&t, Some(4), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b", "d", "e", "f", "c"]);
        // f (rows 4..=4) is closed before c opens at row 1..=5? c spans rows 1..=5 and f 3..=4,
        // so c routes around f.
        assert_eq!(lane_of(&t, &l, "f"), 1);
        assert_eq!(lane_of(&t, &l, "c"), 2);
        // Without the inner fork the outer branch takes lane 1.
        let t2 = tree(&[user("a", None), asst("b", Some("a")), asst("c", Some("a"))]);
        let l2 = build(&t2, Some(1), Filter::Conversation);
        assert_eq!(lane_of(&t2, &l2, "c"), 1);
    }

    #[test]
    fn active_path_is_marked_and_leads_every_fork() {
        let t = tree(&[
            user("a", None),
            asst("b", Some("a")),
            user("c", Some("b")),
            user("d", Some("b")),
        ]);
        // Leaf = d although c comes first in file order.
        let l = build(&t, Some(3), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b", "d", "c"]);
        let on: Vec<bool> = l.rows.iter().map(|r| r.on_path).collect();
        assert_eq!(on, [true, true, true, false]);
        assert!(l.rows[2].is_leaf && !l.rows[3].is_leaf);
        assert_eq!(l.rows[1].children, 2);
        // No leaf: nothing is on the path and file order is kept.
        let none = build(&t, None, Filter::Conversation);
        assert_eq!(ids(&t, &none), ["a", "b", "c", "d"]);
        assert!(none.rows.iter().all(|r| !r.on_path) && none.leaf_row.is_none());
    }

    #[test]
    fn filters_select_the_documented_kinds() {
        let spec = vec![
            user("u", None),
            ("m", Some("u"), NodeKind::ModelChange, false),
            tool("t", Some("m")),
            (
                "e",
                Some("t"),
                NodeKind::Assistant,
                false, // no text: hidden by default/conversation
            ),
            asst("a", Some("e")),
            ("k", Some("a"), NodeKind::Compaction, false),
            ("s", Some("k"), NodeKind::BranchSummary, false),
            ("x", Some("s"), NodeKind::Label, false),
            user("z", Some("x")),
        ];
        let mut t = tree(&spec);
        t.labels.insert("a".to_owned(), "keep".to_owned());
        // Leaf = z so no node is forced visible by being the leaf.
        let leaf = Some(t.index["z"]);
        let conv = build(&t, leaf, Filter::Conversation);
        assert_eq!(ids(&t, &conv), ["u", "a", "k", "s", "z"]);
        let def = build(&t, leaf, Filter::Default);
        assert_eq!(ids(&t, &def), ["u", "t", "a", "k", "s", "z"]);
        let all = build(&t, leaf, Filter::All);
        // `All` still applies omp's base rule: the empty assistant step "e" stays hidden.
        assert_eq!(all.rows.len(), 8);
        // Labeled lists only the labeled node; the hidden leaf's marker moves up to it.
        let lab = build(&t, leaf, Filter::Labeled);
        assert_eq!(ids(&t, &lab), ["a"]);
        assert!(lab.rows[0].is_leaf);
        // An assistant step that stopped abnormally shows in the default view.
        let mut t2 = tree(&[user("u", None), asst("a", Some("u")), user("v", Some("a"))]);
        Arc::make_mut(&mut t2.nodes[1]).has_text = false;
        Arc::make_mut(&mut t2.nodes[1]).stop_reason = Some("error".to_owned());
        let def2 = build(&t2, Some(2), Filter::Default);
        assert_eq!(ids(&t2, &def2), ["u", "a", "v"]);
        let conv2 = build(&t2, Some(2), Filter::Conversation);
        assert_eq!(ids(&t2, &conv2), ["u", "v"]);
    }

    #[test]
    fn a_hidden_leaf_moves_you_are_here_to_its_nearest_listed_ancestor() {
        let t = tree(&[user("a", None), asst("b", Some("a")), tool("t", Some("b"))]);
        let l = build(&t, Some(2), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b"]);
        assert!(l.rows[1].is_leaf && !l.rows[0].is_leaf);
        assert_eq!(l.leaf_row, Some(1));
        // The path still runs through the hidden node.
        assert!(l.rows.iter().all(|r| r.on_path));
        let all = build(&t, Some(2), Filter::All);
        assert!(all.rows[2].is_leaf);
    }

    #[test]
    fn deep_chains_do_not_recurse() {
        let mut spec: Vec<(String, Option<String>)> = vec![("n0".into(), None)];
        for i in 1..30_000 {
            spec.push((format!("n{i}"), Some(format!("n{}", i - 1))));
        }
        let owned: Vec<Spec<'_>> = spec
            .iter()
            .map(|(id, p)| (id.as_str(), p.as_deref(), NodeKind::User, true))
            .collect();
        let t = tree(&owned);
        let l = build(&t, Some(29_999), Filter::Conversation);
        assert_eq!(l.rows.len(), 30_000);
        assert_eq!(l.lanes, 1);
    }

    #[test]
    fn search_is_omp_fuzzy_over_label_role_and_text_and_highlights_literal_spans() {
        let mut t = tree(&[user("a", None), asst("b", Some("a"))]);
        Arc::make_mut(&mut t.nodes[0]).search_text = "user Fix the Login bug".to_owned();
        Arc::make_mut(&mut t.nodes[1]).search_text = "assistant Done".to_owned();
        t.labels.insert("b".to_owned(), "milestone".to_owned());
        let l = build(&t, Some(1), Filter::Conversation);
        let mut index = SearchIndex::default();
        let mut hits = |q: &str| search_matches(&t, &l, &FuzzySearch::new(q), &mut index);
        // Every piece must match, in any order and case.
        assert_eq!(hits("  login FIX "), [true, false]);
        assert_eq!(hits("login crash"), [false, false]);
        // Prefix of a label word; the role is searchable; an in-order abbreviation of a word.
        assert_eq!(hits("mile"), [false, true]);
        assert_eq!(hits("assistant"), [false, true]);
        assert_eq!(hits("lgn"), [true, false]);
        // Acronym over words: Fix The Login → "ftl".
        assert_eq!(hits("ftl"), [true, false]);
        // Blank queries match everything.
        assert_eq!(hits(""), [true, true]);
        assert_eq!(hits("?!"), [true, true]);
        let tok = search_tokens("  login FIX ");
        assert_eq!(
            highlight_spans("Fix the Login bug fix", &tok),
            vec![0..3, 8..13, 18..21]
        );
        assert_eq!(highlight_spans("aaaa", &search_tokens("aa a")), vec![0..4]);
    }

    /// A node's search text is prepared once and reused until the owner clears the index.
    #[test]
    fn search_index_is_reused_until_cleared() {
        let mut t = tree(&[user("a", None)]);
        let l = build(&t, Some(0), Filter::Conversation);
        let mut index = SearchIndex::default();
        let q = FuzzySearch::new("login");
        assert_eq!(search_matches(&t, &l, &q, &mut index), [false]);
        t.labels.insert("a".to_owned(), "login".to_owned());
        assert_eq!(search_matches(&t, &l, &q, &mut index), [false], "cached");
        index.clear();
        assert_eq!(search_matches(&t, &l, &q, &mut index), [true]);
    }

    /// omp searches a tool result's output, which the row text (the call summary) does not show.
    #[test]
    fn search_reaches_tool_output_and_the_bash_command_not_only_the_row_text() {
        use NodeKind::*;
        let mut t = chain(&[
            ("u", User, true),
            ("t", ToolResult, false),
            ("sh", BashExecution, false),
        ]);
        let set = |t: &mut SessionTree, i: usize, text: &str, search: &str| {
            let n = Arc::make_mut(&mut t.nodes[i]);
            n.text = text.to_owned();
            n.search_text = search.to_owned();
        };
        set(
            &mut t,
            1,
            "[read: src/a.rs:1-5]",
            "toolResult src/a.rs panicked at overflow",
        );
        set(
            &mut t,
            2,
            "[bash]: cargo test",
            "bashExecution 3 passed cargo test",
        );
        let l = build(&t, Some(2), Filter::All);
        let mut index = SearchIndex::default();
        let mut hits = |q: &str| search_matches(&t, &l, &FuzzySearch::new(q), &mut index);
        assert_eq!(hits("overflow"), [false, true, false]);
        assert_eq!(hits("passed"), [false, false, true]);
        // A label is prepended to the entry's own text.
        t.labels.insert("u".to_owned(), "kickoff".to_owned());
        index.clear();
        let mut hits = |q: &str| search_matches(&t, &l, &FuzzySearch::new(q), &mut index);
        assert_eq!(hits("kickoff"), [true, false, false]);
    }

    /// A linear chain of (id, kind, has_text) entries.
    fn chain(entries: &[(&str, NodeKind, bool)]) -> SessionTree {
        let spec: Vec<Spec<'_>> = entries
            .iter()
            .enumerate()
            .map(|(i, (id, kind, text))| {
                (
                    *id,
                    i.checked_sub(1).map(|p| entries[p].0),
                    kind.clone(),
                    *text,
                )
            })
            .collect();
        tree(&spec)
    }

    fn listed<'a>(t: &'a SessionTree, filter: Filter, leaf: &str) -> Vec<&'a str> {
        let leaf = t.index.get(leaf).copied();
        (0..t.nodes.len())
            .filter(|&i| is_visible(t, i, filter, leaf))
            .map(|i| t.nodes[i].id.as_str())
            .collect()
    }

    #[test]
    fn filters_follow_omp_predicates() {
        use NodeKind::*;
        let mut t = chain(&[
            ("u", User, true),
            ("skill", CustomMessage("skill-prompt".into()), false),
            ("collab", CustomMessage("collab-prompt".into()), false),
            ("note", CustomMessage("advisor".into()), false),
            ("t", ToolResult, false),
            ("sh", BashExecution, false),
            ("a", Assistant, true),
            ("noop", Assistant, false), // text-less, stop reason unset
            ("tooluse", Assistant, false),
            ("stop", Assistant, false),
            ("empty", Assistant, false),
            ("cut", Assistant, false), // text-less, stopped for another reason
            ("m", ModelChange, false),
            ("title", Setting("title_change".into()), false),
            ("reset", Setting("reset_boundary".into()), false),
            ("future", Setting("some_future_entry".into()), false),
            ("here", Assistant, false), // the leaf, text-less
        ]);
        for (id, reason) in [
            ("tooluse", "toolUse"),
            ("stop", "stop"),
            ("empty", ""),
            ("cut", "length"),
        ] {
            let i = t.index[id];
            Arc::make_mut(&mut t.nodes[i]).stop_reason = Some(reason.to_owned());
        }
        for id in ["t", "noop", "m"] {
            t.labels.insert(id.to_owned(), "tag".to_owned());
        }
        for id in ["skill", "collab"] {
            let i = t.index[id];
            Arc::make_mut(&mut t.nodes[i]).user_request = true;
        }
        let by = |f| listed(&t, f, "here");
        assert_eq!(by(Filter::Conversation), ["u", "a"]);
        // Default: no settings entries; empty assistant steps only when abnormal or the leaf; an
        // unknown entry type is listed.
        assert_eq!(
            by(Filter::Default),
            [
                "u", "skill", "collab", "note", "t", "sh", "a", "cut", "future", "here"
            ]
        );
        assert_eq!(
            by(Filter::NoTools),
            [
                "u", "skill", "collab", "note", "sh", "a", "cut", "future", "here"
            ]
        );
        // User requests: prompts and the two user-attributed custom prompts, not other customs.
        assert_eq!(by(Filter::UserOnly), ["u", "skill", "collab"]);
        // Labeled: the base rule still hides the labeled but empty "noop".
        assert_eq!(by(Filter::Labeled), ["t", "m"]);
        assert_eq!(
            by(Filter::All),
            [
                "u", "skill", "collab", "note", "t", "sh", "a", "cut", "m", "title", "reset",
                "future", "here"
            ]
        );
    }

    #[test]
    fn only_a_user_attributed_skill_prompt_counts_as_a_user_request() {
        use NodeKind::*;
        use deck_core::tree::Attribution;
        let mut t = chain(&[
            ("u", User, true),
            ("mine", CustomMessage("skill-prompt".into()), false),
            ("agents", CustomMessage("skill-prompt".into()), false),
            ("a", Assistant, true),
        ]);
        for (id, who, request) in [
            ("mine", Attribution::User, true),
            ("agents", Attribution::Agent, false),
        ] {
            let n = Arc::make_mut(&mut t.nodes[t.index[id]]);
            n.attribution = Some(who);
            n.user_request = request;
        }
        assert_eq!(listed(&t, Filter::UserOnly, "a"), ["u", "mine"]);
        assert!(listed(&t, Filter::Default, "a").contains(&"agents"));
    }

    #[test]
    fn conversation_filter_keeps_prompts_and_replies_only() {
        use NodeKind::*;
        let t = chain(&[
            ("u", User, true),
            ("t", ToolResult, false),
            ("a", Assistant, true),
            ("k", Compaction, false),
            ("empty", Assistant, false),
        ]);
        // Even the text-less leaf is not forced into this view.
        assert_eq!(listed(&t, Filter::Conversation, "empty"), ["u", "a", "k"]);
    }

    #[test]
    fn filters_cycle_in_order_and_wrap() {
        assert_eq!(Filter::Conversation.cycled(1), Filter::Default);
        assert_eq!(Filter::Default.cycled(1), Filter::NoTools);
        assert_eq!(Filter::NoTools.cycled(1), Filter::UserOnly);
        assert_eq!(Filter::UserOnly.cycled(1), Filter::Labeled);
        assert_eq!(Filter::Labeled.cycled(1), Filter::All);
        assert_eq!(Filter::All.cycled(1), Filter::Conversation);
        assert_eq!(Filter::Conversation.cycled(-1), Filter::All);
        assert_eq!(Filter::NoTools.cycled(-1), Filter::Default);
        let letters: Vec<char> = Filter::ALL.iter().map(|f| f.shortcut()).collect();
        assert_eq!(letters, ['C', 'D', 'T', 'U', 'L', 'A']);
    }

    #[test]
    fn turn_jump_skips_everything_but_user_and_assistant_messages_and_does_not_wrap() {
        use NodeKind::*;
        let t = chain(&[
            ("t0", ToolResult, false),
            ("u1", User, true),
            ("t1", ToolResult, false),
            ("k", Compaction, false),
            ("a1", Assistant, true),
            ("sh", BashExecution, false),
            ("t2", ToolResult, false),
        ]);
        let l = build(&t, Some(6), Filter::Default);
        let row = |id: &str| l.row_of_node(t.index[id]).unwrap();
        assert_eq!(turn_jump(&t, &l, row("t2"), false), Some(row("a1")));
        assert_eq!(turn_jump(&t, &l, row("a1"), false), Some(row("u1")));
        assert_eq!(turn_jump(&t, &l, row("u1"), true), Some(row("a1")));
        assert_eq!(turn_jump(&t, &l, row("u1"), false), None);
        assert_eq!(turn_jump(&t, &l, row("a1"), true), None);
        assert_eq!(turn_jump(&t, &l, row("t0"), true), Some(row("u1")));
    }

    #[test]
    fn a_hidden_selection_falls_back_to_its_nearest_listed_ancestor() {
        use NodeKind::*;
        let t = chain(&[
            ("u", User, true),
            ("a", Assistant, true),
            ("t", ToolResult, false),
            ("sh", BashExecution, false),
            ("a2", Assistant, true),
        ]);
        let l = build(&t, Some(4), Filter::NoTools);
        let node = |id: &str| t.index[id];
        assert_eq!(l.nearest_listed(&t, node("a2")), Some(node("a2")));
        assert_eq!(l.nearest_listed(&t, node("sh")), Some(node("sh")));
        assert_eq!(l.nearest_listed(&t, node("t")), Some(node("a")));
        let users = build(&t, Some(4), Filter::UserOnly);
        assert_eq!(users.nearest_listed(&t, node("a2")), Some(node("u")));
    }

    /// a → b → {c → {e, f}, d → g}; the leaf g lays d's branch first: a b d g c e f.
    fn forked() -> SessionTree {
        tree(&[
            user("a", None),
            asst("b", Some("a")),
            user("c", Some("b")),
            user("d", Some("b")),
            asst("e", Some("c")),
            asst("f", Some("c")),
            asst("g", Some("d")),
        ])
    }

    /// Walk branch jumps from `start` and return the ids visited.
    fn branch_walk(
        t: &SessionTree,
        l: &Layout,
        start: &str,
        forward: bool,
        steps: usize,
    ) -> Vec<String> {
        let mut at = l.row_of_node(t.index[start]).unwrap();
        (0..steps)
            .map(|_| {
                at = branch_jump(t, l, at, forward).unwrap();
                t.nodes[l.rows[at].node].id.clone()
            })
            .collect()
    }

    #[test]
    fn branch_jumps_visit_forks_in_row_order_and_wrap() {
        let t = forked();
        let l = build(&t, Some(t.index["g"]), Filter::Conversation);
        assert_eq!(ids(&t, &l), ["a", "b", "d", "g", "c", "e", "f"]);
        // Targets in row order: d, c (fork at b) then e, f (fork at c).
        assert_eq!(branch_walk(&t, &l, "g", true, 5), ["c", "e", "f", "d", "c"]);
        assert_eq!(
            branch_walk(&t, &l, "g", false, 5),
            ["f", "e", "c", "d", "f"]
        );
        // From inside a branch the nearest fork decides: f is its own branch at c.
        assert_eq!(branch_walk(&t, &l, "f", true, 1), ["d"]);
        assert_eq!(branch_walk(&t, &l, "f", false, 1), ["e"]);
        // c heads its own branch at b, and its jump lands on the next target, e.
        assert_eq!(branch_walk(&t, &l, "c", true, 1), ["e"]);
    }

    #[test]
    fn branch_jump_from_outside_every_branch_starts_at_the_edge() {
        let t = forked();
        let l = build(&t, Some(t.index["g"]), Filter::Conversation);
        // a and b have a single child each: not in any branch.
        assert_eq!(branch_walk(&t, &l, "a", true, 1), ["d"]);
        assert_eq!(branch_walk(&t, &l, "a", false, 1), ["f"]);
        // A linear tree has no branches to jump to.
        let lin = tree(&[user("x", None), asst("y", Some("x"))]);
        let ll = build(&lin, Some(1), Filter::Conversation);
        assert_eq!(branch_jump(&lin, &ll, 0, true), None);
    }

    #[test]
    fn a_filtered_out_branch_head_is_promoted_to_its_first_listed_descendant() {
        let mut t = forked();
        // Only g and e carry labels: the heads d and c are hidden, f has nothing listed below.
        t.labels.insert("g".to_owned(), "x".to_owned());
        t.labels.insert("e".to_owned(), "x".to_owned());
        let l = build(&t, Some(t.index["g"]), Filter::Labeled);
        assert_eq!(ids(&t, &l), ["g", "e"]);
        assert_eq!(branch_walk(&t, &l, "g", true, 3), ["e", "g", "e"]);
        assert_eq!(branch_walk(&t, &l, "g", false, 1), ["e"]);
    }

    #[test]
    fn iso_time_roundtrip_and_ages() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-10-21T12:34:56.789Z"), Some(1_792_586_096));
        assert_eq!(clock(1_792_586_096), "2026-10-21 12:34:56 UTC");
        assert_eq!(parse_iso("nonsense"), None);
        assert_eq!(parse_iso("2026-13-01T00:00:00Z"), None);
        assert_eq!(age(100, 90), "now");
        assert_eq!(age(1_000, 700), "5m");
        assert_eq!(age(10_000, 0), "2h");
        assert_eq!(age(3 * 86_400, 0), "3d");
        assert_eq!(age(21 * 86_400, 0), "3w");
        assert_eq!(age(100 * 86_400, 0), "3mo");
        assert_eq!(age(800 * 86_400, 0), "2y");
        assert_eq!(age(0, 50), "now");
    }

    #[test]
    fn fork_hint_names_the_cut() {
        assert_eq!(fork_hint(&NodeKind::User), "Fork before this prompt");
        assert_eq!(fork_hint(&NodeKind::Assistant), "Fork after this reply");
    }
}

#[cfg(test)]
mod ask_tests {
    use super::*;

    fn node(kind: NodeKind, tool: Option<&str>) -> TreeNode {
        TreeNode {
            id: "n".into(),
            parent: None,
            kind,
            timestamp: None,
            text: "[ask: x]".into(),
            has_text: false,
            stop_reason: None,
            active_leaf: None,
            cost_before: Default::default(),
            search_text: String::new(),
            attribution: None,
            user_request: false,
            tool_name: tool.map(str::to_owned),
        }
    }

    #[test]
    fn only_a_tool_result_named_ask_can_be_re_answered() {
        assert!(is_ask_result(&node(NodeKind::ToolResult, Some("ask"))));
        // Another tool (whatever its row text says), or a result without a name.
        assert!(!is_ask_result(&node(
            NodeKind::ToolResult,
            Some("ask_user")
        )));
        assert!(!is_ask_result(&node(NodeKind::ToolResult, Some("read"))));
        assert!(!is_ask_result(&node(NodeKind::ToolResult, None)));
        // The name belongs to tool results only.
        assert!(!is_ask_result(&node(NodeKind::User, Some("ask"))));
        assert!(!is_ask_result(&node(NodeKind::Assistant, Some("ask"))));
    }
}
