# Outlier forensics: problems 265 and 212

Status: investigated 2026-06-11/12 on the `research-universal-combined`
head (49ab6387), linux-64 corpus (`bench-universal/snapshot-linux-64.json`,
model `model-linux-64.json`), release build with `--features diagnostics`.
All numbers below come from per-cell instrumentation (one trace line per
recorded cell: elapsed, trail depth, cell literal count, per-pinning-rule
literal counts, cumulative conflicts) plus a cell-statistics dump in the
`solve-snapshot` tool (`--cells-dump`: distinct solvable sets, per-axis
literal usage, per-cell conditions). Wall times in this note carry heavy
noise: three sibling benchmark processes ran concurrently for most of the
campaign window, so conclusions rest on the conflict and decision
counters, not on seconds.

Related work happening in parallel (referenced, not duplicated here):
refutation ordering, assertion-scan watermarks, and CDCL
restarts/clause-deletion are being researched by separate efforts; both
cases below are inputs to the restarts/deletion and refutation work.

## Problem 265 (fragmentation explosion, 708 cells)

Requirements: x-transformers, libgbm, arviz-bokeh, r-cvms, larrayenv,
rocr-runtime, analisi, foundry-compute-modules, rich >=13.6.0,
napari-splinedist. Concrete solve: 1.4 s. Universal at campaign time:
OK in 183 s, 708 cells, 808 distinct records.

### Headline: the cost is one transition, not 708 cells

Instrumented run (272 s end to end under contention, identical 708 cells,
verifier-clean):

| phase | time | conflicts |
| --- | --- | --- |
| trail-reuse attempt, 550 cells | 5.8 s | 6,631 |
| prefix-budget abort + fallback re-enumeration of those 550 cells | 24 s | 4,746 |
| transition cell 549 -> 550 (first old-cuda cell) | 206 s | 56,920 |
| remaining 157 old-cuda/cuda-absent cells | ~27 s | ~4,500 |
| final witness search (coverage proof) | 0.03 s | - |

704 of 707 transitions are mechanical: 33 s total, mean 47 ms, a few
conflicts each. Three transitions are walls, and one of them is 80% of
the entire solve:

- cell 549 -> 550: the first cell with `not (__cuda >= 12.0)`. 56,920
  conflicts. Cells 0..549 cover the cuda >= 12 and cuda-absent regions;
  cell 550 is the first time the solver enters the cuda [11, 12) corner,
  where it must refute the entire modern GPU stack (pytorch
  cuda130/cuda12x builds, libtorch, the cuda-* runtime cone) before
  discovering that the solution is the cpu_generic pytorch stack. The
  recorded solution for cell 550 swaps ~36 records of group 0 (the whole
  cuda 13 torch stack plus openmpi) for 6 cpu/mpich records.
- cell 648 -> 649 (11.8 s, 1,528 conflicts) and cell 698 -> 699 (6.7 s,
  1,324 conflicts): the same effect for the cuda-absent and
  cuda [11, 12) corners of the all-microarch-negative slice.
- cell 279 -> 280 (0.9 s, 3,725 conflicts): entry into the cuda-absent
  region of the piledriver slice.

