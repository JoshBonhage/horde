//! Control-channel method dispatch.
//!
//! This is the API agents drive themselves through — every `horde <noun> <verb>` command is
//! one call here. It stays newline JSON so it can be debugged with `nc` and scripted from
//! anything that can write a line.

use serde_json::{json, Value};

use super::{apply_cmd, Engine};
use crate::proto::{AgentState, Cmd, Dir, PaneId, Request, Response};

pub fn dispatch(eng: &mut Engine, req: Request) -> Response {
    let id = req.id.clone();
    match handle(eng, &req) {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response::err(id, e.code, e.message),
    }
}

struct Err_ {
    code: &'static str,
    message: String,
}

type R = Result<Value, Err_>;

fn bad(msg: impl Into<String>) -> Err_ {
    Err_ { code: "bad_request", message: msg.into() }
}

fn not_found(msg: impl Into<String>) -> Err_ {
    Err_ { code: "not_found", message: msg.into() }
}

fn failed(msg: impl Into<String>) -> Err_ {
    Err_ { code: "failed", message: msg.into() }
}

/// `params.pane`, falling back to `HORDE_PANE`-style self-reference then the focused pane.
fn pane_arg(eng: &Engine, req: &Request, key: &str) -> Option<PaneId> {
    if let Some(v) = req.params.get(key) {
        if let Some(n) = v.as_u64() {
            return Some(n as u32);
        }
        if let Some(s) = v.as_str() {
            return super::bus::Bus::resolve(&eng.session, s);
        }
    }
    None
}

fn str_arg<'a>(req: &'a Request, key: &str) -> Option<&'a str> {
    req.params.get(key).and_then(|v| v.as_str())
}

