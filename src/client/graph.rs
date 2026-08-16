//! Force-directed layout for the knowledge graph.
//!
//! Pure arithmetic, no drawing: [`Sim::step`] moves points and [`Sim::project`] turns them
//! into canvas coordinates, and both are testable without a terminal.
//!
//! **The cooling schedule is the load-bearing part.** A spike of this without one ran for a
//! thousand steps with the average node still drifting, which in a daemon built to idle
//! means a core burnt forever on a picture that stopped changing in any way you could see.
//! Fruchterman-Reingold's annealing is what makes "settle then stop" a thing the code
//! actually does: displacement is clamped to a temperature that decays every step, so the
//! layout provably comes to rest instead of merely tending toward it.

use crate::proto::VaultGraph;

/// The field the layout spreads into, in arbitrary units. Only ratios matter — the view
/// scales whatever comes out to fit the terminal — but everything else is derived from it,
/// so it has to be one number rather than three that can drift apart.
const AREA: f64 = 40_000.0;
/// Starting displacement cap. A tenth of the field's width, per Fruchterman-Reingold: large
/// enough that nodes can cross the field early, small enough that they never fly off it.
const TEMP0: f64 = 20.0;
/// Per-step decay. Measured: settles a 300-node graph in ~120 steps, which at the render
/// tick's 8 steps a frame is under two seconds of animation.
const COOLING: f64 = 0.95;
/// Mean movement below which the layout is done and redrawing stops.
const SETTLED: f64 = 0.02;
/// Strength of the pull toward the middle, relative to repulsion at the field's edge.
///
/// One means "balances the typical outward push at the wall", which is about right: below it
/// the loose majority still ends up on the perimeter, and well above it every vault collapses
/// into the same ball regardless of what is actually linked to what.
const GRAVITY: f64 = 1.0;
/// Simulation steps per rendered frame. One step a frame would take half a minute to settle;
/// a whole layout at once would drop the animation people read structure from.
pub const STEPS_PER_FRAME: usize = 8;
/// Past this, animating costs more than it explains: the layout is computed once and drawn.
pub const ANIMATE_LIMIT: usize = 2_000;
/// How wide a terminal cell is relative to its height.
///
/// About a half on every terminal anyone uses. Without it the layout is drawn into a grid
/// twice as tall as it is wide and read as though it were square, which turns every circle
/// into an ellipse and every even spread into a smear.
pub const CELL_ASPECT: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

pub struct Sim {
    pub pos: Vec<Point>,
    edges: Vec<(usize, usize)>,
    /// Ideal edge length, from the node count and the area they have to spread into.
    k: f64,
    temp: f64,
    /// Mean distance moved on the last step — the settle test.
    pub energy: f64,
    /// The padded box the layout occupies. See [`Sim::bounds`].
    bounds: (f64, f64, f64, f64),
}

impl Sim {
    /// Lay out a graph, starting from a deterministic scatter.
    ///
    /// Deterministic on purpose: the same vault should draw the same shape twice, or a
    /// person cannot learn where their own notes live.
    pub fn new(graph: &VaultGraph) -> Sim {
        let n = graph.nodes.len().max(1);
        // A phyllotaxis spiral: an even fill of the field with no two nodes coincident, and
        // no random number generator. Scaled to the same field `k` is derived from, or the
        // layout starts far denser than its own ideal spacing and spends its whole
        // temperature budget merely pushing apart instead of arranging.
        let radius = AREA.sqrt() / 2.0;
        let pos = (0..graph.nodes.len())
            .map(|i| {
                let a = (i as f64 * 2.399_963_2) % std::f64::consts::TAU; // golden angle
                let r = ((i as f64 + 0.5) / n as f64).sqrt() * radius;
                Point { x: r * a.cos(), y: r * a.sin() }
            })
            .collect();
        let edges = graph
            .edges
            .iter()
            .map(|(a, b)| (*a as usize, *b as usize))
            .filter(|(a, b)| *a < graph.nodes.len() && *b < graph.nodes.len())
            .collect();
        let mut sim = Sim {
            pos,
            edges,
            k: (AREA / n as f64).sqrt(),
            temp: TEMP0,
            energy: f64::MAX,
            bounds: (-1.0, 1.0, -1.0, 1.0),
        };
        sim.measure();
        sim
    }

