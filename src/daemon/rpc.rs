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

fn trigger_arg(req: &Request) -> Result<u64, Err_> {
    req.params
        .get("trigger")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| bad("trigger id required"))
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

            // Activity is recorded after the state report, so the agent record exists.
            if let Some(agent) = session.panes.get_mut(&p).and_then(|x| x.agent.as_mut()) {
                if bool_arg(req, "new_turn") {
                    agent.begin_turn();
                }
                if bool_arg(req, "counts_tool") {
                    agent.record_tool(str_arg(req, "tool"), str_arg(req, "file"));
                }
                if bool_arg(req, "tool_failed") {
                    agent.record_error();
                }
            }
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
                        "activity": a.activity,
                            "space": s.name,
                            "tab": tab.name,
                            "pane": pid,
                            "cwd": p.cwd.to_string_lossy(),
                            // Whether you started this one or horde did. The roster is where
                            // you look to answer "who is in here", so it has to say.
                            "spawned_by": p.spawned_by,
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
            let expects_reply = bool_arg(req, "expects_reply");
            let cfg = eng.cfg.clone();
            let Engine { bus, session, .. } = eng;
            let msg = bus
                .send(session, &cfg, from, to, body, force, expects_reply, None)
                .map_err(|e| failed(e.to_string()))?;
            eng.emit(crate::proto::Event::BusMessage(msg.clone()));
            serde_json::to_value(msg).map_err(|e| failed(e.to_string()))
        }

        // Answer a request. The target is whoever sent it, so a replying agent needs to know
        // only the request number it was given.
        "bus.reply" => {
            let id = req
                .params
                .get("request")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| bad("request id required"))?;
            let body = str_arg(req, "body").ok_or_else(|| bad("body required"))?;
            let from = pane_arg(eng, req, "from");
            let cfg = eng.cfg.clone();

            let original = eng
                .bus
                .message(id)
                .ok_or_else(|| not_found(format!("no request #{id} in the log")))?;
            if !original.expects_reply {
                return Err(bad(format!("message #{id} was not a request")));
            }
            // Reply to the sender by name; they may have moved pane since asking.
            let Engine { bus, session, .. } = eng;
            let msg = match crate::daemon::bus::Bus::resolve(session, &original.from) {
                Some(_) => bus
                    .send(session, &cfg, from, &original.from, body, false, false, Some(id))
                    .map_err(|e| failed(e.to_string()))?,
                // The asker has no pane to type into — a `horde ask` from a plain shell. It
                // is polling for the answer, so recording it is what delivers it.
                None => bus.record_reply(session, from, &original.from, body, id),
            };
            eng.emit(crate::proto::Event::BusMessage(msg.clone()));
            serde_json::to_value(msg).map_err(|e| failed(e.to_string()))
        }

        // Has a reply landed yet? The CLI polls this rather than blocking the engine.
        "bus.reply_for" => {
            let id = req
                .params
                .get("request")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| bad("request id required"))?;
            Ok(match eng.bus.reply_for(id) {
                Some(m) => serde_json::to_value(m).map_err(|e| failed(e.to_string()))?,
                None => Value::Null,
            })
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

        // -- tasks -------------------------------------------------------
        // A board agents pull from, rather than a queue you push to. See daemon/tasks.rs.
        "task.add" => {
            let text = str_arg(req, "text").ok_or_else(|| bad("text required"))?;
            let by = super::bus::Bus::sender_name(&eng.session, pane_arg(eng, req, "from"));
            let t = eng.board.add(text, &by).map_err(|e| failed(e.to_string()))?;
            eng.touch();
            serde_json::to_value(t).map_err(|e| failed(e.to_string()))
        }
        "task.claim" => {
            let owner = super::bus::Bus::sender_name(&eng.session, pane_arg(eng, req, "from"));
            let id = req.params.get("task").and_then(|v| v.as_u64());
            match eng.board.claim(&owner, id).map_err(|e| failed(e.to_string()))? {
                Some(t) => {
                    eng.touch();
                    serde_json::to_value(t).map_err(|e| failed(e.to_string()))
                }
                // Nothing to do is a result, not a failure: an agent looping on the board
                // has to be able to tell the difference.
                None => Ok(Value::Null),
            }
        }
        "task.done" => {
            let owner = super::bus::Bus::sender_name(&eng.session, pane_arg(eng, req, "from"));
            let id = req.params.get("task").and_then(|v| v.as_u64());
            let t = eng
                .board
                .done(&owner, id, str_arg(req, "result"))
                .map_err(|e| failed(e.to_string()))?;
            eng.touch();
            serde_json::to_value(t).map_err(|e| failed(e.to_string()))
        }
        "task.release" => {
            let id = req
                .params
                .get("task")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| bad("task id required"))?;
            let t = eng
                .board
                .release(id, bool_arg(req, "drop"))
                .map_err(|e| failed(e.to_string()))?;
            eng.touch();
            serde_json::to_value(t).map_err(|e| failed(e.to_string()))
        }
        "task.list" => {
            serde_json::to_value(eng.board.all()).map_err(|e| failed(e.to_string()))
        }

        // -- triggers ----------------------------------------------------
        // Scheduled rules. Parsing lives in the daemon rather than the CLI so that an agent
        // calling this method gets the same validation, and the same error text, as a person.
        "trigger.add" => {
            let when = match (str_arg(req, "every"), str_arg(req, "at")) {
                (Some(e), None) => super::triggers::parse_every(e).map_err(|e| bad(e.to_string()))?,
                (None, Some(a)) => {
                    let mut w = super::triggers::parse_at(a).map_err(|e| bad(e.to_string()))?;
                    if let (Some(spec), super::triggers::When::At { days, .. }) =
                        (str_arg(req, "days"), &mut w)
                    {
                        *days = super::triggers::parse_days(spec).map_err(|e| bad(e.to_string()))?;
                    }
                    w
                }
                (Some(_), Some(_)) => return Err(bad("give either every or at, not both")),
                (None, None) => return Err(bad("a trigger needs every or at")),
            };
            if str_arg(req, "days").is_some() && !matches!(when, super::triggers::When::At { .. }) {
                return Err(bad("days only applies to at — an interval has no day to land on"));
            }
            let what = match (str_arg(req, "task"), str_arg(req, "to"), str_arg(req, "spawn")) {
                (Some(text), None, None) => super::triggers::What::Task { text: text.to_string() },
                (None, Some(to), None) => {
                    let body = str_arg(req, "body").ok_or_else(|| bad("body required with to"))?;
                    super::triggers::What::Send { to: to.to_string(), body: body.to_string() }
                }
                (None, None, Some(cmd)) => super::triggers::What::Spawn {
                    cmd: cmd.to_string(),
                    name: str_arg(req, "name").map(|s| s.to_string()),
                },
                (None, None, None) => return Err(bad("a trigger needs task, to, or spawn")),
                _ => return Err(bad("give exactly one of task, to, or spawn")),
            };

            // The depth guard. An agent creating a trigger is the interesting part; a
            // machine-started agent creating one closes the loop with no human anywhere in it,
            // and nothing downstream would ever refuse it.
            let from = pane_arg(eng, req, "from");
            if let Some(origin) = from.and_then(|p| eng.session.panes.get(&p)) {
                if let Some(by_trigger) = origin.spawned_by {
                    return Err(bad(format!(
                        "this pane was started by trigger #{by_trigger}, so it cannot create \
                         triggers — put work on the board instead"
                    )));
                }
            }
            let by = super::bus::Bus::sender_name(&eng.session, from);
            let only_if = str_arg(req, "when").map(|s| s.to_string());
            let t = eng
                .triggers
                .add(when, what, &by, only_if)
                .map_err(|e| failed(e.to_string()))?;
            eng.touch();
            // `armed` travels with the reply so the caller never has to re-read the config file
            // and guess whether the daemon agrees with it.
            Ok(json!({ "trigger": t, "armed": eng.cfg.unattended }))
        }
        "trigger.list" => {
            let all: Vec<_> = eng.triggers.all().collect();
            Ok(json!({ "triggers": all, "armed": eng.cfg.unattended }))
        }
        "trigger.rm" => {
            let id = trigger_arg(req)?;
            let t = eng.triggers.remove(id).map_err(|e| not_found(e.to_string()))?;
            eng.touch();
            serde_json::to_value(t).map_err(|e| failed(e.to_string()))
        }
        "trigger.enable" => {
            // `all: false` is the kill switch, and has to work without naming anything.
            if req.params.get("trigger").is_none() {
                if bool_arg(req, "on") {
                    return Err(bad("name a trigger to turn on, or use `off --all`"));
                }
                let off = eng.triggers.disable_all();
                eng.touch();
                return Ok(json!({ "disabled": off.len() }));
            }
            let id = trigger_arg(req)?;
            let t = eng
                .triggers
                .set_enabled(id, bool_arg(req, "on"))
                .map_err(|e| not_found(e.to_string()))?;
            eng.touch();
            serde_json::to_value(t).map_err(|e| failed(e.to_string()))
        }
        "trigger.fire" => {
            let id = trigger_arg(req)?;
            let (what, events) =
                super::triggers::fire_now(eng, id).map_err(|e| failed(e.to_string()))?;
            for ev in events {
                eng.emit(ev);
            }
            eng.touch();
            Ok(json!({ "trigger": id, "did": what }))
        }

        // -- digest ------------------------------------------------------
        // What happened while nobody was watching. Reading it advances the watermark, so
        // the window is always "since you last looked" — `keep` leaves it where it was, for
        // the client's own on-attach peek.
        "digest" => {
            let since = match req.params.get("since").and_then(|v| v.as_u64()) {
                // A caller-supplied window is a lookback in seconds, which is what a human
                // means by `--since 30m`.
                Some(secs) => super::now_millis().saturating_sub(secs * 1000),
                // First ever read has no watermark; fall back to the daemon's own start so
                // the first digest covers this session rather than all of history.
                None if eng.last_seen == 0 => eng.started,
                None => eng.last_seen,
            };
            let d = super::digest::build(eng, since);
            if !bool_arg(req, "keep") {
                eng.last_seen = super::now_millis();
                eng.touch();
            }
            serde_json::to_value(d).map_err(|e| failed(e.to_string()))
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
        "digest" => Cmd::RequestDigest,
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
        "digest",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The depth guard. An agent creating a rule is the interesting part of letting agents
    /// create rules; a *machine-started* agent creating one closes the loop with no human
    /// anywhere in it, and nothing further down would refuse it.
    #[test]
    fn an_agent_horde_started_itself_cannot_create_triggers() {
        let mut eng = super::super::tests::engine_with_idle_agents("rpc-depth", 1);
        let pane = *eng.session.panes.keys().next().unwrap();

        let add = |pane| Request {
            id: String::new(),
            method: "trigger.add".into(),
            params: json!({ "every": "1h", "task": "look busy", "from": pane }),
        };

        // A pane you started: allowed.
        let resp = dispatch(&mut eng, add(pane));
        assert!(resp.error.is_none(), "{:?}", resp.error);

        // The same pane, now marked as one horde started: refused, and told why.
        eng.session.panes.get_mut(&pane).unwrap().spawned_by = Some(7);
        let resp = dispatch(&mut eng, add(pane));
        let err = resp.error.expect("a machine-started pane must be refused").message;
        assert!(err.contains("trigger #7"), "the error should name the origin: {err}");
        assert!(err.contains("board"), "and point at what to do instead: {err}");

        // Putting work on the board is still fine — only rule-making is closed off.
        let resp = dispatch(
            &mut eng,
            Request {
                id: String::new(),
                method: "task.add".into(),
                params: json!({ "text": "real work", "from": pane }),
            },
        );
        assert!(resp.error.is_none(), "{:?}", resp.error);

        for p in eng.session.panes.values_mut() {
            p.kill();
        }
    }

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
