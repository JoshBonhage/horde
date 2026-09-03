//! Lifting a service's address off its screen.
//!
//! The counterpart of [`question`](super::question), and it exists for the same reason.
//! Detection already knows a dev server is up; the one thing its sidebar row can usefully
//! add is *where*, because "serving" is a fact the glyph already carried. A row that reads
//! `◆ vite   :5173` answers the question you actually had.
//!
//! # Why a heuristic is acceptable here
//!
//! Same argument as `question`: manifests decide state, and a wrong state is a lie the whole
//! UI repeats, so those are matched strictly. This decides one right-aligned detail, and a
//! wrong parse costs you a port you read in the pane instead. So it is generic rather than
//! per-manifest — every dev server on earth prints a `http://host:port` at startup, because
//! that is the line the person who started it is waiting for.
//!
//! What it will not do is guess. Nothing matched means no endpoint, and the row falls back
//! to saying `serving`, which is what it said before this module existed.

/// How far up the screen to look.
///
/// Deliberately much wider than `question::LOOK_BACK`. A prompt is the newest thing on a
/// screen and a wider window only finds stale ones; a server's address is the *oldest* thing
/// on its screen — printed once at startup and then buried under request logs forever. The
/// two parsers scan opposite ends of the same problem.
const LOOK_BACK: usize = 400;

/// Longest endpoint kept. `:5173` is the common case; a named host is allowed to be longer,
/// but not so long that it eats the service's own name out of the row.
const MAX_LEN: usize = 24;

/// Loopback hosts, which are worth dropping from the display.
///
/// `localhost:5173` and `127.0.0.1:5173` say the same thing as `:5173` in four times the
/// columns, and the sidebar has none to spare. A *named* host is kept in full: if a server
/// is answering on `api.local` that is the whole point of the line.
const LOOPBACK: [&str; 4] = ["localhost", "127.0.0.1", "0.0.0.0", "[::]"];

/// Characters that frame a URL without being part of it.
fn untrim(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(c, '│' | '┃' | '║' | '▌' | '▐' | '|' | '"' | '\'')
            || matches!(c, '(' | ')' | '<' | '>' | ',')
            || c.is_whitespace()
    })
}

/// `http://localhost:5173/` → `:5173`; `https://api.local:8080` → `api.local:8080`.
fn from_url(word: &str) -> Option<String> {
    let rest = word
        .strip_prefix("http://")
        .or_else(|| word.strip_prefix("https://"))?;
    // Path, query and fragment are noise: nobody needs `/` on a sidebar row, and a dev
    // server that prints a deep link would push its own port off the end.
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, Some(p)),
        // No port means the default one, and `:80` is not what was printed. Keep the host.
        _ => (authority, None),
    };
    if host.is_empty() {
        return None;
    }
    let out = match (LOOPBACK.contains(&host), port) {
        (true, Some(p)) => format!(":{p}"),
        (true, None) => return None,
        (false, Some(p)) => format!("{host}:{p}"),
        (false, None) => host.to_string(),
    };
    (out.len() <= MAX_LEN).then_some(out)
}

/// `listening on port 3000`, `Server started on 8080` → `:3000`, `:8080`.
///
/// Only after the URL forms have all failed. A bare number is much weaker evidence than a
/// scheme, so it is required to follow a phrase that means what we think it means.
fn from_phrase(lower: &str, line: &str) -> Option<String> {
    const LEADS: [&str; 5] =
        ["listening on", "started server on", "server started on", "running at", "server on"];
    let at = LEADS.iter().find_map(|p| lower.find(p).map(|i| i + p.len()))?;
    let tail = &line[at..];
    // The first run of digits after the phrase, skipping the optional word "port".
    let digits: String = tail
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    // A port is 2–5 digits. Anything else is a duration, a byte count, or a timestamp that
    // happened to follow the phrase.
    (2..=5).contains(&digits.len()).then(|| format!(":{digits}"))
}

