//! Experimental v2 renderer for unsatisfiability conflicts.
//!
//! This module is **prototype-quality**. It lives alongside the existing
//! `conflict::DisplayUnsat` renderer for side-by-side comparison and is in
//! its own module so it can be evolved or discarded without touching the
//! existing public surface.
//!
//! Design goals:
//!
//! 1. *Reconstruction-only* — operates on the existing `ConflictGraph`
//!    produced by `Conflict::graph`. No changes to the CDCL pipeline.
//! 2. *Numbered cross-references* — every contradiction gets a `❶/❷/…` label
//!    and second-occurrence references say `(see ❶)` instead of recursing.
//! 3. *One narrative* — locked / excluded / no-candidates / constrains all
//!    appear in the same tree, not as afterthought footers.
//! 4. *A concluding line* — `∴ …` so the reader has somewhere to land.
//! 5. *A hint API* — structured hints (typos, locked-version mismatch,
//!    all-excluded) can be inspected programmatically instead of grepped
//!    out of the rendered text.

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashSet},
    fmt::{self, Display, Formatter, Write as _},
    rc::Rc,
};

use ahash::HashMap;
use itertools::Itertools;
use petgraph::{
    Direction,
    graph::NodeIndex,
    visit::{DfsPostOrder, EdgeRef},
};

use crate::{
    Interner, NameId, Requirement, SolvableId, SolvableOrRootId, StringId, VersionSetId,
    conflict::{ConflictCause, ConflictEdge, ConflictGraph, ConflictNode, MergedConflictNode},
};

// ─── Number labels ───────────────────────────────────────────────────────────

/// Renders a 1-based fact id as a circled number (❶❷❸…), falling back to
/// `(N)` for N > 20.
fn label(id: u32) -> String {
    const CIRCLED: [&str; 20] = [
        "❶", "❷", "❸", "❹", "❺", "❻", "❼", "❽", "❾", "❿", "⓫", "⓬", "⓭", "⓮", "⓯", "⓰", "⓱",
        "⓲", "⓳", "⓴",
    ];
    if id == 0 {
        return String::new();
    }
    let idx = (id as usize) - 1;
    if idx < CIRCLED.len() {
        CIRCLED[idx].to_string()
    } else {
        format!("({id})")
    }
}

// ─── Installability ──────────────────────────────────────────────────────────

/// Compute which nodes correspond to a viable (installable) version.
/// Reimplements `ConflictGraph::get_installable_set` (currently private).
fn installable_set(graph: &ConflictGraph) -> HashSet<NodeIndex> {
    let g = &graph.graph;
    let mut installable: HashSet<NodeIndex> = HashSet::new();

    let mut dfs = DfsPostOrder::new(g, graph.root_node);
    'outer: while let Some(nx) = dfs.next(g) {
        if graph.unresolved_node == Some(nx) {
            continue;
        }
        if g.edges_directed(nx, Direction::Incoming)
            .any(|e| matches!(e.weight(), ConflictEdge::Conflict(ConflictCause::Excluded)))
        {
            continue;
        }
        if g.edges_directed(nx, Direction::Outgoing)
            .any(|e| matches!(e.weight(), ConflictEdge::Conflict(_)))
        {
            continue;
        }
        let by_req = g
            .edges_directed(nx, Direction::Outgoing)
            .filter_map(|e| match e.weight() {
                ConflictEdge::Requires(req) => Some((*req, e.target())),
                _ => None,
            })
            .chunk_by(|(req, _)| *req);
        for (_, mut group) in &by_req {
            if group.all(|(_, target)| !installable.contains(&target)) {
                continue 'outer;
            }
        }
        installable.insert(nx);
    }
    installable
}

// ─── Fact index ──────────────────────────────────────────────────────────────

/// Lazy 1-based numbering of "contradiction" nodes (non-installable solvables).
/// Numbers are assigned on first `get_or_assign` call so they follow the
/// reader's visual encounter order rather than a fixed traversal order.
struct FactIndex {
    eligible: HashSet<NodeIndex>,
    inner: RefCell<FactIndexInner>,
}

struct FactIndexInner {
    by_node: HashMap<NodeIndex, u32>,
    by_id: Vec<NodeIndex>, // position 0 unused, ids are 1-based
}

impl FactIndex {
    /// Build the *eligibility* set up-front (which nodes are eligible to get
    /// a number) but defer numbering itself to first-visit.
    fn new(graph: &ConflictGraph, installable: &HashSet<NodeIndex>) -> Self {
        let g = &graph.graph;
        let mut eligible = HashSet::new();
        for nx in g.node_indices() {
            if nx == graph.root_node {
                continue;
            }
            if !matches!(g[nx], ConflictNode::Solvable(_)) {
                continue;
            }
            if installable.contains(&nx) {
                continue;
            }
            eligible.insert(nx);
        }
        FactIndex {
            eligible,
            inner: RefCell::new(FactIndexInner {
                by_node: HashMap::default(),
                by_id: vec![NodeIndex::end()],
            }),
        }
    }

    /// Get the id of a node if one has already been assigned. Does not assign.
    fn get(&self, nx: NodeIndex) -> Option<u32> {
        self.inner.borrow().by_node.get(&nx).copied()
    }

    /// Get the id of a node, assigning one on first call. Returns None if the
    /// node is not eligible for numbering (root / installable / non-solvable).
    fn get_or_assign(&self, nx: NodeIndex) -> Option<u32> {
        if !self.eligible.contains(&nx) {
            return None;
        }
        let mut inner = self.inner.borrow_mut();
        if let Some(&id) = inner.by_node.get(&nx) {
            return Some(id);
        }
        let next_id = inner.by_id.len() as u32;
        inner.by_id.push(nx);
        inner.by_node.insert(nx, next_id);
        Some(next_id)
    }

    /// Snapshot of currently-assigned (id, node) pairs in id order.
    fn assigned(&self) -> Vec<(u32, NodeIndex)> {
        let inner = self.inner.borrow();
        inner
            .by_id
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, n)| (i as u32, *n))
            .collect()
    }
}

// ─── Indent ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Indent {
    levels: Vec<bool>, // true = last child at that level
}

impl Indent {
    fn new() -> Self {
        Self { levels: Vec::new() }
    }
    fn push(&self, last: bool) -> Self {
        let mut levels = self.levels.clone();
        levels.push(last);
        Self { levels }
    }
    fn render(&self) -> String {
        if self.levels.is_empty() {
            return String::new();
        }
        let mut s = String::new();
        let deepest = self.levels.len() - 1;
        for (i, &is_last) in self.levels.iter().enumerate() {
            if i == deepest {
                s.push_str(if is_last { "└─ " } else { "├─ " });
            } else {
                s.push_str(if is_last { "   " } else { "│  " });
            }
        }
        s
    }
}

// ─── Helpers on Requirement ──────────────────────────────────────────────────

fn requirement_name_id<I: Interner>(req: &Requirement, interner: &I) -> NameId {
    let vs = match *req {
        Requirement::Single(vs) => vs,
        Requirement::Union(union) => interner
            .version_sets_in_union(union)
            .next()
            .expect("union must have at least one version set"),
    };
    interner.version_set_name(vs)
}

fn render_solvable<I: Interner>(
    s: SolvableOrRootId,
    merged: &HashMap<SolvableId, Rc<MergedConflictNode>>,
    interner: &I,
) -> String {
    if let Some(sid) = s.solvable() {
        if let Some(m) = merged.get(&sid) {
            return interner.display_merged_solvables(&m.ids).to_string();
        }
        return interner.display_merged_solvables(&[sid]).to_string();
    }
    "<root>".to_string()
}

// ─── Tree renderer ───────────────────────────────────────────────────────────

/// Pubgrub-style tree renderer with numbered back-references and a concluding
/// sentence.
pub struct DisplayUnsatTree<'g, 'i, I: Interner> {
    /// The conflict graph being rendered.
    pub graph: &'g ConflictGraph,
    /// The interner used to resolve solvable / version-set / name strings.
    pub interner: &'i I,
}

impl<'g, 'i, I: Interner> DisplayUnsatTree<'g, 'i, I> {
    /// Construct a tree renderer for a conflict graph.
    pub fn new(graph: &'g ConflictGraph, interner: &'i I) -> Self {
        Self { graph, interner }
    }
}

