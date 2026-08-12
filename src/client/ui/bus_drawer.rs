//! The right panel: a live feed of messages horde routed between agents.
//!
//! This is the part herdr cannot show. Because the daemon routes rather than letting agents
//! type at each other, there is an actual record to display — including which messages are
//! still held, so a stuck message is visible instead of silently lost.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TRect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::{color, fill, put_line, wrap_text};
use crate::proto::{Delivery, Message, MsgKind};
use crate::theme::Theme;

pub struct BusDrawer<'a> {
    pub messages: &'a [Message],
    pub theme: &'a Theme,
    /// Wall-clock now, in unix millis, for relative timestamps.
    pub now: u64,
}

impl Widget for BusDrawer<'_> {
    fn render(self, area: TRect, buf: &mut Buffer) {
        if area.width < 12 || area.height < 3 {
            return;
        }
        let t = self.theme;
        fill(buf, area, t.ui.panel_bg);

        let inner_w = area.width.saturating_sub(2);
        let mut y = area.y;

        put_line(
            buf,
            area.x + 1,
            y,
            inner_w,
            Line::from(vec![Span::styled(
                "bus",
                Style::default()
                    .fg(color(t.ui.accent_alt))
                    .bg(color(t.ui.panel_bg))
                    .add_modifier(Modifier::BOLD),
            )]),
        );
        y += 1;
        for i in 0..area.width {
            if let Some(c) = buf.cell_mut((area.x + i, y)) {
                c.set_symbol("─");
                c.set_style(Style::default().fg(color(t.ui.border)).bg(color(t.ui.panel_bg)));
            }
        }
        y += 1;

        if self.messages.is_empty() {
            for line in wrap_text("No messages yet. Agents reach each other with `horde send`.", inner_w as usize)
            {
                if y >= area.y + area.height {
                    break;
                }
                put_line(
                    buf,
                    area.x + 1,
                    y,
                    inner_w,
                    Line::from(vec![Span::styled(
                        line,
                        Style::default().fg(color(t.ui.text_faint)).bg(color(t.ui.panel_bg)),
                    )]),
                );
                y += 1;
            }
            return;
        }

        // Newest last, like a chat log. Render from the bottom up so the newest message is
        // always the one that survives a short panel.
        let bottom = area.y + area.height;
        let mut blocks: Vec<Vec<Line<'static>>> = Vec::new();
        for msg in self.messages {
            blocks.push(message_block(msg, inner_w as usize, t, self.now));
        }

        // Drop oldest blocks until the rest fit.
        let mut total: usize = blocks.iter().map(|b| b.len() + 1).sum();
        let room = bottom.saturating_sub(y) as usize;
        let mut start = 0usize;
        while total > room && start < blocks.len() {
            total -= blocks[start].len() + 1;
            start += 1;
        }

        for block in &blocks[start..] {
            for line in block {
                if y >= bottom {
                    return;
                }
                put_line(buf, area.x + 1, y, inner_w, line.clone());
                y += 1;
            }
            y += 1; // gap between messages
        }
    }
}

/// One message rendered as a small card: time and delivery, the route, then the body.
fn message_block(msg: &Message, w: usize, t: &Theme, now: u64) -> Vec<Line<'static>> {
    let panel = Style::default().bg(color(t.ui.panel_bg));
    let (marker, mcolor, mlabel) = match msg.delivery {
        Delivery::Delivered => ("✓", t.ui.ok, ""),
        // A held message is the interesting case, so it says so in words.
        Delivery::Queued => ("⧗", t.ui.warn, " queued"),
        Delivery::Dropped => ("✕", t.ui.error, " dropped"),
    };

    let mut out = Vec::new();
    out.push(Line::from(vec![
        Span::styled(
            relative_time(msg.ts, now),
            panel.fg(color(t.ui.text_faint)),
        ),
        Span::styled("  ", panel),
        Span::styled(marker.to_string(), panel.fg(color(mcolor))),
        Span::styled(mlabel.to_string(), panel.fg(color(mcolor))),
    ]));

    // A request and its reply are a pair, so label them and they read as a thread rather
    // than as two unrelated messages.
    let route = match msg.kind() {
        MsgKind::Request => format!("{} ⇢ {}  ask #{}", msg.from, msg.to, msg.id),
        MsgKind::Reply => {
            format!("{} ⇠ {}  re #{}", msg.to, msg.from, msg.reply_to.unwrap_or(0))
        }
        MsgKind::Plain if msg.broadcast => format!("{} → all", msg.from),
        MsgKind::Plain => format!("{} → {}", msg.from, msg.to),
    };
    let route_color = match msg.kind() {
        MsgKind::Request => t.ui.warn,
        MsgKind::Reply => t.ui.ok,
        MsgKind::Plain => t.ui.accent_alt,
    };
    for (i, line) in wrap_text(&route, w).into_iter().enumerate() {
        out.push(Line::from(vec![Span::styled(
            line,
            panel
                .fg(color(if i == 0 { route_color } else { t.ui.text_dim }))
                .add_modifier(Modifier::BOLD),
        )]));
    }

    for line in wrap_text(msg.body.trim(), w) {
        out.push(Line::from(vec![Span::styled(line, panel.fg(color(t.ui.text)))]));
    }
    out
}