fn bool_arg(req: &Request, key: &str) -> bool {
    req.params.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn dir_arg(req: &Request, key: &str, default: Dir) -> Dir {
    match str_arg(req, key) {
        Some("left") => Dir::Left,
        Some("right") => Dir::Right,
        Some("up") => Dir::Up,
        Some("down") => Dir::Down,
        _ => default,
    }
}

fn handle(eng: &mut Engine, req: &Request) -> R {
    match req.method.as_str() {
        // -- server ------------------------------------------------------
        "ping" => Ok(json!({ "type": "pong", "protocol": crate::proto::PROTOCOL_VERSION })),
        "server.stop" => Ok(json!({ "stopping": true })),
        "server.reload_config" => {
            let (cfg, warnings) = crate::config::Config::load();
            eng.cfg = cfg;
            eng.agents.reload();
            let cfg = eng.cfg.clone();
            // Panel visibility lives in ViewState, so a reload has to push the new config
            // into it or a settings change would not take effect until restart.
            eng.session.view.sidebar_open = cfg.sidebar;
            eng.session.view.bus_open = cfg.bus;
            eng.session.view.sidebar_width = cfg.sidebar_width;
            eng.session.view.bus_width = cfg.bus_width;
            eng.session.relayout(&cfg);
            eng.touch();
            let mut all = warnings;
            all.extend(eng.agents.warnings.clone());
            Ok(json!({ "reloaded": true, "warnings": all }))
        }
        "server.status" => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": crate::proto::PROTOCOL_VERSION,
            "spaces": eng.session.spaces.len(),
            "tabs": eng.session.tabs.len(),
            "panes": eng.session.panes.len(),
            "agents": eng.session.panes.values().filter(|p| p.agent.is_some()).count(),
            "socket": crate::config::socket_path().to_string_lossy(),
            "theme": eng.cfg.theme.name,
            "agent_manifests": eng.agents.manifest_names(),
        })),

        // -- session -----------------------------------------------------
        "session.snapshot" => {
            let cfg = eng.cfg.clone();
            serde_json::to_value(eng.session.snapshot(&cfg)).map_err(|e| failed(e.to_string()))
        }

        // -- spaces ------------------------------------------------------
        "space.list" => {
            let cfg = eng.cfg.clone();
            Ok(json!(eng.session.snapshot(&cfg).spaces))
        }
        "space.create" => {
            let cwd = str_arg(req, "cwd")
                .map(std::path::PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .ok_or_else(|| bad("no cwd"))?;
            let cfg = eng.cfg.clone();
            let id = eng
                .session
                .create_space(&cfg, str_arg(req, "name"), &cwd)
                .map_err(|e| failed(e.to_string()))?;
            eng.touch();
            eng.detect_now();
            Ok(json!({ "space": id }))
        }
        "space.focus" => {
            let name = str_arg(req, "name").ok_or_else(|| bad("name required"))?;
            let id = eng
                .session
                .find_space_by_name(name)
                .ok_or_else(|| not_found(format!("no space called {name:?}")))?;
            eng.session.focus_space(id);
            eng.touch();
            Ok(json!({ "space": id }))
        }
        "space.close" => {
            let name = str_arg(req, "name").ok_or_else(|| bad("name required"))?;
            let id = eng
                .session
                .find_space_by_name(name)
                .ok_or_else(|| not_found(format!("no space called {name:?}")))?;
            let cfg = eng.cfg.clone();
            eng.session.close_space(&cfg, id).map_err(|e| failed(e.to_string()))?;
            eng.touch();
            Ok(json!({ "closed": id }))
        }

        // -- tabs --------------------------------------------------------
        "tab.list" => {
            let cfg = eng.cfg.clone();
            Ok(json!(eng.session.snapshot(&cfg).tabs))
        }
        "tab.create" => {
            let space = eng
                .session
                .focused_space
                .ok_or_else(|| failed("no focused space"))?;
            let cfg = eng.cfg.clone();
            let id = eng
                .session
                .create_tab(&cfg, space, str_arg(req, "name"))
                .map_err(|e| failed(e.to_string()))?;
            eng.touch();
            eng.detect_now();
            Ok(json!({ "tab": id }))
        }
        "tab.close" => {
            let t = eng.session.focused_tab().ok_or_else(|| failed("no focused tab"))?;
            let cfg = eng.cfg.clone();
            eng.session.close_tab(&cfg, t).map_err(|e| failed(e.to_string()))?;
            eng.touch();
            Ok(json!({ "closed": t }))
        }

        // -- panes -------------------------------------------------------
        "pane.list" => {
            let cfg = eng.cfg.clone();
            Ok(json!(eng.session.snapshot(&cfg).panes))
        }
        "pane.current" => Ok(json!({ "pane": eng.session.focused_pane() })),
        "pane.split" => {
            let target = pane_arg(eng, req, "pane");
            let dir = dir_arg(req, "direction", Dir::Right);
            let cfg = eng.cfg.clone();
            let id = eng
                .session
                .split(&cfg, target, dir, str_arg(req, "cmd"))
                .map_err(|e| failed(e.to_string()))?;
            if let (Some(n), Some(p)) = (str_arg(req, "name"), eng.session.panes.get_mut(&id)) {
                p.name = Some(n.to_string());
            }
            eng.touch();
            eng.detect_now();
            Ok(json!({ "pane": id }))
        }
        "pane.close" => {
            let p = pane_arg(eng, req, "pane")
                .or_else(|| eng.session.focused_pane())
                .ok_or_else(|| bad("no pane"))?;
            let cfg = eng.cfg.clone();
            eng.session.close_pane(&cfg, p).map_err(|e| failed(e.to_string()))?;
            eng.touch();
            Ok(json!({ "closed": p }))
        }
        "pane.focus" => {
            let p = pane_arg(eng, req, "pane").ok_or_else(|| bad("pane required"))?;
            if !eng.session.focus_pane(p) {
                return Err(not_found("no such pane"));
            }
            eng.mark_seen(p);
            eng.touch();
            Ok(json!({ "focused": p }))
        }
        "pane.rename" => {
            let p = pane_arg(eng, req, "pane").ok_or_else(|| bad("pane required"))?;
            let name = str_arg(req, "name").unwrap_or("").to_string();
            let pane = eng.session.panes.get_mut(&p).ok_or_else(|| not_found("no such pane"))?;
            pane.name = if name.is_empty() { None } else { Some(name) };
            eng.touch();
            Ok(json!({ "renamed": p }))
        }
        "pane.read" => {
            let p = pane_arg(eng, req, "pane")
                .or_else(|| eng.session.focused_pane())
                .ok_or_else(|| bad("no pane"))?;
            let lines = req.params.get("lines").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let source = str_arg(req, "source").unwrap_or("visible");
            let pane = eng.session.panes.get(&p).ok_or_else(|| not_found("no such pane"))?;
            let text = match source {
                // What detection sees: the live bottom of the buffer, blanks trimmed.
                "detection" => pane.detection_snapshot(lines),
                "recent" => {
                    let all = pane.visible_text();
                    let start = all.len().saturating_sub(lines);
                    all[start..].to_vec()
                }
                _ => pane.visible_text(),
            };
            Ok(json!({ "pane": p, "source": source, "lines": text }))
        }
        "pane.send_text" | "pane.send-text" => {
            let p = pane_arg(eng, req, "pane")
                .or_else(|| eng.session.focused_pane())
                .ok_or_else(|| bad("no pane"))?;
            let text = str_arg(req, "text").ok_or_else(|| bad("text required"))?;
            let submit = bool_arg(req, "submit");
            let pane = eng.session.panes.get_mut(&p).ok_or_else(|| not_found("no such pane"))?;
            pane.write(text.as_bytes()).map_err(|e| failed(e.to_string()))?;
            if submit {
                // Enter goes as its own write, or the agent reads text+CR as a paste and
                // inserts a newline instead of submitting. See bus::SUBMIT_DELAY.
                pane.write_later(vec![b'\r'], super::bus::SUBMIT_DELAY);
            }
            Ok(json!({ "sent": p }))
        }
        "pane.report_agent" | "pane.report-agent" => {
            let p = pane_arg(eng, req, "pane")
                .or_else(|| eng.session.focused_pane())
                .ok_or_else(|| bad("no pane"))?;
            let state = match str_arg(req, "state") {
                Some("working") => AgentState::Working,
                Some("blocked") => AgentState::Blocked,
                Some("idle") => AgentState::Idle,
                Some("done") => AgentState::Done,
                Some(other) => return Err(bad(format!("unknown state {other:?}"))),
                None => return Err(bad("state required")),
            };
            let session_id = str_arg(req, "session").map(|s| s.to_string());
            let Engine { agents, session, .. } = eng;
            let ev = agents.report(session, p, state, session_id);
            if let Some(ev) = ev {
                eng.emit(ev);
            }
            eng.touch();
            Ok(json!({ "reported": p, "state": state.label() }))
        }
        "pane.scroll" => {
            let p = pane_arg(eng, req, "pane")
                .or_else(|| eng.session.focused_pane())
                .ok_or_else(|| bad("no pane"))?;
            let lines = req.params.get("lines").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let pane = eng.session.panes.get_mut(&p).ok_or_else(|| not_found("no such pane"))?;
            if lines == 0 {
                pane.scroll_bottom();
            } else {
                pane.scroll(lines);
            }
            Ok(json!({ "offset": pane.scroll_offset() }))
        }

        // -- layout ------------------------------------------------------
        "layout.apply" => {
            let preset = str_arg(req, "preset").ok_or_else(|| bad("preset required"))?;
            let cfg = eng.cfg.clone();
            eng.session.apply_preset(&cfg, preset).map_err(|e| failed(e.to_string()))?;
            eng.touch();
            Ok(json!({ "applied": preset }))
        }

        // -- agents ------------------------------------------------------
        "agent.list" | "roster" => {
            let mut out: Vec<Value> = Vec::new();
            // Ordered by space then tab so the roster reads like the sidebar.
            for s in &eng.session.spaces {
                for &t in &s.tabs {
                    let Some(tab) = eng.session.tab(t) else { continue };
                    for pid in tab.layout.panes() {
                        let Some(p) = eng.session.panes.get(&pid) else { continue };
                        let Some(a) = p.agent.as_ref() else { continue };
                        out.push(json!({
                            "name": a.name,
                            "kind": a.kind,
                            "state": a.state.label(),
                            "elapsed": a.since.elapsed().as_secs(),
                            "authority": a.authority,
                            "reason": a.reason,
                            "queued": a.queued.len(),
                            "space": s.name,
                            "tab": tab.name,
                            "pane": pid,
                            "cwd": p.cwd.to_string_lossy(),
                        }));
                    }
                }
            }
            Ok(json!(out))
        }
        "agent.explain" => {
            let p = pane_arg(eng, req, "pane")
                .or_else(|| eng.session.focused_pane())
                .ok_or_else(|| bad("no pane"))?;
            Ok(eng.agents.explain(&eng.session, p, &eng.cfg))
        }
        "agent.spawn" => {
            let cmd = str_arg(req, "cmd").unwrap_or("claude").to_string();
            let dir = dir_arg(req, "split", Dir::Right);
            let cfg = eng.cfg.clone();
            let id = eng
                .session
                .split(&cfg, None, dir, Some(&cmd))
                .map_err(|e| failed(e.to_string()))?;
            if let (Some(n), Some(p)) = (str_arg(req, "name"), eng.session.panes.get_mut(&id)) {
                p.name = Some(n.to_string());
            }
            eng.touch();
            eng.detect_now();
            Ok(json!({ "pane": id, "cmd": cmd }))
        }
        "agent.wait" => {
            // Waiting blocks the caller, which would stall the single-threaded engine. The
            // CLI polls `agent.list` instead; this exists so the error explains itself.
            Err(bad("use the CLI's `horde wait`, which polls rather than blocking the daemon"))
        }

        // -- bus ---------------------------------------------------------
        "bus.send" => {
            let to = str_arg(req, "to").ok_or_else(|| bad("to required"))?;
            let body = str_arg(req, "body").ok_or_else(|| bad("body required"))?;
            let from = pane_arg(eng, req, "from");
            let force = bool_arg(req, "force");
            let cfg = eng.cfg.clone();
            let Engine { bus, session, .. } = eng;
            let msg = bus
                .send(session, &cfg, from, to, body, force)
                .map_err(|e| failed(e.to_string()))?;
            eng.emit(crate::proto::Event::BusMessage(msg.clone()));
            serde_json::to_value(msg).map_err(|e| failed(e.to_string()))
        }
        "bus.broadcast" => {
            let body = str_arg(req, "body").ok_or_else(|| bad("body required"))?;
            let from = pane_arg(eng, req, "from");
            let space = str_arg(req, "space").and_then(|s| eng.session.find_space_by_name(s));
            let cfg = eng.cfg.clone();
            let Engine { bus, session, .. } = eng;
            let msgs = bus.broadcast(session, &cfg, from, space, body);
            for m in &msgs {
                eng.emit(crate::proto::Event::BusMessage(m.clone()));
            }
            serde_json::to_value(msgs).map_err(|e| failed(e.to_string()))
        }
        "bus.tail" => {
            let n = req.params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            serde_json::to_value(eng.bus.recent(n)).map_err(|e| failed(e.to_string()))
        }

        // -- commands ----------------------------------------------------
        // Everything a keybinding can do is also reachable by name, which is what makes
        // the command palette and scripting share one implementation.
        "command" => {
            let name = str_arg(req, "name").ok_or_else(|| bad("name required"))?;
            let cmd = command_by_name(name, req)
                .ok_or_else(|| bad(format!("unknown command {name:?}")))?;
            apply_cmd(eng, cmd);
            Ok(json!({ "ran": name }))
        }

        other => Err(not_found(format!("unknown method {other:?}"))),
    }
}