impl<I: Interner> Display for DisplayUnsatTree<'_, '_, I> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let graph = self.graph;
        let interner = self.interner;
        let installable = installable_set(graph);
        let merged = graph.simplify(interner);
        let facts = FactIndex::new(graph, &installable);

        let g = &graph.graph;

        writeln!(f, "× Cannot resolve requirements:")?;

        // Partition root edges.
        let mut top_requires: Vec<(Requirement, Vec<NodeIndex>)> = Vec::new();
        let mut top_locked: Vec<SolvableId> = Vec::new();
        let mut top_constrains: Vec<VersionSetId> = Vec::new();

        for e in g.edges(graph.root_node) {
            match e.weight() {
                ConflictEdge::Requires(req) => {
                    if let Some(slot) = top_requires.iter_mut().find(|(r, _)| r == req) {
                        slot.1.push(e.target());
                    } else {
                        top_requires.push((*req, vec![e.target()]));
                    }
                }
                ConflictEdge::Conflict(ConflictCause::Locked(locked)) => top_locked.push(*locked),
                ConflictEdge::Conflict(ConflictCause::Constrains(vs)) => top_constrains.push(*vs),
                ConflictEdge::Conflict(_) => {}
            }
        }
        top_requires.sort_by_key(|(_, tgts)| {
            tgts.iter().any(|t| installable.contains(t)) as i32
        });

        let mut seen_facts: HashSet<u32> = HashSet::new();
        let total_top = top_requires.len() + top_locked.len() + top_constrains.len();

        let mut emitted = 0usize;
        for (req, targets) in &top_requires {
            emitted += 1;
            let indent = Indent::new().push(emitted == total_top);
            self.render_requirement(
                f,
                req,
                targets,
                &installable,
                &merged,
                &facts,
                &mut seen_facts,
                indent,
            )?;
        }
        for locked in &top_locked {
            emitted += 1;
            let indent = Indent::new().push(emitted == total_top);
            writeln!(
                f,
                "{}{} is locked, but a different version is required.",
                indent.render(),
                interner.display_merged_solvables(&[*locked]),
            )?;
        }
        for vs in &top_constrains {
            emitted += 1;
            let indent = Indent::new().push(emitted == total_top);
            writeln!(
                f,
                "{}the constraint {} {} cannot be fulfilled.",
                indent.render(),
                interner.display_name(interner.version_set_name(*vs)),
                interner.display_version_set(*vs),
            )?;
        }

        // Concluding line.
        let failing_names: BTreeSet<String> = top_requires
            .iter()
            .filter(|(_, tgts)| !tgts.iter().any(|t| installable.contains(t)))
            .map(|(req, _)| {
                let nid = requirement_name_id(req, interner);
                interner.display_name(nid).to_string()
            })
            .collect();
        if !failing_names.is_empty() {
            writeln!(
                f,
                "∴ no solution exists for the requested {}.",
                if failing_names.len() == 1 { "package" } else { "packages" },
            )?;
        }

        Ok(())
    }
}

