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
/// Simulation steps per rendered frame. One step a frame would take half a minute to settle;
/// a whole layout at once would drop the animation people read structure from.
pub const STEPS_PER_FRAME: usize = 8;
/// Past this, animating costs more than it explains: the layout is computed once and drawn.
pub const ANIMATE_LIMIT: usize = 2_000;

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
        Sim { pos, edges, k: (AREA / n as f64).sqrt(), temp: TEMP0, energy: f64::MAX }
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
        for &(a, b) in &self.edges {
            let dx = self.pos[a].x - self.pos[b].x;
            let dy = self.pos[a].y - self.pos[b].y;
            let d = (dx * dx + dy * dy).sqrt().max(0.01);
            let f = d / self.k;
            fx[a] -= dx / d * f;
            fy[a] -= dy / d * f;
            fx[b] += dx / d * f;
            fy[b] += dy / d * f;
        }

        // Move, then hold inside the field. The clamp is not decoration: while the layout
        // is hot, repulsion beats the springs at close range and a graph with no wall to
        // push against simply expands — every distance grows, nothing clusters, and the
        // temperature runs out mid-flight leaving a picture of an explosion.
        let half = AREA.sqrt() / 2.0;
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
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        let (mut lo_x, mut hi_x, mut lo_y, mut hi_y) =
            (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for p in &self.pos {
            lo_x = lo_x.min(p.x);
            hi_x = hi_x.max(p.x);
            lo_y = lo_y.min(p.y);
            hi_y = hi_y.max(p.y);
        }
        if !lo_x.is_finite() {
            return (-1.0, 1.0, -1.0, 1.0);
        }
        let pad = ((hi_x - lo_x).max(hi_y - lo_y) * 0.08).max(1.0);
        (lo_x - pad, hi_x + pad, lo_y - pad, hi_y + pad)
    }

    /// Where node `i` lands in a `w` x `h` cell area, under `zoom` about `centre`.
    ///
    /// Rounded to a cell, because a node is a glyph and a glyph occupies one.
    pub fn project(&self, i: usize, w: u16, h: u16, zoom: f64, centre: Point) -> Option<(u16, u16)> {
        let p = self.pos.get(i)?;
        let (lo_x, hi_x, lo_y, hi_y) = self.bounds();
        let (span_x, span_y) = ((hi_x - lo_x).max(1e-6) / zoom, (hi_y - lo_y).max(1e-6) / zoom);
        let fx = (p.x - centre.x) / span_x + 0.5;
        let fy = (p.y - centre.y) / span_y + 0.5;
        if !(0.0..=1.0).contains(&fx) || !(0.0..=1.0).contains(&fy) {
            return None; // off screen at this zoom
        }
        Some((
            (fx * (w.saturating_sub(1)) as f64).round() as u16,
            (fy * (h.saturating_sub(1)) as f64).round() as u16,
        ))
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