    pub fn settled(&self) -> bool {
        self.energy < SETTLED
    }

    /// One Fruchterman-Reingold step: repulsion between every pair, springs along edges,
    /// then a move clamped to the current temperature.
    pub fn step(&mut self) {
        let n = self.pos.len();
        if n < 2 {
            self.energy = 0.0;
            return;
        }
        let mut fx = vec![0.0f64; n];
        let mut fy = vec![0.0f64; n];

        for i in 0..n {
            for j in (i + 1)..n {
                let dx = self.pos[i].x - self.pos[j].x;
                let dy = self.pos[i].y - self.pos[j].y;
                let d2 = (dx * dx + dy * dy).max(0.01);
                let f = self.k * self.k / d2;
                fx[i] += dx * f;
                fy[i] += dy * f;
                fx[j] -= dx * f;
                fy[j] -= dy * f;
            }
        }
        // Attraction is `d²/k`, as Fruchterman-Reingold specifies — not `d/k`, which is what
        // this was and which is a factor of `d` too weak at every distance. With repulsion
        // right and the springs that slack, the layout spread until the field's own wall
        // stopped it: on the reference vault nearly every node ended up pinned to the
        // perimeter with the middle empty, which reads as a box rather than a graph. The
        // clamp was holding a failure rather than preventing one.
        for &(a, b) in &self.edges {
            let dx = self.pos[a].x - self.pos[b].x;
            let dy = self.pos[a].y - self.pos[b].y;
            let d = (dx * dx + dy * dy).sqrt().max(0.01);
            let f = d * d / self.k;
            fx[a] -= dx / d * f;
            fy[a] -= dy / d * f;
            fx[b] += dx / d * f;
            fy[b] += dy / d * f;
        }

        // Gravity: a weak pull toward the middle, felt by everything.
        //
        // Repulsion acts between every pair while the springs act only along edges, so a note
        // with one link feels a hundred and seventy pushes and a single pull. Left alone the
        // loosely-connected majority — which is most of any real vault — drifts outward until
        // the field's wall stops it, and the picture is a box with its edges lined with notes
        // and nothing in the middle. Gravity is what the wall was standing in for.
        //
        // Scaled so it balances typical repulsion at the field's edge, which keeps it a
        // property of the graph rather than a number that has to be retuned per vault.
        let half = AREA.sqrt() / 2.0;
        let g = GRAVITY * n as f64 * self.k * self.k / (half * half);
        for i in 0..n {
            fx[i] -= self.pos[i].x * g;
            fy[i] -= self.pos[i].y * g;
        }

        // Move, then hold inside the field. The clamp is a backstop, not the layout: with
        // gravity doing the containing, nothing should reach it — and a test says so, because
        // a layout pressed against its own frame is the failure this all came from.
        let mut moved = 0.0;
        for i in 0..n {
            let d = (fx[i] * fx[i] + fy[i] * fy[i]).sqrt().max(1e-9);
            let s = d.min(self.temp);
            self.pos[i].x = (self.pos[i].x + fx[i] / d * s).clamp(-half, half);
            self.pos[i].y = (self.pos[i].y + fy[i] / d * s).clamp(-half, half);
            moved += s;
        }
        self.energy = moved / n as f64;
        self.temp *= COOLING;
        self.measure();
    }

    /// Run until it stops moving, for the case where animating is not worth it.
    pub fn settle(&mut self, max_steps: usize) {
        for _ in 0..max_steps {
            self.step();
            if self.settled() {
                break;
            }
        }
    }