impl<I: Interner> DisplayUnsatTree<'_, '_, I> {
    #[allow(clippy::too_many_arguments)]
    fn render_requirement(
        &self,
        f: &mut Formatter<'_>,
        req: &Requirement,
        targets: &[NodeIndex],
        installable: &HashSet<NodeIndex>,
        merged: &HashMap<SolvableId, Rc<MergedConflictNode>>,
        facts: &FactIndex,
        seen_facts: &mut HashSet<u32>,
        indent: Indent,
    ) -> fmt::Result {
        let interner = self.interner;
        let g = &self.graph.graph;

        let is_missing =
            targets.len() == 1 && self.graph.unresolved_node == Some(targets[0]);
        let req_text = req.display(interner).to_string();

        if is_missing {
            writeln!(
                f,
                "{}{} — no candidates were found",
                indent.render(),
                req_text
            )?;
            return Ok(());
        }

        let any_installable = targets.iter().any(|t| installable.contains(t));
        if any_installable {
            writeln!(
                f,
                "{}{} — satisfied by one of:",
                indent.render(),
                req_text
            )?;
        } else {
            writeln!(f, "{}{} — no version works:", indent.render(), req_text)?;
        }

        // Dedup via merged-class to avoid showing identical sibling versions.
        let mut already_emitted: HashSet<SolvableId> = HashSet::new();
        let mut display_targets: Vec<NodeIndex> = Vec::new();
        for &t in targets {
            if any_installable && !installable.contains(&t) {
                continue;
            }
            if let ConflictNode::Solvable(sor) = g[t] {
                if let Some(sid) = sor.solvable() {
                    if let Some(m) = merged.get(&sid) {
                        if m.ids.iter().any(|id| already_emitted.contains(id)) {
                            continue;
                        }
                        already_emitted.extend(m.ids.iter().copied());
                    } else if !already_emitted.insert(sid) {
                        continue;
                    }
                }
            }
            display_targets.push(t);
        }

        let last_idx = display_targets.len().saturating_sub(1);
        for (i, t) in display_targets.iter().enumerate() {
            let is_last = i == last_idx;
            let child_indent = indent.push(is_last);
            self.render_candidate(
                f,
                *t,
                installable,
                merged,
                facts,
                seen_facts,
                child_indent,
            )?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_candidate(
        &self,
        f: &mut Formatter<'_>,
        nx: NodeIndex,
        installable: &HashSet<NodeIndex>,
        merged: &HashMap<SolvableId, Rc<MergedConflictNode>>,
        facts: &FactIndex,
        seen_facts: &mut HashSet<u32>,
        indent: Indent,
    ) -> fmt::Result {
        let g = &self.graph.graph;
        let interner = self.interner;

        let s_or_root = match g[nx] {
            ConflictNode::Solvable(s) => s,
            ConflictNode::UnresolvedDependency => {
                writeln!(f, "{}(no candidates)", indent.render())?;
                return Ok(());
            }
            ConflictNode::Excluded(_) => {
                return Ok(());
            }
        };

        let version_str = render_solvable(s_or_root, merged, interner);

        // If this node has already been numbered, it's a repeat → emit a
        // back-reference and stop. Otherwise, assign a number on first
        // encounter so numbers grow in render order.
        if let Some(prior) = facts.get(nx) {
            if !seen_facts.insert(prior) {
                writeln!(
                    f,
                    "{}{} (see {})",
                    indent.render(),
                    version_str,
                    label(prior)
                )?;
                return Ok(());
            }
        }
        let fact_id = facts.get_or_assign(nx);
        if let Some(id) = fact_id {
            seen_facts.insert(id);
        }
        let fact_label = fact_id
            .map(|id| format!(" {}", label(id)))
            .unwrap_or_default();

        let excluded_reason =
            g.edges_directed(nx, Direction::Outgoing)
                .find_map(|e| match e.weight() {
                    ConflictEdge::Conflict(ConflictCause::Excluded) => match g[e.target()] {
                        ConflictNode::Excluded(s) => Some(s),
                        _ => None,
                    },
                    _ => None,
                });
        let forbid_multi_target = g.edges_directed(nx, Direction::Outgoing).find_map(|e| {
            matches!(
                e.weight(),
                ConflictEdge::Conflict(ConflictCause::ForbidMultipleInstances)
            )
            .then(|| e.target())
        });
        let constrains: Vec<VersionSetId> = g
            .edges_directed(nx, Direction::Outgoing)
            .filter_map(|e| match e.weight() {
                ConflictEdge::Conflict(ConflictCause::Constrains(vs)) => Some(*vs),
                _ => None,
            })
            .collect();
        let requirements: Vec<(Requirement, Vec<NodeIndex>)> = {
            let mut out: Vec<(Requirement, Vec<NodeIndex>)> = Vec::new();
            for e in g.edges_directed(nx, Direction::Outgoing) {
                if let ConflictEdge::Requires(r) = e.weight() {
                    if let Some(slot) = out.iter_mut().find(|(r0, _)| r0 == r) {
                        slot.1.push(e.target());
                    } else {
                        out.push((*r, vec![e.target()]));
                    }
                }
            }
            out
        };

        if let Some(reason) = excluded_reason {
            writeln!(
                f,
                "{}{} is excluded: {}{}",
                indent.render(),
                version_str,
                interner.display_string(reason),
                fact_label,
            )?;
        } else if let Some(sib_target) = forbid_multi_target {
            let sib_fact = facts.get(sib_target);
            match sib_fact {
                Some(id) => writeln!(
                    f,
                    "{}{} conflicts with {}{}",
                    indent.render(),
                    version_str,
                    label(id),
                    fact_label,
                )?,
                None => writeln!(
                    f,
                    "{}{} conflicts with another already-required version{}",
                    indent.render(),
                    version_str,
                    fact_label,
                )?,
            }
        } else if !constrains.is_empty() {
            writeln!(
                f,
                "{}{} constrains:{}",
                indent.render(),
                version_str,
                fact_label,
            )?;
            let last_idx = constrains.len() - 1;
            for (i, vs) in constrains.iter().enumerate() {
                let is_last = i == last_idx;
                let sub = indent.push(is_last);
                writeln!(
                    f,
                    "{}{} {} — conflicts with an installable version above",
                    sub.render(),
                    interner.display_name(interner.version_set_name(*vs)),
                    interner.display_version_set(*vs),
                )?;
            }
        } else if requirements.is_empty() {
            writeln!(f, "{}{}{}", indent.render(), version_str, fact_label)?;
        } else {
            writeln!(
                f,
                "{}{} requires:{}",
                indent.render(),
                version_str,
                fact_label,
            )?;
            let mut reqs = requirements;
            reqs.sort_by_key(|(_, tgts)| {
                tgts.iter().any(|t| installable.contains(t)) as i32
            });
            let last_idx = reqs.len() - 1;
            for (i, (req, tgts)) in reqs.iter().enumerate() {
                let is_last = i == last_idx;
                let sub = indent.push(is_last);
                self.render_requirement(
                    f,
                    req,
                    tgts,
                    installable,
                    merged,
                    facts,
                    seen_facts,
                    sub,
                )?;
            }
        }

        Ok(())
    }
}

// ─── Prose renderer ──────────────────────────────────────────────────────────

/// Pubgrub-style flowing prose: one sentence per non-installable solvable
/// emitted in fact-id order, chained with "And because…" and ending with
/// `∴ …`.
pub struct DisplayUnsatProse<'g, 'i, I: Interner> {
    /// The conflict graph being rendered.
    pub graph: &'g ConflictGraph,
    /// The interner used to resolve solvable / version-set / name strings.
    pub interner: &'i I,
}

impl<'g, 'i, I: Interner> DisplayUnsatProse<'g, 'i, I> {
    /// Construct a prose renderer for a conflict graph.
    pub fn new(graph: &'g ConflictGraph, interner: &'i I) -> Self {
        Self { graph, interner }
    }
}

impl<I: Interner> Display for DisplayUnsatProse<'_, '_, I> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let graph = self.graph;
        let interner = self.interner;
        let installable = installable_set(graph);
        let merged = graph.simplify(interner);
        let facts = FactIndex::new(graph, &installable);

        let g = &graph.graph;

        // Pre-assign numbers in post-order DFS so leaves get the smallest
        // numbers and the prose narrative reads bottom-up. Skip merge-class
        // duplicates so a single merged group ("foo 1 | 2 | 3 | 4 | 5")
        // gets one fact rather than one per merged version.
        let mut merge_seen: HashSet<SolvableId> = HashSet::default();
        let mut dfs = DfsPostOrder::new(g, graph.root_node);
        while let Some(nx) = dfs.next(g) {
            if let ConflictNode::Solvable(s) = g[nx] {
                if let Some(sid) = s.solvable() {
                    if let Some(mclass) = merged.get(&sid) {
                        if mclass.ids.iter().any(|id| merge_seen.contains(id)) {
                            continue;
                        }
                        merge_seen.extend(mclass.ids.iter().copied());
                    } else if !merge_seen.insert(sid) {
                        continue;
                    }
                }
            }
            facts.get_or_assign(nx);
        }

        writeln!(f, "× No solution found:")?;

        let assigned = facts.assigned();
        let mut first = true;
        for (id, nx) in &assigned {
            let id = *id;
            let nx = *nx;
            let s_or_root = match g[nx] {
                ConflictNode::Solvable(s) => s,
                _ => continue,
            };
            let version_str = render_solvable(s_or_root, &merged, interner);

            let excluded_reason =
                g.edges_directed(nx, Direction::Outgoing)
                    .find_map(|e| match e.weight() {
                        ConflictEdge::Conflict(ConflictCause::Excluded) => match g[e.target()] {
                            ConflictNode::Excluded(s) => Some(s),
                            _ => None,
                        },
                        _ => None,
                    });
            let forbid_multi = g.edges_directed(nx, Direction::Outgoing).any(|e| {
                matches!(
                    e.weight(),
                    ConflictEdge::Conflict(ConflictCause::ForbidMultipleInstances)
                )
            });

            let connector = if first { "  Because" } else { "  And because" };
            first = false;

            if let Some(reason) = excluded_reason {
                writeln!(
                    f,
                    "{} {} is excluded ({}), {} cannot be installed. {}",
                    connector,
                    version_str,
                    interner.display_string(reason),
                    version_str,
                    label(id),
                )?;
                continue;
            }
            if forbid_multi {
                writeln!(
                    f,
                    "{} {} conflicts with another required version of the same package, {} cannot be installed. {}",
                    connector, version_str, version_str, label(id),
                )?;
                continue;
            }

            let reqs: Vec<(Requirement, Vec<NodeIndex>)> = {
                let mut out: Vec<(Requirement, Vec<NodeIndex>)> = Vec::new();
                for e in g.edges_directed(nx, Direction::Outgoing) {
                    if let ConflictEdge::Requires(r) = e.weight() {
                        if let Some(slot) = out.iter_mut().find(|(r0, _)| r0 == r) {
                            slot.1.push(e.target());
                        } else {
                            out.push((*r, vec![e.target()]));
                        }
                    }
                }
                out
            };

            let mut clauses: Vec<String> = Vec::new();
            for (req, tgts) in &reqs {
                if tgts.iter().any(|t| installable.contains(t)) {
                    continue;
                }
                if tgts.len() == 1 && self.graph.unresolved_node == Some(tgts[0]) {
                    let mut s = String::new();
                    let _ = write!(
                        s,
                        "{} depends on {} which has no candidates",
                        version_str,
                        req.display(interner)
                    );
                    clauses.push(s);
                } else {
                    let refs: Vec<String> = tgts
                        .iter()
                        .filter_map(|t| facts.get(*t))
                        .map(label)
                        .collect();
                    let mut s = String::new();
                    if refs.is_empty() {
                        let _ = write!(
                            s,
                            "{} depends on {}, which has no satisfiable version",
                            version_str,
                            req.display(interner)
                        );
                    } else {
                        let _ = write!(
                            s,
                            "{} depends on {}, ruled out by {}",
                            version_str,
                            req.display(interner),
                            refs.join(", ")
                        );
                    }
                    clauses.push(s);
                }
            }

            if clauses.is_empty() {
                writeln!(
                    f,
                    "{} {} cannot be installed. {}",
                    connector,
                    version_str,
                    label(id)
                )?;
            } else {
                writeln!(
                    f,
                    "{} {}, {} cannot be installed. {}",
                    connector,
                    clauses.join(" and "),
                    version_str,
                    label(id),
                )?;
            }
        }

        // Concluding line. Separate requirement-failures from locked-package
        // conflicts so the sentence remains grammatical even when only one
        // kind is present.
        let mut failing_reqs: Vec<String> = Vec::new();
        let mut locked: Vec<String> = Vec::new();
        for e in g.edges(graph.root_node) {
            match e.weight() {
                ConflictEdge::Requires(req) => {
                    if !installable.contains(&e.target()) {
                        failing_reqs.push(req.display(interner).to_string());
                    }
                }
                ConflictEdge::Conflict(ConflictCause::Locked(s)) => {
                    locked.push(interner.display_merged_solvables(&[*s]).to_string());
                }
                _ => {}
            }
        }
        let failing_reqs: Vec<String> = failing_reqs.into_iter().unique().collect();
        let locked: Vec<String> = locked.into_iter().unique().collect();
        if !failing_reqs.is_empty() && locked.is_empty() {
            writeln!(
                f,
                "  ∴ {} cannot be satisfied.",
                failing_reqs.join(" and "),
            )?;
        } else if failing_reqs.is_empty() && !locked.is_empty() {
            writeln!(
                f,
                "  ∴ {} is locked at an incompatible version.",
                locked.join(" and "),
            )?;
        } else if !failing_reqs.is_empty() && !locked.is_empty() {
            writeln!(
                f,
                "  ∴ {} cannot be satisfied while {} remains locked.",
                failing_reqs.join(" and "),
                locked.join(" and "),
            )?;
        }

        Ok(())
    }
}

