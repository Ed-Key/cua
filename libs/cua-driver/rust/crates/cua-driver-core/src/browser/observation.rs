//! Who owns a semantic page ref, and what an observation may be compared with.
//!
//! A semantic ref (`p<space>:<index>`) names one node of one live document as
//! one session saw it through one debugger attachment. The ref keeps its name
//! across that session's later observations of the same document, so an agent
//! can act on a ref it read earlier, and an action can answer with the change
//! since the observation the session holds instead of a whole new snapshot.
//!
//! Rule for every row below: when identity cannot be proven, invalidate and
//! return a full snapshot. A capability is never refreshed into validity: a
//! node whose fingerprint changed gets a new ref, and the old one is stale.
//!
//! # Ownership table
//!
//! Columns: the session's ref -> node capabilities; the session's observation
//! revision and diff baseline; what the agent is told.
//!
//! | Event | Refs | Revision and baseline | The agent is told |
//! |---|---|---|---|
//! | `get_browser_state` (default view) | Nodes seen before keep their refs; new nodes get new refs. Refs are retired when their node left a completely covered document or its fingerprint changed. | New revision; the baseline becomes this view. | A full snapshot (a diff when `since_revision` names the baseline). |
//! | Action by this session (click, type, navigate, steps) | Proven at use: binding, frame document, attachment, live fingerprint. The observation after the action then follows the row above. | New revision once the page settles; one per `browser_steps` batch. | The diff from the baseline to the new revision. A full snapshot with a reason when no diff is possible or it would be larger. `unavailable` with a reason when the observation failed or was refused; then nothing here changes. |
//! | Page mutates on its own | Unchanged until the next observation or use. A use re-reads the node's fingerprint and refuses `browser_ref_stale` when it differs or the node is gone; a ref refused for a changed fingerprint is retired for good, even if the node reads as before again. | Unchanged. | Nothing until the next result, whose diff lists the mutations as observed changes, never as caused by the action. A refusal never says what the node reads as now: that is page content, which only a read tells. |
//! | Navigation, document replaced | Every ref of the old document is stale: at once for `browser_navigate`, otherwise when the loader identity is next proven (use or observation). | Dropped. | `browser_ref_stale` for an old ref; the next observation is a full snapshot, reason `document_changed`. |
//! | Child frame navigated, removed, or moved to another process | Refs carry their frame's document and process identity. A use fails that proof (`browser_ref_stale`, and the space is dropped as above). An observation retires only that frame's refs; its new nodes get new refs. | Kept across an observation; dropped by a failed use. | The diff reports the frame's old lines as gone and its new lines as added. |
//! | Same-document history change (pushState, fragment) | Kept: the loader identity is unchanged, and fingerprints still guard each use. | Kept. | A diff; it carries the page URL when that changed. |
//! | Another session acts on the same tab | Nothing is shared between sessions. Actions on one tab run one at a time (mutation gate). | This session's baseline is unchanged. | This session's next diff includes the other session's effects as observed changes. |
//! | Tab closed, crashed, discarded, moved, rebound | Closed or discarded: `browser_tab_not_found`. Moved to another window: `browser_wrong_target_refused`. Crashed: the node no longer resolves, `browser_ref_stale`. Rebound: the new target id starts with no refs; the old target's refs live only under the old ids. | Unusable with the tab; a rebound target has none. | The refusal; after a new bind, a full snapshot, reason `no_baseline`. |
//! | User pressed Stop in the extension | The debugger detached: the attachment identity changed, so every ref is stale. | Dropped with the refs. | The Stop refusal while it holds; after the user allows Cua again, a full snapshot, reason `attachment_changed`. |
//! | Consent revoked | The binding is refused (`browser_consent_required`) or dropped (`browser_binding_stale`) before any ref is read. | Dropped with the binding. | The refusal. |
//! | Extension disconnect or reconnect, debugger detach and reattach | Reconnect drops the endpoint generation and every target on it (`browser_binding_stale`). Detach and reattach on a live connection changes the attachment identity: refs are stale. | Dropped. | The refusal, then a full snapshot: `no_baseline` after a new bind, `attachment_changed` after a reattach. |
//! | Session end, idle expiry, revive | The whole namespace goes with the session. A revived session starts empty. | Dropped. | `browser_binding_stale`; after a new bind, a full snapshot, reason `no_baseline`. |
//! | JavaScript dialog opens | Unchanged. | Unchanged: the page cannot be observed while the dialog is up. | The action that opened it returns at once, names the dialog and its `dialog_id`, and its `changes` is `unavailable`, reason `javascript_dialog_open`. A batch stops there. Until `browser_dialog` resolves it, reads and actions refuse `browser_dialog_open`. |
//! | Observation and action at the same time in one session | Recording an observation is one atomic step, and observations of a tab run one at a time. | Revisions are totally ordered; each result names its base and new revision. | A diff whose `base_revision` is the latest baseline, or a full snapshot with reason `revision_unknown` when the baseline moved meanwhile. |
//! | Call cancelled, response lost | Unchanged: refs stay valid either way. | Unchanged when cancelled before recording; advanced when the response was lost after it. | Every diff names `base_revision`. An agent that does not hold that revision asks `get_browser_state` for a full snapshot. |
//! | Change of format, query, scope, or a continuation | A query, scope or continuation read takes its refs from the same space (a continuation only after the space is proven to be the live document on the live attachment; a browser that reports no frame tree cannot prove it and gets a fresh snapshot instead). A `dom_refs_v1` snapshot replaces the space: semantic refs are stale. | A query, scope or continuation read leaves both alone. `dom_refs_v1` drops them. | A side read returns its own outline, never a diff. After `dom_refs_v1`, actions return no `changes`; the next semantic observation is a full snapshot, reason `no_baseline`. |
//!
//! "Left the observation" is not "removed": ranking and the size budget can
//! drop a node that still exists. A diff says `gone` only for a ref retired by
//! the rules above. A read that will be diffed is cut where its baseline was
//! cut (the same lowest-ranked node), so a line that grew does not push
//! unrelated lines out of the view.
//!
//! A fingerprint is the role, the name and, for a link, the destination. The
//! name does not count for text nodes, whose name is their content.

use std::collections::{HashMap, HashSet};

use super::store::{format_ref, FrameIdentity, RefEntry};

/// What must hold for two observations to be of the same live document
/// through the same attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocumentIdentity {
    /// Browser connection generation of the binding (a reconnect changes it).
    pub(crate) generation: u64,
    /// Debugger attachment of the tab on that connection (a detach changes it).
    pub(crate) attachment: u64,
    pub(crate) cdp_target_id: String,
    /// Main frame id and loader id. `None` when the browser cannot report a
    /// frame tree: such a document is never proven the same as another.
    pub(crate) root: Option<FrameIdentity>,
}

impl DocumentIdentity {
    pub(crate) fn proves(&self, later: &Self) -> bool {
        self.root.is_some() && self == later
    }
}