This is the "intrinsically hard corner" outlier class of the benchmark
report (problem 34's class), not a new one. The fragmentation itself is
cheap: at ~47 ms per mechanical cell, 708 cells cost ~33 s of the 272 s
(and proportionally less without the diagnostics build overhead).

### The partition is mostly genuine, not condition-fragmentation

Cell statistics over the final partition:

- 708 cells, **563 distinct solvable sets** (80%). The theoretical
  minimum partition size is therefore 563 if arbitrary region shapes
  were allowed.
- Merging same-set cells whose conditions differ in the sign of exactly
  one literal (run to fixpoint, the same rule `merged()` uses) only gets
  to **692 cells**: the conjunctive cell vocabulary cannot express most
  of the same-set unions (e.g. "cuda absent OR cuda in [11, 12)" needs a
  disjunction). 140 groups have more than one cell (285 cells between
  them, simplifiable to 269).
- The product structure is real: 708 = 12 microarch slices (11
  `__archspec 1.* <level>` literals plus the all-negative baseline,
  each positive in exactly 59 cells) x 59 cells of cuda bands (absent,
  [11, 12), [12, 12.2), [12.2, 12.4), [12.4, 12.8), [12.8, 13),
  >= 13) x glibc bands (2.17, 2.28, 2.34, 2.39 floors). The
  per-slice solutions genuinely differ: `_x86_64-microarch-level`
  build variants per archspec level, libsystemd0/libudev1 257 vs 260
  and sysroot variants per glibc band, the cuda runtime cone per cuda
  band (median set difference vs the baseline cell: 15 records, max
  118 in the old-glibc corners).

So: fragmentation is semantically forced by the snapshot's dependencies
to first order. A sound generalization pass over identical-set cells
(interval-union vocabulary or DNF cell conditions) could remove at most
145 of 708 cells (20%) and none of the cost that makes this problem an
outlier.

### Where the cell literals come from (per-pinning-rule counters)

12,472 cell literals over 708 cells (median 18, max 25 per cell):

| source | literals | share |
| --- | --- | --- |
| requirement on env package satisfied by env literal (`req_env`) | 3,997 | 32% |
| `EnvConstrains` absent/matches pins | 1,756 | 14% |
| disjointness repair: distinguishing appends | 6,114 | 49% |
| disjointness repair: agreement pins | 605 | 5% |
| conditional-guard pins | 0 | 0% |
| support-scan pins (skip / negative) | 0 | 0% |

Two findings:

- The trail-reuse hardening rules (guard pins, support-scan pins) are
  inert on this problem; the load-bearing extraction is entirely driven
  by direct requirements/constrains on `__cuda`, `__glibc`, `__archspec`
  etc. (~9 literals per cell).
- Half of all cell literals are repair appends, and they grow linearly
  with the cell index (`rep_dist` median 9, max 18): each re-discovery
  of a similar solution in an adjacent region must be distinguished
  from every earlier overlapping cell one literal at a time. They make
  conditions long, but they do not create cells, and their cost
  (quadratic scan over recorded cells at ~47 ms/cell total) is
  negligible at this scale. They are however the reason `env_literals`
  per condition (28 distinct) and the median condition length (18) look
  scarier than the true axis count (3 axes + 2 always-true literals).

### The prefix-budget abort fires on intrinsic hardness and wastes work

The first enumeration attempt (trail reuse on) recorded 550 cells in
5.8 s, entered the cell 550 wall, burned its cumulative work budget
~4 s in, and aborted the whole attempt (`ReuseAbandoned`). The fallback
re-enumerated the same 550 cells in 19.9 s without reuse (3.4x slower
for the mechanical phase) and then hit the **same** 206 s wall: the
hardness is intrinsic to the cuda [11, 12) region, not damage from the
reused trail (the very evidence the budget mechanism assumes). Net
waste: roughly 25-30 s of the 272 s, the budget-protection worst case
documented in the trail-reuse verdict, paid here for nothing.

This is by design (the budget cannot distinguish reuse-damage from
intrinsic hardness without trying the fallback), but 265 quantifies the
false-positive cost for the first time on a corpus problem. If the
restarts/deletion work gives the solver a way to escape a stuck search
cheaply, the budget's calibration (16x fresh cost, one fresh solve per
recorded cell) deserves a revisit on top of it.

### Recommendations (ranked by expected payoff)

1. **Model bounds remain the dominant lever.** Every wall in this
   problem lives in `cuda < 12` corners. A model floor of cuda >= 12
   (the glibc 2.28 analog) removes the 206 s transition, the 12 s and
   7 s ones, and ~150 cells outright. No solver work matches that.