// ─── Narrative renderer ──────────────────────────────────────────────────────

/// Hybrid renderer combining a banner ("the conflict involves …"), numbered
/// multi-line fact paragraphs that inline forbid-multi as "is required by
/// root", grouping of sibling versions ("menu has only versions {10, 15}: …"),
/// and a single concluding `∴` sentence pointing at the failing top-level
/// dependency.
pub struct DisplayUnsatNarrative<'g, 'i, I: Interner> {
    /// The conflict graph being rendered.
    pub graph: &'g ConflictGraph,
    /// The interner used to resolve solvable / version-set / name strings.
    pub interner: &'i I,
}

impl<'g, 'i, I: Interner> DisplayUnsatNarrative<'g, 'i, I> {
    /// Construct a narrative renderer for a conflict graph.
    pub fn new(graph: &'g ConflictGraph, interner: &'i I) -> Self {
        Self { graph, interner }
    }
}

/// One pre-rendered cause line inside a fact.
#[derive(Clone)]
enum NarrativeCause {
    /// "X depends on Y" — optionally points at another fact that explains
    /// why Y can't be satisfied.
    Depends {
        text: String,
        ruled_out_by: Option<u32>,
    },
    /// "Y is required by root" — used to inline a forbid-multi conclusion.
    RootRequires(String),
    /// Raw text — escape hatch.
    Raw(String),
}

/// One body shape a fact paragraph can take.
enum NarrativeFactBody {
    /// `Because <cause1>, and <cause2>, <subject> cannot be installed.`
    Simple {
        subject: String,
        causes: Vec<NarrativeCause>,
    },
    /// `<name> has only versions {a, b, c}: … so <name> cannot be installed.`
    Group {
        name: String,
        members: Vec<(String, NarrativeCause)>,
        subject: String,
    },
    /// `<subject> is locked at version <v>.` — single-line terminal fact.
    LockedFact { subject: String, locked: String },
}

struct NarrativeFact {
    id: u32,
    body: NarrativeFactBody,
}

impl<I: Interner> Display for DisplayUnsatNarrative<'_, '_, I> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let graph = self.graph;
        let interner = self.interner;
        let installable = installable_set(graph);
        let merged = graph.simplify(interner);
        let g = &graph.graph;

        // ── Build phase ────────────────────────────────────────────────────
        let mut builder = NarrativeBuilder {
            graph,
            interner,
            installable: &installable,
            merged: &merged,
            facts: Vec::new(),
            assigned: HashMap::default(),
            group_assigned: HashMap::default(),
        };

        // Top-level: a failing root requirement either groups its non-installable
        // targets or, if there's only one, recurses into it.
        let mut root_failing_subjects: Vec<String> = Vec::new();
        for e in g.edges(graph.root_node) {
            let ConflictEdge::Requires(req) = e.weight() else {
                continue;
            };
            // Collect non-installable targets for this requirement.
            let targets: Vec<NodeIndex> = g
                .edges(graph.root_node)
                .filter(|e2| matches!(e2.weight(), ConflictEdge::Requires(r) if r == req))
                .map(|e2| e2.target())
                .collect();
            if targets.iter().all(|t| installable.contains(t)) {
                continue;
            }
            // Only process this requirement once.
            if !targets.iter().any(|t| !installable.contains(t)) {
                continue;
            }
            let req_text = req.display(interner).to_string();
            // Deduplicate per requirement-text since we iterate over individual edges.
            if root_failing_subjects.contains(&req_text) {
                continue;
            }
            let name = interner.display_name(requirement_name_id(req, interner)).to_string();
            root_failing_subjects.push(req_text.clone());
            let _ = builder.build_from_root_requirement(req, &targets, &name);
        }

        // Locked at root — try to build a chain-based explanation that
        // narrates the transitive dependency path leading into the locked
        // package, falling back to a terminal `LockedFact` when no chain
        // can be found. Dedup by solvable id so the same lock doesn't
        // produce multiple facts.
        let mut locked_subjects: Vec<String> = Vec::new();
        let mut locked_seen: HashSet<SolvableId> = HashSet::new();
        for e in g.edges(graph.root_node) {
            if let ConflictEdge::Conflict(ConflictCause::Locked(s)) = e.weight() {
                let s = *s;
                if !locked_seen.insert(s) {
                    continue;
                }
                let name = interner
                    .display_name(interner.solvable_name(s))
                    .to_string();
                let version_str = interner.display_merged_solvables(&[s]).to_string();
                if let Some(root_req) = builder.try_build_locked_chain(s) {
                    let req_text = root_req.display(interner).to_string();
                    if !root_failing_subjects.contains(&req_text) {
                        root_failing_subjects.push(req_text);
                    }
                } else {
                    let id = builder.next_id();
                    builder.facts.push(NarrativeFact {
                        id,
                        body: NarrativeFactBody::LockedFact {
                            subject: name.clone(),
                            locked: version_str.clone(),
                        },
                    });
                    locked_subjects.push(format!("{name} is locked at {version_str}"));
                }
            }
        }

        // Constraints-at-root — root has a Conflict(Constrains(_)) edge that
        // isn't expressible as a normal "requires" chain. Emit one terminal
        // fact per unique constraint. We deliberately don't push these
        // into `root_failing_subjects` (the fact already reads as a
        // complete sentence and the ∴ line would duplicate it), but we
        // collect them into `constraint_subjects` so the concluding line
        // can mention them when there are no other failure causes.
        let mut constraint_subjects: Vec<String> = Vec::new();
        let mut constrains_seen: HashSet<VersionSetId> = HashSet::new();
        for e in g.edges(graph.root_node) {
            if let ConflictEdge::Conflict(ConflictCause::Constrains(vs)) = e.weight() {
                let vs = *vs;
                if !constrains_seen.insert(vs) {
                    continue;
                }
                let name = interner
                    .display_name(interner.version_set_name(vs))
                    .to_string();
                let vset = interner.display_version_set(vs).to_string();
                let id = builder.next_id();
                builder.facts.push(NarrativeFact {
                    id,
                    body: NarrativeFactBody::Simple {
                        subject: format!("the constraint {name} {vset}"),
                        causes: vec![NarrativeCause::Raw(format!(
                            "The constraint {name} {vset} cannot be fulfilled"
                        ))],
                    },
                });
                constraint_subjects.push(format!("the constraint {name} {vset}"));
            }
        }

        // ── Header ─────────────────────────────────────────────────────────
        // List the top-level requirements. We deliberately don't say "root";
        // it's a solver-internal concept that leaks if exposed.
        let mut root_reqs: Vec<String> = Vec::new();
        for e in g.edges(graph.root_node) {
            if let ConflictEdge::Requires(req) = e.weight() {
                let text = req.display(interner).to_string();
                if !root_reqs.contains(&text) {
                    root_reqs.push(text);
                }
            }
        }
        if root_reqs.is_empty() {
            writeln!(f, "× No solution.")?;
        } else {
            writeln!(f, "× No solution found for: {}.", root_reqs.join(", "))?;
        }

        // Names involved.
        let mut involved_names: BTreeSet<String> = BTreeSet::new();
        for nx in g.node_indices() {
            if let ConflictNode::Solvable(s) = g[nx] {
                if let Some(sid) = s.solvable() {
                    involved_names
                        .insert(interner.display_name(interner.solvable_name(sid)).to_string());
                }
            }
        }
        if involved_names.len() > 1 {
            writeln!(
                f,
                "  The conflict involves {}.",
                involved_names.iter().cloned().collect::<Vec<_>>().join(", ")
            )?;
        }
        writeln!(f)?;

        // ── Render facts ───────────────────────────────────────────────────
        for fact in &builder.facts {
            render_narrative_fact(f, fact)?;
        }

        // ── Conclusion ─────────────────────────────────────────────────────
        if !root_failing_subjects.is_empty() && !constraint_subjects.is_empty() {
            writeln!(
                f,
                "  ∴ {} cannot be satisfied alongside {}.",
                root_failing_subjects.join(" and "),
                constraint_subjects.join(" and "),
            )?;
        } else if !root_failing_subjects.is_empty() {
            writeln!(
                f,
                "  ∴ {} cannot be satisfied.",
                root_failing_subjects.join(" and ")
            )?;
        } else if !constraint_subjects.is_empty() {
            // Header already lists the root requirements; mention only that
            // the constraints block them.
            writeln!(
                f,
                "  ∴ no solution exists given {}.",
                constraint_subjects.join(" and "),
            )?;
        } else if !locked_subjects.is_empty() {
            writeln!(
                f,
                "  ∴ {} (incompatible with the other requirements).",
                locked_subjects.join(" and ")
            )?;
        }

        Ok(())
    }
}