/// One node of one document: its frame's document identity, the process the
/// frame lives in, and the node id there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct NodeKey {
    frame: Option<FrameIdentity>,
    oopif_target_id: Option<String>,
    backend_node_id: i64,
}

impl NodeKey {
    pub(crate) fn of(entry: &RefEntry) -> Self {
        Self::in_frame(&entry.frame, entry.backend_node_id)
    }

    /// The node `backend_node_id` of the document `frame` names.
    pub(crate) fn in_frame(frame: &super::store::FrameRef, backend_node_id: i64) -> Self {
        Self {
            frame: frame.identity.clone(),
            oopif_target_id: frame.oopif_target_id.clone(),
            backend_node_id,
        }
    }
}

/// What a ref named when it was issued. A node that now reads differently is
/// another entity, whatever its node id.
///
/// The name does not count for a text node: there the name is the content,
/// and text that changed is the same line saying something else, not another
/// entity. For every other role it counts, whether or not the element can be
/// acted on right now: a disabled "Delete Alice" reused as an enabled
/// "Delete Bob" is not the ref the agent read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    pub(crate) role: String,
    pub(crate) name: Option<String>,
    pub(crate) destination: Option<String>,
}

impl Fingerprint {
    pub(crate) fn of(entry: &RefEntry) -> Self {
        Self::read(
            entry.node_name.clone(),
            entry.label.clone(),
            entry.destination.clone(),
        )
    }

    /// The fingerprint of what a node reads as now, by the same rule.
    pub(crate) fn read(role: String, name: Option<String>, destination: Option<String>) -> Self {
        let text = matches!(role.as_str(), "statictext" | "text");
        Self {
            name: name.filter(|_| !text),
            role,
            destination,
        }
    }
}

/// Why a result carries a full snapshot where a diff was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FullReason {
    NoBaseline,
    DocumentChanged,
    AttachmentChanged,
    RevisionUnknown,
    CoverageChanged,
    DiffLargerThanSnapshot,
}

impl FullReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NoBaseline => "no_baseline",
            Self::DocumentChanged => "document_changed",
            Self::AttachmentChanged => "attachment_changed",
            Self::RevisionUnknown => "revision_unknown",
            Self::CoverageChanged => "coverage_changed",
            Self::DiffLargerThanSnapshot => "diff_larger_than_snapshot",
        }
    }
}

/// One outline line of a view, keyed by its ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewLine {
    pub(crate) key: String,
    pub(crate) line: String,
}

/// One keyed change between two views of the same space. `after` is the key
/// of the line directly above in the new view (`None`: first line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiffOp {
    /// In the old view, not in the new one. `gone`: the ref was retired (its
    /// node left the document or became another entity); otherwise the node
    /// only left the view and the ref still works.
    Leave { key: String, gone: bool },
    /// Same place, different line (value, state, depth).
    Change { key: String, line: String },
    Add {
        key: String,
        after: Option<String>,
        line: String,
    },
    /// In both views, at another place among the lines that stayed.
    Move {
        key: String,
        after: Option<String>,
        line: String,
    },
}

/// Keyed difference between two views. Lines that keep their relative order
/// stay; the rest are moves, so duplicates and reorders need no guessing.
pub(crate) fn diff(old: &[ViewLine], new: &[ViewLine], retired: &HashSet<String>) -> Vec<DiffOp> {
    let old_position: HashMap<&str, usize> = old
        .iter()
        .enumerate()
        .map(|(position, line)| (line.key.as_str(), position))
        .collect();
    let new_keys: HashSet<&str> = new.iter().map(|line| line.key.as_str()).collect();
    let mut ops: Vec<DiffOp> = old
        .iter()
        .filter(|line| !new_keys.contains(line.key.as_str()))
        .map(|line| DiffOp::Leave {
            key: line.key.clone(),
            gone: retired.contains(&line.key),
        })
        .collect();

    // Longest run of kept lines whose old order matches their new order.
    let kept: Vec<(usize, usize)> = new
        .iter()
        .enumerate()
        .filter_map(|(new_index, line)| Some((new_index, *old_position.get(line.key.as_str())?)))
        .collect();
    let mut best = vec![1usize; kept.len()];
    let mut previous = vec![usize::MAX; kept.len()];
    for i in 0..kept.len() {
        for j in 0..i {
            if kept[j].1 < kept[i].1 && best[j] + 1 > best[i] {
                best[i] = best[j] + 1;
                previous[i] = j;
            }
        }
    }
    let mut in_place = HashSet::new();
    let mut cursor = (0..kept.len()).max_by_key(|i| (best[*i], usize::MAX - *i));
    while let Some(i) = cursor {
        in_place.insert(kept[i].0);
        cursor = (previous[i] != usize::MAX).then_some(previous[i]);
    }

    for (index, line) in new.iter().enumerate() {
        let after = index.checked_sub(1).map(|above| new[above].key.clone());
        match old_position.get(line.key.as_str()) {
            None => ops.push(DiffOp::Add {
                key: line.key.clone(),
                after,
                line: line.line.clone(),
            }),
            Some(_) if !in_place.contains(&index) => ops.push(DiffOp::Move {
                key: line.key.clone(),
                after,
                line: line.line.clone(),
            }),
            Some(position) if old[*position].line != line.line => ops.push(DiffOp::Change {
                key: line.key.clone(),
                line: line.line.clone(),
            }),
            Some(_) => {}
        }
    }
    ops
}

/// What one recorded observation tells the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Told {
    /// The whole view. `reason` says why a requested diff was not possible.
    Snapshot { reason: Option<FullReason> },
    Diff {
        base_revision: u64,
        ops: Vec<DiffOp>,
        /// The page address or title differs from the baseline's.
        page_changed: bool,
    },
}

/// Marks where a line's ref goes; the ref is known only once it is assigned.
pub(crate) const REF_SLOT: char = '\u{0}';

/// One node of a view: its capability and its line with [`REF_SLOT`] in
/// place of the ref.
#[derive(Debug, Clone)]
pub(crate) struct ViewNode {
    pub(crate) entry: RefEntry,
    pub(crate) template: String,
}

/// The default view is the one diffs are made of. A query, scope or
/// continuation read is a side view: it shares the refs and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewKind {
    Default,
    Side,
}

#[derive(Debug, Clone)]
pub(crate) struct Recorded {
    pub(crate) space_id: u64,
    /// The baseline revision after this record (unchanged by a side view).
    pub(crate) revision: Option<u64>,
    pub(crate) lines: Vec<ViewLine>,
    pub(crate) told: Told,
}

#[derive(Debug, Clone)]
struct Baseline {
    revision: u64,
    view: Vec<ViewLine>,
    complete: bool,
    /// Page address and title when it was recorded.
    page: (String, String),
    /// The lowest-ranked node the view held when a budget cut it short.
    tail: Option<NodeKey>,
}