2. **Restart/deletion policies for wall transitions** (separate research
   effort): 57 k conflicts in a single `run_sat` with stale VSIDS state
   from 550 prior cells is exactly the profile a restart strategy
   targets. Reference: this transition is a better benchmark candidate
   than problem 34 (which the combined head already solves in 3 s).
3. **Do not invest in cell merging for cost reasons.** At most 20% of
   cells are mergeable and they are the cheap ones. Merging is a
   cosmetic/lockfile-size question (`merged()` already provides the
   compact per-record view), not a performance one.
4. **Budget false-positive mitigation** (small, optional): when the
   fallback enumeration is about to re-pay a transition that the
   aborted attempt already proved expensive, the only available lever
   today is wholesale; measured cost here is bounded (~10% of the
   solve) and the protection is still net-positive corpus-wide, so
   leave as is unless restarts land.

## Problem 212 (the remaining mystery)

Requirements: openmmdl, vcs_versioning, aio-pika, flake8-bugbear,
sphinxcontrib-programoutput, polling2, gwollum. Concrete solve: 17.6 s
(itself heavy). Universal: timed out in every variant ever benchmarked;
its shape was unknown until now.

### Shape: it enumerates fine, then drowns wall after wall

Instrumented run at a 600 s cap (CANCELLED). The solve made two
enumeration attempts:

- **Attempt 0 (trail reuse)**: cell 0 alone costs 11.9 s and 6,195
  conflicts (the concrete-hard base problem). 118 cells recorded by
  t = 155 s. Four walls on the way: transition to cell 44 (927
  conflicts), to cell 66 (37 s, 14,630), to cell 88 (25 s, 3,160), to
  cell 96 (72 s, 42,757). Then the transition out of cell 117 ran 152 s
  and 38,518 conflicts without completing, at which point the
  cumulative prefix work budget aborted the attempt (t = 307 s, 803 M
  propagated decisions total).
- **Attempt 1 (fallback, no reuse)**: re-recorded the identical 118
  cells by t = 154 s, hit the same walls (66: 4,386 conflicts, 88: 684,
  96: 36,858; cheaper in conflicts but the same shape), and was then
  cancelled by the cap 139 s and 18,709 conflicts into the **same**
  unfinished transition after cell 117 (552 M propagated decisions).

Combined: ~182 k conflicts and 1.36 G propagated decisions in 600 s,
118 + 118 cells, and the refutation/witness phase was never reached.
The mechanical majority is healthy: 111 of 117 transitions in attempt 0
cost 1.6 s total.

### The walls are corner entries (per-cell condition trace)

A re-run printing each cell's condition identifies every wall as the
*first* cell of a new corner of the environment space:

| wall | region entered |
| --- | --- |
| cell 44 | first `__cuda` [12, 13) cell |
| cell 66 | first old-glibc cell (`not (__glibc >= 2.28)`, cuda absent) |
| cell 88 | first cell of the all-negative `__archspec` baseline slice |
| cell 96 | **first `__cuda` [11, 12) cell** (in the old-glibc sapphirerapids corner): the worst completed wall, 42.8 k conflicts |
| after 117 | the next cuda [11, 12) region; never completed in 290+ s across two attempts |

The cells between walls are nearly free, including the old-cuda cells
97..117: once the first [11, 12) refutation is learned, the next six
microarch slices of the same corner ride the learnt clauses at
mechanical cost. The wall cost is the first-entry refutation of a
corner (refuting the entire cuda 12/13 stack under `not >= 12`, the
modern-glibc stacks under `not >= 2.28`, and their intersections), and
the corners stack multiplicatively with the 12-way microarch split.

### Classification and what it is NOT