    /// The bounding box of the layout, padded so nothing sits on the frame.
    ///
    /// Cached rather than measured on demand, for two reasons. It is read once per node per
    /// frame through [`Sim::scale`], so measuring it made drawing quadratic in the node count
    /// for no reason. And it must *not* follow a node being dragged: a box that grows as you
    /// pull something toward the edge rescales the whole picture under your hand, and the
    /// graph swims away from the pointer. It moves when the simulation moves, and not
    /// otherwise.
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        self.bounds
    }

    fn measure(&mut self) {
        let (mut lo_x, mut hi_x, mut lo_y, mut hi_y) =
            (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for p in &self.pos {
            lo_x = lo_x.min(p.x);
            hi_x = hi_x.max(p.x);
            lo_y = lo_y.min(p.y);
            hi_y = hi_y.max(p.y);
        }
        self.bounds = if lo_x.is_finite() {
            let pad = ((hi_x - lo_x).max(hi_y - lo_y) * 0.08).max(1.0);
            (lo_x - pad, hi_x + pad, lo_y - pad, hi_y + pad)
        } else {
            (-1.0, 1.0, -1.0, 1.0)
        };
    }

    /// Cells per model unit, for a `w` x `h` plot at this zoom.
    ///
    /// One scale for both axes, with `CELL_ASPECT` applied to y. Fitting the bounding box to
    /// the width and the height *independently* — which is what this used to do — stretches
    /// the layout by whatever the box happens to be shaped like, and then a terminal cell
    /// being twice as tall as it is wide stretches it again. A circular cluster came out as
    /// an ellipse at some arbitrary angle, which is most of why this never looked like the
    /// thing it is imitating.
    fn scale(&self, w: u16, h: u16, zoom: f64) -> f64 {
        let (lo_x, hi_x, lo_y, hi_y) = self.bounds();
        let span_x = (hi_x - lo_x).max(1e-6);
        let span_y = (hi_y - lo_y).max(1e-6);
        let fit = (w as f64 / span_x).min(h as f64 / (span_y * CELL_ASPECT));
        fit * zoom
    }

    /// Where node `i` lands, in cells but not rounded to one.
    ///
    /// The fractional part is the whole point. Edges are drawn on a braille canvas that packs
    /// 2x4 dots into a cell, and rounding the endpoints to cells first — which is what this
    /// used to do for everything — threw away eight times the resolution the canvas was
    /// offering. That discarded precision *was* the blockiness.
    pub fn project_f(
        &self,
        i: usize,
        w: u16,
        h: u16,
        zoom: f64,
        centre: Point,
    ) -> Option<(f64, f64)> {
        let p = self.pos.get(i)?;
        let s = self.scale(w, h, zoom);
        let x = (p.x - centre.x) * s + w as f64 / 2.0;
        let y = (p.y - centre.y) * s * CELL_ASPECT + h as f64 / 2.0;
        let (max_x, max_y) = (w.saturating_sub(1) as f64, h.saturating_sub(1) as f64);
        if !(0.0..=max_x).contains(&x) || !(0.0..=max_y).contains(&y) {
            return None; // off screen at this zoom
        }
        Some((x, y))
    }

    /// The same, rounded to a cell — because a node is a glyph and a glyph occupies one.
    pub fn project(&self, i: usize, w: u16, h: u16, zoom: f64, centre: Point) -> Option<(u16, u16)> {
        self.project_f(i, w, h, zoom, centre)
            .map(|(x, y)| (x.round() as u16, y.round() as u16))
    }

    /// A point on screen, back into the layout's own coordinates.
    ///
    /// The inverse of [`Sim::project_f`], and the reason dragging a node and zooming toward
    /// the pointer are possible at all: both are questions about where in the graph a
    /// particular cell is.
    pub fn unproject(&self, cell: (f64, f64), w: u16, h: u16, zoom: f64, centre: Point) -> Point {
        let s = self.scale(w, h, zoom).max(1e-9);
        Point {
            x: (cell.0 - w as f64 / 2.0) / s + centre.x,
            y: (cell.1 - h as f64 / 2.0) / (s * CELL_ASPECT) + centre.y,
        }
    }

    /// Put a node somewhere deliberately — a drag, rather than a force.
    pub fn place(&mut self, i: usize, p: Point) {
        let half = AREA.sqrt() / 2.0;
        if let Some(q) = self.pos.get_mut(i) {
            q.x = p.x.clamp(-half, half);
            q.y = p.y.clamp(-half, half);
        }
    }

    /// Warm the layout back up, so a node that was moved pulls its neighbours after it.
    ///
    /// A fraction of the starting temperature rather than all of it: dropping a node should
    /// tidy the corner it landed in, not throw the whole map back into the air and lose the
    /// arrangement somebody had just learned.
    pub fn nudge(&mut self) {
        self.temp = self.temp.max(TEMP0 * 0.25);
        self.energy = f64::MAX;
    }

    /// The middle of the laid-out graph, which is where the view starts.
    pub fn centre(&self) -> Point {
        let (lo_x, hi_x, lo_y, hi_y) = self.bounds();
        Point { x: (lo_x + hi_x) / 2.0, y: (lo_y + hi_y) / 2.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::GraphNode;

    fn graph(n: usize, edges: &[(u16, u16)]) -> VaultGraph {
        VaultGraph {
            nodes: (0..n)
                .map(|i| GraphNode {
                    path: format!("n{i}.md"),
                    label: format!("n{i}"),
                    degree: 0,
                    group: "g".into(),
                    ghost: false,
                    by: None,
                    mtime: 0,
                })
                .collect(),
            edges: edges.to_vec(),
        }
    }

    /// The failure this module is arranged around. Without annealing the layout keeps
    /// jittering forever, and a graph left open would hold a core at full tilt drawing a
    /// picture that has visibly stopped changing.
    #[test]
    fn the_layout_comes_to_a_stop_rather_than_drifting_forever() {
        let g = graph(120, &(0..119).map(|i| (i, i + 1)).collect::<Vec<_>>());
        let mut sim = Sim::new(&g);
        let mut steps = 0;
        while !sim.settled() && steps < 600 {
            sim.step();
            steps += 1;
        }
        assert!(sim.settled(), "did not settle in {steps} steps, energy {}", sim.energy);
        assert!(steps < 400, "settled but took {steps} steps");

        // And it stays stopped: another hundred steps must not wake it up.
        for _ in 0..100 {
            sim.step();
        }
        assert!(sim.settled(), "a settled layout stays settled");
    }

    /// The failure the real vault showed, and the reason gravity exists.
    ///
    /// Repulsion acts between every pair and the springs only along edges, so the loosely
    /// connected majority — most of any real vault — drifted outward until the field's wall
    /// stopped it. The picture was a box with its edges lined with notes and nothing in the
    /// middle, and the clamp that produced it looked like it was preventing a failure rather
    /// than holding one.
    #[test]
    fn a_sparse_vault_does_not_end_up_pinned_to_the_frame() {
        // The shape of a real vault: a few hubs, and a long tail of notes with one link each.
        let mut edges: Vec<(u16, u16)> = Vec::new();
        for i in 3..170u16 {
            edges.push((i % 3, i));
        }
        let mut sim = Sim::new(&graph(170, &edges));
        sim.settle(600);

        let half = AREA.sqrt() / 2.0;
        let on_the_wall = sim
            .pos
            .iter()
            .filter(|p| p.x.abs() > half * 0.98 || p.y.abs() > half * 0.98)
            .count();
        assert_eq!(on_the_wall, 0, "{on_the_wall} of 170 notes pressed against the frame");

        // And it has not collapsed to a dot instead, which is the other way to pass the above.
        let (lo_x, hi_x, lo_y, hi_y) = sim.bounds();
        let spread = (hi_x - lo_x).max(hi_y - lo_y);
        assert!(spread > half * 0.3, "it still uses the space it has: {spread:.0}");
    }

    /// A layout that did not put linked notes together would be a picture of nothing.
    #[test]
    fn linked_notes_end_up_closer_together_than_unlinked_ones() {
        // Two clumps of five, joined by a single edge.
        let mut edges: Vec<(u16, u16)> = Vec::new();
        for a in 0..5u16 {
            for b in (a + 1)..5 {
                edges.push((a, b));
                edges.push((a + 5, b + 5));
            }
        }
        edges.push((0, 5));
        let mut sim = Sim::new(&graph(10, &edges));
        sim.settle(600);

        let dist = |a: usize, b: usize| {
            ((sim.pos[a].x - sim.pos[b].x).powi(2) + (sim.pos[a].y - sim.pos[b].y).powi(2)).sqrt()
        };
        let within = (1..5).map(|i| dist(0, i)).sum::<f64>() / 4.0;
        let across = (6..10).map(|i| dist(0, i)).sum::<f64>() / 4.0;
        assert!(within < across, "clustered: {within:.1} within vs {across:.1} across");
    }

    /// The same vault has to draw the same shape twice, or nobody can learn the map.
    #[test]
    fn the_same_graph_lays_out_the_same_way_every_time() {
        let g = graph(30, &[(0, 1), (1, 2), (2, 3), (3, 0), (4, 5)]);
        let mut a = Sim::new(&g);
        let mut b = Sim::new(&g);
        a.settle(300);
        b.settle(300);
        assert_eq!(a.pos, b.pos);
    }

    /// Projection has to stay inside the area it was given — a node drawn at column 200 of a
    /// 100-column terminal is a panic in the buffer, not a cosmetic problem.
    #[test]
    fn projection_stays_inside_the_area_it_is_given() {
        let mut sim = Sim::new(&graph(40, &[(0, 1), (2, 3)]));
        sim.settle(200);
        let c = sim.centre();
        for zoom in [0.5, 1.0, 2.5] {
            for i in 0..40 {
                if let Some((x, y)) = sim.project(i, 80, 24, zoom, c) {
                    assert!(x < 80 && y < 24, "node {i} at {x},{y} zoom {zoom}");
                }
            }
        }
    }

    /// The blockiness this module was rebuilt to fix. Rounding an endpoint to a cell before
    /// handing it to a canvas that packs 2x4 dots into one throws away eight times the
    /// resolution it was offering, and every line comes out built from blocks.
    #[test]
    fn edges_get_positions_finer_than_a_cell() {
        let mut sim = Sim::new(&graph(24, &[(0, 1)]));
        sim.settle(300);
        let c = sim.centre();
        let fractional = (0..24)
            .filter_map(|i| sim.project_f(i, 80, 24, 1.0, c))
            .filter(|(x, y)| x.fract() > 1e-9 || y.fract() > 1e-9)
            .count();
        assert!(fractional > 0, "every node landed exactly on a cell, which cannot be right");

        // And the rounded form still agrees with it, because nodes are glyphs.
        for i in 0..24 {
            match (sim.project_f(i, 80, 24, 1.0, c), sim.project(i, 80, 24, 1.0, c)) {
                (Some((fx, fy)), Some((x, y))) => {
                    assert_eq!((fx.round() as u16, fy.round() as u16), (x, y));
                }
                (None, None) => {}
                (a, b) => panic!("the two disagree about whether node {i} is on screen: {a:?} {b:?}"),
            }
        }
    }

    /// A terminal cell is about twice as tall as it is wide, and the layout used to be fitted
    /// to width and height independently on top of that. Between them, a circular cluster
    /// drew as an ellipse — which is most of why this never looked like the thing it imitates.
    #[test]
    fn a_round_cluster_draws_round() {
        // A ring of points, placed by hand so the shape is known rather than simulated.
        let mut sim = Sim::new(&graph(16, &[]));
        for (i, p) in sim.pos.iter_mut().enumerate() {
            let a = i as f64 / 16.0 * std::f64::consts::TAU;
            p.x = a.cos() * 60.0;
            p.y = a.sin() * 60.0;
        }
        let c = Point { x: 0.0, y: 0.0 };
        let pts: Vec<(f64, f64)> =
            (0..16).filter_map(|i| sim.project_f(i, 100, 30, 1.0, c)).collect();
        assert_eq!(pts.len(), 16, "all of them on screen");

        // In pixels — cells are twice as tall as wide — the ring's width and height must
        // match. In cells they must not, and that difference is the whole correction.
        let span = |f: fn(&(f64, f64)) -> f64| {
            let (lo, hi) = pts.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| {
                (lo.min(f(p)), hi.max(f(p)))
            });
            hi - lo
        };
        let (wide, tall) = (span(|p| p.0), span(|p| p.1));
        let ratio = wide / (tall / CELL_ASPECT);
        assert!((ratio - 1.0).abs() < 0.02, "round in pixels: {wide:.1} wide, {tall:.1} tall");
        assert!(tall < wide * 0.6, "and taller than it is wide in *cells*: {tall:.1} vs {wide:.1}");
    }

    /// Dragging a node and zooming toward the pointer are both the same question — where in
    /// the graph is this cell — so the inverse has to actually be one.
    #[test]
    fn unproject_undoes_project() {
        let mut sim = Sim::new(&graph(30, &[(0, 1), (2, 3)]));
        sim.settle(300);
        let c = sim.centre();
        for zoom in [0.5, 1.0, 2.5] {
            for i in 0..30 {
                let Some(cell) = sim.project_f(i, 90, 28, zoom, c) else { continue };
                let back = sim.unproject(cell, 90, 28, zoom, c);
                assert!(
                    (back.x - sim.pos[i].x).abs() < 1e-6 && (back.y - sim.pos[i].y).abs() < 1e-6,
                    "node {i} at zoom {zoom}: {:?} came back as {back:?}",
                    sim.pos[i]
                );
            }
        }
    }

    /// A node put down by hand stays where it was put, and stays inside the field — a drag
    /// that could throw a node past the wall would break the layout it lands in.
    #[test]
    fn a_dragged_node_lands_where_it_was_put_and_no_further() {
        let mut sim = Sim::new(&graph(5, &[]));
        sim.place(2, Point { x: 12.0, y: -7.0 });
        assert_eq!(sim.pos[2], Point { x: 12.0, y: -7.0 });

        sim.place(3, Point { x: 1e9, y: -1e9 });
        let half = AREA.sqrt() / 2.0;
        assert_eq!(sim.pos[3], Point { x: half, y: -half }, "held inside the field");
    }

    /// Dropping a node should tidy the corner it landed in, not throw the whole map into the
    /// air — somebody has just spent time learning where things are.
    #[test]
    fn a_nudge_warms_the_layout_without_starting_it_over() {
        let mut sim = Sim::new(&graph(40, &[(0, 1), (1, 2)]));
        sim.settle(400);
        assert!(sim.settled());

        sim.nudge();
        assert!(!sim.settled(), "there is work to do again");
        assert!(sim.temp < TEMP0, "but not as much as at the start: {}", sim.temp);
        let mut steps = 0;
        while !sim.settled() && steps < 400 {
            sim.step();
            steps += 1;
        }
        assert!(sim.settled(), "and it comes back to rest");
    }

    /// One node has nothing to repel from, and an empty graph has nothing at all. Both are
    /// real states — a new vault passes through them — and neither may divide by zero.
    #[test]
    fn a_graph_of_one_or_none_settles_immediately() {
        let mut empty = Sim::new(&graph(0, &[]));
        empty.step();
        assert!(empty.settled());
        let mut one = Sim::new(&graph(1, &[]));
        one.step();
        assert!(one.settled());
    }

    /// An edge index out of range would panic the simulation. The wire carries indices, so
    /// this is the one place a daemon a build ahead could take the client down.
    #[test]
    fn an_edge_pointing_past_the_end_is_dropped_rather_than_panicking() {
        let mut sim = Sim::new(&graph(3, &[(0, 1), (2, 99)]));
        sim.settle(50);
        assert_eq!(sim.pos.len(), 3);
    }
}