/// `now`, `12s`, `4m`, `2h`, else a bare clock time. Relative reads faster than absolute
/// for a feed you are watching live.
fn relative_time(ts: u64, now: u64) -> String {
    let secs = now.saturating_sub(ts) / 1000;
    match secs {
        0..=2 => "now".to_string(),
        3..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => {
            let total = ts / 1000;
            format!("{:02}:{:02}", (total / 3600) % 24, (total / 60) % 60)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u64, delivery: Delivery, body: &str) -> Message {
        Message {
            id,
            ts: 1_000_000,
            from: "builder".into(),
            to: "reviewer".into(),
            body: body.into(),
            delivery,
            broadcast: false,
            expects_reply: false,
            reply_to: None,
        }
    }

    fn render(messages: &[Message], w: u16, h: u16) -> Buffer {
        let area = TRect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let theme = Theme::horde();
        BusDrawer { messages, theme: &theme, now: 1_000_000 }.render(area, &mut buf);
        buf
    }

    fn text(buf: &Buffer, w: u16, h: u16) -> String {
        (0..h)
            .map(|y| (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn empty_state_explains_how_to_send() {
        let out = text(&render(&[], 30, 10), 30, 10);
        // The hint wraps across lines at this width, so compare on normalised whitespace.
        let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("horde send"), "{out}");
    }

    #[test]
    fn renders_route_and_body() {
        let out = text(&render(&[msg(1, Delivery::Delivered, "schema is ready")], 30, 12), 30, 12);
        assert!(out.contains("bus"), "{out}");
        assert!(out.contains("builder → reviewer"), "{out}");
        assert!(out.contains("schema is ready"), "{out}");
        assert!(out.contains('✓'), "{out}");
    }

    #[test]
    fn a_request_and_its_reply_read_as_a_thread() {
        let mut ask = msg(7, Delivery::Delivered, "is the gating sound?");
        ask.expects_reply = true;
        let mut reply = msg(8, Delivery::Delivered, "yes, it holds");
        reply.reply_to = Some(7);
        reply.from = "reviewer".into();
        reply.to = "builder".into();

        let out = text(&render(&[ask, reply], 34, 16), 34, 16);
        assert!(out.contains("ask #7"), "the request should be marked: {out}");
        assert!(out.contains("re #7"), "the reply should name its request: {out}");
        assert!(out.contains("is the gating sound?"), "{out}");
        assert!(out.contains("yes, it holds"), "{out}");
    }

    #[test]
    fn queued_messages_are_labelled_so_a_stuck_one_is_visible() {
        let out = text(&render(&[msg(1, Delivery::Queued, "hi")], 30, 12), 30, 12);
        assert!(out.contains("queued"), "{out}");
        assert!(out.contains('⧗'), "{out}");
    }

    #[test]
    fn dropped_messages_are_labelled_too() {
        let out = text(&render(&[msg(1, Delivery::Dropped, "hi")], 30, 12), 30, 12);
        assert!(out.contains("dropped"), "{out}");
    }

    #[test]
    fn broadcast_shows_all_rather_than_one_target() {
        let mut m = msg(1, Delivery::Delivered, "standup");
        m.broadcast = true;
        let out = text(&render(&[m], 30, 12), 30, 12);
        assert!(out.contains("builder → all"), "{out}");
    }

    #[test]
    fn newest_message_survives_a_short_panel() {
        let msgs: Vec<Message> = (1..=10)
            .map(|i| msg(i, Delivery::Delivered, &format!("message-number-{i}")))
            .collect();
        let out = text(&render(&msgs, 30, 10), 30, 10);
        assert!(out.contains("message-number-10"), "newest must be kept: {out}");
        assert!(!out.contains("message-number-1\n"), "oldest should be dropped: {out}");
    }

    #[test]
    fn long_bodies_wrap_within_the_panel_width() {
        let long = "this is a very long message body that certainly needs to wrap across \
                    several lines to be readable";
        let out = text(&render(&[msg(1, Delivery::Delivered, long)], 24, 20), 24, 20);
        for line in out.lines() {
            assert!(line.chars().count() <= 24, "overflow: {line:?}");
        }
        assert!(out.contains("this is a"), "{out}");
    }

    #[test]
    fn tiny_areas_render_nothing_rather_than_panicking() {
        let out = text(&render(&[msg(1, Delivery::Delivered, "x")], 8, 10), 8, 10);
        assert_eq!(out.trim(), "");
    }

    #[test]
    fn relative_time_reads_naturally() {
        let now = 1_000_000_000u64;
        assert_eq!(relative_time(now, now), "now");
        assert_eq!(relative_time(now - 10_000, now), "10s ago");
        assert_eq!(relative_time(now - 240_000, now), "4m ago");
        assert_eq!(relative_time(now - 7_200_000, now), "2h ago");
        // Beyond a day, a clock time is more use than "31h ago".
        let old = relative_time(now - 200_000_000, now);
        assert!(old.contains(':'), "{old}");
    }

    #[test]
    fn future_timestamps_do_not_underflow() {
        // Clock skew between a write and a read must not panic.
        assert_eq!(relative_time(2_000, 1_000), "now");
    }
}