- **Existing class: intrinsically hard corners** (problem 34 / problem
  265's wall), in its most extreme observed form: at least five walls,
  one of which never completed. Not a new outlier class.
- **Not the witness search.** The enumeration never reaches UNSAT, so
  the propagation-free witness search never runs; the known latent
  issue (no cancellation check inside the witness search) was NOT
  observed in either case. Cancellation worked: the 600 s cap fired
  inside `run_sat` propagation as designed.
- **The prefix-budget abort amplifies the loss.** As in 265, the budget
  fired mid-wall on intrinsic hardness (the very transition the
  fallback then re-paid), discarding 118 cells and ~300 s of learnt
  refutations. With the abort, the cap can never be met: half the
  budget is spent twice. At the campaign's 60 s cap the solve dies
  inside wall 66 or 96 of attempt 0, which is why every variant ever
  benchmarked timed out.

The full partition size is unknown (>= 119 cells; by 265's analogy and
the 12 x cuda x glibc product, likely several hundred cells with more
unfinished walls among them). Problem 212 is not finishable by cap
extension alone at current per-wall costs.

### Recommendations (ranked by expected payoff)

1. **Model bounds, again.** Every wall is inside `cuda < 12` or
   `glibc < 2.28`. With ecosystem-baseline floors (glibc >= 2.28,
   cuda >= 12) the walls disappear along with the corners; the
   remaining enumeration is the healthy mechanical majority. For pixi,
   212 is an argument for baseline-default models, not for solver
   surgery.
2. **Restarts / clause deletion** (separate research effort): the
   unfinished transition is a single `run_sat` spending 150 s and tens
   of thousands of conflicts in one corner with activity and watchlists
   shaped by 118 prior cells. This is the canonical restart scenario;
   problem 212's transition after cell 117 is the sharpest reproducible
   benchmark for that work (and problem 265's cell 550 transition the
   second).
3. **Refutation ordering** (separate research effort): the trace shows
   corner refutations DO transfer within a corner (cells 97..117 are
   free after wall 96). An enumeration order that enters each corner
   once and exhausts it before moving on is already emergent; what the
   ordering work could add is making the FIRST entry cheaper by seeding
   the corner's refutation from the adjacent corner's learnt clauses
   instead of relearning from scratch.
4. **Prefix-budget policy**: leave as is for now. In both observed
   cases the abort fired on intrinsic hardness (false positive), but
   any in-place repair was measured worse during the trail-reuse
   campaign, and the protection is net-positive corpus-wide. Revisit
   only if restarts make a stuck prefix-started run recoverable without
   wholesale abandonment.

## Instrumentation kept

- **Per-cell pin-source counters** (permanent, `diagnostics` feature):
  `Solver::universal_cell_pins()` returns one `CellPinCounts` per
  recorded cell, attributing every cell literal to the rule that pushed
  it (requirement-on-env-package, conditional guard, support-scan skip,
  support complement, `EnvConstrains`, repair agreement, repair
  distinguishing append). The trail-reuse diagnostics test asserts the
  attribution is total (per-cell counts sum to the condition length).
- **`solve-snapshot --cells-dump <path>`** (tool): writes per-solution
  cell statistics: distinct solvable sets, the simplified partition
  size (same-set sign-flip merging to fixpoint), per-axis literal
  usage, per-group record diffs against the baseline cell, and the
  full per-cell condition listing. This is what produced the 265
  fragmentation numbers above.
- The per-cell stderr trace lines and witness-search heartbeat used
  during the investigation were scratch and are not committed.

## Summary

- **Problem 265**: the 708-cell partition is 80% genuinely distinct
  solutions and costs ~47 ms per mechanical cell; the outlier cost is a
  single intrinsically hard transition (entering the cuda [11, 12)
  corner: 206 s, 57 k conflicts, 80% of the solve) plus a
  prefix-budget false positive that re-enumerates 550 cells. Cell-count
  reduction is not the lever; model floors and restart research are.
- **Problem 212**: an extreme member of the same hard-corner class, not
  a new one and not the witness search; it records cells briskly but
  must refute a modern stack from scratch at every corner entry, and
  one old-cuda corner entry never completes; the budget abort then
  doubles the bill. Model floors remove the corners; restarts/deletion
  are the solver-side lever.