struct NarrativeBuilder<'a, I: Interner> {
    graph: &'a ConflictGraph,
    interner: &'a I,
    installable: &'a HashSet<NodeIndex>,
    merged: &'a HashMap<SolvableId, Rc<MergedConflictNode>>,
    facts: Vec<NarrativeFact>,
    /// Node → fact id, for facts we've already built.
    assigned: HashMap<NodeIndex, u32>,
    /// `(parent, requirement)` → fact id, for group facts.
    group_assigned: HashMap<(NodeIndex, Requirement), u32>,
}

impl<'a, I: Interner> NarrativeBuilder<'a, I> {
    fn next_id(&self) -> u32 {
        (self.facts.len() as u32) + 1
    }

    /// Deduplicate a list of solvable nodes by merge-class so a group of
    /// versions like `foo {1, 2, 3, 4, 5}` that share the same predecessors
    /// and successors renders as a single member instead of five identical
    /// ones.
    fn dedup_by_merge_class(&self, members: &[NodeIndex]) -> Vec<NodeIndex> {
        let g = &self.graph.graph;
        let mut seen: HashSet<SolvableId> = HashSet::new();
        let mut out: Vec<NodeIndex> = Vec::new();
        for &m in members {
            if let ConflictNode::Solvable(s) = g[m] {
                if let Some(sid) = s.solvable() {
                    if let Some(mclass) = self.merged.get(&sid) {
                        if mclass.ids.iter().any(|id| seen.contains(id)) {
                            continue;
                        }
                        seen.extend(mclass.ids.iter().copied());
                    } else if !seen.insert(sid) {
                        continue;
                    }
                }
            }
            out.push(m);
        }
        out
    }

    /// Returns true if `nx` is a "pure forbid-multi" node — its only reason
    /// for being non-installable is that another version of the same package
    /// is required. Such nodes get inlined into their parent's narrative
    /// rather than getting their own fact.
    fn is_inlinable_forbid(&self, nx: NodeIndex) -> Option<NodeIndex> {
        let g = &self.graph.graph;
        if self.installable.contains(&nx) {
            return None;
        }
        let mut has_other_conflict = false;
        let mut has_requires = false;
        let mut forbid_target: Option<NodeIndex> = None;
        for e in g.edges_directed(nx, Direction::Outgoing) {
            match e.weight() {
                ConflictEdge::Requires(_) => has_requires = true,
                ConflictEdge::Conflict(ConflictCause::ForbidMultipleInstances) => {
                    forbid_target = Some(e.target());
                }
                ConflictEdge::Conflict(_) => has_other_conflict = true,
            }
        }
        if has_other_conflict || has_requires {
            return None;
        }
        forbid_target
    }

    /// Given a node that is the forbid-multi target (i.e. the version that
    /// *is* required), find a Requires edge that pinned it. Prefer a root
    /// edge, then fall back to any other incoming Requires. The returned
    /// source lets the caller phrase the inlined cause appropriately —
    /// "is required" for root, "X also depends on Y" for a self-conflict,
    /// "X depends on Y" for a sibling.
    fn find_pinning(&self, forbid_target: NodeIndex) -> Option<(NodeIndex, Requirement)> {
        let g = &self.graph.graph;
        // Prefer root edges.
        for e in g.edges_directed(forbid_target, Direction::Incoming) {
            if e.source() == self.graph.root_node {
                if let ConflictEdge::Requires(req) = e.weight() {
                    return Some((e.source(), *req));
                }
            }
        }
        // Fall back to any other Requires edge.
        for e in g.edges_directed(forbid_target, Direction::Incoming) {
            if let ConflictEdge::Requires(req) = e.weight() {
                return Some((e.source(), *req));
            }
        }
        None
    }

    /// Render a pinning result as a `NarrativeCause`, with phrasing chosen
    /// to fit the relationship between `current_node` (the fact's subject)
    /// and the pinning source:
    /// - source is root → "is required"
    /// - source IS current_node → "X also depends on Y" (self-conflict)
    /// - source is a different solvable → "X depends on Y"
    fn cause_for_pinning(
        &self,
        pin: Option<(NodeIndex, Requirement)>,
        current_node: NodeIndex,
        current_subject: &str,
    ) -> NarrativeCause {
        match pin {
            Some((source, pin_req)) if source == self.graph.root_node => {
                NarrativeCause::RootRequires(pin_req.display(self.interner).to_string())
            }
            Some((source, pin_req)) if source == current_node => NarrativeCause::Raw(format!(
                "{} also depends on {}",
                current_subject,
                pin_req.display(self.interner)
            )),
            Some((source, pin_req)) => {
                let source_str = self.render_node(source);
                NarrativeCause::Raw(format!(
                    "{} depends on {}",
                    source_str,
                    pin_req.display(self.interner)
                ))
            }
            None => NarrativeCause::Raw(
                "another version of the required package is selected".to_string(),
            ),
        }
    }

    /// Render a candidate node's display text (with merged-versions handling).
    fn render_node(&self, nx: NodeIndex) -> String {
        let g = &self.graph.graph;
        match g[nx] {
            ConflictNode::Solvable(s) => render_solvable(s, self.merged, self.interner),
            ConflictNode::UnresolvedDependency => "(no candidates)".to_string(),
            ConflictNode::Excluded(_) => String::new(),
        }
    }

