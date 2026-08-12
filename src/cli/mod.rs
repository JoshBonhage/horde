//! Command line surface. Every subcommand is one control-channel call, which is what lets
//! an agent orchestrate horde with nothing but a shell.

pub mod docs;
mod integration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::proto::{Request, Response};

#[derive(Parser)]
#[command(
    name = "horde",
    about = "An agent-aware terminal multiplexer",
    version,
    // With no subcommand, `horde` attaches.
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the daemon in the foreground (normally started automatically).
    Daemon {
        /// Internal: take over an existing session from a predecessor daemon.
        #[arg(long, hide = true)]
        import: bool,
    },
    /// Stop the daemon and every pane it owns.
    Stop,
    /// Replace the running daemon with this binary, keeping every pane and agent alive.
    ///
    /// The PTYs are handed to the new daemon over a socket, so the processes attached to
    /// them never notice. Use this after rebuilding instead of `horde stop`.
    Upgrade {
        /// Binary to hand over to. Defaults to the one running this command.
        #[arg(long)]
        exe: Option<String>,
    },
    /// Show daemon status.
    Status,
    /// List agents and their states.
    Roster {
        #[arg(long)]
        json: bool,
    },
    /// Send a message to another agent.
    Send {
        /// Agent name, pane name, pane id, or space:tab:pane.
        to: String,
        /// Message body.
        body: Vec<String>,
        /// Deliver even if the target is mid-stream. Unsafe at a permission prompt.
        #[arg(long)]
        now: bool,
    },
    /// Ask another agent something and wait for its answer.
    ///
    /// Unlike `send`, this blocks until the agent replies and prints the reply to stdout, so
    /// it can be captured: `answer=$(horde ask reviewer "is this sound?")`. The recipient is
    /// told the exact command to answer with.
    Ask {
        /// Agent name, pane name, pane id, or space:tab:pane.
        to: String,
        /// The question.
        body: Vec<String>,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Answer a request you were sent. The request number is in the message.
    Reply {
        /// Request number, as printed in `[horde] request #N`.
        request: u64,
        body: Vec<String>,
    },
    /// Send a message to every agent.
    Broadcast {
        body: Vec<String>,
        /// Limit to one space.
        #[arg(long)]
        space: Option<String>,
    },
    /// Start an agent in a new pane.
    Spawn {
        /// Command to run.
        #[arg(long, default_value = "claude")]
        cmd: String,
        /// Addressable name for the new agent.
        #[arg(long)]
        name: Option<String>,
        /// Where to put it: right, down, left, up.
        #[arg(long, default_value = "right")]
        split: String,
    },
    /// Block until an agent reaches a state.
    Wait {
        /// Agent name.
        target: String,
        #[arg(long, default_value = "idle")]
        until: String,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Apply a named layout: solo, duo, trio, dev, quad.
    Layout { preset: String },
    /// Show recent bus messages.
    Bus {
        #[command(subcommand)]
        cmd: BusCmd,
    },
    /// Space management.
    Space {
        #[command(subcommand)]
        cmd: SpaceCmd,
    },
    /// Tab management.
    Tab {
        #[command(subcommand)]
        cmd: TabCmd,
    },
    /// Pane management.
    Pane {
        #[command(subcommand)]
        cmd: PaneCmd,
    },
    /// Agent inspection.
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Install or remove an agent's lifecycle hooks.
    Integration {
        #[command(subcommand)]
        cmd: IntegrationCmd,
    },
    /// Internal: called by an agent's lifecycle hook. Reads the hook payload on stdin.
    #[command(hide = true)]
    Hook { agent: String, event: String },
    /// Read the documentation. `horde docs` lists topics.
    ///
    /// Agents: `horde docs orchestration` explains how to talk to other agents.
    Docs { topic: Option<String> },
    /// Show the active keybindings.
    Keys,
    /// Call a control method directly.
    Api {
        method: String,
        #[arg(long, default_value = "{}")]
        params: String,
    },
}

#[derive(Subcommand)]
pub enum BusCmd {
    /// Print recent messages.
    Tail {
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Keep printing new messages as they are routed.
        #[arg(long, short)]
        follow: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum SpaceCmd {
    List,
    New {
        name: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
    },
    Focus {
        name: String,
    },
    Close {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum TabCmd {
    List,
    New { name: Option<String> },
    Close,
}

#[derive(Subcommand)]
pub enum PaneCmd {
    List,
    /// Print the current pane id, as seen from inside a pane.
    Current,
    Split {
        #[arg(long, default_value = "right")]
        direction: String,
        #[arg(long)]
        cmd: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    Close {
        pane: Option<String>,
    },
    /// Read a pane's contents.
    Read {
        pane: Option<String>,
        #[arg(long, default_value_t = 50)]
        lines: usize,
        /// visible, recent, or detection.
        #[arg(long, default_value = "visible")]
        source: String,
    },
    Rename {
        pane: String,
        name: String,
    },
    SendText {
        pane: String,
        text: Vec<String>,
        #[arg(long)]
        submit: bool,
    },
    /// Report an agent's state. Used by lifecycle hooks.
    ReportAgent {
        #[arg(long)]
        pane: Option<String>,
        #[arg(long)]
        state: String,
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum AgentCmd {
    List,
    /// Show how a pane's state was decided.
    Explain { pane: Option<String> },
}

#[derive(Subcommand)]
pub enum IntegrationCmd {
    /// Wire up lifecycle hooks so the agent reports its own state.
    Install { agent: String },
    /// Remove horde's hooks, leaving anything else untouched.
    Uninstall { agent: String },
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// One request, one response, over a fresh connection.
pub fn call(method: &str, params: Value) -> Result<Value> {
    let socket = crate::config::socket_path();
    let stream = UnixStream::connect(&socket).with_context(|| {
        format!("no horde daemon at {} — start one by running `horde`", socket.display())
    })?;
    call_on(stream, method, params)
}

fn call_on(mut stream: UnixStream, method: &str, params: Value) -> Result<Value> {
    let req = Request { id: "cli".into(), method: method.to_string(), params };
    let mut line = serde_json::to_vec(&req)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    if reader.read_line(&mut buf)? == 0 {
        return Err(anyhow!("daemon closed the connection"));
    }
    let resp: Response = serde_json::from_str(buf.trim())
        .with_context(|| format!("unreadable response: {}", buf.trim()))?;
    match (resp.result, resp.error) {
        (Some(v), _) => Ok(v),
        (None, Some(e)) => Err(anyhow!("{}: {}", e.code, e.message)),
        (None, None) => Ok(Value::Null),
    }
}

pub fn daemon_running() -> bool {
    UnixStream::connect(crate::config::socket_path()).is_ok()
}

/// The pane this process is running in, from the environment horde injects.
fn self_pane() -> Option<u32> {
    std::env::var("HORDE_PANE").ok()?.parse().ok()
}

fn dir_name(s: &str) -> Result<&str> {
    match s {
        "right" | "left" | "up" | "down" => Ok(s),
        other => Err(anyhow!("direction must be right, left, up, or down (got {other:?})")),
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::Daemon { .. } => unreachable!("handled in main"),

        Command::Upgrade { exe } => {
            if !daemon_running() {
                println!("no daemon running — just start one with `horde`");
                return Ok(());
            }
            let exe = match exe {
                Some(e) => std::fs::canonicalize(&e)
                    .with_context(|| format!("no such binary: {e}"))?
                    .to_string_lossy()
                    .to_string(),
                None => std::env::current_exe()?.to_string_lossy().to_string(),
            };
            let before = call("server.status", json!({}))?;
            let was = before.get("version").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let panes = before.get("panes").and_then(|v| v.as_u64()).unwrap_or(0);

            println!("handing {panes} panes to {exe}…");
            // The daemon exits the moment it commits, so losing the connection here is the
            // expected outcome rather than a failure.
            match call("server.handoff", json!({ "exe": exe })) {
                Ok(_) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("closed the connection") {
                        return Err(e).context("handoff refused; nothing changed");
                    }
                }
            }

            // Wait for the successor to take over the socket.
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let Ok(v) = call("server.status", json!({})) {
                    let now = v.get("version").and_then(|x| x.as_str()).unwrap_or("?");
                    let n = v.get("panes").and_then(|x| x.as_u64()).unwrap_or(0);
                    println!("upgraded {was} -> {now}, {n} panes still running");
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(anyhow!(
                        "the new daemon did not come up within 15s — see {}",
                        crate::config::log_path().display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        }

        Command::Stop => {
            if !daemon_running() {
                println!("no daemon running");
                return Ok(());
            }
            call("server.stop", json!({}))?;
            println!("daemon stopped");
        }

        Command::Status => {
            let v = call("server.status", json!({}))?;
            print_kv(&v);
        }

        Command::Roster { json: as_json } => {
            let v = call("agent.list", json!({}))?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_roster(&v);
            }
        }

        Command::Send { to, body, now } => {
            let body = body.join(" ");
            if body.trim().is_empty() {
                return Err(anyhow!("message body is empty"));
            }
            let v = call(
                "bus.send",
                json!({ "to": to, "body": body, "from": self_pane(), "force": now }),
            )?;
            let delivery = v.get("delivery").and_then(|d| d.as_str()).unwrap_or("?");
            let target = v.get("to").and_then(|d| d.as_str()).unwrap_or(&to);
            match delivery {
                "delivered" => println!("delivered to {target}"),
                // Say plainly that it is waiting, and why that is the safe outcome.
                "queued" => println!(
                    "queued for {target} — it is busy or at a prompt; horde will deliver when it is free"
                ),
                other => println!("{other} for {target}"),
            }
        }

        Command::Ask { to, body, timeout } => {
            let body = body.join(" ");
            if body.trim().is_empty() {
                return Err(anyhow!("question is empty"));
            }
            let sent = call(
                "bus.send",
                json!({ "to": to, "body": body, "from": self_pane(), "expects_reply": true }),
            )?;
            let id = sent.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let target = sent.get("to").and_then(|v| v.as_str()).unwrap_or(&to).to_string();
            let delivery = sent.get("delivery").and_then(|v| v.as_str()).unwrap_or("");
            if delivery == "queued" {
                eprintln!("request #{id} queued for {target} — it is busy; still waiting");
            } else {
                eprintln!("asked {target} (request #{id})");
            }

            let deadline = Instant::now() + Duration::from_secs(timeout);
            loop {
                let v = call("bus.reply_for", json!({ "request": id }))?;
                if let Some(reply) = v.as_object() {
                    // The answer goes to stdout alone, so it can be captured in a variable.
                    println!("{}", reply.get("body").and_then(|b| b.as_str()).unwrap_or(""));
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(anyhow!(
                        "no reply to request #{id} from {target} within {timeout}s \
                         (it may still answer — check `horde bus tail`)"
                    ));
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        }

        Command::Reply { request, body } => {
            let body = body.join(" ");
            if body.trim().is_empty() {
                return Err(anyhow!("reply is empty"));
            }
            let v = call(
                "bus.reply",
                json!({ "request": request, "body": body, "from": self_pane() }),
            )?;
            let to = v.get("to").and_then(|x| x.as_str()).unwrap_or("?");
            let how = v.get("delivery").and_then(|x| x.as_str()).unwrap_or("sent");
            println!("{how} to {to} (re #{request})");
        }

        Command::Broadcast { body, space } => {
            let body = body.join(" ");
            if body.trim().is_empty() {
                return Err(anyhow!("message body is empty"));
            }
            let v = call(
                "bus.broadcast",
                json!({ "body": body, "from": self_pane(), "space": space }),
            )?;
            let n = v.as_array().map(|a| a.len()).unwrap_or(0);
            println!("sent to {n} agent(s)");
        }

        Command::Spawn { cmd, name, split } => {
            let v = call(
                "agent.spawn",
                json!({ "cmd": cmd, "name": name, "split": dir_name(&split)? }),
            )?;
            println!("pane {} running {cmd}", v.get("pane").unwrap_or(&Value::Null));
        }

        Command::Wait { target, until, timeout } => {
            let want = match until.as_str() {
                "idle" | "done" | "blocked" | "working" => until.clone(),
                other => return Err(anyhow!("--until must be idle, done, blocked, or working (got {other:?})")),
            };
            let deadline = Instant::now() + Duration::from_secs(timeout);
            loop {
                let v = call("agent.list", json!({}))?;
                let found = v.as_array().and_then(|a| {
                    a.iter().find(|x| x.get("name").and_then(|n| n.as_str()) == Some(&target))
                });
                match found {
                    Some(agent) => {
                        let state = agent.get("state").and_then(|s| s.as_str()).unwrap_or("");
                        // `done` also satisfies a wait for `idle`: both mean finished.
                        let hit = state == want || (want == "idle" && state == "done");
                        if hit {
                            println!("{target} is {state}");
                            return Ok(());
                        }
                    }
                    None => return Err(anyhow!("no agent called {target:?} (try `horde roster`)")),
                }
                if Instant::now() >= deadline {
                    return Err(anyhow!("timed out after {timeout}s waiting for {target} to be {want}"));
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        }

        Command::Layout { preset } => {
            call("layout.apply", json!({ "preset": preset }))?;
            println!("applied layout {preset}");
        }

        Command::Bus { cmd } => match cmd {
            BusCmd::Tail { limit, follow, json: as_json } => {
                let mut seen: u64 = 0;
                loop {
                    let v = call("bus.tail", json!({ "limit": limit }))?;
                    if as_json {
                        println!("{}", serde_json::to_string_pretty(&v)?);
                        if !follow {
                            return Ok(());
                        }
                    } else {
                        for m in v.as_array().unwrap_or(&vec![]) {
                            let id = m.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
                            if follow && id <= seen {
                                continue;
                            }
                            seen = seen.max(id);
                            print_message(m);
                        }
                    }
                    if !follow {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        },

        Command::Space { cmd } => match cmd {
            SpaceCmd::List => {
                let v = call("space.list", json!({}))?;
                for s in v.as_array().unwrap_or(&vec![]) {
                    let name = s.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    let agents = s.get("agent_count").and_then(|n| n.as_u64()).unwrap_or(0);
                    let attn = s.get("attention_count").and_then(|n| n.as_u64()).unwrap_or(0);
                    let cwd = s.get("cwd").and_then(|n| n.as_str()).unwrap_or("");
                    let mut line = format!("{name:<20} {agents} agents");
                    if attn > 0 {
                        line.push_str(&format!(", {attn} need you"));
                    }
                    println!("{line:<44} {cwd}");
                }
            }
            SpaceCmd::New { name, cwd } => {
                let v = call("space.create", json!({ "name": name, "cwd": cwd }))?;
                println!("space {}", v.get("space").unwrap_or(&Value::Null));
            }
            SpaceCmd::Focus { name } => {
                call("space.focus", json!({ "name": name }))?;
                println!("focused {name}");
            }
            SpaceCmd::Close { name } => {
                call("space.close", json!({ "name": name }))?;
                println!("closed {name}");
            }
        },

        Command::Tab { cmd } => match cmd {
            TabCmd::List => {
                let v = call("tab.list", json!({}))?;
                println!("{}", serde_json::to_string_pretty(&v)?);
            }
            TabCmd::New { name } => {
                let v = call("tab.create", json!({ "name": name }))?;
                println!("tab {}", v.get("tab").unwrap_or(&Value::Null));
            }
            TabCmd::Close => {
                call("tab.close", json!({}))?;
                println!("tab closed");
            }
        },

        Command::Pane { cmd } => match cmd {
            PaneCmd::List => {
                let v = call("pane.list", json!({}))?;
                for p in v.as_array().unwrap_or(&vec![]) {
                    let id = p.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
                    let title = p.get("title").and_then(|t| t.as_str()).unwrap_or("");
                    let agent = p
                        .get("agent")
                        .and_then(|a| a.get("state"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("-");
                    println!("{id:<5} {title:<24} {agent}");
                }
            }
            PaneCmd::Current => {
                let v = call("pane.current", json!({}))?;
                println!("{}", v.get("pane").unwrap_or(&Value::Null));
            }
            PaneCmd::Split { direction, cmd, name } => {
                let v = call(
                    "pane.split",
                    json!({ "direction": dir_name(&direction)?, "cmd": cmd, "name": name }),
                )?;
                println!("pane {}", v.get("pane").unwrap_or(&Value::Null));
            }
            PaneCmd::Close { pane } => {
                call("pane.close", json!({ "pane": pane_param(pane) }))?;
                println!("closed");
            }
            PaneCmd::Read { pane, lines, source } => {
                let v = call(
                    "pane.read",
                    json!({ "pane": pane_param(pane), "lines": lines, "source": source }),
                )?;
                for l in v.get("lines").and_then(|l| l.as_array()).unwrap_or(&vec![]) {
                    println!("{}", l.as_str().unwrap_or(""));
                }
            }
            PaneCmd::Rename { pane, name } => {
                call("pane.rename", json!({ "pane": pane, "name": name }))?;
                println!("renamed");
            }
            PaneCmd::SendText { pane, text, submit } => {
                call(
                    "pane.send_text",
                    json!({ "pane": pane, "text": text.join(" "), "submit": submit }),
                )?;
                println!("sent");
            }
            PaneCmd::ReportAgent { pane, state, session } => {
                let pane = pane.map(Value::from).or_else(|| self_pane().map(Value::from));
                call(
                    "pane.report_agent",
                    json!({ "pane": pane, "state": state, "session": session }),
                )?;
            }
        },

        Command::Agent { cmd } => match cmd {
            AgentCmd::List => {
                let v = call("agent.list", json!({}))?;
                print_roster(&v);
            }
            AgentCmd::Explain { pane } => {
                let v = call("agent.explain", json!({ "pane": pane_param(pane) }))?;
                println!("{}", serde_json::to_string_pretty(&v)?);
            }
        },

        Command::Integration { cmd } => match cmd {
            IntegrationCmd::Install { agent } => integration::install(&agent)?,
            IntegrationCmd::Uninstall { agent } => integration::uninstall(&agent)?,
        },

        Command::Hook { agent, event } => integration::run_hook(&agent, &event)?,

        Command::Docs { topic } => docs::show(topic.as_deref())?,

        Command::Keys => {
            let (cfg, _) = crate::config::Config::load();
            let prefix = cfg.prefix.describe();
            println!("prefix: {prefix}\n");
            for (name, trigger, _) in cfg.keys.described() {
                let key = match trigger {
                    crate::config::Trigger::Prefix(c) => format!("{prefix} {}", c.describe()),
                    crate::config::Trigger::Direct(c) => c.describe(),
                };
                println!("  {key:<18} {name}");
            }
        }

        Command::Api { method, params } => {
            let params: Value = serde_json::from_str(&params)
                .with_context(|| format!("--params is not valid JSON: {params}"))?;
            let v = call(&method, params)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
    }
    Ok(())
}

/// A pane argument, defaulting to the calling pane so agents can omit it.
fn pane_param(pane: Option<String>) -> Value {
    match pane {
        Some(p) => match p.parse::<u32>() {
            Ok(n) => Value::from(n),
            Err(_) => Value::from(p),
        },
        None => self_pane().map(Value::from).unwrap_or(Value::Null),
    }
}

fn print_kv(v: &Value) {
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            let s = match val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            println!("{k:<12} {s}");
        }
    }
}

fn print_roster(v: &Value) {
    let items = v.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("no agents running — start one with `horde spawn --cmd claude`");
        return;
    }
    println!("{:<14} {:<9} {:<8} {:<14} {}", "NAME", "STATE", "FOR", "SPACE", "WHY");
    for a in items {
        let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("?");
        let state = a.get("state").and_then(|x| x.as_str()).unwrap_or("?");
        let secs = a.get("elapsed").and_then(|x| x.as_u64()).unwrap_or(0);
        let space = a.get("space").and_then(|x| x.as_str()).unwrap_or("");
        let reason = a.get("reason").and_then(|x| x.as_str()).unwrap_or("");
        let queued = a.get("queued").and_then(|x| x.as_u64()).unwrap_or(0);
        let mut why = reason.to_string();
        if queued > 0 {
            why.push_str(&format!(" (+{queued} queued)"));
        }
        println!(
            "{name:<14} {state:<9} {:<8} {space:<14} {why}",
            crate::client::ui::pane_widget::fmt_elapsed(secs)
        );
    }
}

fn print_message(m: &Value) {
    let from = m.get("from").and_then(|x| x.as_str()).unwrap_or("?");
    let to = m.get("to").and_then(|x| x.as_str()).unwrap_or("?");
    let body = m.get("body").and_then(|x| x.as_str()).unwrap_or("");
    let delivery = m.get("delivery").and_then(|x| x.as_str()).unwrap_or("");
    let mark = match delivery {
        "delivered" => "✓",
        "queued" => "⧗",
        _ => "✕",
    };
    let id = m.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
    // Requests and replies are the interesting traffic, so label them.
    let tag = match (
        m.get("expects_reply").and_then(|x| x.as_bool()).unwrap_or(false),
        m.get("reply_to").and_then(|x| x.as_u64()),
    ) {
        (_, Some(n)) => format!("re #{n} "),
        (true, None) => format!("ask #{id} "),
        _ => String::new(),
    };
    println!("{mark} {tag}{from} → {to}: {body}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_names_are_validated() {
        assert!(dir_name("right").is_ok());
        assert!(dir_name("up").is_ok());
        let err = dir_name("sideways").unwrap_err().to_string();
        assert!(err.contains("right, left, up, or down"), "{err}");
    }

    #[test]
    fn pane_param_prefers_numbers_but_accepts_names() {
        assert_eq!(pane_param(Some("7".into())), Value::from(7u32));
        assert_eq!(pane_param(Some("reviewer".into())), Value::from("reviewer"));
    }

    #[test]
    fn cli_parses_the_documented_invocations() {
        // A parse failure here would only show up at runtime, so assert the shapes.
        Cli::try_parse_from(["horde"]).unwrap();
        Cli::try_parse_from(["horde", "roster"]).unwrap();
        Cli::try_parse_from(["horde", "send", "reviewer", "check", "this", "file"]).unwrap();
        Cli::try_parse_from(["horde", "send", "reviewer", "hi", "--now"]).unwrap();
        Cli::try_parse_from(["horde", "broadcast", "standup", "--space", "api"]).unwrap();
        Cli::try_parse_from(["horde", "spawn", "--cmd", "claude", "--name", "reviewer"]).unwrap();
        Cli::try_parse_from(["horde", "wait", "reviewer", "--until", "done"]).unwrap();
        Cli::try_parse_from(["horde", "pane", "read", "3", "--source", "detection"]).unwrap();
        Cli::try_parse_from(["horde", "bus", "tail", "-f"]).unwrap();
        Cli::try_parse_from(["horde", "integration", "install", "claude"]).unwrap();
        Cli::try_parse_from(["horde", "hook", "claude", "Stop"]).unwrap();
        Cli::try_parse_from(["horde", "api", "ping"]).unwrap();
        Cli::try_parse_from(["horde", "layout", "duo"]).unwrap();
    }

    #[test]
    fn multi_word_message_bodies_are_joined() {
        let cli = Cli::try_parse_from(["horde", "send", "reviewer", "check", "src/bus.rs"]).unwrap();
        match cli.command {
            Some(Command::Send { to, body, now }) => {
                assert_eq!(to, "reviewer");
                assert_eq!(body.join(" "), "check src/bus.rs");
                assert!(!now);
            }
            _ => panic!("wrong variant"),
        }
    }
}