/// The refs one session holds for one document on one attachment.
#[derive(Debug, Clone)]
pub(crate) struct RefSpace {
    pub(crate) id: u64,
    pub(crate) identity: DocumentIdentity,
    capabilities: HashMap<u32, RefEntry>,
    by_node: HashMap<NodeKey, u32>,
    next_index: u32,
    baseline: Option<Baseline>,
    /// Refs retired since the baseline was recorded: gone, not just out of view.
    retired: HashSet<String>,
    /// Why this space replaced the one before it.
    began: FullReason,
}

impl RefSpace {
    fn new(id: u64, identity: DocumentIdentity, began: FullReason) -> Self {
        Self {
            id,
            identity,
            capabilities: HashMap::new(),
            by_node: HashMap::new(),
            next_index: 0,
            baseline: None,
            retired: HashSet::new(),
            began,
        }
    }

    pub(crate) fn baseline_revision(&self) -> Option<u64> {
        self.baseline.as_ref().map(|baseline| baseline.revision)
    }

    /// Where the baseline at `revision` was cut by its budget. A read that
    /// will be diffed against it stops at the same node, so a line that grew
    /// or went away does not shift the cut and show up as unrelated lines
    /// leaving or entering.
    pub(crate) fn baseline_tail(&self, revision: u64) -> Option<&NodeKey> {
        self.baseline
            .as_ref()
            .filter(|baseline| baseline.revision == revision)
            .and_then(|baseline| baseline.tail.as_ref())
    }

    fn retire(&mut self, index: u32) {
        if let Some(entry) = self.capabilities.remove(&index) {
            let key = NodeKey::of(&entry);
            if self.by_node.get(&key) == Some(&index) {
                self.by_node.remove(&key);
            }
            self.retired.insert(format_ref(self.id, index));
        }
    }

    /// Retire refs the collected document no longer supports: a node that
    /// reads as another entity, and (only when the document was covered
    /// completely) a node that is not there.
    fn reconcile(&mut self, document: &[RefEntry], complete: bool) {
        let live: HashMap<NodeKey, Fingerprint> = document
            .iter()
            .map(|entry| (NodeKey::of(entry), Fingerprint::of(entry)))
            .collect();
        let stale: Vec<u32> = self
            .capabilities
            .iter()
            .filter(|(_, held)| match live.get(&NodeKey::of(held)) {
                Some(now) => *now != Fingerprint::of(held),
                None => complete,
            })
            .map(|(index, _)| *index)
            .collect();
        for index in stale {
            self.retire(index);
        }
    }

    /// The ref for `entry`'s node: the one it already has when the node still
    /// reads the same, otherwise a new one (the old one is retired).
    fn assign(&mut self, mut entry: RefEntry) -> String {
        entry.attachment = Some(self.identity.attachment);
        let key = NodeKey::of(&entry);
        if let Some(index) = self.by_node.get(&key).copied() {
            if self
                .capabilities
                .get(&index)
                .is_some_and(|held| Fingerprint::of(held) == Fingerprint::of(&entry))
            {
                // Same entity: only what it allows now (actions, visibility) moves.
                self.capabilities.insert(index, entry);
                return format_ref(self.id, index);
            }
            self.retire(index);
        }
        let index = self.next_index;
        self.next_index += 1;
        self.by_node.insert(key, index);
        self.capabilities.insert(index, entry);
        format_ref(self.id, index)
    }

