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

/// Above this many edges, drawing them all fills every cell with braille and the picture
/// becomes a solid block with a graph somewhere inside it.
///
/// Found by pointing this at a real 170-note vault: 888 edges over a terminal-sized canvas
/// is not a diagram, it is a texture. Past the limit only the selected node's links are
/// drawn, which is the question a person actually has — *what does this connect to* — and
/// the clustering is still legible, because that is carried by where nodes sit rather than
/// by the lines between them.
const EDGE_LIMIT: usize = 160;

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

    // Leave a row top and bottom for the header and hint.
    let plot = TRect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(2),
    };

    // Edges first, on their own layer, so nodes always sit on top of the lines.
    let (w, h) = (plot.width, plot.height);
    let projected: Vec<Option<(u16, u16)>> =
        (0..graph.nodes.len()).map(|i| sim.project(i, w, h, zoom, centre)).collect();
    let sel_edges: Vec<bool> = graph
        .edges
        .iter()
        .map(|(a, b)| *a as usize == sel || *b as usize == sel)
        .collect();

    let dim = theme.ui.border;
    let hot = theme.ui.accent;
    Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, w.max(1) as f64])
        .y_bounds([0.0, h.max(1) as f64])
        .paint(|ctx| {
            let dense = graph.edges.len() > EDGE_LIMIT;
            for (i, (a, b)) in graph.edges.iter().enumerate() {
                if dense && !sel_edges[i] {
                    continue;
                }
                let (Some((x1, y1)), Some((x2, y2))) =
                    (projected[*a as usize], projected[*b as usize])
                else {
                    continue;
                };
                // The canvas has y increasing upward; the buffer has it increasing down.
                let flip = |y: u16| (h.saturating_sub(1) - y.min(h.saturating_sub(1))) as f64;
                ctx.draw(&CanvasLine {
                    x1: x1 as f64,
                    y1: flip(y1),
                    x2: x2 as f64,
                    y2: flip(y2),
                    color: color(if sel_edges[i] { hot } else { dim }),
                });
            }
        })
        .render(plot, buf);

    // Nodes over the top. Drawn selection-last so its glyph and label win any overlap.
    let mut label_rows: Vec<(u16, u16, u16)> = Vec::new();
    let mut order: Vec<usize> = (0..graph.nodes.len()).collect();
    order.sort_by_key(|i| (*i == sel, graph.nodes[*i].degree));

    for i in order {
        let node = &graph.nodes[i];
        let Some((x, y)) = projected[i] else { continue };
        let (cx, cy) = (plot.x + x, plot.y + y);
        if cx >= plot.x + plot.width || cy >= plot.y + plot.height {
            continue;
        }
        let selected = i == sel;
        // A ghost is a note nobody has written: hollow, and in no cluster's colour, because
        // it does not belong to one yet.
        let (glyph, fg) = if node.ghost {
            ("○", theme.ui.text_faint)
        } else if node.degree >= 6 {
            ("●", group_color(&node.group, theme))
        } else {
            ("•", group_color(&node.group, theme))
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
        if selected || node.degree >= 10 {
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
            let kind = if n.ghost { "  (not written yet)" } else { "" };
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
        area.y + area.height.saturating_sub(1),
        area.width.saturating_sub(4),
        Line::from(Span::styled(
            "tab next   ↑↓←→ pan   +- zoom   enter open   esc close".to_string(),
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
        draw(&mut buf, area, &Theme::horde(), &g, &sim, sel, 1.0, sim.centre());
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

    /// Something has to actually be drawn — edges as braille, nodes as glyphs. A canvas that
    /// silently rendered nothing would still pass a header assertion.
    #[test]
    fn nodes_and_edges_are_both_drawn() {
        let text = render(0);
        let nodes = text.chars().filter(|c| "●•○".contains(*c)).count();
        assert_eq!(nodes, 3, "one glyph per node:\n{text}");
        let braille = text.chars().filter(|c| ('\u{2800}'..='\u{28FF}').contains(c)).count();
        assert!(braille > 0, "edges drawn on the braille canvas:\n{text}");
    }

    /// The rule a real vault forced. Every edge drawn at once turns a 170-note graph into a
    /// solid block of braille, so past a limit only the selection's links are drawn — and the
    /// header has to say so, or a missing line reads as a missing link.
    #[test]
    fn a_dense_graph_draws_only_the_selected_nodes_links_and_says_so() {
        let n = 40;
        let mut nodes = Vec::new();
        for i in 0..n {
            nodes.push(GraphNode {
                path: format!("n{i}.md"),
                label: format!("n{i}"),
                degree: 4,
                group: "g".into(),
                ghost: false,
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
        draw(&mut buf, area, &Theme::horde(), &g, &sim, 0, 1.0, sim.centre());
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
        draw(&mut buf, area, &Theme::horde(), &empty, &sim, 0, 1.0, Point { x: 0.0, y: 0.0 });
        let text: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();
        assert!(text.contains("no notes to draw"), "{text}");
    }
}