    /// Build a fact for `nx` (or return its existing id). Returns the fact id
    /// the caller should reference. Returns `None` if `nx` is itself
    /// inlinable (caller should inline rather than reference).
    fn build_for_node(&mut self, nx: NodeIndex) -> Option<u32> {
        if let Some(&id) = self.assigned.get(&nx) {
            return Some(id);
        }
        if self.is_inlinable_forbid(nx).is_some() {
            return None;
        }

        let g = &self.graph.graph;
        let subject = self.render_node(nx);

        // Inspect outgoing edges.
        let mut excluded_reason: Option<String> = None;
        let mut constrains: Vec<VersionSetId> = Vec::new();
        let mut requirements: Vec<(Requirement, Vec<NodeIndex>)> = Vec::new();
        for e in g.edges_directed(nx, Direction::Outgoing) {
            match e.weight() {
                ConflictEdge::Conflict(ConflictCause::Excluded) => {
                    if let ConflictNode::Excluded(s) = g[e.target()] {
                        excluded_reason =
                            Some(self.interner.display_string(s).to_string());
                    }
                }
                ConflictEdge::Conflict(ConflictCause::Constrains(vs)) => {
                    constrains.push(*vs);
                }
                ConflictEdge::Requires(r) => {
                    if let Some(slot) = requirements.iter_mut().find(|(r0, _)| r0 == r) {
                        slot.1.push(e.target());
                    } else {
                        requirements.push((*r, vec![e.target()]));
                    }
                }
                ConflictEdge::Conflict(_) => {}
            }
        }

        // Compute causes for each unsatisfied requirement.
        let mut causes: Vec<NarrativeCause> = Vec::new();

        for (req, targets) in &requirements {
            // If at least one target is installable, this dep is fine — skip.
            if targets.iter().any(|t| self.installable.contains(t)) {
                continue;
            }
            let req_text = req.display(self.interner).to_string();
            let non_installable: Vec<NodeIndex> = targets
                .iter()
                .copied()
                .filter(|t| !self.installable.contains(t))
                .collect();

            // Case A: all non-installable targets are pure-forbid-multi → inline.
            let inlinable_all: Vec<NodeIndex> = non_installable
                .iter()
                .filter_map(|&t| self.is_inlinable_forbid(t).map(|_| t))
                .collect();
            if inlinable_all.len() == non_installable.len() && !non_installable.is_empty() {
                // First cause: "X depends on R"
                causes.push(NarrativeCause::Depends {
                    text: format!("{} depends on {}", subject, req_text),
                    ruled_out_by: None,
                });
                // Second cause: trace back from the forbidden version to
                // find what required the conflicting version. Pinning may
                // be root, the current node itself (self-conflict), or
                // some other ancestor.
                let pin = inlinable_all
                    .iter()
                    .find_map(|&t| {
                        self.is_inlinable_forbid(t)
                            .and_then(|fb_target| self.find_pinning(fb_target))
                    });
                causes.push(self.cause_for_pinning(pin, nx, &subject));
                continue;
            }

            // Case B: multiple non-installable targets of the same name → group.
            let same_name = non_installable.iter().all(|&t| match g[t] {
                ConflictNode::Solvable(s) => match (s.solvable(), non_installable.first().and_then(|&first| {
                    if let ConflictNode::Solvable(s0) = g[first] { s0.solvable() } else { None }
                })) {
                    (Some(a), Some(b)) => {
                        self.interner.solvable_name(a) == self.interner.solvable_name(b)
                    }
                    _ => false,
                },
                _ => false,
            });
            if non_installable.len() > 1 && same_name {
                // Build (or reuse) a group fact for this (parent, requirement).
                let group_key = (nx, *req);
                let group_id = if let Some(&id) = self.group_assigned.get(&group_key) {
                    id
                } else {
                    self.build_group(nx, *req, &non_installable)
                };
                causes.push(NarrativeCause::Depends {
                    text: format!("{} depends on {}", subject, req_text),
                    ruled_out_by: Some(group_id),
                });
                continue;
            }

            // Case C: no candidates (unresolved sink) — check *before* the
            // generic recurse so we don't try to build a fact for the sink.
            if non_installable.len() == 1
                && self.graph.unresolved_node == Some(non_installable[0])
            {
                causes.push(NarrativeCause::Raw(format!(
                    "{} depends on {}, which has no candidates",
                    subject, req_text
                )));
                continue;
            }

            // Case D: a single non-installable target — recurse and reference.
            if non_installable.len() == 1 {
                let target = non_installable[0];
                let target_id = self.build_for_node(target);
                let suffix_id = target_id.unwrap_or(0);
                if suffix_id != 0 {
                    causes.push(NarrativeCause::Depends {
                        text: format!("{} depends on {}", subject, req_text),
                        ruled_out_by: Some(suffix_id),
                    });
                } else {
                    // Target was inlinable but we got Case-A handling first;
                    // this shouldn't reach here under the current logic.
                    causes.push(NarrativeCause::Depends {
                        text: format!("{} depends on {}", subject, req_text),
                        ruled_out_by: None,
                    });
                }
                continue;
            }
        }

        // Excluded? Inline the subject so the resulting sentence reads
        // "{subject} is excluded ({reason})" rather than "{which is excluded
        // ({reason})}" — the Excluded variant doesn't know its subject.
        if let Some(reason) = excluded_reason {
            causes.push(NarrativeCause::Raw(format!(
                "{subject} is excluded ({reason})"
            )));
        }

        // Constrains?
        for vs in &constrains {
            let name = self
                .interner
                .display_name(self.interner.version_set_name(*vs))
                .to_string();
            let vset = self.interner.display_version_set(*vs).to_string();
            causes.push(NarrativeCause::Raw(format!(
                "{} constrains {} {}, which conflicts with an installable version",
                subject, name, vset
            )));
        }

        // If we have no causes at all, this node's reason isn't captured by
        // the current rules — emit a fallback so the user still sees something.
        if causes.is_empty() {
            causes.push(NarrativeCause::Raw(format!("{} cannot be selected", subject)));
        }

        let id = self.next_id();
        self.assigned.insert(nx, id);
        self.facts.push(NarrativeFact {
            id,
            body: NarrativeFactBody::Simple {
                subject: subject.clone(),
                causes,
            },
        });
        Some(id)
    }

    /// Build a group fact for `parent → requirement → {member1, member2, ...}`.
    /// Members are processed first so their facts get *lower* ids than the
    /// group's, which makes the rendered output read leaf-first.
    fn build_group(
        &mut self,
        parent: NodeIndex,
        req: Requirement,
        members: &[NodeIndex],
    ) -> u32 {
        if let Some(&id) = self.group_assigned.get(&(parent, req)) {
            return id;
        }

        // Apply merge-class dedup so {foo 1, foo 2, foo 3, foo 4, foo 5} that
        // were collapsed by `ConflictGraph::simplify` render as one bullet.
        let members_owned: Vec<NodeIndex> = self.dedup_by_merge_class(members);
        let members = &members_owned;

        let g = &self.graph.graph;

        // The group's display name — common name of all members.
        let name = members
            .first()
            .and_then(|&t| match g[t] {
                ConflictNode::Solvable(s) => s.solvable(),
                _ => None,
            })
            .map(|s| self.interner.display_name(self.interner.solvable_name(s)).to_string())
            .unwrap_or_else(|| "<unknown>".to_string());

        // Build member lines, recursing into each member's cause *first* so
        // child fact ids are assigned before the group's own id.
        let mut member_lines: Vec<(String, NarrativeCause)> = Vec::new();
        for &m in members {
            let member_full = self.render_node(m);
            // Strip the leading name so the bullet reads "10 depends on …"
            // instead of "menu 10 depends on …" inside a group keyed by menu.
            let member_short = strip_name_prefix(&member_full, &name);

            // Check first whether this member is excluded — exclusion is a
            // terminal reason and trumps requirement-derived reasons.
            let excluded_reason =
                g.edges_directed(m, Direction::Outgoing)
                    .find_map(|e| match e.weight() {
                        ConflictEdge::Conflict(ConflictCause::Excluded) => match g[e.target()] {
                            ConflictNode::Excluded(s) => Some(s),
                            _ => None,
                        },
                        _ => None,
                    });

            // Find this member's outgoing requirement(s).
            let mut maybe_req: Option<(Requirement, Vec<NodeIndex>)> = None;
            for e in g.edges_directed(m, Direction::Outgoing) {
                if let ConflictEdge::Requires(r) = e.weight() {
                    let entry = maybe_req.get_or_insert((*r, Vec::new()));
                    entry.1.push(e.target());
                }
            }

            let cause = if let Some(reason) = excluded_reason {
                NarrativeCause::Raw(format!(
                    "{} is excluded ({})",
                    member_full,
                    self.interner.display_string(reason)
                ))
            } else if let Some((mreq, mtargets)) = maybe_req {
                let req_text = mreq.display(self.interner).to_string();
                let non_inst: Vec<NodeIndex> = mtargets
                    .iter()
                    .copied()
                    .filter(|t| !self.installable.contains(t))
                    .collect();
                // No-candidates target?
                if non_inst.len() == 1 && self.graph.unresolved_node == Some(non_inst[0]) {
                    NarrativeCause::Raw(format!(
                        "{} depends on {}, which has no candidates",
                        member_full, req_text
                    ))
                } else if non_inst.len() == 1 {
                    if let Some(fb) = self.is_inlinable_forbid(non_inst[0]) {
                        let pin = self.find_pinning(fb);
                        let pin_phrase = match pin {
                            Some((source, pin_req))
                                if source == self.graph.root_node =>
                            {
                                format!(
                                    " and {} is required",
                                    pin_req.display(self.interner)
                                )
                            }
                            Some((source, pin_req)) if source == m => format!(
                                " and {} also depends on {}",
                                member_full,
                                pin_req.display(self.interner)
                            ),
                            Some((source, pin_req)) => {
                                let source_str = self.render_node(source);
                                format!(
                                    " and {} depends on {}",
                                    source_str,
                                    pin_req.display(self.interner)
                                )
                            }
                            None => String::new(),
                        };
                        NarrativeCause::Depends {
                            text: format!(
                                "{} depends on {}{}",
                                member_full, req_text, pin_phrase
                            ),
                            ruled_out_by: None,
                        }
                    } else {
                        let child_id = self.build_for_node(non_inst[0]);
                        NarrativeCause::Depends {
                            text: format!("{} depends on {}", member_full, req_text),
                            ruled_out_by: child_id,
                        }
                    }
                } else if !non_inst.is_empty() {
                    let child_id = self.build_for_node(non_inst[0]);
                    NarrativeCause::Depends {
                        text: format!("{} depends on {}", member_full, req_text),
                        ruled_out_by: child_id,
                    }
                } else {
                    NarrativeCause::Depends {
                        text: format!("{} depends on {}", member_full, req_text),
                        ruled_out_by: None,
                    }
                }
            } else {
                NarrativeCause::Raw(format!("{} cannot be selected", member_full))
            };
            member_lines.push((member_short, cause));
        }

        // Now allocate the group's id and push it *after* children.
        let id = self.next_id();
        self.group_assigned.insert((parent, req), id);
        self.facts.push(NarrativeFact {
            id,
            body: NarrativeFactBody::Group {
                name: name.clone(),
                members: member_lines,
                subject: name,
            },
        });
        id
    }

