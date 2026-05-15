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
        // numbers and the prose narrative reads bottom-up.
        let mut dfs = DfsPostOrder::new(g, graph.root_node);
        while let Some(nx) = dfs.next(g) {
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
                "  ∴ root requires {}; this cannot be satisfied.",
                failing_reqs.join(" and "),
            )?;
        } else if failing_reqs.is_empty() && !locked.is_empty() {
            writeln!(
                f,
                "  ∴ {} is locked at a version that is incompatible with what the solve requires.",
                locked.join(" and "),
            )?;
        } else if !failing_reqs.is_empty() && !locked.is_empty() {
            writeln!(
                f,
                "  ∴ root requires {} and {} is locked; these cannot both be satisfied.",
                failing_reqs.join(" and "),
                locked.join(" and "),
            )?;
        }

        Ok(())
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

    // 1. Locked mismatches.
    for e in g.edges(graph.root_node) {
        if let ConflictEdge::Conflict(ConflictCause::Locked(locked)) = e.weight() {
            hints.push(ConflictHint::LockedVersionMismatch { locked: *locked });
        }
    }

    // 2. No-candidates hints: edges into the unresolved sink.
    if let Some(unresolved) = graph.unresolved_node {
        for e in g.edges_directed(unresolved, Direction::Incoming) {
            if let ConflictEdge::Requires(req) = e.weight() {
                let name = requirement_name_id(req, interner);
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
