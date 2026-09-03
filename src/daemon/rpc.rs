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

#[derive(Debug)]
struct Err_ {
    code: &'static str,
    message: String,
}

type R = Result<Value, Err_>;

/// The project a call belongs to, by space name: the calling pane's, else the focused one.
///
/// Board work is scoped to a project, and this is where the scope comes from. Taking it from
/// the caller rather than from an argument is what makes `horde task add` do the obvious thing
/// from inside a pane, which is where agents run it.
fn caller_space(eng: &Engine, req: &Request) -> Option<String> {
    let pane = pane_arg(eng, req, "from").or_else(|| eng.session.focused_pane())?;
    let space = eng.session.panes.get(&pane)?.space;
    eng.session.space(space).map(|s| s.name.clone())
}

/// Which directory worktree commands act on: the focused pane's, so `horde worktree list`
/// answers about the project you are looking at rather than wherever the daemon started.
/// The directory a worktree operation is about: the *caller's* project.
///
/// The calling pane when the request names one (`from`, which the CLI always sends from
/// inside a pane), the focused pane only as the fallback for a human typing outside horde.
/// The focused pane is whatever the human happens to be looking at, in any space — an agent
/// orchestrating a fleet in project A must not have its worktrees rooted in project B
/// because the human clicked over there mid-spawn.
fn worktree_origin(eng: &Engine, req: &Request) -> Result<std::path::PathBuf, Err_> {
    pane_arg(eng, req, "from")
        .or_else(|| eng.session.focused_pane())
        .and_then(|p| eng.session.panes.get(&p))
        .map(|p| p.cwd.clone())
        .ok_or_else(|| failed("no pane to take a directory from"))
}

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

