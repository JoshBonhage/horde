//! The knowledge graph, full screen.
//!
//! Edges are drawn on ratatui's braille canvas, which packs 2x4 dots into a cell and is the
//! only way a terminal draws a diagonal line that reads as one. Nodes are glyphs laid over
//! it in the same visual language the sidebar uses, because a node is a thing you select and
//! a glyph is a thing that sits in a cell.
//!
//! The layout is [`crate::client::graph`]; this file only decides what colour things are and
//! where the labels go.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::Widget;

use super::{color, fill, put_line, truncate};
use crate::client::graph::{Point, Sim};
use crate::proto::{Rgb, VaultGraph};
use crate::theme::Theme;

/// A guard against a pathological vault, not a display decision.
///
/// This used to be 160, and past it only the selected node's links were drawn — because 888
/// edges over a terminal-sized canvas measured as a texture rather than a diagram. What made
/// that true was that every endpoint was rounded to a cell first, so the lines were eight
/// times coarser than the canvas could draw. With fractional endpoints the whole web can be
/// laid down faintly and read as a web, which is the thing this was always imitating.
const EDGE_LIMIT: usize = 4_000;

/// How far the unselected web is pushed toward the background.
///
/// High, deliberately. Every edge at a readable weight is a grey rectangle; the web is meant
/// to be felt rather than followed, with the one neighbourhood you asked about picked out on
/// top of it.
const WEB_FADE: f32 = 0.72;

/// The share of the canvas the faint web may cover.
///
/// Measured rather than chosen. A terminal 150 cells wide is 300x148 braille dots — about 44
/// thousand. The reference vault's 888 edges are roughly 40 dots each, so drawing all of them
/// covers about eighty per cent of the canvas and the result is a solid block: not a dim web
/// behind the graph, a wall in front of it. Clusters are legible because of the *space*
/// between them, so the ink has to leave some.
const WEB_INK: f64 = 0.15;

/// Shorter than this, in braille dots, an edge is inside its own endpoints.
///
/// Two notes a cell apart are already drawn as two glyphs touching; the line between them
/// says nothing and costs a cell of ink. Skipping them is what stops the ink budget being
/// spent entirely inside the densest cluster, where it buys the least.
const WEB_MIN: f64 = 4.0;

/// How recently a note has to have been written to read as "just now".
///
/// An hour, which is about the span of "while I was doing something else". Longer and every
/// note an agent has ever written looks urgent; shorter and a fleet working through the
/// morning shows nothing.
const FRESH: u64 = 60 * 60 * 1000;

fn fresh(mtime: u64, now: u64) -> bool {
    mtime > 0 && now.saturating_sub(mtime) < FRESH
}

/// Colour for a cluster, from the six project accents.
///
/// Reusing the space ramp rather than inventing a palette: the point of a group colour is
/// that two things sharing one belong together, and horde already has a set of colours that
/// means exactly that.
fn group_color(group: &str, theme: &Theme) -> Rgb {
    let mut h: u32 = 2166136261;
    for b in group.as_bytes() {
        h = (h ^ *b as u32).wrapping_mul(16777619);
    }
    theme.space_accent((h % crate::theme::SPACE_ACCENTS as u32) as u8)
}

/// The area the graph itself is drawn into, inside the header and hint rows.
///
/// Public and used by both the renderer and the mouse handler, because the two have to agree
/// about where a cell is. Computed twice, they would agree until one of them changed.
pub fn plot_of(area: TRect) -> TRect {
    TRect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(3),
    }
}