/// The address a service is answering on, or `None` when its screen does not say.
///
/// `lines` is the pane's detection snapshot, oldest first. The *last* match wins: a server
/// that was restarted on a new port has printed both, and the newer line is the true one.
///
/// A local address outranks a LAN one however late the LAN one was printed, which is not the
/// same rule and has to be separate. Vite, Astro and Next all print `Local:` and then
/// `Network:` on the line below, so last-match-wins on its own would put `192.168.1.5:5173`
/// on the row — an address that is correct, is not the one you are going to open, and costs
/// three times the columns to say so.
pub fn extract(lines: &[String]) -> Option<String> {
    let from = lines.len().saturating_sub(LOOK_BACK);
    let mut local = None;
    let mut remote = None;
    let mut take = |e: String| {
        if e.starts_with(':') {
            local = Some(e);
        } else {
            remote = Some(e);
        }
    };
    for line in &lines[from..] {
        let lower = line.to_ascii_lowercase();
        // Whole-URL forms first, across every word on the line: `Local: http://localhost:5173/`
        // puts the address in the second word, and Vite's box-drawn banner in the third.
        for word in line.split_whitespace() {
            if let Some(e) = from_url(untrim(word)) {
                take(e);
            }
        }
        if let Some(e) = from_phrase(&lower, line) {
            take(e);
        }
    }
    local.or(remote)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(s: &[&str]) -> Option<String> {
        extract(&s.iter().map(|l| l.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_loopback_url_keeps_only_the_port() {
        assert_eq!(ex(&["  ➜  Local:   http://localhost:5173/"]).as_deref(), Some(":5173"));
        assert_eq!(ex(&["Server: http://127.0.0.1:3000"]).as_deref(), Some(":3000"));
    }

    /// The point of a named host is that it is not the one you would have guessed, so it is
    /// the one thing here worth spending columns on.
    #[test]
    fn a_named_host_is_kept_in_full() {
        let out = ex(&["ready on http://api.local:8080/health"]);
        assert_eq!(out.as_deref(), Some("api.local:8080"));
    }

    #[test]
    fn a_path_is_not_part_of_the_endpoint() {
        assert_eq!(ex(&["open http://localhost:1313/posts/hello/"]).as_deref(), Some(":1313"));
    }

    /// A restart prints a second address, and the row must show the one that is live.
    #[test]
    fn the_newest_address_on_the_screen_wins() {
        let out = ex(&[
            "Local: http://localhost:5173/",
            "Port 5173 is in use, trying another one...",
            "Local: http://localhost:5174/",
        ]);
        assert_eq!(out.as_deref(), Some(":5174"));
    }

    #[test]
    fn a_phrase_with_a_bare_port_is_read_when_there_is_no_url() {
        assert_eq!(ex(&["listening on port 3000"]).as_deref(), Some(":3000"));
        assert_eq!(ex(&["Started server on 8080"]).as_deref(), Some(":8080"));
    }

    /// The weakest tier must not fire on numbers that merely follow the phrase.
    #[test]
    fn a_bare_number_that_is_not_a_port_is_not_one() {
        assert_eq!(ex(&["listening on 1758923400 events"]), None);
        assert_eq!(ex(&["running at 9 percent"]), None);
    }

    /// The banners people actually see, verbatim. The parser is generic on purpose, so the
    /// thing worth testing is that the generic rule survives contact with real output.
    #[test]
    fn the_banners_dev_servers_actually_print() {
        // Vite. `Network:` comes after `Local:` and must not win.
        let vite = ex(&[
            "  VITE v5.4.2  ready in 342 ms",
            "",
            "  ➜  Local:   http://localhost:5173/",
            "  ➜  Network: http://192.168.1.14:5173/",
            "  ➜  press h + enter to show help",
        ]);
        assert_eq!(vite.as_deref(), Some(":5173"));

        // Next.js.
        let next = ex(&[
            "   ▲ Next.js 14.2.3",
            "   - Local:        http://localhost:3000",
            "   - Environments: .env.local",
        ]);
        assert_eq!(next.as_deref(), Some(":3000"));

        // Astro, whose banner is box-drawn right up against the label.
        let astro = ex(&[
            "┃ Local    http://localhost:4321/",
            "┃ Network  use --host to expose",
        ]);
        assert_eq!(astro.as_deref(), Some(":4321"));

        // uvicorn, which binds all interfaces and says so.
        let uvicorn =
            ex(&["INFO:     Uvicorn running on http://0.0.0.0:8000 (Press CTRL+C to quit)"]);
        assert_eq!(uvicorn.as_deref(), Some(":8000"));
    }

    /// A server with no local address at all still has one worth showing.
    #[test]
    fn a_lan_address_is_used_when_there_is_no_local_one() {
        let out = ex(&["Network: http://192.168.1.14:5173/"]);
        assert_eq!(out.as_deref(), Some("192.168.1.14:5173"));
    }

    /// A parser that guesses is worse than one that says nothing: the row already has a
    /// correct thing to fall back to.
    #[test]
    fn an_ordinary_log_line_yields_nothing() {
        assert_eq!(ex(&["webpack compiled successfully in 812 ms"]), None);
        assert_eq!(ex(&["GET /favicon.ico 200"]), None);
    }

    /// Loopback with no port is exactly the case where the shortened form would say nothing
    /// at all, so it has to decline rather than render an empty detail.
    #[test]
    fn loopback_with_no_port_is_not_an_endpoint() {
        assert_eq!(ex(&["proxying http://localhost/"]), None);
    }
}