/// A space, by name or by id, defaulting to the focused one.
///
/// Names are how spaces are addressed everywhere else (`space focus`, `horde broadcast
/// --space`), so they come first; the id is there for a caller holding a snapshot.
fn space_arg(eng: &Engine, req: &Request) -> Result<crate::proto::SpaceId, Err_> {
    if let Some(name) = str_arg(req, "name") {
        return eng
            .session
            .find_space_by_name(name)
            .ok_or_else(|| not_found(format!("no space called {name:?}")));
    }
    if let Some(id) = req.params.get("space").and_then(|v| v.as_u64()) {
        return Ok(id as u32);
    }
    eng.session.focused_space.ok_or_else(|| bad("no space"))
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
    // The board can be switched off without switching off the bus. They are separate promises:
    // messaging is agents talking to each other, and the board is agents *taking work* nobody
    // watched them take. Someone may reasonably want the first without the second.
    //
    // Enforced here rather than by leaving it out of the skill, because an agent that reads
    // about `horde task claim` anywhere else would otherwise find it working.
    if req.method.starts_with("task.") && !eng.cfg.board {
        return Err(failed(
            "the task board is off — set agents.board = true in config.toml to enable it",
        ));
    }
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
            // Colours are resolved into the mirror when a row is built, so a pane already on
            // screen is holding rows painted in the old palette and nothing about them has
            // changed — the client is sent nothing and the terminal keeps the previous theme
            // until whatever is running happens to redraw. Rebuilding every row against the
            // new theme is what makes a theme change visible without a `clear`.
            let theme = cfg.theme.clone();
            for p in eng.session.panes.values_mut() {
                p.set_theme(&theme);
                p.request_full_repaint();
            }
            eng.touch();
            let mut all = warnings;
            all.extend(eng.agents.warnings.clone());
            // Raised as notices, not just returned. A reload is usually triggered from the
            // settings screen, which does not read this reply — so a config horde could not
            // use went unmentioned, and the only symptom was a setting that did nothing.
            for w in &all {
                eng.notice(crate::proto::NoticeLevel::Warn, format!("config: {w}"));
            }
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
            // Which languages this binary can colour. Grammars are compile-time features,
            // so "why is my Rust not highlighted" is answerable without guessing at how it
            // was built.
            "languages": crate::client::syntax::available(),
            // The clock triggers actually fire on. `--at 09:00` means nine *here*, and a distro
            // whose timezone was never set sits on UTC while the person setting the trigger does
            // not — which looks like triggers firing at random rather than like a wrong clock.
            // Printing it is the cheapest way for that to be noticed before it matters.
            "local_time": super::triggers::local_clock(super::now_millis()),
            // Three descriptors per pane, and `horde upgrade` briefly needs one more each. When
            // that fails it fails as `Too many open files` during a handoff, with the process
            // that could have been measured already gone — so the number is reported while
            // things are working, not looked for afterwards.
            "open_files": crate::platform::file_limit(),
        })),

        // -- session -----------------------------------------------------
        "session.snapshot" => {
            serde_json::to_value(eng.snapshot()).map_err(|e| failed(e.to_string()))
        }

        // -- spaces ------------------------------------------------------
        "space.list" => {
            let cfg = eng.cfg.clone();
            Ok(json!(eng.session.snapshot(&cfg, &eng.repos).spaces))
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
        // Renaming a space existed only as a client frame, unreachable from a script while
        // `pane.rename` was fully exposed. Answering with the name it actually got matters
        // because a clash is uniquified silently, and the caller has no other way to find out.
        "space.rename" => {
            let name = str_arg(req, "name").ok_or_else(|| bad("name required"))?;
            let to = str_arg(req, "to").ok_or_else(|| bad("to required"))?;
            let id = eng
                .session
                .find_space_by_name(name)
                .ok_or_else(|| not_found(format!("no space called {name:?}")))?;
            if !eng.session.rename_space(id, to) {
                return Err(bad("a space needs a name"));
            }
            let actual = eng.session.space(id).map(|s| s.name.clone()).unwrap_or_default();
            eng.touch();
            Ok(json!({ "renamed": id, "name": actual }))
        }
        "space.accent" => {
            let id = space_arg(eng, req)?;
            let slot = req.params.get("slot").and_then(|v| v.as_u64()).map(|v| v as u8);
            let slot = eng
                .session
                .set_space_accent(id, slot)
                .ok_or_else(|| not_found("no such space"))?;
            eng.touch();
            Ok(json!({ "space": id, "slot": slot }))
        }
        "space.collapse" => {
            let id = space_arg(eng, req)?;
            let to = req.params.get("collapsed").and_then(|v| v.as_bool());
            let now = eng
                .session
                .toggle_space_collapsed(id, to)
                .ok_or_else(|| not_found("no such space"))?;
            eng.touch();
            Ok(json!({ "space": id, "collapsed": now }))
        }

        // -- tabs --------------------------------------------------------
        "tab.list" => {
            let cfg = eng.cfg.clone();
            Ok(json!(eng.session.snapshot(&cfg, &eng.repos).tabs))
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
        "tab.rename" => {
            let t = req
                .params
                .get("tab")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .or_else(|| eng.session.focused_tab())
                .ok_or_else(|| bad("no tab"))?;
            let name = str_arg(req, "name").ok_or_else(|| bad("name required"))?;
            if !eng.session.rename_tab(t, name) {
                return Err(bad("a tab needs a name"));
            }
            eng.touch();
            Ok(json!({ "renamed": t }))
        }

        // -- panes -------------------------------------------------------
        // A pane showing a file reports the file, so `pane.list` answers "what is this"
        // for both kinds rather than only for programs.
        "pane.list" => {
            let cfg = eng.cfg.clone();
            Ok(json!(eng.session.snapshot(&cfg, &eng.repos).panes))
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
        // The normalised name comes back, because the caller has to be able to learn what it
        // actually got — `Code Reviewer` is stored as `code-reviewer`, and a script that
        // filters on the role it just set would otherwise never match.
        "pane.role" => {
            let p = pane_arg(eng, req, "pane")
                .or_else(|| eng.session.focused_pane())
                .ok_or_else(|| bad("no pane"))?;

            // An agent may not relabel anything, including itself.
            //
            // A role decides what work the board offers you and, where `agents.task_authors` is
            // set, whether you may put work on it at all. The moment it decides either of those,
            // an agent that can run `horde pane role` can promote itself into the role that
            // decides — and it would have been *right* to, from inside its own reasoning, which
            // is what makes it a hole rather than a bug.
            //
            // Assignment therefore happens at creation (`spawn --role`) or from the human's own
            // UI, which reaches this through a different path entirely. The test is "is there an
            // agent in the calling pane", the same distinction horde draws everywhere else: a
            // person at a shell inside horde is still a person.
            if let Some(caller) = pane_arg(eng, req, "from") {
                if eng.session.panes.get(&caller).is_some_and(|c| c.agent.is_some()) {
                    return Err(failed(
                        "an agent cannot set a role — a role decides what work is offered to \
                         whom. Give one at spawn (`horde spawn --role <role>`), or ask your human",
                    ));
                }
            }

            let role = str_arg(req, "role").unwrap_or("");
            let now = eng.session.set_pane_role(p, role).ok_or_else(|| not_found("no such pane"))?;
            eng.touch();
            Ok(json!({ "pane": p, "role": now }))
        }
        "pane.pin" => {
            let p = pane_arg(eng, req, "pane")
                .or_else(|| eng.session.focused_pane())
                .ok_or_else(|| bad("no pane"))?;
            let to = req.params.get("pinned").and_then(|v| v.as_bool());
            let now =
                eng.session.toggle_pane_pinned(p, to).ok_or_else(|| not_found("no such pane"))?;
            eng.touch();
            Ok(json!({ "pane": p, "pinned": now }))
        }
        // Every role in use, and how many panes wear it. The payoff for a role being a name
        // rather than a note: "who is reviewing, across every project" is one call rather than
        // a walk of the whole session.
        "role.list" => {
            let mut counts: std::collections::BTreeMap<String, usize> =
                eng.cfg.roles.iter().map(|r| (r.name.clone(), 0)).collect();
            for p in eng.session.panes.values() {
                if let Some(r) = &p.role {
                    *counts.entry(r.clone()).or_insert(0) += 1;
                }
            }
            let declared: std::collections::HashSet<&str> =
                eng.cfg.roles.iter().map(|r| r.name.as_str()).collect();
            Ok(json!(counts
                .into_iter()
                .map(|(name, panes)| json!({
                    "declared": declared.contains(name.as_str()),
                    "name": name,
                    "panes": panes,
                }))
                .collect::<Vec<_>>()))
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
                            // "agent" or "service". A dev server appears in the roster
                            // because you want to see it; it is not one of your agents.
                            "class": a.class,
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
            let cfg = eng.cfg.clone();
            // A profile names a list of models rather than a command, and beats `--cmd` when
            // both are given: asking for a profile is the more specific request.
            //
            // Refused rather than defaulted when the name is unknown. A typo that silently
            // started `claude` on someone's Anthropic key, when they asked for the free tier,
            // is the wrong direction to fail in.
            let profile_name = str_arg(req, "profile").map(|s| s.to_string());
            let cmd = match str_arg(req, "profile") {
                Some(p) => {
                    let profile = cfg.models.get(p).ok_or_else(|| {
                        let mut known: Vec<&str> = cfg.models.keys().map(|s| s.as_str()).collect();
                        known.sort();
                        failed(match known.is_empty() {
                            true => format!(
                                "no model profile {p:?} — none are defined; add a [models.{p}] \
                                 block to config.toml"
                            ),
                            false => format!(
                                "no model profile {p:?} — defined: {}",
                                known.join(", ")
                            ),
                        })
                    })?;
                    profile.command(0).ok_or_else(|| {
                        failed(format!("model profile {p:?} lists no models"))
                    })?
                }
                None => str_arg(req, "cmd").unwrap_or("claude").to_string(),
            };
            let dir = dir_arg(req, "split", Dir::Right);
            let name = str_arg(req, "name").map(|s| s.to_string());

            // Who asked. An agent spawning agents is the whole point of a lead agent, and also
            // the one way a pane count runs away without anybody noticing, so it is capped.
            // A spawn from outside a pane is you, and you are not capped.
            let from = pane_arg(eng, req, "from");
            let by_agent = from.filter(|p| eng.session.panes.contains_key(p));
            if by_agent.is_some() {
                let live = eng
                    .session
                    .panes
                    .values()
                    .filter(|p| p.spawned_by_pane.is_some() && p.exited.is_none())
                    .count();
                if live >= cfg.max_fleet {
                    return Err(failed(format!(
                        "agents already have {live} panes open, which is the limit \
                         (agents.max_fleet). Close some, or raise it in config.toml."
                    )));
                }
            }

            // A worktree, when asked for. Named after the agent unless you said otherwise,
            // because "which tree is Kenny working in" should not need looking up.
            let worktree = match req.params.get("worktree") {
                None | Some(Value::Null) => None,
                Some(Value::Bool(false)) => None,
                Some(v) => {
                    let want = v
                        .as_str()
                        .map(|s| s.to_string())
                        .or_else(|| name.clone())
                        .ok_or_else(|| bad("--worktree needs a name, or give the agent one"))?;
                    // From the pane the split comes off — the caller's — so the worktree
                    // hangs off the project the *asking agent* is in. The focused pane is
                    // only the fallback for a human spawning from outside horde: focus
                    // follows the human's eyes, and a lead agent building a fleet in one
                    // project must not have a worker rooted in whichever project the human
                    // happens to be looking at.
                    let from = by_agent
                        .or_else(|| eng.session.focused_pane())
                        .and_then(|p| eng.session.panes.get(&p))
                        .map(|p| p.cwd.clone())
                        .ok_or_else(|| failed("no pane to take a directory from"))?;
                    Some(super::repo::add_worktree(&from, &want, None).map_err(|e| failed(format!("{e:#}")))?)
                }
            };

            // Split beside the caller for the same reason: the new agent belongs to the
            // caller's tab and space, not to wherever the human's focus sits right now.
            let id = eng
                .session
                .split_in(&cfg, by_agent, dir, Some(&cmd), worktree.as_deref())
                .map_err(|e| failed(e.to_string()))?;
            if let Some(p) = eng.session.panes.get_mut(&id) {
                if let Some(n) = name.clone() {
                    p.name = Some(n);
                }
                // Where in the profile this agent is, so exhaustion has a next model to move to.
                if let Some(profile) = profile_name.clone() {
                    p.model = Some(crate::daemon::pane::ModelRun {
                        profile,
                        index: 0,
                        switched: None,
                    });
                }
                // The job it is for, so a fleet reads as a team rather than as claude-2..7.
                if let Some(r) = str_arg(req, "role").and_then(crate::config::normalise_role) {
                    p.role = Some(r);
                }
                // Enlisting at spawn is how a lead agent builds a fleet that will take board
                // work, without every other agent in the project being volunteered too.
                if bool_arg(req, "board") {
                    p.board = true;
                }
                p.spawned_by_pane = by_agent;
            }

            // A first job, handed over at spawn. Goes on the board rather than into the pane:
            // the agent is still booting and has no prompt to type at yet, and the board is
            // the thing that survives it not being ready.
            let task = match str_arg(req, "task") {
                // A third route to the board, and the RPC gate does not cover it — `agent.spawn`
                // is not a `task.*` method. Same hole as the trigger path, same fix.
                Some(_) if !cfg.board => {
                    return Err(failed(
                        "the task board is off (agents.board) — use --brief to give the new agent \
                         its first instruction instead",
                    ))
                }
                Some(text) => {
                    let space = eng
                        .session
                        .panes
                        .get(&id)
                        .and_then(|p| eng.session.space(p.space))
                        .map(|s| s.name.clone());
                    let by = super::bus::Bus::sender_name(&eng.session, from);
                    // Tagged with the role the new agent was given, so the work it was spawned
                    // for is *its* work. Leaving it general would mean spawning a reviewer with a
                    // first job that the nearest idle builder can take instead — which is both
                    // agents doing the wrong thing, and the pane you just made sitting idle.
                    let role = eng.session.panes.get(&id).and_then(|p| p.role.clone());
                    Some(
                        eng.board
                            .add(super::tasks::NewTask {
                                role: role.as_deref(),
                                ..super::tasks::NewTask::new(text, &by, space.as_deref())
                            })
                            .map_err(|e| failed(e.to_string()))?,
                    )
                }
                None => None,
            };

            // A first instruction, held until the agent is actually there to read it.
            //
            // Not sent: a pane one millisecond old has no agent, so the bus would type the text
            // into a booting TUI without a newline. Holding it as an orphan means it lands the
            // moment detection names the agent and it reaches its prompt — which is what the
            // board was doing for `--task`, without needing a board.
            let brief = match str_arg(req, "brief") {
                Some(text) if !text.trim().is_empty() => {
                    // Addressed by the name it will answer to once detection runs; the pane id
                    // is the fallback for a spawn that was not given one.
                    let to = name.clone().unwrap_or_else(|| id.to_string());
                    let by = super::bus::Bus::sender_name(&eng.session, from);
                    Some(eng.bus.hold_for(&to, text, &by).id)
                }
                _ => None,
            };

            eng.touch();
            eng.detect_now();
            let mut out = json!({ "pane": id, "cmd": cmd });
            if let Some(b) = brief {
                out["brief"] = json!(b);
            }
            if let Some(w) = worktree {
                out["worktree"] = json!(w.to_string_lossy());
            }
            if let Some(t) = task {
                out["task"] = json!(t.id);
            }
            Ok(out)
        }
        "worktree.list" => {
            let from = worktree_origin(eng, req)?;
            let found = super::repo::list_worktrees(&from).map_err(|e| failed(format!("{e:#}")))?;
            // Which pane is in each tree, so the listing answers "can I remove this" without
            // a second lookup. A worktree with nobody in it is the removable kind.
            Ok(json!(found
                .iter()
                .map(|w| {
                    let pane = eng.session.panes.values().find(|p| p.cwd == w.path);
                    json!({
                        "name": w.name,
                        "branch": w.branch,
                        "path": w.path.to_string_lossy(),
                        "dirty": w.dirty,
                        "pane": pane.map(|p| p.id),
                        "agent": pane.and_then(|p| p.agent.as_ref()).map(|a| a.name.clone()),
                    })
                })
                .collect::<Vec<_>>()))
        }
        "worktree.remove" => {
            let name = str_arg(req, "name").ok_or_else(|| bad("name required"))?;
            let from = worktree_origin(eng, req)?;
            // A live pane in the tree is the one refusal git cannot make for us: it would
            // happily delete the directory out from under a running agent.
            //
            // Resolved from the listing rather than computed from the name: a tree an older
            // horde nested inside the repository is not where the current scheme would put it,
            // and guessing wrong here means checking an empty directory for occupants and
            // deleting an occupied one.
            let path = super::repo::worktree_for(&from, name)
                .map_err(|e| failed(format!("{e:#}")))?
                .path;
            if let Some(p) = eng.session.panes.values().find(|p| p.cwd == path) {
                return Err(bad(format!(
                    "pane {} is still working in {name}; close it first",
                    p.id
                )));
            }
            let removed = super::repo::remove_worktree(&from, name, bool_arg(req, "force"))
                .map_err(|e| failed(format!("{e:#}")))?;
            Ok(json!({ "removed": removed.to_string_lossy() }))
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
                .send(
                    session,
                    &cfg,
                    super::bus::Outgoing {
                        force,
                        expects_reply,
                        ..super::bus::Outgoing::plain(from, to, body)
                    },
                )
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
                    .send(
                        session,
                        &cfg,
                        super::bus::Outgoing {
                            reply_to: Some(id),
                            ..super::bus::Outgoing::plain(from, &original.from, body)
                        },
                    )
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
            let from = pane_arg(eng, req, "from");
            let by = super::bus::Bus::sender_name(&eng.session, from);
            // An explicit `space` overrides, so a lead agent can stage work for a project it
            // is not sitting in. Otherwise the caller's own.
            let space =
                str_arg(req, "space").map(|s| s.to_string()).or_else(|| caller_space(eng, req));
            let role = str_arg(req, "role").and_then(crate::config::normalise_role);

            // Who may put work on the board at all. Empty means anyone, which is the default and
            // how the board has always worked. Naming roles is how you say "agents propose work
            // to their lead, and the lead decides" — the guardrail against a fleet writing its
            // own next job and waking each other up to do it.
            //
            // The human is never gated: a call from outside a pane is a person at a keyboard, and
            // a board its owner cannot add to is not a feature.
            if let Some(pane) = from {
                let allowed = &eng.cfg.task_authors;
                if !allowed.is_empty() {
                    let mine = eng.session.panes.get(&pane).and_then(|p| p.role.clone());
                    if !mine.as_ref().is_some_and(|r| allowed.contains(r)) {
                        return Err(failed(format!(
                            "only {} may add tasks here (agents.task_authors){} — ask one of them, \
                             or say what you need with `horde send`",
                            allowed.join(", "),
                            match &mine {
                                Some(r) => format!(", and you are {r}"),
                                None => ", and you have no role".to_string(),
                            }
                        )));
                    }
                }
            }

            let t = eng
                .board
                .add(super::tasks::NewTask {
                    role: role.as_deref(),
                    ..super::tasks::NewTask::new(text, &by, space.as_deref())
                })
                .map_err(|e| failed(e.to_string()))?;
            eng.touch();

            // Said at the moment the work is written, because that is when it can still be
            // changed. A task naming a role nobody here has is not offered to anyone and reads
            // afterwards as a quiet board rather than as a mistake.
            let mut out = serde_json::to_value(&t).map_err(|e| failed(e.to_string()))?;
            if let (Some(want), Some(space)) = (&t.role, &t.space) {
                if !eng.roles_enlisted_in(space).iter().any(|r| r == want) {
                    out["warning"] = Value::from(format!(
                        "nobody enlisted in {space} has the role {want}, so this will sit until \
                         one does — `horde spawn --role {want} --board` starts one"
                    ));
                }
            }
            Ok(out)
        }
        // Which roles could take work in the caller's project right now. What a listing needs to
        // tell "waiting for a reviewer" from "waiting for a reviewer who does not exist".
        "task.roles" => {
            let space = caller_space(eng, req);
            let roles = space.map(|s| eng.roles_enlisted_in(&s)).unwrap_or_default();
            serde_json::to_value(roles).map_err(|e| failed(e.to_string()))
        }
        "task.clear" => {
            // Scoped like everything else, so clearing one project's board does not wipe the
            // others. `--all` widens it back out, because "I have stopped caring about all of
            // this" is also a real intention.
            let space = if bool_arg(req, "everywhere") { None } else { caller_space(eng, req) };
            let dropped = eng.board.clear(space.as_deref(), bool_arg(req, "claimed"));
            eng.touch();
            Ok(json!({ "dropped": dropped.len(), "space": space }))
        }
        "task.work" => {
            // Enlist. Deliberately a thing an agent does to itself rather than something done
            // to it: the board's failure mode was work arriving at agents that never asked.
            let pane = pane_arg(eng, req, "from")
                .or_else(|| eng.session.focused_pane())
                .ok_or_else(|| bad("no pane"))?;
            let on = req.params.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
            let p = eng.session.panes.get_mut(&pane).ok_or_else(|| not_found("no such pane"))?;
            p.board = on;
            eng.touch();
            Ok(json!({ "pane": pane, "board": on }))
        }
        "task.claim" => {
            let from = pane_arg(eng, req, "from");
            let owner = super::bus::Bus::sender_name(&eng.session, from);
            let id = req.params.get("task").and_then(|v| v.as_u64());
            let space = caller_space(eng, req);
            // The claimant's own label, from the pane rather than the request: a role that could
            // be asserted in the call would be a filter an agent could talk its way past.
            let role = from.and_then(|p| eng.session.panes.get(&p)).and_then(|p| p.role.clone());
            let who = super::tasks::Claimant { space: space.as_deref(), role: role.as_deref() };
            match eng.board.claim(&owner, id, who).map_err(|e| failed(e.to_string()))? {
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
            // If this task came off a kanban card, the card wants to hear about it. The
            // agent said something worth reading and the person who wrote the card is the
            // one who needs to read it — see `hand_over`.
            eng.kanban.on_task_settled(t.id, &owner, t.result.as_deref(), false);
            eng.touch();
            serde_json::to_value(t).map_err(|e| failed(e.to_string()))
        }
        "task.release" => {
            let id = req
                .params
                .get("task")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| bad("task id required"))?;
            let dropped = bool_arg(req, "drop");
            let t = eng.board.release(id, dropped).map_err(|e| failed(e.to_string()))?;
            // Only a dropped task is settled. Putting one back on the board is the middle of
            // its life, and a card that announced "gave up on it" every time an agent handed
            // work back would be lying about the most common case.
            if dropped {
                let by = t.owner.clone().unwrap_or_else(|| "horde".into());
                eng.kanban.on_task_settled(t.id, &by, None, true);
            }
            eng.touch();
            serde_json::to_value(t).map_err(|e| failed(e.to_string()))
        }
        "task.list" => {
            // This project's board, unless asked otherwise. Reading someone else's board by
            // accident is how you conclude there is work waiting that is not yours to do —
            // the same confusion the scoping fixed for the nudge, in the other direction.
            let space = if bool_arg(req, "everywhere") { None } else { caller_space(eng, req) };
            let shown: Vec<&super::tasks::Task> = match &space {
                Some(want) => eng
                    .board
                    .all()
                    .iter()
                    .filter(|t| t.space.as_deref() == Some(want.as_str()) || t.space.is_none())
                    .collect(),
                None => eng.board.all().iter().collect(),
            };
            serde_json::to_value(shown).map_err(|e| failed(e.to_string()))
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
                (Some(text), None, None) => super::triggers::What::Task {
                    text: text.to_string(),
                    role: str_arg(req, "role").and_then(crate::config::normalise_role),
                },
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
                        "this pane was started by horde itself (trigger #{by_trigger}), so it cannot create \
                         triggers — put work on the board instead"
                    )));
                }
            }
            let by = super::bus::Bus::sender_name(&eng.session, from);
            let only_if = str_arg(req, "when").map(|s| s.to_string());
            let t = eng
                .triggers
                .add(when, what, &by, only_if, caller_space(eng, req))
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

        // -- vault -------------------------------------------------------
        // The read side of a project's notes, for agents. JSON rather than the render
        // channel because this is the surface an agent drives, and `nc` has to be enough to
        // debug it. `space` defaults to the focused one, like every other space argument.
        "vault.list" | "vault.search" | "vault.read" => {
            let space = match str_arg(req, "space") {
                Some(name) => eng
                    .session
                    .find_space_by_name(name)
                    .ok_or_else(|| not_found(format!("no space called {name:?}")))?,
                None => eng.session.focused_space.ok_or_else(|| bad("no focused space"))?,
            };
            let kind = match req.method.as_str() {
                "vault.search" => crate::proto::VaultQuery::Search {
                    q: str_arg(req, "q").ok_or_else(|| bad("q required"))?.to_string(),
                },
                "vault.read" => crate::proto::VaultQuery::Note {
                    path: str_arg(req, "path").ok_or_else(|| bad("path required"))?.to_string(),
                },
                _ => crate::proto::VaultQuery::List,
            };
            let reply = eng.vault_answer(space, &kind).ok_or_else(|| {
                not_found("this project has no vault — see `vault.dir` in the config")
            })?;
            // `vault.read` on a path that is not in the index answers with an empty list
            // rather than a body, which would be a confusing way to say "no such note".
            if matches!(kind, crate::proto::VaultQuery::Note { .. }) && reply.body.is_none() {
                return Err(not_found("no such note in this vault"));
            }
            serde_json::to_value(reply).map_err(|e| failed(e.to_string()))
        }

        // Setting one up, and writing to it. Separate from the read verbs because these
        // are the ones that touch a disk: everything they take is jailed to the vault, and
        // "write a note" must never be a way to write anything else.
        "vault.init" => {
            let root = match str_arg(req, "path") {
                Some(p) => std::path::PathBuf::from(p),
                None => {
                    let space = eng.session.focused_space.ok_or_else(|| bad("no focused space"))?;
                    eng.session
                        .space(space)
                        .map(|s| s.cwd.join(&eng.cfg.vault_dir))
                        .unwrap_or_else(|| eng.cfg.vault_home.clone())
                }
            };
            let fresh = super::vault::init(&root).map_err(|e| failed(e.to_string()))?;
            super::refresh_vaults(eng);
            Ok(json!({ "root": root.to_string_lossy(), "created": fresh }))
        }
        // Writing a note by path. `title` is the friendlier way in — Obsidian's rule is that
        // the title is the filename, so a note written by title is one a `[[link]]` finds.
        "vault.write" => {
            let path = match (str_arg(req, "path"), str_arg(req, "title")) {
                (Some(p), _) => p.to_string(),
                (None, Some(t)) => super::vault::note_filename(t),
                (None, None) => return Err(bad("path or title required")),
            };
            let body = str_arg(req, "body").ok_or_else(|| bad("body required"))?.to_string();
            let space = match str_arg(req, "space") {
                Some(name) => eng
                    .session
                    .find_space_by_name(name)
                    .ok_or_else(|| not_found(format!("no space called {name:?}")))?,
                None => eng.session.focused_space.ok_or_else(|| bad("no focused space"))?,
            };
            // Who to credit: whoever the caller says, else the agent in the pane the call
            // came from. A note nobody signed is one you cannot decide how much to trust.
            let by = str_arg(req, "by").map(|s| s.to_string()).or_else(|| {
                pane_arg(eng, req, "pane").map(|p| super::bus::Bus::sender_name(&eng.session, Some(p)))
            });
            let written = eng
                .vault_put(space, &path, &body, by.as_deref(), bool_arg(req, "append"))
                .map_err(|e| failed(e.to_string()))?;
            Ok(json!({ "path": written.to_string_lossy() }))
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
            // Filed as well as answered. A digest read and closed is a thing that happened to
            // you; a digest in the vault is a thing you can go back to, search, and link from
            // — which is the whole point of horde having a vault at all.
            let mut value = serde_json::to_value(&d).map_err(|e| failed(e.to_string()))?;
            if bool_arg(req, "note") {
                let space = eng.session.focused_space.ok_or_else(|| bad("no focused space"))?;
                let day = super::triggers::local_date(super::now_millis());
                let written = eng
                    .vault_put(space, &format!("{day}.md"), &super::digest::markdown(&d), Some("horde"), true)
                    .map_err(|e| failed(e.to_string()))?;
                value["note"] = json!(written.to_string_lossy());
            }
            Ok(value)
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
        "redraw" => Cmd::Redraw,
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

    /// An agent's spawn is scoped to the agent, not to the human's eyes.
    ///
    /// The focused pane is whatever the human happens to be looking at, in any project. A
    /// lead agent in project A building a fleet must get its worker beside itself and its
    /// worktree rooted in A's repository — not in project B because the human clicked over
    /// there mid-spawn. Before this, the worktree grew in B's repo when B was one, and the
    /// spawn failed outright when B was not ("not in a git repository").
    #[test]
    fn a_fleet_spawn_lands_in_the_callers_project_not_the_focused_one() {
        let mut eng = super::super::tests::engine_with_shell(Some("cat"));
        let cfg = eng.cfg.clone();
        let lead = *eng.session.panes.keys().next().unwrap();

        // The lead agent's space is a real repository.
        let repo = std::env::temp_dir().join(format!("horde-rpc-focus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(&repo).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        // macOS: /var is a symlink to /private/var, and git reports canonical paths.
        let repo = repo.canonicalize().unwrap();
        eng.session.panes.get_mut(&lead).unwrap().cwd = repo.clone();

        // The human wanders off to a second space that is not a repository at all.
        let elsewhere = std::env::temp_dir().join(format!("horde-rpc-focus-else-{}", std::process::id()));
        std::fs::create_dir_all(&elsewhere).unwrap();
        eng.session.create_space(&cfg, Some("elsewhere"), &elsewhere).unwrap();
        assert_ne!(eng.session.focused_pane(), Some(lead), "the human's focus moved away");

        // The lead spawns a worktree worker.
        let resp = dispatch(&mut eng, Request {
            id: String::new(),
            method: "agent.spawn".into(),
            params: json!({ "cmd": "cat", "name": "builder", "worktree": true, "from": lead }),
        });
        let v = resp.result.unwrap_or_else(|| panic!("spawn failed: {:?}", resp.error));

        // The worktree belongs to the caller's repository. Asserted through git rather than by
        // path shape, so it stays true of wherever the layout puts the directory.
        let wt = std::path::PathBuf::from(v.get("worktree").and_then(|w| w.as_str()).expect("a worktree"));
        assert_eq!(
            super::super::repo::main_root(&wt),
            Some(repo.clone()),
            "worktree {wt:?} does not belong to the caller's repo {repo:?}"
        );
        // ...and the new pane sits in the caller's space, in that worktree.
        let id = v.get("pane").and_then(|p| p.as_u64()).unwrap() as u32;
        let (lead_space, new_space) =
            (eng.session.panes[&lead].space, eng.session.panes[&id].space);
        assert_eq!(new_space, lead_space, "the worker left the caller's project");
        assert_eq!(eng.session.panes[&id].cwd, wt);

        for p in eng.session.panes.values_mut() {
            p.kill();
        }
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&elsewhere);
        let _ = std::fs::remove_dir_all(&wt);
    }

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

    fn req(method: &str, params: serde_json::Value) -> Request {
        Request { id: String::new(), method: method.into(), params }
    }

    /// An agent spawning agents is the point of a lead agent, and also the one way a pane
    /// count runs away without anyone noticing. The cap is on live panes agents opened, so
    /// closing one frees a slot — a lifetime counter would retire a fleet that had merely
    /// been busy.
    /// A brief must survive the gap between spawning a pane and an agent existing in it.
    ///
    /// That gap is the whole reason `--task` used the board: for a second or two the pane has no
    /// agent, and the bus would type the text into a booting TUI without a newline. The brief
    /// waits instead, and arrives once the agent is named and at its prompt.
    #[test]
    fn a_brief_waits_for_the_agent_to_exist_and_then_arrives() {
        let mut eng = super::super::tests::engine();
        eng.cfg.board = false;

        let r = handle(
            &mut eng,
            &req("agent.spawn", json!({ "cmd": "cat", "name": "builder", "brief": "start here" })),
        )
        .unwrap();
        assert!(r.get("brief").is_some(), "the brief was accepted: {r}");
        // Relative: the test bus log is a file in the temp dir shared by every test in this
        // binary, and `Bus::new` recovers held messages from it — so an absolute count is an
        // assertion about what else ran today.
        let held = eng.bus.orphan_count();
        assert!(held >= 1, "the brief is waiting rather than delivered");

        // The pane exists but has no agent yet, so a flush changes nothing.
        let cfg = eng.cfg.clone();
        let Engine { bus, session, .. } = &mut eng;
        bus.flush_queued(session, &cfg);
        assert_eq!(eng.bus.orphan_count(), held, "nothing to deliver to yet");

        // Detection names the agent; now it lands.
        let pane = *eng
            .session
            .panes
            .iter()
            .find(|(_, p)| p.name.as_deref() == Some("builder"))
            .map(|(id, _)| id)
            .expect("the spawned pane");
        super::super::tests::give_agent_named(&mut eng.session, pane, "builder");
        let cfg = eng.cfg.clone();
        let Engine { bus, session, .. } = &mut eng;
        bus.flush_queued(session, &cfg);
        assert!(eng.bus.orphan_count() < held, "the brief found its agent");

        for p in eng.session.panes.values_mut() {
            p.kill();
        }
    }

    /// `--task` reaches the board directly from the spawn path, so the RPC gate does not cover
    /// it — the same shape of hole as the trigger path.
    #[test]
    fn spawning_with_a_task_is_refused_when_the_board_is_closed() {
        let mut eng = super::super::tests::engine();
        eng.cfg.board = false;
        let e = handle(
            &mut eng,
            &req("agent.spawn", json!({ "cmd": "cat", "name": "w", "task": "do it" })),
        )
        .unwrap_err();
        assert!(e.message.contains("agents.board"), "{}", e.message);
        assert!(e.message.contains("--brief"), "it should say what to use instead: {}", e.message);
        for p in eng.session.panes.values_mut() {
            p.kill();
        }
    }

    /// The board and the bus are separate switches, and turning one off must not touch the other.
    #[test]
    fn the_board_can_be_closed_while_the_bus_stays_open() {
        let mut eng = super::super::tests::engine();
        eng.cfg.board = false;

        let e = handle(&mut eng, &req("task.add", json!({ "text": "something" }))).unwrap_err();
        assert!(e.message.contains("agents.board"), "{}", e.message);
        // Every board verb, not just the one that writes: claiming and listing are how an agent
        // discovers the board exists at all.
        for m in ["task.list", "task.claim", "task.work", "task.done", "task.clear"] {
            let e = handle(&mut eng, &req(m, json!({}))).unwrap_err();
            assert!(e.message.contains("board is off"), "{m}: {}", e.message);
        }

        // The bus is untouched: resolving a target still works, which is the first thing
        // `bus.send` does and the part that would break if the gate were too broad.
        let panes: Vec<_> = eng.session.panes.keys().copied().collect();
        assert!(!panes.is_empty(), "the fixture has a pane to address");
        assert!(handle(&mut eng, &req("agent.list", json!({}))).is_ok(), "the roster still works");

        // And with the board on again, the same call goes through.
        eng.cfg.board = true;
        assert!(handle(&mut eng, &req("task.list", json!({}))).is_ok());
    }

    /// A profile names a list of models; spawning on one starts at its head.
    #[test]
    fn spawning_on_a_profile_runs_the_first_model_in_it() {
        let mut eng = super::super::tests::engine();
        eng.cfg.models.insert(
            "free".into(),
            crate::config::ModelProfile {
                cmd: "cat --model openrouter/{model}".into(),
                order: vec!["qwen/qwen3-coder:free".into(), "second/model".into()],
                exhausted: Vec::new(),
                switch: None,
            },
        );
        let r = handle(&mut eng, &req("agent.spawn", json!({ "profile": "free" }))).unwrap();
        assert_eq!(
            r.get("cmd").and_then(|c| c.as_str()),
            Some("cat --model openrouter/qwen/qwen3-coder:free")
        );
        for p in eng.session.panes.values_mut() {
            p.kill();
        }
    }

    /// Refused, not defaulted. Quietly falling back to `claude` when someone asked for the free
    /// tier would spend the wrong budget on the wrong provider and look like it worked.
    #[test]
    fn an_unknown_profile_is_refused_and_lists_the_real_ones() {
        let mut eng = super::super::tests::engine();
        eng.cfg.models.insert(
            "free".into(),
            crate::config::ModelProfile {
                cmd: "cat {model}".into(),
                order: vec!["a".into()],
                exhausted: Vec::new(),
                switch: None,
            },
        );
        let e = handle(&mut eng, &req("agent.spawn", json!({ "profile": "fre" }))).unwrap_err();
        assert!(e.message.contains("fre"), "{}", e.message);
        assert!(e.message.contains("free"), "it should say what does exist: {}", e.message);

        // And with none defined at all, it says how to define one.
        eng.cfg.models.clear();
        let e = handle(&mut eng, &req("agent.spawn", json!({ "profile": "free" }))).unwrap_err();
        assert!(e.message.contains("[models.free]"), "{}", e.message);
    }

    #[test]
    fn an_agent_cannot_open_more_panes_than_the_fleet_cap() {
        let mut eng = super::super::tests::engine_with_idle_agents("fleetcap", 1);
        eng.cfg.max_fleet = 2;
        let from = *eng.session.panes.keys().next().unwrap();
        // `cat` rather than an agent: a unit test that launches claude would be neither fast
        // nor polite.
        let spawn = |n: &str| req("agent.spawn", json!({ "cmd": "cat", "name": n, "from": from }));

        assert!(handle(&mut eng, &spawn("a")).is_ok());
        assert!(handle(&mut eng, &spawn("b")).is_ok());
        let refused = handle(&mut eng, &spawn("c")).unwrap_err();
        assert!(refused.message.contains("max_fleet"), "{}", refused.message);

        // Closing one frees its slot.
        let opened = *eng
            .session
            .panes
            .values()
            .find(|p| p.spawned_by_pane == Some(from))
            .map(|p| &p.id)
            .unwrap();
        eng.session.close_pane(&eng.cfg.clone(), opened).unwrap();
        assert!(handle(&mut eng, &spawn("d")).is_ok());

        for id in eng.session.panes.keys().copied().collect::<Vec<_>>() {
            if let Some(p) = eng.session.panes.get_mut(&id) {
                p.kill();
            }
        }
    }

    /// You are not an agent, and you are not capped.
    #[test]
    fn a_spawn_from_outside_a_pane_is_not_counted_against_the_fleet() {
        let mut eng = super::super::tests::engine_with_idle_agents("fleetuser", 1);
        eng.cfg.max_fleet = 1;
        for n in ["a", "b", "c"] {
            let r = handle(&mut eng, &req("agent.spawn", json!({ "cmd": "cat", "name": n })));
            assert!(r.is_ok(), "{n}: {:?}", r.err());
        }
        for id in eng.session.panes.keys().copied().collect::<Vec<_>>() {
            if let Some(p) = eng.session.panes.get_mut(&id) {
                p.kill();
            }
        }
    }

    /// Renaming a space existed only as a client frame — unreachable from the CLI or a
    /// script, while `pane.rename` was fully exposed. This is the regression test for that
    /// gap, not just for the new method.
    #[test]
    fn renaming_a_space_is_reachable_from_the_control_api() {
        let mut eng = super::super::tests::engine_with_idle_agents("rpc-rename", 1);
        let old = eng.session.spaces[0].name.clone();
        let v = handle(&mut eng, &req("space.rename", json!({ "name": old, "to": "api" })))
            .expect("space.rename must exist");
        assert_eq!(v["name"], "api");
        assert_eq!(eng.session.spaces[0].name, "api");
    }

    /// A clash is uniquified silently, so the caller has no way to learn what it actually got
    /// unless the method says.
    #[test]
    fn a_renamed_space_reports_the_name_it_actually_got() {
        let mut eng = super::super::tests::engine_with_idle_agents("rpc-rename-clash", 1);
        let cfg = eng.cfg.clone();
        eng.session.create_space(&cfg, Some("api"), &std::env::temp_dir()).unwrap();
        let other = eng.session.spaces[0].name.clone();
        let v = handle(&mut eng, &req("space.rename", json!({ "name": other, "to": "api" })))
            .unwrap();
        assert_eq!(v["name"], "api-2", "not the name that was asked for");
    }

    /// `Code Reviewer` is stored as `code-reviewer`; a script filtering on the role it just
    /// set would never match unless the normalised form comes back.
    #[test]
    fn setting_a_role_returns_the_normalised_name() {
        let mut eng = super::super::tests::engine_with_idle_agents("rpc-role", 1);
        let pane = *eng.session.panes.keys().next().unwrap();
        let v = handle(&mut eng, &req("pane.role", json!({ "pane": pane, "role": "Code Reviewer" })))
            .unwrap();
        assert_eq!(v["role"], "code-reviewer");

        // And an empty role clears it rather than naming nothing.
        let v = handle(&mut eng, &req("pane.role", json!({ "pane": pane, "role": "" }))).unwrap();
        assert!(v["role"].is_null());
    }

    /// An agent must not be able to relabel itself.
    ///
    /// The moment a role decides what work is offered and who may add it, `horde pane role` is a
    /// promotion, and an agent taking it would be acting reasonably from inside its own
    /// reasoning. The pane the call came *from* is what decides: an agent is refused, a person at
    /// a shell inside horde is not.
    #[test]
    fn an_agent_cannot_set_a_role_but_a_person_at_a_shell_can() {
        let mut eng = super::super::tests::engine_with_idle_agents("rpc-role-self", 2);
        let panes: Vec<u32> = eng.session.panes.keys().copied().collect();
        let (agent_pane, target) = (panes[0], panes[1]);

        // From the agent's own pane, at itself: refused, and the error says where roles come from.
        let e = handle(
            &mut eng,
            &req("pane.role", json!({ "pane": agent_pane, "role": "pm", "from": agent_pane })),
        )
        .unwrap_err();
        assert!(e.message.contains("cannot set a role"), "{}", e.message);
        assert!(e.message.contains("--role"), "it names the way roles are given: {}", e.message);
        assert!(eng.session.panes[&agent_pane].role.is_none(), "and nothing changed");

        // At somebody else, too: the hole is not only self-promotion — labelling a neighbour
        // decides what work that neighbour is handed.
        assert!(handle(
            &mut eng,
            &req("pane.role", json!({ "pane": target, "role": "pm", "from": agent_pane })),
        )
        .is_err());

        // A pane with no agent in it is a person. Emptying it is the whole difference.
        eng.session.panes.get_mut(&agent_pane).unwrap().agent = None;
        let v = handle(
            &mut eng,
            &req("pane.role", json!({ "pane": target, "role": "pm", "from": agent_pane })),
        )
        .unwrap();
        assert_eq!(v["role"], "pm");
    }

    /// `agents.task_authors` is the lead-agent pattern: workers propose, the lead writes.
    #[test]
    fn only_a_named_role_may_add_tasks_when_authors_are_configured() {
        let mut eng = super::super::tests::engine_with_idle_agents("rpc-authors", 2);
        eng.cfg.task_authors = vec!["pm".to_string()];
        let panes: Vec<u32> = eng.session.panes.keys().copied().collect();
        let (worker, lead) = (panes[0], panes[1]);
        eng.session.set_pane_role(lead, "pm");

        // A worker is refused, and told what would work instead.
        let e = handle(&mut eng, &req("task.add", json!({ "text": "extra work", "from": worker })))
            .unwrap_err();
        assert!(e.message.contains("task_authors"), "{}", e.message);
        assert!(e.message.contains("pm"), "it names who may: {}", e.message);
        assert!(e.message.contains("horde send"), "and what to do instead: {}", e.message);
        assert_eq!(eng.board.open_count(), 0, "and nothing was written");

        // The lead may.
        handle(&mut eng, &req("task.add", json!({ "text": "real work", "from": lead }))).unwrap();
        assert_eq!(eng.board.open_count(), 1);

        // And the human is never gated: a call from outside a pane is a person at a keyboard, and
        // a board its owner cannot add to is not a feature.
        handle(&mut eng, &req("task.add", json!({ "text": "mine", "from": Value::Null }))).unwrap();
        assert_eq!(eng.board.open_count(), 2);
    }

    /// Work for a role nobody has is said at the moment it is written, because that is when it
    /// can still be changed. Afterwards it reads as a board with nothing happening on it.
    #[test]
    fn adding_work_for_a_role_nobody_has_says_so() {
        let mut eng = super::super::tests::engine_with_idle_agents("rpc-strand", 1);
        let pane = *eng.session.panes.keys().next().unwrap();

        let v = handle(
            &mut eng,
            &req("task.add", json!({ "text": "review it", "role": "reviewer", "from": pane })),
        )
        .unwrap();
        assert_eq!(v["role"], "reviewer");
        let warning = v["warning"].as_str().expect("it must warn");
        assert!(warning.contains("reviewer"), "{warning}");
        assert!(warning.contains("--role reviewer"), "and how to fix it: {warning}");

        // With a reviewer enlisted there is nothing to warn about.
        eng.session.set_pane_role(pane, "reviewer");
        let v = handle(
            &mut eng,
            &req("task.add", json!({ "text": "review this too", "role": "reviewer", "from": pane })),
        )
        .unwrap();
        assert!(v.get("warning").is_none(), "somebody can take it: {v}");
    }

    #[test]
    fn an_accent_can_be_set_or_cycled_over_the_control_api() {
        let mut eng = super::super::tests::engine_with_idle_agents("rpc-accent", 1);
        let name = eng.session.spaces[0].name.clone();
        let v = handle(&mut eng, &req("space.accent", json!({ "name": name, "slot": 3 }))).unwrap();
        assert_eq!(v["slot"], 3);
        // Omitting the slot steps to the next one, so a caller with no snapshot can still move it.
        let v = handle(&mut eng, &req("space.accent", json!({ "name": name }))).unwrap();
        assert_eq!(v["slot"], 4);
    }

    /// The payoff for a role being a name rather than a note: one call answers "who is
    /// reviewing", across every project.
    #[test]
    fn role_list_counts_every_pane_wearing_each_role() {
        let mut eng = super::super::tests::engine_with_idle_agents("rpc-rolelist", 2);
        let panes: Vec<_> = eng.session.panes.keys().copied().collect();
        for p in &panes {
            eng.session.set_pane_role(*p, "reviewer");
        }
        let v = handle(&mut eng, &req("role.list", json!({}))).unwrap();
        let rows = v.as_array().unwrap();
        let row = rows.iter().find(|r| r["name"] == "reviewer").unwrap();
        assert_eq!(row["panes"], panes.len());
        assert_eq!(row["declared"], false, "it works without being declared");
    }
}