/// Draw the graph. Returns `(y, x, node index)` hits for the mouse.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    buf: &mut Buffer,
    area: TRect,
    theme: &Theme,
    graph: &VaultGraph,
    sim: &Sim,
    sel: usize,
    zoom: f64,
    centre: Point,
    now: u64,
) -> Vec<(u16, u16, usize)> {
    fill(buf, area, theme.ui.bg);
    let mut hits = Vec::new();
    if area.height < 6 || area.width < 30 || graph.nodes.is_empty() {
        let msg = if graph.nodes.is_empty() { "no notes to draw" } else { "" };
        put_line(
            buf,
            area.x + 2,
            area.y + 1,
            area.width.saturating_sub(4),
            Line::from(Span::styled(
                msg.to_string(),
                Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
            )),
        );
        return hits;
    }

    let plot = plot_of(area);

    // Edges first, on their own layer, so nodes always sit on top of the lines.
    //
    // Kept fractional. The braille canvas packs 2x4 dots into a cell, so an endpoint that has
    // been rounded to a cell first is an endpoint drawn at an eighth of the resolution the
    // canvas is capable of — which is what made every line look like it was built out of
    // blocks. Nodes still round, because a node is a glyph.
    let (w, h) = (plot.width, plot.height);
    let projected: Vec<Option<(f64, f64)>> =
        (0..graph.nodes.len()).map(|i| sim.project_f(i, w, h, zoom, centre)).collect();
    let sel_edges: Vec<bool> = graph
        .edges
        .iter()
        .map(|(a, b)| *a as usize == sel || *b as usize == sel)
        .collect();

    // Which of the web to draw, shortest first, until the ink runs out.
    //
    // Shortest first is the whole trick. The long edges are the ones that cross the canvas
    // and turn it into texture; the short ones trace the outlines of the clusters, which is
    // the thing a graph is being looked at for. Spending a bounded amount of ink on the short
    // ones draws the shape of the vault and leaves the space that makes it readable.
    let mut lengths: Vec<(usize, f64)> = graph
        .edges
        .iter()
        .enumerate()
        .filter_map(|(i, (a, b))| {
            let (Some(p), Some(q)) = (projected[*a as usize], projected[*b as usize]) else {
                return None;
            };
            // In braille dots: two across a cell, four down it.
            let (dx, dy) = ((p.0 - q.0) * 2.0, (p.1 - q.1) * 4.0);
            let len = (dx * dx + dy * dy).sqrt();
            (len >= WEB_MIN).then_some((i, len))
        })
        .collect();
    lengths.sort_by(|a, b| a.1.total_cmp(&b.1));
    let ink = w as f64 * 2.0 * h as f64 * 4.0 * WEB_INK;
    let mut spent = 0.0;
    let mut web = vec![false; graph.edges.len()];
    for (i, len) in lengths {
        if spent + len > ink {
            break;
        }
        spent += len;
        web[i] = true;
    }

    let dim = crate::theme::mix(theme.ui.border, theme.ui.bg, WEB_FADE);
    let hot = theme.ui.accent;
    Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, w.max(1) as f64])
        .y_bounds([0.0, h.max(1) as f64])
        .paint(|ctx| {
            let dense = graph.edges.len() > EDGE_LIMIT;
            // The neighbourhood last, so it lies on top of the web rather than under it.
            for pass in [false, true] {
                for (i, (a, b)) in graph.edges.iter().enumerate() {
                    if sel_edges[i] != pass || (!sel_edges[i] && (dense || !web[i])) {
                        continue;
                    }
                    let (Some((x1, y1)), Some((x2, y2))) =
                        (projected[*a as usize], projected[*b as usize])
                    else {
                        continue;
                    };
                    // Canvas y increases upward; the buffer's increases down.
                    let flip = |y: f64| (h.saturating_sub(1) as f64 - y).max(0.0);
                    ctx.draw(&CanvasLine {
                        x1,
                        y1: flip(y1),
                        x2,
                        y2: flip(y2),
                        color: color(if sel_edges[i] { hot } else { dim }),
                    });
                }
            }
        })
        .render(plot, buf);

    // Which nodes get a name.
    //
    // A budget, not a threshold. A threshold that reads well on one vault is a wall of text on
    // the next — this one has sixty notes with ten or more links, and naming all of them
    // buries the diagram they are drawn on. So: the biggest handful that will fit, and more of
    // them as you zoom in, which is what zooming is for.
    let room = (plot.width as usize * plot.height as usize) / 400;
    let budget = ((room as f64) * zoom.clamp(0.6, 3.0)) as usize;
    let mut named: Vec<usize> = (0..graph.nodes.len()).collect();
    named.sort_by_key(|i| std::cmp::Reverse(graph.nodes[*i].degree));
    named.truncate(budget.clamp(3, 40));
    let named: std::collections::HashSet<usize> = named.into_iter().collect();

    // Nodes over the top. Drawn selection-last so its glyph and label win any overlap.
    let mut label_rows: Vec<(u16, u16, u16)> = Vec::new();
    let mut order: Vec<usize> = (0..graph.nodes.len()).collect();
    order.sort_by_key(|i| (*i == sel, graph.nodes[*i].degree));

    for i in order {
        let node = &graph.nodes[i];
        let Some((x, y)) = sim.project(i, w, h, zoom, centre) else { continue };
        let (cx, cy) = (plot.x + x, plot.y + y);
        if cx >= plot.x + plot.width || cy >= plot.y + plot.height {
            continue;
        }
        let selected = i == sel;
        // A ghost is a note nobody has written: hollow, and in no cluster's colour, because
        // it does not belong to one yet.
        let (glyph, fg) = if node.ghost {
            ("○", theme.ui.text_faint)
        } else if node.by.is_some() {
            // Written on somebody's behalf rather than by them. The graph is the one view
            // that shows the vault whole, so it is the one place "how much of this did I
            // write" is answerable at a glance. A diamond, for the same reason a service
            // gets one: it is not in the same cycle as the rest.
            ("◆", if fresh(node.mtime, now) { theme.ui.working } else { theme.ui.serving })
        } else if node.degree >= 6 {
            ("●", group_color(&node.group, theme))
        } else if node.degree >= 2 {
            ("•", group_color(&node.group, theme))
        } else {
            // A third size for the leaves. Most of a vault is notes with one link, and drawing
            // them at the same weight as a hub is what makes a graph read as gravel.
            ("·", group_color(&node.group, theme))
        };
        let style = Style::default().bg(color(theme.ui.bg)).fg(color(if selected {
            theme.ui.accent
        } else {
            fg
        }));
        let style = if selected { style.add_modifier(Modifier::BOLD) } else { style };
        put_line(buf, cx, cy, 1, Line::from(Span::styled(glyph.to_string(), style)));
        hits.push((cy, cx, i));

        // Labels for the selection and for hubs — but only where one fits without landing
        // on another. Unchecked, a dense vault writes every name over its neighbour and the
        // result is a paragraph nobody can read, laid over a diagram nobody can see.
        if selected || named.contains(&i) {
            let label = truncate(&node.label, 18);
            let lx = cx + 2;
            let lw = super::width(&label) as u16;
            let clear = lx + lw < plot.x + plot.width
                && !label_rows.iter().any(|(ly, a, b)| *ly == cy && lx <= *b + 1 && *a <= lx + lw);
            if clear {
                label_rows.push((cy, lx, lx + lw));
                put_line(
                    buf,
                    lx,
                    cy,
                    (plot.x + plot.width).saturating_sub(lx),
                    Line::from(Span::styled(
                        label,
                        Style::default()
                            .bg(color(theme.ui.bg))
                            .fg(color(if selected { theme.ui.text } else { theme.ui.text_dim })),
                    )),
                );
            }
        }
    }

    // Header: what is selected, and what it connects to.
    let header = match graph.nodes.get(sel) {
        Some(n) => {
            let links = graph
                .edges
                .iter()
                .filter(|(a, b)| *a as usize == sel || *b as usize == sel)
                .count();
            let kind = match (&n.by, n.ghost) {
                (_, true) => "  (not written yet)".to_string(),
                (Some(by), _) => format!("  (by {by})"),
                _ => String::new(),
            };
            let dense = if graph.edges.len() > EDGE_LIMIT { "   showing this node's links" } else { "" };
            format!("{}{kind}   {links} links   {} notes{dense}", n.label, graph.nodes.len())
        }
        None => format!("{} notes", graph.nodes.len()),
    };
    put_line(
        buf,
        area.x + 2,
        area.y,
        area.width.saturating_sub(4),
        Line::from(Span::styled(
            header,
            Style::default()
                .fg(color(theme.ui.text))
                .bg(color(theme.ui.bg))
                .add_modifier(Modifier::BOLD),
        )),
    );
    put_line(
        buf,
        area.x + 2,
        // One up from the bottom: the last row belongs to the status bar, which is drawn
        // after this. Three views had this wrong.
        area.y + area.height.saturating_sub(2),
        area.width.saturating_sub(4),
        Line::from(Span::styled(
            "drag pan   scroll zoom   click select   tab next   enter open   esc close".to_string(),
            Style::default().fg(color(theme.ui.text_faint)).bg(color(theme.ui.bg)),
        )),
    );
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::GraphNode;

    fn graph() -> VaultGraph {
        let node = |label: &str, degree: u16, ghost: bool| GraphNode {
            path: if ghost { String::new() } else { format!("{label}.md") },
            label: label.into(),
            degree,
            group: "dev".into(),
            ghost,
            by: None,
            mtime: 0,
        };
        VaultGraph {
            nodes: vec![
                node("Hub", 9, false),
                node("Leaf", 1, false),
                node("Unwritten", 1, true),
            ],
            edges: vec![(0, 1), (0, 2)],
        }
    }

    fn render(sel: usize) -> String {
        let g = graph();
        let mut sim = Sim::new(&g);
        sim.settle(200);
        let area = TRect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &Theme::horde(), &g, &sim, sel, 1.0, sim.centre(), 0);
        (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect()
    }

    /// The header answers "what am I looking at" for the node under the cursor, including
    /// the case that matters most: a link to a note that does not exist yet.
    #[test]
    fn the_header_names_the_selected_node_and_says_when_it_is_unwritten() {
        assert!(render(0).contains("Hub"), "{}", render(0));
        assert!(render(0).contains("2 links"), "the hub has both edges");
        let ghost = render(2);
        assert!(ghost.contains("Unwritten"), "{ghost}");
        assert!(ghost.contains("not written yet"), "a ghost says so: {ghost}");
    }

    /// The graph is the one view that shows the vault whole, so it is the one place "how
    /// much of this did I write" is answerable at a glance. A note written on somebody's
    /// behalf has to look different from one they wrote — and say whose it is when selected.
    #[test]
    fn a_note_written_by_an_agent_is_marked_as_one() {
        let mut g = graph();
        g.nodes[0].by = Some("reviewer".into());
        let mut sim = Sim::new(&g);
        sim.settle(200);
        let area = TRect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &Theme::horde(), &g, &sim, 0, 1.0, sim.centre(), 0);
        let text: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();

        assert!(text.contains('◆'), "marked apart from the notes a person wrote:\n{text}");
        assert!(text.contains("(by reviewer)"), "and the header says whose:\n{text}");
    }

    /// An hour, which is about the span of "while I was doing something else". A note with no
    /// mtime at all is not fresh, or a graph built from a fixture would glow.
    #[test]
    fn freshness_is_about_the_last_hour() {
        let now = 10 * FRESH;
        assert!(fresh(now - 1000, now), "a minute ago");
        assert!(!fresh(now - FRESH - 1, now), "and not two hours ago");
        assert!(!fresh(0, now), "nor a note that never said when");
    }

    /// Something has to actually be drawn — edges as braille, nodes as glyphs. A canvas that
    /// silently rendered nothing would still pass a header assertion.
    #[test]
    fn nodes_and_edges_are_both_drawn() {
        let text = render(0);
        let nodes = text.chars().filter(|c| "●•·○◆".contains(*c)).count();
        assert_eq!(nodes, 3, "one glyph per node:\n{text}");
        let braille = text.chars().filter(|c| ('\u{2800}'..='\u{28FF}').contains(c)).count();
        assert!(braille > 0, "edges drawn on the braille canvas:\n{text}");
    }

    /// The rule a real vault forced. Every edge drawn at once turns a 170-note graph into a
    /// solid block of braille, so past a limit only the selection's links are drawn — and the
    /// header has to say so, or a missing line reads as a missing link.
    #[test]
    fn a_dense_graph_draws_only_the_selected_nodes_links_and_says_so() {
        // Past `EDGE_LIMIT`, which is now a guard against a pathological vault rather than a
        // display decision — the ordinary dense case draws its whole web faintly.
        let n = 95;
        let mut nodes = Vec::new();
        for i in 0..n {
            nodes.push(GraphNode {
                path: format!("n{i}.md"),
                label: format!("n{i}"),
                degree: 4,
                group: "g".into(),
                ghost: false,
                by: None,
                mtime: 0,
            });
        }
        // Well past EDGE_LIMIT, with node 0 in only one of them.
        let mut edges = vec![(0u16, 1u16)];
        for a in 1..n as u16 {
            for b in (a + 1)..n as u16 {
                edges.push((a, b));
            }
        }
        let g = VaultGraph { nodes, edges };
        assert!(g.edges.len() > super::EDGE_LIMIT, "the fixture has to be dense");

        let mut sim = Sim::new(&g);
        sim.settle(300);
        let area = TRect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &Theme::horde(), &g, &sim, 0, 1.0, sim.centre(), 0);
        let text: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();

        assert!(text.contains("showing this node's links"), "the header explains it:\n{text}");
        let braille = text.chars().filter(|c| ('\u{2800}'..='\u{28FF}').contains(c)).count();
        let cells = (area.width * area.height) as usize;
        assert!(braille > 0, "the one edge it does draw is drawn");
        assert!(
            braille < cells / 8,
            "and the canvas stays mostly empty: {braille} of {cells} cells"
        );
    }

    /// A vault with no notes is the state every project starts in, and an empty canvas with
    /// no explanation reads as a bug.
    #[test]
    fn an_empty_graph_says_so_rather_than_drawing_nothing() {
        let empty = VaultGraph::default();
        let sim = Sim::new(&empty);
        let area = TRect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, &Theme::horde(), &empty, &sim, 0, 1.0, Point { x: 0.0, y: 0.0 }, 0);
        let text: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();
        assert!(text.contains("no notes to draw"), "{text}");
    }
}