    /// Entry point for a root-level requirement: returns the fact id of the
    /// outermost fact describing why this requirement fails.
    fn build_from_root_requirement(
        &mut self,
        req: &Requirement,
        targets: &[NodeIndex],
        name: &str,
    ) -> Option<u32> {
        let g = &self.graph.graph;
        let non_inst: Vec<NodeIndex> = targets
            .iter()
            .copied()
            .filter(|t| !self.installable.contains(t))
            .collect();

        if non_inst.is_empty() {
            return None;
        }

        // Apply merge-class dedup so we don't recurse into multiple aliased
        // versions of the same package.
        let non_inst = self.dedup_by_merge_class(&non_inst);

        // No-candidates case: the only target is the unresolved sink.
        if non_inst.len() == 1 && self.graph.unresolved_node == Some(non_inst[0]) {
            let id = self.next_id();
            self.facts.push(NarrativeFact {
                id,
                body: NarrativeFactBody::Simple {
                    subject: req.display(self.interner).to_string(),
                    causes: vec![NarrativeCause::Raw(format!(
                        "No candidates were found for {}",
                        req.display(self.interner)
                    ))],
                },
            });
            return Some(id);
        }

        // If there are multiple non-installable targets and they share a
        // name, build a top-level group fact.
        let same_name = non_inst.iter().all(|&t| match g[t] {
            ConflictNode::Solvable(s) => match (s.solvable(), non_inst.first().and_then(|&first| {
                if let ConflictNode::Solvable(s0) = g[first] { s0.solvable() } else { None }
            })) {
                (Some(a), Some(b)) => {
                    self.interner.solvable_name(a) == self.interner.solvable_name(b)
                }
                _ => false,
            },
            _ => false,
        });

        if non_inst.len() > 1 && same_name {
            // If every member is a pure-forbid-multi node, the group fact
            // would just list "X 2.0 cannot be selected", "X 2.1 cannot
            // be selected", … which is uninformative — the actual cause
            // is already captured by some sibling fact (e.g. another
            // root requirement that pulls in the conflicting version).
            // Suppress the group; the concluding ∴ line still lists this
            // requirement.
            let all_pure_forbid = non_inst
                .iter()
                .all(|&t| self.is_inlinable_forbid(t).is_some());
            if all_pure_forbid {
                return None;
            }
            let id = self.build_group(self.graph.root_node, *req, &non_inst);
            return Some(id);
        }

        // Single non-installable target (or all different names) → recurse.
        if non_inst.len() == 1 {
            // Same suppression for single-target pure-forbid-multi cases:
            // the cause is covered elsewhere by inlined "and X is required".
            if self.is_inlinable_forbid(non_inst[0]).is_some() {
                return None;
            }
            return self.build_for_node(non_inst[0]);
        }

        // Mixed names — recurse into each individually.
        let mut last: Option<u32> = None;
        for t in non_inst {
            if let Some(id) = self.build_for_node(t) {
                last = Some(id);
            }
        }
        let _ = name; // currently unused; reserved for richer group framing
        last
    }

    /// Walk Requires edges from `start` looking for a requirement whose
    /// version-set name equals `target_name`. Returns the path of nodes
    /// traversed (excluding the final candidate node) and the matching
    /// requirement at the endpoint. This lets us narrate transitive chains
    /// to a package that's external to the chain (e.g. a locked package
    /// reached only through installable intermediate nodes).
    fn find_chain_to_name(
        &self,
        start: NodeIndex,
        target_name: NameId,
    ) -> Option<(Vec<NodeIndex>, Requirement)> {
        let g = &self.graph.graph;
        let mut visited: HashSet<NodeIndex> = HashSet::new();
        let mut stack: Vec<(NodeIndex, Vec<NodeIndex>)> = vec![(start, vec![start])];
        while let Some((nx, path)) = stack.pop() {
            if !visited.insert(nx) {
                continue;
            }
            for e in g.edges_directed(nx, Direction::Outgoing) {
                if let ConflictEdge::Requires(r) = e.weight() {
                    let req_name = requirement_name_id(r, self.interner);
                    if req_name == target_name {
                        return Some((path.clone(), *r));
                    }
                    let target = e.target();
                    let mut new_path = path.clone();
                    new_path.push(target);
                    stack.push((target, new_path));
                }
            }
        }
        None
    }

    /// For a locked solvable at root, attempt to build an explanation
    /// covering one of two shapes:
    ///
    /// 1. **Direct conflict** — root has a Requires(R) where R's name equals
    ///    the locked package's name (e.g. root requires `bors >=2` and bors
    ///    is locked at 1.0). The fact reads "R is required, but X is locked
    ///    at version V".
    /// 2. **Transitive chain** — root requires some other package whose
    ///    dependency chain leads through Requires edges to the locked name
    ///    (e.g. root requires `asdf`, asdf depends on `c >=2`, c is locked
    ///    at 1). The fact reads "Because A depends on B, ... and c is locked
    ///    at V, A cannot be installed."
    ///
    /// Returns the root-level `Requirement` that initiated the chain so the
    /// caller can list it in the failing-subjects conclusion. Falls back to
    /// `None` (caller emits a terminal `LockedFact`) only when neither shape
    /// applies.
    fn try_build_locked_chain(&mut self, locked: SolvableId) -> Option<Requirement> {
        let g = &self.graph.graph;
        let locked_name = self.interner.solvable_name(locked);
        let locked_name_str = self.interner.display_name(locked_name).to_string();
        let locked_full = self.interner.display_merged_solvables(&[locked]).to_string();
        let version_only = strip_name_prefix(&locked_full, &locked_name_str);

        // Case A: direct conflict — root requires the same package name.
        for e in g.edges(self.graph.root_node) {
            if let ConflictEdge::Requires(root_req) = e.weight() {
                let req_name = requirement_name_id(root_req, self.interner);
                if req_name == locked_name {
                    let req_text = root_req.display(self.interner).to_string();
                    let id = self.next_id();
                    // Phrasing: avoid starting the sentence with the
                    // (lowercase) package name. The "But " inversion
                    // reads naturally after the ❶ marker.
                    self.facts.push(NarrativeFact {
                        id,
                        body: NarrativeFactBody::Simple {
                            subject: req_text.clone(),
                            causes: vec![NarrativeCause::Raw(format!(
                                "{locked_name_str} is locked at version {version_only}, but {req_text} is required"
                            ))],
                        },
                    });
                    return Some(*root_req);
                }
            }
        }

        // Case B: transitive chain.
        let mut chain_info: Option<(Vec<NodeIndex>, Requirement, Requirement)> = None;
        for e in g.edges(self.graph.root_node) {
            let ConflictEdge::Requires(root_req) = e.weight() else {
                continue;
            };
            let target = e.target();
            if !self.installable.contains(&target) {
                continue;
            }
            if let Some((path, endpoint)) = self.find_chain_to_name(target, locked_name) {
                chain_info = Some((path, endpoint, *root_req));
                break;
            }
        }

        let (chain, endpoint_req, root_req) = chain_info?;
        let subject = self.render_node(chain[0]);
        let mut causes: Vec<NarrativeCause> = Vec::new();

        for window in chain.windows(2) {
            let from = self.render_node(window[0]);
            let req = g
                .edges_directed(window[0], Direction::Outgoing)
                .find_map(|e| match e.weight() {
                    ConflictEdge::Requires(r) if e.target() == window[1] => Some(*r),
                    _ => None,
                })?;
            causes.push(NarrativeCause::Depends {
                text: format!("{} depends on {}", from, req.display(self.interner)),
                ruled_out_by: None,
            });
        }
        let last_node = self.render_node(*chain.last()?);
        causes.push(NarrativeCause::Depends {
            text: format!(
                "{} depends on {}",
                last_node,
                endpoint_req.display(self.interner)
            ),
            ruled_out_by: None,
        });
        causes.push(NarrativeCause::Raw(format!(
            "{locked_name_str} is locked at version {version_only}",
        )));

        let id = self.next_id();
        self.facts.push(NarrativeFact {
            id,
            body: NarrativeFactBody::Simple { subject, causes },
        });
        let _ = id;
        Some(root_req)
    }
}