fn command_by_name(name: &str, req: &Request) -> Option<Cmd> {
    Some(match name {
        "split-right" => Cmd::SplitRight,
        "split-down" => Cmd::SplitDown,
        "close-pane" => Cmd::ClosePane,
        "zoom" => Cmd::ToggleZoom,
        "focus-left" => Cmd::FocusDir(Dir::Left),
        "focus-right" => Cmd::FocusDir(Dir::Right),
        "focus-up" => Cmd::FocusDir(Dir::Up),
        "focus-down" => Cmd::FocusDir(Dir::Down),
        "new-tab" => Cmd::NewTab,
        "next-tab" => Cmd::NextTab,
        "prev-tab" => Cmd::PrevTab,
        "close-tab" => Cmd::CloseTab,
        "new-space" => Cmd::NewSpace { name: str_arg(req, "name").map(|s| s.to_string()) },
        "next-space" => Cmd::NextSpace,
        "prev-space" => Cmd::PrevSpace,
        "toggle-sidebar" => Cmd::ToggleSidebar,
        "toggle-bus" => Cmd::ToggleBus,
        "jump-attention" => Cmd::JumpAttention,
        _ => return None,
    })
}

/// Names the command palette offers.
pub fn command_names() -> &'static [&'static str] {
    &[
        "split-right",
        "split-down",
        "close-pane",
        "zoom",
        "focus-left",
        "focus-right",
        "focus-up",
        "focus-down",
        "new-tab",
        "next-tab",
        "prev-tab",
        "close-tab",
        "new-space",
        "next-space",
        "prev-space",
        "toggle-sidebar",
        "toggle-bus",
        "jump-attention",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_palette_name_maps_to_a_command() {
        let req = Request { id: String::new(), method: String::new(), params: json!({}) };
        for name in command_names() {
            assert!(command_by_name(name, &req).is_some(), "{name} has no command");
        }
        assert!(command_by_name("nonsense", &req).is_none());
    }

    #[test]
    fn dir_arg_defaults_when_absent_or_invalid() {
        let req = Request { id: String::new(), method: String::new(), params: json!({}) };
        assert_eq!(dir_arg(&req, "direction", Dir::Right), Dir::Right);

        let req = Request {
            id: String::new(),
            method: String::new(),
            params: json!({ "direction": "up" }),
        };
        assert_eq!(dir_arg(&req, "direction", Dir::Right), Dir::Up);

        let req = Request {
            id: String::new(),
            method: String::new(),
            params: json!({ "direction": "sideways" }),
        };
        assert_eq!(dir_arg(&req, "direction", Dir::Down), Dir::Down);
    }
}