    fn render(&mut self, view: Vec<ViewNode>) -> Vec<ViewLine> {
        view.into_iter()
            .map(|node| {
                let key = self.assign(node.entry);
                let line = node.template.replace(REF_SLOT, &key);
                ViewLine { key, line }
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn advance(
        &mut self,
        view: Vec<ViewLine>,
        complete: bool,
        page: (String, String),
        tail: Option<NodeKey>,
        since: Option<u64>,
        revision: u64,
    ) -> Told {
        let told = match (since, &self.baseline) {
            (None, _) => Told::Snapshot { reason: None },
            (Some(_), None) => Told::Snapshot {
                reason: Some(self.began),
            },
            (Some(since), Some(baseline)) if since != baseline.revision => Told::Snapshot {
                reason: Some(FullReason::RevisionUnknown),
            },
            (Some(_), Some(baseline)) if baseline.complete != complete => Told::Snapshot {
                reason: Some(FullReason::CoverageChanged),
            },
            (Some(_), Some(baseline)) => Told::Diff {
                base_revision: baseline.revision,
                ops: diff(&baseline.view, &view, &self.retired),
                page_changed: baseline.page != page,
            },
        };
        self.retired.clear();
        self.baseline = Some(Baseline {
            revision,
            view,
            complete,
            page,
            tail,
        });
        told
    }
}

/// One session's semantic refs for one tab: at most one live space.
#[derive(Debug, Clone, Default)]
pub(crate) struct TabRefs {
    space: Option<RefSpace>,
    /// Why the last space was dropped, until the next one begins.
    lost: Option<FullReason>,
}

impl TabRefs {
    pub(crate) fn space(&self) -> Option<&RefSpace> {
        self.space.as_ref()
    }

    /// Drop the space: every ref is stale and the baseline is gone.
    pub(crate) fn invalidate(&mut self, reason: FullReason) {
        if self.space.take().is_some() {
            self.lost = Some(reason);
        }
    }

    pub(crate) fn resolve(&self, space_id: u64, index: u32) -> Option<&RefEntry> {
        self.space
            .as_ref()
            .filter(|space| space.id == space_id)
            .and_then(|space| space.capabilities.get(&index))
    }

    /// Retire the ref a use just refused: its node reads as another element
    /// than `issued`, the capability the use resolved. It stays stale even if
    /// the node later reads as before, and the next diff reports its line
    /// gone. The ref is found by its node and retired only while it is still
    /// that capability: an observation that ran meanwhile may already have
    /// retired it and given the node a new ref, which must stay.
    pub(crate) fn retire_refused(&mut self, issued: &RefEntry) {
        let Some(space) = self.space.as_mut() else {
            return;
        };
        let held = space.by_node.get(&NodeKey::of(issued)).copied();
        if let Some(index) = held.filter(|index| {
            space.capabilities.get(index).is_some_and(|held| {
                held.attachment == issued.attachment
                    && Fingerprint::of(held) == Fingerprint::of(issued)
            })
        }) {
            space.retire(index);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_identity_for_test(&mut self, identity: DocumentIdentity) {
        if let Some(space) = self.space.as_mut() {
            space.identity = identity;
        }
    }

    /// The ref this session holds for a node, if it holds one.
    pub(crate) fn ref_of(&self, node: &NodeKey) -> Option<String> {
        let space = self.space.as_ref()?;
        space
            .by_node
            .get(node)
            .map(|index| format_ref(space.id, *index))
    }

    /// The space for `identity`: the current one when it is provably the same
    /// document on the same attachment, otherwise a new one.
    fn enter(
        &mut self,
        identity: DocumentIdentity,
        mint: &mut dyn FnMut() -> u64,
    ) -> &mut RefSpace {
        let kept = self
            .space
            .as_ref()
            .is_some_and(|space| space.identity.proves(&identity));
        if !kept {
            let began = match (self.lost.take(), &self.space) {
                (Some(reason), _) => reason,
                (None, None) => FullReason::NoBaseline,
                (None, Some(old))
                    if old.identity.generation != identity.generation
                        || old.identity.attachment != identity.attachment =>
                {
                    FullReason::AttachmentChanged
                }
                (None, Some(_)) => FullReason::DocumentChanged,
            };
            self.space = Some(RefSpace::new(mint(), identity, began));
        }
        self.space.as_mut().expect("entered above")
    }

    /// Record one fresh observation: `document` is every node collected that
    /// can carry a ref, `view` the lines shown, `page` the address and title,
    /// `tail` the lowest-ranked node shown when a budget cut the view.
    /// `since` asks for the change from that baseline revision (default view
    /// only).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        mint: &mut dyn FnMut() -> u64,
        identity: DocumentIdentity,
        document: &[RefEntry],
        complete: bool,
        view: Vec<ViewNode>,
        page: (&str, &str),
        tail: Option<NodeKey>,
        kind: ViewKind,
        since: Option<u64>,
    ) -> Recorded {
        let space = self.enter(identity, mint);
        space.reconcile(document, complete);
        let lines = space.render(view);
        let told = match kind {
            ViewKind::Side => Told::Snapshot { reason: None },
            ViewKind::Default => {
                let revision = mint();
                let page = (page.0.to_owned(), page.1.to_owned());
                space.advance(lines.clone(), complete, page, tail, since, revision)
            }
        };
        Recorded {
            space_id: space.id,
            revision: space.baseline_revision(),
            lines,
            told,
        }
    }

    /// Give refs to more lines of the document last recorded (a
    /// continuation). `None` when there is no space to extend.
    pub(crate) fn extend(&mut self, view: Vec<ViewNode>) -> Option<Vec<ViewLine>> {
        self.space.as_mut().map(|space| space.render(view))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::store::{BrowserActionKind, FrameKind, FrameRef};

    fn frame(frame_id: &str, loader_id: &str) -> FrameRef {
        FrameRef {
            kind: FrameKind::Main,
            oopif_target_id: None,
            identity: Some(FrameIdentity {
                frame_id: frame_id.into(),
                loader_id: loader_id.into(),
            }),
        }
    }

    fn node_in(frame: FrameRef, backend_node_id: i64, role: &str, name: &str) -> RefEntry {
        RefEntry {
            backend_node_id,
            node_name: role.into(),
            label: Some(name.into()),
            actions: vec![BrowserActionKind::Click],
            visibility: None,
            semantic: true,
            frame,
            destination: None,
            attachment: None,
        }
    }

    /// A node that declares an action.
    fn node(backend_node_id: i64, role: &str, name: &str) -> RefEntry {
        node_in(frame("MAIN", "L1"), backend_node_id, role, name)
    }

    /// A node with no action: text or structure.
    fn content(backend_node_id: i64, role: &str, name: &str) -> RefEntry {
        RefEntry {
            actions: Vec::new(),
            ..node(backend_node_id, role, name)
        }
    }

    fn identity(loader: &str, attachment: u64) -> DocumentIdentity {
        DocumentIdentity {
            generation: 1,
            attachment,
            cdp_target_id: "T".into(),
            root: Some(FrameIdentity {
                frame_id: "MAIN".into(),
                loader_id: loader.into(),
            }),
        }
    }

    /// One session's refs for one tab, observing whole documents: every node
    /// is both in the document and in the view, with `state` on its line.
    struct Session {
        refs: TabRefs,
        next: u64,
        url: &'static str,
    }

    impl Session {
        fn new(first_id: u64) -> Self {
            Self {
                refs: TabRefs::default(),
                next: first_id,
                url: "https://x.test/",
            }
        }

        fn record(
            &mut self,
            identity: DocumentIdentity,
            document: &[RefEntry],
            view: &[(&RefEntry, &str)],
            kind: ViewKind,
            since: Option<u64>,
        ) -> Recorded {
            let next = &mut self.next;
            let mut mint = || {
                *next += 1;
                *next
            };
            let view = view
                .iter()
                .map(|(entry, state)| ViewNode {
                    entry: (*entry).clone(),
                    template: format!(
                        "- {} {:?} [{REF_SLOT}]{state}",
                        entry.node_name,
                        entry.label.as_deref().unwrap_or("")
                    ),
                })
                .collect();
            self.refs.record(
                &mut mint,
                identity,
                document,
                true,
                view,
                (self.url, "Title"),
                None,
                kind,
                since,
            )
        }

        /// A default-view observation that shows the whole document.
        fn observe(
            &mut self,
            identity: DocumentIdentity,
            document: &[RefEntry],
            since: Option<u64>,
        ) -> Recorded {
            let view: Vec<(&RefEntry, &str)> = document.iter().map(|entry| (entry, "")).collect();
            self.record(identity, document, &view, ViewKind::Default, since)
        }

        fn resolves(&self, key: &str) -> bool {
            let (space, index) = crate::browser::store::parse_ref(key).expect("a page ref");
            self.refs.resolve(space, index).is_some()
        }
    }

    fn keys(recorded: &Recorded) -> Vec<String> {
        recorded.lines.iter().map(|line| line.key.clone()).collect()
    }

    fn apply(old: &[ViewLine], ops: &[DiffOp]) -> Vec<ViewLine> {
        let mut lines: Vec<ViewLine> = old.to_vec();
        for op in ops {
            match op {
                DiffOp::Leave { key, .. } | DiffOp::Move { key, .. } => {
                    lines.retain(|line| line.key != *key)
                }
                DiffOp::Change { key, line } => {
                    lines
                        .iter_mut()
                        .find(|held| held.key == *key)
                        .expect("a changed line is in the old view")
                        .line = line.clone();
                }
                DiffOp::Add { .. } => {}
            }
        }
        for op in ops {
            if let DiffOp::Add { key, after, line } | DiffOp::Move { key, after, line } = op {
                let at = match after {
                    None => 0,
                    Some(after) => {
                        lines
                            .iter()
                            .position(|held| held.key == *after)
                            .expect("the line above is already placed")
                            + 1
                    }
                };
                lines.insert(
                    at,
                    ViewLine {
                        key: key.clone(),
                        line: line.clone(),
                    },
                );
            }
        }
        lines
    }

    // ── One test per table row ──────────────────────────────────────────

    #[test]
    fn row_get_browser_state_keeps_refs_of_nodes_seen_before_and_resets_the_baseline() {
        let mut session = Session::new(0);
        let email = node(10, "textbox", "Email");
        let send = node(11, "button", "Send invite");
        let first = session.observe(identity("L1", 1), &[email.clone(), send.clone()], None);
        assert_eq!(first.told, Told::Snapshot { reason: None });

        let list = node(12, "listitem", "ada@x.com (Editor)");
        let second = session.observe(identity("L1", 1), &[email, send, list], None);
        assert_eq!(second.space_id, first.space_id);
        assert_eq!(keys(&second)[..2], keys(&first)[..]);
        assert_eq!(keys(&second).len(), 3);
        assert!(
            second.revision > first.revision,
            "the baseline is this view now"
        );
        // A plain read is always a full snapshot, never a diff.
        assert_eq!(second.told, Told::Snapshot { reason: None });
    }

    #[test]
    fn row_an_action_returns_the_diff_from_the_baseline_and_advances_it() {
        let mut session = Session::new(0);
        let role = node(10, "button", "Role");
        let first = session.observe(identity("L1", 1), &[role.clone()], None);

        // The click opened a list: the observation after it is diffed.
        let option = node(20, "option", "Editor");
        let after = session.record(
            identity("L1", 1),
            &[role.clone(), option.clone()],
            &[(&role, " (expanded)"), (&option, "")],
            ViewKind::Default,
            first.revision,
        );
        let Told::Diff {
            base_revision, ops, ..
        } = &after.told
        else {
            panic!("an action's observation is a diff: {:?}", after.told)
        };
        assert_eq!(Some(*base_revision), first.revision);
        assert_eq!(
            ops,
            &vec![
                DiffOp::Change {
                    key: keys(&first)[0].clone(),
                    line: after.lines[0].line.clone(),
                },
                DiffOp::Add {
                    key: keys(&after)[1].clone(),
                    after: Some(keys(&first)[0].clone()),
                    line: after.lines[1].line.clone(),
                },
            ]
        );
        assert_eq!(apply(&first.lines, ops), after.lines);
        assert!(after.revision > first.revision);
    }

    #[test]
    fn row_a_page_that_mutates_on_its_own_changes_nothing_until_it_is_observed_or_used() {
        let mut session = Session::new(0);
        let alice = RefEntry {
            destination: Some("https://x.test/u/alice".into()),
            ..node(10, "link", "Alice")
        };
        let first = session.observe(identity("L1", 1), &[alice.clone()], None);
        let held = keys(&first)[0].clone();

        // The page reuses the node for Bob. Nothing here hears about it.
        assert!(session.resolves(&held));
        assert_eq!(
            session.refs.space().unwrap().baseline_revision(),
            first.revision
        );

        // At use the live fingerprint is compared with the issued one.
        let (space, index) = crate::browser::store::parse_ref(&held).unwrap();
        let issued = Fingerprint::of(session.refs.resolve(space, index).unwrap());
        let live = Fingerprint {
            role: "link".into(),
            name: Some("Bob".into()),
            destination: Some("https://x.test/u/bob".into()),
        };
        assert_ne!(issued, live, "the use is refused as stale");
        assert_eq!(
            live,
            Fingerprint::read("link".into(), Some("Bob".into()), live.destination.clone())
        );

        // The next observation retires the ref instead of renaming it.
        let bob = RefEntry {
            destination: Some("https://x.test/u/bob".into()),
            ..node(10, "link", "Bob")
        };
        let second = session.observe(identity("L1", 1), &[bob], first.revision);
        assert!(!session.resolves(&held), "never refreshed into validity");
        assert_ne!(keys(&second)[0], held);
        let Told::Diff { ops, .. } = &second.told else {
            panic!("{:?}", second.told)
        };
        assert_eq!(
            ops[0],
            DiffOp::Leave {
                key: held,
                gone: true
            }
        );
    }

    #[test]
    fn row_a_replaced_document_retires_every_ref_and_the_next_result_is_a_full_snapshot() {
        let mut session = Session::new(0);
        let send = node(10, "button", "Send");
        let first = session.observe(identity("L1", 1), &[send.clone()], None);
        let held = keys(&first)[0].clone();

        // Same node id in the next document: not the same node.
        let reloaded = node_in(frame("MAIN", "L2"), 10, "button", "Send");
        let second = session.observe(identity("L2", 1), &[reloaded], first.revision);
        assert_ne!(second.space_id, first.space_id);
        assert!(!session.resolves(&held));
        assert_eq!(
            second.told,
            Told::Snapshot {
                reason: Some(FullReason::DocumentChanged)
            }
        );

        // browser_navigate drops the space before anything is observed.
        session.refs.invalidate(FullReason::DocumentChanged);
        assert!(!session.resolves(&keys(&second)[0]));
        let third = session.observe(identity("L3", 1), &[], second.revision);
        assert_eq!(
            third.told,
            Told::Snapshot {
                reason: Some(FullReason::DocumentChanged)
            }
        );
    }

    #[test]
    fn row_a_child_frame_that_navigates_or_changes_process_loses_only_its_own_refs() {
        let mut session = Session::new(0);
        let main = node(10, "button", "Pay");
        let card = node_in(
            FrameRef {
                kind: FrameKind::Iframe,
                ..frame("CHILD", "C1")
            },
            30,
            "textbox",
            "Card",
        );
        let first = session.observe(identity("L1", 1), &[main.clone(), card.clone()], None);
        let (main_ref, card_ref) = (keys(&first)[0].clone(), keys(&first)[1].clone());

        // The child navigated: same frame id and node id, another loader.
        let navigated = node_in(
            FrameRef {
                kind: FrameKind::Iframe,
                ..frame("CHILD", "C2")
            },
            30,
            "textbox",
            "Card",
        );
        let second = session.observe(
            identity("L1", 1),
            &[main.clone(), navigated.clone()],
            first.revision,
        );
        assert_eq!(second.space_id, first.space_id);
        assert_eq!(keys(&second)[0], main_ref);
        assert_ne!(keys(&second)[1], card_ref);
        assert!(!session.resolves(&card_ref));
        let Told::Diff { ops, .. } = &second.told else {
            panic!("{:?}", second.told)
        };
        assert_eq!(
            ops[0],
            DiffOp::Leave {
                key: card_ref,
                gone: true
            }
        );

        // The frame moved to its own process: another node key again.
        let swapped = node_in(
            FrameRef {
                kind: FrameKind::Oopif,
                oopif_target_id: Some("CHILD_TARGET".into()),
                ..frame("CHILD", "C2")
            },
            30,
            "textbox",
            "Card",
        );
        let third = session.observe(identity("L1", 1), &[main, swapped], second.revision);
        assert_eq!(keys(&third)[0], main_ref);
        assert_ne!(keys(&third)[1], keys(&second)[1]);
        assert!(!session.resolves(&keys(&second)[1]));
    }

    #[test]
    fn row_a_same_document_history_change_keeps_refs_and_baseline() {
        let mut session = Session::new(0);
        let tab = node(10, "tab", "Settings");
        let first = session.observe(identity("L1", 1), &[tab.clone()], None);
        // pushState changes the URL, not the loader: same identity.
        let second = session.record(
            identity("L1", 1),
            &[tab.clone()],
            &[(&tab, " (selected)")],
            ViewKind::Default,
            first.revision,
        );
        assert_eq!(keys(&second), keys(&first));
        assert!(
            matches!(
                second.told,
                Told::Diff {
                    page_changed: false,
                    ..
                }
            ),
            "{:?}",
            second.told
        );
        // The diff says when the address changed, so the agent is told.
        session.url = "https://x.test/settings";
        let third = session.record(
            identity("L1", 1),
            &[tab.clone()],
            &[(&tab, " (selected)")],
            ViewKind::Default,
            second.revision,
        );
        assert!(
            matches!(&third.told, Told::Diff { page_changed: true, ops, .. } if ops.is_empty()),
            "{:?}",
            third.told
        );
    }

    #[test]
    fn row_another_session_on_the_same_tab_shares_no_refs_and_no_baseline() {
        let (mut ours, mut theirs) = (Session::new(0), Session::new(1_000));
        let count = node(10, "status", "count");
        let first = ours.observe(identity("L1", 1), &[count.clone()], None);
        let other = theirs.observe(identity("L1", 2), &[count.clone()], None);
        assert_ne!(keys(&first), keys(&other));
        assert!(
            !ours.resolves(&keys(&other)[0]),
            "a ref is its session's alone"
        );

        // Their action changed the page; our baseline did not move.
        theirs.record(
            identity("L1", 2),
            &[count.clone()],
            &[(&count, " = 1")],
            ViewKind::Default,
            other.revision,
        );
        assert_eq!(
            ours.refs.space().unwrap().baseline_revision(),
            first.revision
        );
        // Our next diff reports their effect as an observed change.
        let ours_next = ours.record(
            identity("L1", 1),
            &[count.clone()],
            &[(&count, " = 1")],
            ViewKind::Default,
            first.revision,
        );
        let Told::Diff { ops, .. } = &ours_next.told else {
            panic!("{:?}", ours_next.told)
        };
        assert!(matches!(ops[..], [DiffOp::Change { .. }]), "{ops:?}");
    }

    #[test]
    fn row_a_rebound_tab_starts_with_no_refs() {
        // Closed, moved and crashed tabs are refused by revalidation before a
        // ref is read (engine tests). A new bind mints a new target with its
        // own empty state; the first observation there has no baseline.
        let mut old_target = Session::new(0);
        let first = old_target.observe(identity("L1", 1), &[node(10, "button", "Send")], None);
        let mut rebound = Session::new(100);
        assert!(!rebound.resolves(&keys(&first)[0]));
        let again = rebound.observe(
            identity("L1", 1),
            &[node(10, "button", "Send")],
            first.revision,
        );
        assert_eq!(
            again.told,
            Told::Snapshot {
                reason: Some(FullReason::NoBaseline)
            }
        );
    }

    #[test]
    fn row_stop_in_the_extension_detaches_and_every_ref_is_stale() {
        let mut session = Session::new(0);
        let send = node(10, "button", "Send");
        let first = session.observe(identity("L1", 1), &[send.clone()], None);
        let held = keys(&first)[0].clone();
        // While Stop holds, attaching is refused and nothing is recorded.
        // Once the user allows Cua again the tab has a new attachment.
        let second = session.observe(identity("L1", 2), &[send], first.revision);
        assert!(!session.resolves(&held));
        assert_eq!(
            second.told,
            Told::Snapshot {
                reason: Some(FullReason::AttachmentChanged)
            }
        );
    }

    #[test]
    fn row_revoked_consent_drops_the_binding_and_its_refs_with_it() {
        // The store drops every target of the revoked endpoint generation
        // (store tests); what is left for a new grant is an empty state.
        let mut session = Session::new(0);
        let first = session.observe(identity("L1", 1), &[node(10, "button", "Send")], None);
        session.refs = TabRefs::default();
        assert!(!session.resolves(&keys(&first)[0]));
        assert!(session.refs.space().is_none(), "no baseline survives");
    }

    #[test]
    fn row_a_debugger_reattach_or_a_reconnect_never_keeps_refs() {
        let send = node(10, "button", "Send");
        // Detach and reattach on a live connection: a new attachment.
        let mut session = Session::new(0);
        let first = session.observe(identity("L1", 7), &[send.clone()], None);
        let reattached = session.observe(identity("L1", 8), &[send.clone()], first.revision);
        assert!(!session.resolves(&keys(&first)[0]));
        assert_eq!(
            reattached.told,
            Told::Snapshot {
                reason: Some(FullReason::AttachmentChanged)
            }
        );
        // Reconnect: a new connection generation, even with the same document.
        let reconnected = DocumentIdentity {
            generation: 2,
            ..identity("L1", 8)
        };
        let after = session.observe(reconnected, &[send], reattached.revision);
        assert!(!session.resolves(&keys(&reattached)[0]));
        assert_eq!(
            after.told,
            Told::Snapshot {
                reason: Some(FullReason::AttachmentChanged)
            }
        );
    }

    #[test]
    fn row_a_session_that_ended_and_revived_starts_from_nothing() {
        let mut session = Session::new(0);
        let first = session.observe(identity("L1", 1), &[node(10, "button", "Send")], None);
        // Session end removes the namespace; the revived one is new.
        let mut revived = Session::new(session.next);
        assert!(!revived.resolves(&keys(&first)[0]));
        let again = revived.observe(
            identity("L1", 1),
            &[node(10, "button", "Send")],
            first.revision,
        );
        assert_eq!(
            again.told,
            Told::Snapshot {
                reason: Some(FullReason::NoBaseline)
            }
        );
    }

    #[test]
    fn row_an_open_javascript_dialog_leaves_refs_and_baseline_as_they_were() {
        let mut session = Session::new(0);
        let delete = node(10, "button", "Delete");
        let first = session.observe(identity("L1", 1), &[delete.clone()], None);
        // The click opened confirm(): the page cannot be observed, so nothing
        // is recorded and the result says `unavailable`.
        assert!(session.resolves(&keys(&first)[0]));
        assert_eq!(
            session.refs.space().unwrap().baseline_revision(),
            first.revision
        );
        // After the dialog is resolved the same baseline still diffs.
        let after = session.observe(identity("L1", 1), &[delete], first.revision);
        assert_eq!(
            after.told,
            Told::Diff {
                base_revision: first.revision.unwrap(),
                ops: Vec::new(),
                page_changed: false,
            }
        );
    }

    #[test]
    fn row_an_observation_racing_an_action_keeps_revisions_ordered() {
        let mut session = Session::new(0);
        let count = node(10, "status", "count");
        let first = session.observe(identity("L1", 1), &[count.clone()], None);
        // The action read the baseline (first), then a plain read landed.
        let read = session.record(
            identity("L1", 1),
            &[count.clone()],
            &[(&count, " = 1")],
            ViewKind::Default,
            None,
        );
        // The action's observation names a baseline that has moved on.
        let late = session.record(
            identity("L1", 1),
            &[count.clone()],
            &[(&count, " = 2")],
            ViewKind::Default,
            first.revision,
        );
        assert_eq!(
            late.told,
            Told::Snapshot {
                reason: Some(FullReason::RevisionUnknown)
            }
        );
        assert!(first.revision < read.revision && read.revision < late.revision);
        // Read in the other order, the action diffs from the read's revision.
        let next = session.record(
            identity("L1", 1),
            &[count.clone()],
            &[(&count, " = 3")],
            ViewKind::Default,
            late.revision,
        );
        assert!(
            matches!(next.told, Told::Diff { base_revision, .. } if Some(base_revision) == late.revision)
        );
    }

    #[test]
    fn row_a_lost_response_is_recoverable_because_every_diff_names_its_base() {
        let mut session = Session::new(0);
        let count = node(10, "status", "count");
        let held = session.observe(identity("L1", 1), &[count.clone()], None);
        // This result never reached the agent; the baseline advanced anyway.
        let lost = session.record(
            identity("L1", 1),
            &[count.clone()],
            &[(&count, " = 1")],
            ViewKind::Default,
            held.revision,
        );
        // The next action diffs from the lost revision, and says so.
        let next = session.record(
            identity("L1", 1),
            &[count.clone()],
            &[(&count, " = 2")],
            ViewKind::Default,
            lost.revision,
        );
        let Told::Diff { base_revision, .. } = next.told else {
            panic!("{:?}", next.told)
        };
        assert_ne!(Some(base_revision), held.revision, "the agent can tell");
        // Its old refs still work, and a plain read gives the whole view.
        assert!(session.resolves(&keys(&held)[0]));
        let full = session.observe(identity("L1", 1), &[count.clone()], None);
        assert_eq!(full.told, Told::Snapshot { reason: None });
        // Naming the revision it does hold gets a full snapshot, not a wrong diff.
        let asked = session.observe(identity("L1", 1), &[count], held.revision);
        assert_eq!(
            asked.told,
            Told::Snapshot {
                reason: Some(FullReason::RevisionUnknown)
            }
        );
    }

    #[test]
    fn row_a_query_scope_or_continuation_read_shares_refs_and_leaves_the_baseline() {
        let mut session = Session::new(0);
        let email = node(10, "textbox", "Email");
        let send = node(11, "button", "Send");
        let document = [email.clone(), send.clone()];
        let first = session.observe(identity("L1", 1), &document, None);

        // A query read of one node: same ref, no revision, never a diff.
        let side = session.record(
            identity("L1", 1),
            &document,
            &[(&send, "")],
            ViewKind::Side,
            first.revision,
        );
        assert_eq!(keys(&side), vec![keys(&first)[1].clone()]);
        assert_eq!(side.revision, first.revision);
        assert_eq!(side.told, Told::Snapshot { reason: None });

        // A continuation adds lines of the recorded document.
        let more = node(12, "link", "Help");
        let extended = session
            .refs
            .extend(vec![ViewNode {
                entry: more,
                template: format!("- link \"Help\" [{REF_SLOT}]"),
            }])
            .unwrap();
        assert!(session.resolves(&extended[0].key));
        assert_eq!(
            session.refs.space().unwrap().baseline_revision(),
            first.revision
        );

        // The next action still diffs from the default view it replaced.
        let after = session.observe(identity("L1", 1), &document, first.revision);
        assert_eq!(
            after.told,
            Told::Diff {
                base_revision: first.revision.unwrap(),
                ops: Vec::new(),
                page_changed: false,
            }
        );

        // dom_refs_v1 replaces the space: the store resets this state.
        session.refs = TabRefs::default();
        assert!(!session.resolves(&keys(&first)[0]));
    }

    // ── Rules the rows rest on ──────────────────────────────────────────

    #[test]
    fn a_document_that_cannot_be_proven_is_never_the_same_document() {
        let mut session = Session::new(0);
        let unproven = DocumentIdentity {
            root: None,
            ..identity("L1", 1)
        };
        let send = node_in(FrameRef::main_unproven(), 10, "button", "Send");
        let first = session.observe(unproven.clone(), &[send.clone()], None);
        let second = session.observe(unproven, &[send], first.revision);
        assert_ne!(second.space_id, first.space_id);
        assert!(!session.resolves(&keys(&first)[0]));
    }

    #[test]
    fn a_node_that_left_the_view_is_not_reported_gone_and_keeps_its_ref() {
        let mut session = Session::new(0);
        let top = node(10, "button", "Top");
        let far = node(11, "link", "Footer");
        let document = [top.clone(), far.clone()];
        let first = session.observe(identity("L1", 1), &document, None);
        let far_ref = keys(&first)[1].clone();

        // Still in the document, no longer in the ranked view.
        let scrolled = session.record(
            identity("L1", 1),
            &document,
            &[(&top, "")],
            ViewKind::Default,
            first.revision,
        );
        let Told::Diff { ops, .. } = &scrolled.told else {
            panic!("{:?}", scrolled.told)
        };
        assert_eq!(
            ops,
            &vec![DiffOp::Leave {
                key: far_ref.clone(),
                gone: false
            }]
        );
        assert!(
            session.resolves(&far_ref),
            "out of view, still a capability"
        );

        // Gone from the document: retired, and said so.
        let removed = session.observe(
            identity("L1", 1),
            &[top.clone(), far.clone()],
            scrolled.revision,
        );
        assert!(
            matches!(&removed.told, Told::Diff { ops, .. } if matches!(ops[..], [DiffOp::Add { .. }]))
        );
        let gone = session.observe(identity("L1", 1), &[top], removed.revision);
        let Told::Diff { ops, .. } = &gone.told else {
            panic!("{:?}", gone.told)
        };
        assert_eq!(
            ops,
            &vec![DiffOp::Leave {
                key: far_ref.clone(),
                gone: true
            }]
        );
        assert!(!session.resolves(&far_ref));
    }

    #[test]
    fn a_partly_covered_document_proves_no_absence() {
        let mut session = Session::new(0);
        let top = node(10, "button", "Top");
        let far = node(11, "link", "Footer");
        let first = session.observe(identity("L1", 1), &[top.clone(), far.clone()], None);
        let far_ref = keys(&first)[1].clone();
        let mut mint = || 50;
        // The collection was truncated: the missing node may still exist.
        session.refs.record(
            &mut mint,
            identity("L1", 1),
            std::slice::from_ref(&top),
            false,
            Vec::new(),
            ("https://x.test/", "Title"),
            None,
            ViewKind::Side,
            None,
        );
        assert!(session.resolves(&far_ref));
    }

    #[test]
    fn a_change_of_coverage_is_a_full_snapshot() {
        let mut session = Session::new(0);
        let top = node(10, "button", "Top");
        let first = session.observe(identity("L1", 1), &[top.clone()], None);
        let mut mint = || 60;
        let partial = session.refs.record(
            &mut mint,
            identity("L1", 1),
            std::slice::from_ref(&top),
            false,
            Vec::new(),
            ("https://x.test/", "Title"),
            None,
            ViewKind::Default,
            first.revision,
        );
        assert_eq!(
            partial.told,
            Told::Snapshot {
                reason: Some(FullReason::CoverageChanged)
            }
        );
    }

    #[test]
    fn text_that_changes_is_a_changed_line_and_a_renamed_element_is_another_ref() {
        let mut session = Session::new(0);
        let count = content(10, "statictext", "count = 1");
        let first = session.observe(identity("L1", 1), &[count], None);
        let held = keys(&first)[0].clone();

        // The same text node says something else: same ref, changed line.
        let count = content(10, "statictext", "count = 2");
        let second = session.observe(identity("L1", 1), &[count], first.revision);
        assert_eq!(keys(&second)[0], held);
        assert!(
            matches!(&second.told, Told::Diff { ops, .. } if matches!(ops[..], [DiffOp::Change { .. }])),
            "{:?}",
            second.told
        );

        // A disabled button keeps its ref when it becomes enabled...
        let disabled = content(20, "button", "Delete Alice");
        let third = session.observe(identity("L1", 1), &[disabled], second.revision);
        let button_ref = keys(&third)[0].clone();
        let enabled = node(20, "button", "Delete Alice");
        let fourth = session.observe(identity("L1", 1), &[enabled], third.revision);
        assert_eq!(keys(&fourth)[0], button_ref);
        // ...and loses it when the node is reused for someone else, enabled
        // or not: its name is what the agent read it by.
        let reused = content(20, "button", "Delete Bob");
        let fifth = session.observe(identity("L1", 1), &[reused], fourth.revision);
        assert_ne!(keys(&fifth)[0], button_ref);
        assert!(!session.resolves(&button_ref));
    }

    #[test]
    fn a_ref_retired_at_use_stays_stale_when_the_node_reads_as_before_again() {
        let mut session = Session::new(0);
        let reply = node(10, "button", "Reply");
        let first = session.observe(identity("L1", 1), &[reply.clone()], None);
        let held = keys(&first)[0].clone();
        assert_eq!(
            session.refs.ref_of(&NodeKey::of(&reply)),
            Some(held.clone())
        );

        // A use found the node reading as "Delete" and retired the ref.
        let (space, index) = crate::browser::store::parse_ref(&held).unwrap();
        let issued = session.refs.resolve(space, index).unwrap().clone();
        session.refs.retire_refused(&issued);
        assert!(!session.resolves(&held));
        assert_eq!(session.refs.ref_of(&NodeKey::of(&reply)), None);

        // The page put the old name back. The node gets a new ref; the old
        // one is reported gone and never resolves again.
        let second = session.observe(identity("L1", 1), &[reply], first.revision);
        assert_ne!(keys(&second)[0], held);
        assert!(!session.resolves(&held));
        let Told::Diff { ops, .. } = &second.told else {
            panic!("{:?}", second.told)
        };
        assert_eq!(
            ops[0],
            DiffOp::Leave {
                key: held,
                gone: true
            }
        );
    }

    #[test]
    fn a_refused_use_never_retires_the_ref_an_observation_gave_the_node_meanwhile() {
        let mut session = Session::new(0);
        let first = session.observe(identity("L1", 1), &[node(10, "button", "Reply")], None);
        let old = keys(&first)[0].clone();
        // The action resolved the old ref...
        let (space, index) = crate::browser::store::parse_ref(&old).unwrap();
        let issued = session.refs.resolve(space, index).unwrap().clone();
        // ...and before its live check finished, an observation saw the node
        // as "Delete", retired the old ref and issued a new one.
        let second = session.observe(identity("L1", 1), &[node(10, "button", "Delete")], None);
        let new = keys(&second)[0].clone();
        assert_ne!(new, old);
        // The action's refusal retires what it resolved, which is gone
        // already: the new ref is not its to take.
        session.refs.retire_refused(&issued);
        assert!(session.resolves(&new));
        assert!(!session.resolves(&old));
    }

    #[test]
    fn a_ref_carries_the_attachment_it_was_issued_on() {
        let mut session = Session::new(0);
        let first = session.observe(identity("L1", 41), &[node(10, "button", "Send")], None);
        let (space, index) = crate::browser::store::parse_ref(&keys(&first)[0]).unwrap();
        assert_eq!(
            session.refs.resolve(space, index).unwrap().attachment,
            Some(41)
        );
    }

    // ── Diff properties ─────────────────────────────────────────────────

    fn line(key: &str, text: &str) -> ViewLine {
        ViewLine {
            key: key.into(),
            line: text.into(),
        }
    }

    #[test]
    fn identical_views_have_an_empty_diff() {
        let view = vec![line("p1:0", "- a"), line("p1:1", "- b")];
        assert!(diff(&view, &view, &HashSet::new()).is_empty());
    }

    #[test]
    fn a_reorder_is_a_move_of_the_fewest_lines() {
        let old = vec![line("a", "A"), line("b", "B"), line("c", "C")];
        let new = vec![line("c", "C"), line("a", "A"), line("b", "B")];
        let ops = diff(&old, &new, &HashSet::new());
        assert_eq!(
            ops,
            vec![DiffOp::Move {
                key: "c".into(),
                after: None,
                line: "C".into()
            }]
        );
        assert_eq!(apply(&old, &ops), new);
    }

    #[test]
    fn duplicate_lines_stay_apart_because_ops_are_keyed() {
        let old = vec![
            line("a", "- text \"Remove\""),
            line("b", "- text \"Remove\""),
        ];
        let new = vec![line("b", "- text \"Remove\"")];
        let ops = diff(&old, &new, &HashSet::from(["a".to_owned()]));
        assert_eq!(
            ops,
            vec![DiffOp::Leave {
                key: "a".into(),
                gone: true
            }]
        );
        assert_eq!(apply(&old, &ops), new);
    }

    #[test]
    fn applying_a_diff_to_the_old_view_gives_the_new_view() {
        // Deterministic pseudo-random views over a small key space, so keeps,
        // moves, adds, leaves and changes all mix.
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        let view = |next: &mut dyn FnMut(u64) -> u64| {
            let mut keys: Vec<u64> = (0..12).filter(|_| next(3) > 0).collect();
            for i in (1..keys.len()).rev() {
                keys.swap(i, next(i as u64 + 1) as usize);
            }
            keys.into_iter()
                .map(|key| line(&format!("p1:{key}"), &format!("- node {key} v{}", next(2))))
                .collect::<Vec<_>>()
        };
        for case in 0..2_000 {
            let old = view(&mut next);
            let new = view(&mut next);
            let ops = diff(&old, &new, &HashSet::new());
            assert_eq!(
                apply(&old, &ops),
                new,
                "case {case}: {old:?} -> {new:?} via {ops:?}"
            );
            if old == new {
                assert!(ops.is_empty(), "case {case}");
            }
        }
    }
}