fn render_narrative_fact(f: &mut Formatter<'_>, fact: &NarrativeFact) -> fmt::Result {
    let l = label(fact.id);
    match &fact.body {
        NarrativeFactBody::Simple { subject, causes } => match causes.len() {
            0 => {
                writeln!(f, "  {l} {subject} cannot be installed.")?;
            }
            1 => {
                // If the single cause is a self-contained statement
                // (NarrativeCause::Raw), just print it directly. The
                // "Because X, Y cannot be installed." wrapper reads
                // awkwardly when X already includes both the subject and
                // the verb (e.g. "issue_717 2.1 is excluded (...)"). The
                // cause text is responsible for its own case — we don't
                // auto-capitalize because that would mangle lowercase
                // package names.
                if matches!(&causes[0], NarrativeCause::Raw(_)) {
                    writeln!(f, "  {l} {}.", render_cause(&causes[0]))?;
                } else {
                    writeln!(
                        f,
                        "  {l} Because {}, {} cannot be installed.",
                        render_cause(&causes[0]),
                        subject
                    )?;
                }
            }
            _ => {
                writeln!(f, "  {l} Because {},", render_cause(&causes[0]))?;
                for c in causes.iter().skip(1) {
                    writeln!(f, "     and {},", render_cause(c))?;
                }
                writeln!(f, "     {} cannot be installed.", subject)?;
            }
        },
        NarrativeFactBody::Group {
            name,
            members,
            subject,
        } => {
            let version_set: Vec<String> = members.iter().map(|(s, _)| s.clone()).collect();
            writeln!(
                f,
                "  {l} {name} has only versions {{{}}}:",
                version_set.join(", ")
            )?;
            for (_, c) in members {
                writeln!(f, "        - {}", render_cause(c))?;
            }
            writeln!(f, "     so {} cannot be installed.", subject)?;
        }
        NarrativeFactBody::LockedFact { subject, locked } => {
            writeln!(f, "  {l} {subject} is locked at version {locked}.")?;
        }
    }
    Ok(())
}

fn strip_name_prefix(full: &str, name: &str) -> String {
    if let Some(rest) = full.strip_prefix(name) {
        rest.trim_start().to_string()
    } else {
        full.to_string()
    }
}

fn render_cause(c: &NarrativeCause) -> String {
    match c {
        NarrativeCause::Depends { text, ruled_out_by } => match ruled_out_by {
            Some(id) => format!("{text}, ruled out by {}", label(*id)),
            None => text.clone(),
        },
        NarrativeCause::RootRequires(s) => format!("{s} is required"),
        NarrativeCause::Raw(s) => s.clone(),
    }
}

// ─── Hints API ───────────────────────────────────────────────────────────────

/// A structured, programmatically-inspectable hint about why a conflict
/// occurred. Hints are advice *on top of* the rendered explanation — they let
/// callers offer "did you mean X?" or "did you forget to update the lockfile?"
/// without parsing the rendered text.
#[derive(Debug, Clone)]
pub enum ConflictHint {
    /// A required package name has no candidates at all.
    NoCandidatesFor {
        /// The name with no candidates.
        name: NameId,
        /// The version set that was requested.
        requirement: Requirement,
        /// A similar known name, if a suggestion was provided.
        suggestion: Option<NameId>,
    },

    /// A locked solvable's version is incompatible with what's required.
    LockedVersionMismatch {
        /// The solvable that is locked.
        locked: SolvableId,
    },

    /// Every candidate of a package was excluded.
    AllCandidatesExcluded {
        /// The name whose candidates were all excluded.
        name: NameId,
        /// One exclusion reason per excluded candidate.
        reasons: Vec<StringId>,
    },
}

/// Caller-supplied hook for hint sources that need external information
/// (today, only similar-name lookups). Default returns no suggestions, so
/// implementing this is optional.
pub trait HintContext {
    /// Optionally return a known name that is "close" to the given one.
    fn similar_name(&self, _name: NameId) -> Option<NameId> {
        None
    }
}

/// A no-op hint context — useful for callers that don't want to feed any
/// name-similarity information.
pub struct NoHintContext;
impl HintContext for NoHintContext {}

/// Walk the conflict graph and return all structural hints found.
pub fn compute_hints(
    graph: &ConflictGraph,
    interner: &impl Interner,
    ctx: &impl HintContext,
) -> Vec<ConflictHint> {
    let g = &graph.graph;
    let mut hints: Vec<ConflictHint> = Vec::new();

    // 1. Locked mismatches. Dedup by SolvableId — a graph can have the
    //    same locked clause referenced from multiple root requires.
    {
        let mut seen: HashSet<SolvableId> = HashSet::default();
        for e in g.edges(graph.root_node) {
            if let ConflictEdge::Conflict(ConflictCause::Locked(locked)) = e.weight() {
                if seen.insert(*locked) {
                    hints.push(ConflictHint::LockedVersionMismatch { locked: *locked });
                }
            }
        }
    }

    // 2. No-candidates hints: edges into the unresolved sink. Dedup by name
    //    so a single missing `bar` doesn't surface 5× when 5 versions of
    //    `foo` each require it.
    if let Some(unresolved) = graph.unresolved_node {
        let mut seen_names: HashSet<NameId> = HashSet::default();
        for e in g.edges_directed(unresolved, Direction::Incoming) {
            if let ConflictEdge::Requires(req) = e.weight() {
                let name = requirement_name_id(req, interner);
                if !seen_names.insert(name) {
                    continue;
                }
                let suggestion = ctx.similar_name(name);
                hints.push(ConflictHint::NoCandidatesFor {
                    name,
                    requirement: *req,
                    suggestion,
                });
            }
        }
    }

    // 3. All-excluded hints.
    let mut by_name: HashMap<NameId, Vec<(NodeIndex, Option<StringId>)>> = HashMap::default();
    for nx in g.node_indices() {
        if nx == graph.root_node {
            continue;
        }
        let s = match g[nx] {
            ConflictNode::Solvable(s) => s,
            _ => continue,
        };
        let Some(sid) = s.solvable() else { continue };
        let name = interner.solvable_name(sid);

        let excluded_reason =
            g.edges_directed(nx, Direction::Outgoing)
                .find_map(|e| match e.weight() {
                    ConflictEdge::Conflict(ConflictCause::Excluded) => match g[e.target()] {
                        ConflictNode::Excluded(s) => Some(s),
                        _ => None,
                    },
                    _ => None,
                });
        by_name.entry(name).or_default().push((nx, excluded_reason));
    }
    for (name, entries) in by_name {
        if !entries.is_empty() && entries.iter().all(|(_, r)| r.is_some()) {
            let reasons: Vec<StringId> =
                entries.iter().filter_map(|(_, r)| *r).collect();
            hints.push(ConflictHint::AllCandidatesExcluded { name, reasons });
        }
    }

    hints
}
