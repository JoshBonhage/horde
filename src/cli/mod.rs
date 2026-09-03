//! Command line surface. Every subcommand is one control-channel call, which is what lets
//! an agent orchestrate horde with nothing but a shell.

pub mod docs;
/// Public because the client drives the install directly: the settings page and the setup
/// walkthrough do the same work as `horde integration install claude` without its printing.
pub mod integration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::daemon::tasks::Task;
use crate::daemon::triggers::Trigger;
use crate::proto::{Digest, Request, Response};

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
pub enum ThemeCmd {
    /// Every theme horde can use: the built-ins, then your own.
    List,
    /// Write a theme out to `themes/<name>.toml` so you can edit it.
    ///
    /// The file is a `base` plus the colours worth touching, not a full palette dump:
    /// anything left out keeps following the base, so a two-line file stays a two-line file.
    Edit {
        /// The theme to start from. Defaults to the one in your config.
        name: Option<String>,
        /// What to call the copy. Defaults to `<name>-mine`.
        #[arg(long = "as")]
        rename: Option<String>,
    },
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
    /// List themes, or write one out as a file you can edit.
    ///
    ///   horde theme list
    ///   horde theme edit gruvbox --as mine    # ~/.config/horde/themes/mine.toml
    Theme {
        #[command(subcommand)]
        cmd: ThemeCmd,
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
    /// Answer a request you were sent.
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
        /// Start on a model profile from `[models.<name>]` instead of `--cmd`.
        ///
        /// Wins over `--cmd` when both are given.
        #[arg(long)]
        profile: Option<String>,
        /// A first instruction, delivered once the new agent is up and at its prompt.
        ///
        /// Unlike `--task` this needs no board: it waits for the agent to exist, then arrives
        /// as an ordinary bus message.
        #[arg(long)]
        brief: Option<String>,
        /// Addressable name for the new agent.
        #[arg(long)]
        name: Option<String>,
        /// Where to put it: right, down, left, up.
        #[arg(long, default_value = "right")]
        split: String,
        /// Give this agent its own git worktree beside the project, so it cannot overwrite what its neighbours
        /// are editing. Takes a name; defaults to the agent's.
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        worktree: Option<String>,
        /// What it is for: reviewer, builder, docs.
        #[arg(long)]
        role: Option<String>,
        /// Enlist it for board work in this project.
        #[arg(long)]
        board: bool,
        /// A first job, put on the project's board for it to claim.
        #[arg(long)]
        task: Option<String>,
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
    /// The shared task board agents pull work from, one board per project.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Scheduled rules that put work on the board while nobody is watching.
    ///
    /// Nothing fires until `triggers.unattended = true` is set in config.toml: acting on its
    /// own is a different promise from running side by side, and has to be asked for.
    Trigger {
        #[command(subcommand)]
        cmd: TriggerCmd,
    },
    /// What happened while you were away.
    ///
    /// Reading it advances the window, so the next digest starts where this one ended.
    Digest {
        /// Look back this far instead, e.g. 30m, 2h, 90s.
        #[arg(long)]
        since: Option<String>,
        /// Do not advance the window.
        #[arg(long)]
        keep: bool,
        /// File it in the vault too, on today's dated note.
        #[arg(long)]
        note: bool,
        #[arg(long)]
        json: bool,
    },
    /// Check whether this terminal can draw real images, by drawing one.
    ///
    /// Detecting the protocol by name is easy and being wrong is not symmetrical: a terminal
    /// wrongly believed capable reserves rows and draws nothing in them, so a picture that
    /// worked becomes a blank gap. This makes the answer evidence.
    Images,
    /// Write a note into the vault.
    ///
    /// The way an agent records something worth keeping. Notes written this way are
    /// attributed, size-capped, and land in their own folder — so what a fleet wrote down is
    /// always separable from what you did.
    ///
    ///   horde note "Auth findings" --body "…"
    ///   somecommand | horde note "Build log" --append
    Note {
        /// The note's title, which is also its filename. `[[Auth findings]]` finds it.
        title: String,
        /// The body. Read from stdin when not given, so output can be piped in.
        #[arg(long)]
        body: Option<String>,
        /// Add to the note rather than replacing it.
        #[arg(long)]
        append: bool,
        /// Write it somewhere other than the agent folder.
        #[arg(long)]
        path: Option<String>,
        /// Credit someone other than the calling pane's agent.
        #[arg(long)]
        by: Option<String>,
        /// Which project's vault. Defaults to the focused one.
        #[arg(long)]
        space: Option<String>,
    },
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
    /// What your agents are for, across every project.
    Role {
        #[command(subcommand)]
        cmd: RoleCmd,
    },
    /// Git worktrees horde made, so agents in one repository do not overwrite each other.
    Worktree {
        #[command(subcommand)]
        cmd: WorktreeCmd,
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
pub enum TriggerCmd {
    /// Add a rule. Needs one of --every/--at, and one of --task/--to/--spawn.
    ///
    /// `--task` is usually the one to reach for: the work lands on the board and whichever
    /// agent is free claims it, so the rule never has to know who is idle. It is refused when
    /// `agents.board = false`.
    Add {
        /// How often, e.g. 30m, 2h, 1d. At least 60s.
        #[arg(long, conflicts_with = "at")]
        every: Option<String>,
        /// Daily at a local time, e.g. 09:00.
        #[arg(long)]
        at: Option<String>,
        /// Which days, with --at: mon-fri, sat,sun, mon,wed,fri. Every day if omitted.
        #[arg(long)]
        days: Option<String>,
        /// Only act when this shell command exits 0, checked when the schedule comes round.
        /// e.g. "! cargo test -q" to act when the tests fail.
        #[arg(long = "when")]
        when: Option<String>,
        /// Work to put on the board.
        #[arg(long, conflicts_with = "to")]
        task: Option<String>,
        /// Only an agent with this role may take the work, with --task.
        #[arg(long, requires = "task")]
        role: Option<String>,
        /// Agent to push a line at instead of using the board.
        #[arg(long, requires = "body")]
        to: Option<String>,
        /// Message body, with --to.
        #[arg(long)]
        body: Option<String>,
        /// Command to start an agent with, e.g. "claude". Runs with whatever permissions you
        /// give it — horde adds no flags of its own.
        #[arg(long, conflicts_with_all = ["task", "to"])]
        spawn: Option<String>,
        /// Addressable name for a spawned agent, with --spawn.
        #[arg(long)]
        name: Option<String>,
    },
    /// Show every rule, when it last fired, and what it does.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Delete a rule.
    Rm { trigger: u64 },
    /// Turn a rule back on.
    On { trigger: u64 },
    /// Turn a rule off, keeping it. `--all` is the kill switch.
    Off {
        trigger: Option<u64>,
        /// Turn every rule off at once.
        #[arg(long)]
        all: bool,
    },
    /// Run a rule now, ignoring its schedule.
    ///
    /// The only way to test a rule set for nine in the morning at any other time of day.
    Fire { trigger: u64 },
}

#[derive(Subcommand)]
pub enum TaskCmd {
    /// Put work on the board for whoever is free in this project.
    Add {
        text: Vec<String>,
        /// Put it on another project's board, by space name.
        #[arg(long)]
        space: Option<String>,
        /// Only an agent with this role may take it. Omit for work anyone free can do.
        #[arg(long)]
        role: Option<String>,
    },
    /// Take board work in this project from now on. Without this, nothing is ever offered.
    Work {
        /// Stop taking it.
        #[arg(long)]
        off: bool,
    },
    /// Drop every open task in this project.
    Clear {
        /// Every project, not just this one.
        #[arg(long)]
        everywhere: bool,
        /// Also drop tasks an agent is currently holding.
        #[arg(long)]
        claimed: bool,
    },
    /// Take the oldest open task, or a specific one. Prints nothing if the board is empty.
    Claim {
        /// Task number. Omit to take the oldest open one.
        task: Option<u64>,
    },
    /// Finish the task you claimed.
    Done {
        /// Task number. Omit for your own claimed task.
        task: Option<u64>,
        /// A one-line note about the outcome.
        #[arg(long)]
        result: Option<String>,
    },
    /// Put a task back on the board, or abandon it.
    Release {
        task: u64,
        /// Retire it instead of reopening it.
        #[arg(long)]
        drop: bool,
    },
    /// Show this project's board.
    List {
        /// Include finished and abandoned tasks.
        #[arg(long)]
        all: bool,
        /// Every project's board, not just this one.
        #[arg(long)]
        everywhere: bool,
        #[arg(long)]
        json: bool,
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
    /// Rename a space. Names address spaces in `horde send`, so a clash is uniquified —
    /// the name you actually got is printed back.
    Rename {
        name: String,
        to: String,
    },
    /// Set or cycle a space's accent colour. Omit the slot to step to the next one.
    Accent {
        name: String,
        slot: Option<u8>,
    },
    /// Fold a space's agents away in the sidebar.
    Collapse {
        name: String,
        #[arg(long)]
        expand: bool,
    },
}

#[derive(Subcommand)]
pub enum TabCmd {
    List,
    New { name: Option<String> },
    Close,
    Rename { name: String },
}

#[derive(Subcommand)]
pub enum WorktreeCmd {
    /// Every worktree horde made for this project, and who is in it.
    List,
    /// Remove one. The branch survives: it may hold commits.
    Remove {
        name: String,
        /// Remove it even with uncommitted changes in it.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum RoleCmd {
    /// Every role in use, and how many panes wear it.
    List,
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
    /// Label what a pane is for: reviewer, builder, docs. Omit the role to clear it.
    Role {
        pane: Option<String>,
        role: Option<String>,
    },
    /// Hold a pane at the top of the sidebar's agent list.
    Pin {
        pane: Option<String>,
        #[arg(long)]
        off: bool,
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

        Command::Theme { cmd } => return theme_cmd(cmd),
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
            // Broken out, because "sent to 4 agents" reads as four agents having heard it when
            // three of them were mid-turn. Queued is a normal outcome and worth naming as one.
            let msgs = v.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
            fn delivery(m: &Value) -> &str {
                m.get("delivery").and_then(|d| d.as_str()).unwrap_or("")
            }
            let now = msgs.iter().filter(|m| delivery(m) == "delivered").count();
            let queued = msgs.iter().filter(|m| delivery(m) == "queued").count();
            match (msgs.len(), queued) {
                (0, _) => println!("nobody to send it to — `horde roster` shows who is here"),
                (n, 0) => println!("delivered to {n} agent(s)"),
                (_, q) if q == msgs.len() => {
                    println!("queued for {q} agent(s) — they are busy; horde delivers as they free up")
                }
                (_, q) => println!("delivered to {now}, queued for {q}"),
            }
        }

        Command::Spawn { cmd, profile, brief, name, split, worktree, role, board, task } => {
            // `--worktree` with no value means "name it after the agent", which the daemon
            // resolves because it is the side that knows what the agent ended up called.
            let worktree = worktree.map(|w| if w.is_empty() { Value::Bool(true) } else { json!(w) });
            let v = call(
                "agent.spawn",
                json!({
                    "cmd": cmd, "profile": profile, "brief": brief, "name": name, "split": dir_name(&split)?,
                    "worktree": worktree, "role": role, "board": board, "task": task,
                    // Who asked, so an agent building a fleet is counted against the cap.
                    "from": self_pane(),
                }),
            )?;
            // What it actually ran, which a profile decides on the daemon side.
            let ran = v.get("cmd").and_then(|c| c.as_str()).unwrap_or(&cmd);
            println!("pane {} running {ran}", v.get("pane").unwrap_or(&Value::Null));
            if let Some(w) = v.get("worktree").and_then(|w| w.as_str()) {
                println!("  worktree {w}");
            }
            if let Some(t) = v.get("task").and_then(|t| t.as_u64()) {
                println!("  task #{t} on the board for it");
            }
            if let Some(b) = v.get("brief").and_then(|b| b.as_u64()) {
                println!("  brief #{b} waiting for it to come up");
            }
        }
        Command::Worktree { cmd } => match cmd {
            WorktreeCmd::List => {
                let v = call("worktree.list", json!({ "from": self_pane() }))?;
                let rows = v.as_array().cloned().unwrap_or_default();
                if rows.is_empty() {
                    println!("no worktrees — `horde spawn --cmd claude --name x --worktree`");
                }
                for w in rows {
                    let s = |k: &str| w.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    // Who is in it, because that is what decides whether it can be removed.
                    let held = match w.get("agent").and_then(|v| v.as_str()) {
                        Some(a) => format!("  {a}"),
                        None => String::new(),
                    };
                    let dirty = if w.get("dirty").and_then(|v| v.as_bool()).unwrap_or(false) {
                        "  uncommitted"
                    } else {
                        ""
                    };
                    // The path last, the way `space list` puts the cwd last. Worth printing
                    // now that it is not derivable from the name: the agent is `ads` and the
                    // directory is `<project>-ads`, and a tree an older horde nested inside
                    // the repository is somewhere else again.
                    let line = format!("{:<14} {:<22}{held}{dirty}", s("name"), s("branch"));
                    println!("{line:<48} {}", s("path"));
                }
            }
            WorktreeCmd::Remove { name, force } => {
                let v =
                    call("worktree.remove", json!({ "name": name, "force": force, "from": self_pane() }))?;
                println!("removed {}", v.get("removed").unwrap_or(&Value::Null));
                println!("  branch horde/{name} kept — `git branch -D horde/{name}` to drop it");
            }
        },

        Command::Wait { target, until, timeout } => {
            let want = match until.as_str() {
                // `serving` is here for the same reason the rest are: waiting for the dev
                // server to be up before pointing anything at it is a real thing to want.
                "idle" | "done" | "blocked" | "working" | "serving" => until.clone(),
                other => {
                    return Err(anyhow!(
                        "--until must be idle, done, blocked, working, or serving (got {other:?})"
                    ))
                }
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

        Command::Task { cmd } => match cmd {
            TaskCmd::Add { text, space, role } => {
                let text = text.join(" ");
                let t = call(
                    "task.add",
                    json!({ "text": text, "from": self_pane(), "space": space, "role": role }),
                )?;
                let where_ = t.get("space").and_then(|s| s.as_str()).unwrap_or("");
                let for_ = t.get("role").and_then(|s| s.as_str()).unwrap_or("");
                println!(
                    "#{} added{}{}",
                    t.get("id").and_then(|x| x.as_u64()).unwrap_or(0),
                    if where_.is_empty() { String::new() } else { format!(" to {where_}") },
                    if for_.is_empty() { String::new() } else { format!(" for {for_}") }
                );
                // Said here because here is where it can still be changed. Work for a role
                // nobody has is not offered to anyone and reads afterwards as a quiet board.
                if let Some(w) = t.get("warning").and_then(|w| w.as_str()) {
                    eprintln!("note: {w}");
                }
            }
            TaskCmd::Work { off } => {
                let v = call("task.work", json!({ "from": self_pane(), "on": !off }))?;
                if v.get("board").and_then(|b| b.as_bool()).unwrap_or(false) {
                    println!("taking board work — `horde task claim` when you are free");
                } else {
                    println!("no longer taking board work");
                }
            }
            TaskCmd::Clear { everywhere, claimed } => {
                let v = call(
                    "task.clear",
                    json!({ "from": self_pane(), "everywhere": everywhere, "claimed": claimed }),
                )?;
                let n = v.get("dropped").and_then(|d| d.as_u64()).unwrap_or(0);
                let scope = v.get("space").and_then(|s| s.as_str()).unwrap_or("every project");
                match n {
                    0 => println!("nothing to clear in {scope}"),
                    1 => println!("1 task dropped from {scope}"),
                    n => println!("{n} tasks dropped from {scope}"),
                }
            }
            TaskCmd::Claim { task } => {
                let v = call("task.claim", json!({ "task": task, "from": self_pane() }))?;
                match v.as_object() {
                    Some(t) => {
                        // The text goes to stdout alone so it can be captured and worked on.
                        println!("{}", t.get("text").and_then(|x| x.as_str()).unwrap_or(""));
                        eprintln!(
                            "claimed #{} — finish with: horde task done --result \"...\"",
                            t.get("id").and_then(|x| x.as_u64()).unwrap_or(0)
                        );
                    }
                    None => eprintln!("nothing on the board"),
                }
            }
            TaskCmd::Done { task, result } => {
                let v = call(
                    "task.done",
                    json!({ "task": task, "result": result, "from": self_pane() }),
                )?;
                println!("#{} done", v.get("id").and_then(|x| x.as_u64()).unwrap_or(0));
            }
            TaskCmd::Release { task, drop } => {
                let v = call("task.release", json!({ "task": task, "drop": drop }))?;
                let state = v.get("state").and_then(|x| x.as_str()).unwrap_or("?");
                println!("#{task} is now {state}");
            }
            TaskCmd::List { all, everywhere, json: as_json } => {
                let v = call(
                    "task.list",
                    json!({ "from": self_pane(), "everywhere": everywhere }),
                )?;
                if as_json {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    return Ok(());
                }
                // Decode the daemon's own type rather than re-deriving glyphs from
                // strings, so the board reads the same here as it does in the sidebar.
                let items: Vec<Task> = serde_json::from_value(v)?;
                let shown: Vec<&Task> =
                    items.iter().filter(|t| all || t.is_open() || t.is_claimed()).collect();
                if shown.is_empty() {
                    println!("board is empty — add work with `horde task add \"...\"`");
                    return Ok(());
                }
                // Which roles could actually take something here, so work that will sit can be
                // marked as such. Asked once for the whole listing rather than per row.
                let covered: Vec<String> = call("task.roles", json!({ "from": self_pane() }))
                    .ok()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let mut stranded = 0usize;
                for t in shown {
                    let owner =
                        t.owner.as_ref().map(|o| format!("  [{o}]")).unwrap_or_default();
                    // Which project, but only when the list spans more than one. In the
                    // ordinary case every row would carry the same word, which is noise.
                    let where_ = match (everywhere, &t.space) {
                        (true, Some(sp)) => format!("  ({sp})"),
                        _ => String::new(),
                    };
                    // Who it is for, and whether anybody here is that. An open task naming a
                    // role no enlisted agent has is never offered and never claimed, so it says
                    // so rather than looking like the next thing to be picked up.
                    let for_ = match &t.role {
                        None => String::new(),
                        Some(r) if !t.is_open() => format!("  <{r}>"),
                        Some(r) if covered.iter().any(|c| c == r) => format!("  <{r}>"),
                        Some(r) => {
                            stranded += 1;
                            format!("  <{r}: nobody here>")
                        }
                    };
                    println!("{} #{:<4} {}{for_}{where_}{owner}", t.state.glyph(), t.id, t.text);
                    if let Some(r) = &t.result {
                        println!("       → {r}");
                    }
                }
                if stranded > 0 {
                    println!(
                        "\n{stranded} task{} waiting on a role nobody enlisted here has — \
                         `horde spawn --role <role> --board`, or claim it by number to override",
                        if stranded == 1 { "" } else { "s" }
                    );
                }
            }
        },

        Command::Trigger { cmd } => match cmd {
            TriggerCmd::Add { every, at, days, when, task, role, to, body, spawn, name } => {
                let v = call(
                    "trigger.add",
                    json!({
                        "every": every, "at": at, "days": days, "when": when,
                        "task": task, "role": role, "to": to, "body": body,
                        "spawn": spawn, "name": name,
                        "from": self_pane(),
                    }),
                )?;
                let armed = v.get("armed").and_then(|x| x.as_bool()).unwrap_or(false);
                let t: Trigger = serde_json::from_value(v["trigger"].clone())?;
                println!("#{} added — {}", t.id, t.describe());
                if !armed {
                    // Adding a rule that cannot fire is the one mistake worth interrupting
                    // for: everything looks right, and nothing ever happens.
                    eprintln!(
                        "note: triggers are off. Set `unattended = true` under [triggers] in \
                         config.toml to arm them, or use `horde trigger fire {}` to run this \
                         one by hand.",
                        t.id
                    );
                }
            }
            TriggerCmd::List { json: as_json } => {
                let v = call("trigger.list", json!({}))?;
                if as_json {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    return Ok(());
                }
                let armed = v.get("armed").and_then(|x| x.as_bool()).unwrap_or(false);
                let items: Vec<Trigger> = serde_json::from_value(v["triggers"].clone())?;
                if items.is_empty() {
                    println!(
                        "no triggers — add one with `horde trigger add --every 1d --task \"...\"`"
                    );
                    return Ok(());
                }
                if !armed {
                    println!("triggers are off — [triggers] unattended = true arms them\n");
                }
                for t in &items {
                    let now = crate::daemon::now_millis();
                    let last = match t.last_fired {
                        Some(ms) => format!("last {} ago", ago(now.saturating_sub(ms))),
                        None => "never fired".to_string(),
                    };
                    println!(
                        "{} #{:<4} {}",
                        if t.enabled { "○" } else { "✕" },
                        t.id,
                        t.describe()
                    );
                    println!("       {last}, {} so far, by {}", t.fire_count, t.by);
                }
            }
            TriggerCmd::Rm { trigger } => {
                call("trigger.rm", json!({ "trigger": trigger }))?;
                println!("#{trigger} removed");
            }
            TriggerCmd::On { trigger } => {
                call("trigger.enable", json!({ "trigger": trigger, "on": true }))?;
                println!("#{trigger} on");
            }
            TriggerCmd::Off { trigger, all } => match (trigger, all) {
                (Some(id), _) => {
                    call("trigger.enable", json!({ "trigger": id, "on": false }))?;
                    println!("#{id} off");
                }
                (None, true) => {
                    let v = call("trigger.enable", json!({ "on": false }))?;
                    let n = v.get("disabled").and_then(|x| x.as_u64()).unwrap_or(0);
                    println!("{n} turned off");
                }
                // Bare `trigger off` is ambiguous between one rule and all of them, and
                // guessing "all" would be the expensive guess.
                (None, false) => {
                    return Err(anyhow!("name a trigger, or pass --all to turn every one off"))
                }
            },
            TriggerCmd::Fire { trigger } => {
                let v = call("trigger.fire", json!({ "trigger": trigger }))?;
                println!("{}", v.get("did").and_then(|x| x.as_str()).unwrap_or("fired"));
            }
        },

        Command::Digest { since, keep, note, json: as_json } => {
            let mut params = json!({ "keep": keep, "note": note });
            if let Some(spec) = &since {
                params["since"] = json!(parse_duration(spec)?);
            }
            let v = call("digest", params)?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let filed = v.get("note").and_then(|p| p.as_str()).map(String::from);
            print_digest(&serde_json::from_value(v)?);
            if let Some(path) = filed {
                println!("\nfiled in {path}");
            }
        }

        Command::Images => {
            use std::io::Write;
            let looks = crate::client::kitty::looks_capable();
            let on = crate::client::kitty::supported();
            println!("terminal:  {}", std::env::var("TERM").unwrap_or_default());
            println!("           {}", std::env::var("TERM_PROGRAM").unwrap_or_default());
            println!("looks capable: {}", if looks { "yes" } else { "no" });
            println!("images on:     {}", if on { "yes (HORDE_IMAGES)" } else { "no" });
            println!();

            // Four coloured squares, big enough to be unmistakable and small enough to be
            // obviously not a rendering artefact.
            let mut img = image::RgbaImage::new(64, 64);
            for (x, y, p) in img.enumerate_pixels_mut() {
                *p = match (x < 32, y < 32) {
                    (true, true) => image::Rgba([220, 40, 40, 255]),
                    (false, true) => image::Rgba([40, 200, 80, 255]),
                    (true, false) => image::Rgba([60, 90, 230, 255]),
                    (false, false) => image::Rgba([240, 200, 40, 255]),
                };
            }
            let dir = std::env::temp_dir().join(format!("horde-imgtest-{}.png", std::process::id()));
            img.save(&dir)?;

            println!("A red/green/blue/yellow square should appear below.");
            println!();
            let place = crate::client::kitty::Place {
                path: dir.clone(),
                // Wherever the cursor is; the escape moves it, so put it just below here.
                x: 0,
                y: 0,
                cols: 20,
                rows: 10,
                crop: None,
            };
            let mut out = std::io::stdout();
            if let Some((png, _, _)) = crate::client::kitty::encode(&dir) {
                // Placed relative to the cursor rather than the screen, so it lands under
                // this text instead of at the top of the window.
                out.write_all(&crate::client::kitty::place_here(1, &png, &place))?;
                out.flush()?;
            }
            for _ in 0..place.rows {
                println!();
            }
            let _ = std::fs::remove_file(&dir);
            println!();
            println!("Saw it?  run horde with HORDE_IMAGES=1 for full-resolution pictures.");
            println!("Did not? leave it unset — notes fall back to coloured half blocks,");
            println!("         which are lower resolution but work in every terminal.");
        }
        Command::Note { title, body, append, path, by, space } => {
            // Piped input is the point: `cargo test | horde note "Test run" --append` is the
            // shape this verb exists for, and asking for `--body` there would mean shelling
            // out to read a file back.
            let body = match body {
                Some(b) => b,
                None => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    buf
                }
            };
            if body.trim().is_empty() {
                return Err(anyhow!("nothing to write — give --body or pipe something in"));
            }
            let mut params = json!({ "title": title, "body": body, "append": append });
            if let Some(p) = &path {
                params["path"] = json!(p);
            }
            if let Some(b) = &by {
                params["by"] = json!(b);
            }
            if let Some(s) = &space {
                params["space"] = json!(s);
            }
            if let Some(p) = self_pane() {
                params["pane"] = json!(p);
            }
            let v = call("vault.write", params)?;
            println!("{}", v.get("path").and_then(|p| p.as_str()).unwrap_or("written"));
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

        Command::Role { cmd } => match cmd {
            RoleCmd::List => {
                let v = call("role.list", json!({}))?;
                let rows = v.as_array().cloned().unwrap_or_default();
                if rows.is_empty() {
                    // Distinguish "nothing is labelled" from "the call failed", the same way
                    // an empty board does. Declared-but-unused roles are still worth listing,
                    // so this is only for a genuinely empty answer.
                    println!("no roles in use");
                }
                for r in rows {
                    let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let panes = r.get("panes").and_then(|v| v.as_u64()).unwrap_or(0);
                    let declared = r.get("declared").and_then(|v| v.as_bool()).unwrap_or(false);
                    let mark = if declared { "" } else { "  (undeclared)" };
                    println!("{name:<16} {panes} panes{mark}");
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
            SpaceCmd::Rename { name, to } => {
                let v = call("space.rename", json!({ "name": name, "to": to }))?;
                // The daemon uniquifies a clash silently, so print what it settled on rather
                // than what was asked for.
                println!("{}", v.get("name").and_then(|v| v.as_str()).unwrap_or(&to));
            }
            SpaceCmd::Accent { name, slot } => {
                let v = call("space.accent", json!({ "name": name, "slot": slot }))?;
                println!("{name} is on colour {}", v.get("slot").and_then(|v| v.as_u64()).unwrap_or(0));
            }
            SpaceCmd::Collapse { name, expand } => {
                let v = call("space.collapse", json!({ "name": name, "collapsed": !expand }))?;
                let now = v.get("collapsed").and_then(|v| v.as_bool()).unwrap_or(false);
                println!("{name} {}", if now { "collapsed" } else { "expanded" });
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
            TabCmd::Rename { name } => {
                call("tab.rename", json!({ "name": name }))?;
                println!("renamed");
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
            PaneCmd::Role { pane, role } => {
                // `from` is sent so the daemon can tell an agent relabelling itself from a person
                // at a shell doing the labelling. See the `pane.role` handler.
                let v = call(
                    "pane.role",
                    json!({
                        "pane": pane,
                        "role": role.unwrap_or_default(),
                        "from": self_pane(),
                    }),
                )?;
                // The normalised form, so a script that filters on what it just set matches.
                match v.get("role").and_then(|v| v.as_str()) {
                    Some(r) => println!("{r}"),
                    None => println!("role cleared"),
                }
            }
            PaneCmd::Pin { pane, off } => {
                let v = call("pane.pin", json!({ "pane": pane, "pinned": !off }))?;
                let now = v.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false);
                println!("{}", if now { "pinned" } else { "unpinned" });
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
            let leader = cfg.leader.describe();
            println!("prefix: {prefix}\nleader: {leader}\n");
            for (name, trigger, _) in cfg.keys.described() {
                let key = match trigger {
                    crate::config::Trigger::Prefix(c) => format!("{prefix} {}", c.describe()),
                    crate::config::Trigger::Direct(c) => c.describe(),
                    crate::config::Trigger::Leader(s) => format!("{leader} {}", s.describe()),
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
        // An agent you did not start is the one fact about a roster row you would most want
        // volunteered rather than discovered.
        if let Some(by) = a.get("spawned_by").and_then(|x| x.as_u64()) {
            // Zero is horde succeeding an agent that ran out, not a rule — there is no
            // trigger #0, and printing one sends you looking for a rule that does not exist.
            why.push_str(&match by {
                0 => " [succeeded a spent agent]".to_string(),
                n => format!(" [by trigger #{n}]"),
            });
        }
        println!(
            "{name:<14} {state:<9} {:<8} {space:<14} {why}",
            crate::client::ui::pane_widget::fmt_elapsed(secs)
        );
    }
}

/// `90s`, `30m`, `2h`, `1d`, or a bare number of seconds. Returned as seconds.
pub(crate) fn parse_duration(spec: &str) -> Result<u64> {
    let spec = spec.trim();
    let (digits, mult) = match spec.chars().last() {
        Some('s') => (&spec[..spec.len() - 1], 1),
        Some('m') => (&spec[..spec.len() - 1], 60),
        Some('h') => (&spec[..spec.len() - 1], 3600),
        Some('d') => (&spec[..spec.len() - 1], 86_400),
        _ => (spec, 1),
    };
    let n: u64 = digits
        .trim()
        .parse()
        .map_err(|_| anyhow!("cannot read {spec:?} as a duration — try 30m, 2h, or 90s"))?;
    Ok(n * mult)
}

/// `42m`, `3h`, `2d` — how long ago, at the coarsest unit that is still true.
fn ago(millis: u64) -> String {
    let secs = millis / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The digest, in the order you would want to be told: what is stuck, what finished, what
/// the board did, then what was said.
fn print_digest(d: &Digest) {
    let elapsed = d.now.saturating_sub(d.since);
    let window = ago(elapsed);
    if d.is_empty() {
        // Say which window was checked — "nothing happened" without a window is
        // unfalsifiable. Except right after a read, when the window is the point.
        if elapsed < 5_000 && !d.fresh {
            println!("nothing new since you last looked");
        } else {
            println!("nothing to report from the last {window}");
        }
        for a in &d.working {
            println!("  ◐ {} still working, {}", a.name, ago(a.elapsed * 1000));
        }
        return;
    }

    println!("while you were away · {window}");

    if !d.needs_you.is_empty() {
        println!("\n  needs you");
        for a in &d.needs_you {
            println!("    ◍ {:<16} stuck {:<6} {}", a.name, ago(a.elapsed * 1000), a.reason);
        }
    }
    if !d.finished.is_empty() {
        println!("\n  finished");
        for a in &d.finished {
            let detail = a.activity.clone().unwrap_or_else(|| a.reason.clone());
            println!("    ● {:<16} {:<6} {}", a.name, ago(a.elapsed * 1000), detail);
        }
    }
    if !d.working.is_empty() {
        println!("\n  still working");
        for a in &d.working {
            let detail = a.activity.clone().unwrap_or_default();
            println!("    ◐ {:<16} {:<6} {}", a.name, ago(a.elapsed * 1000), detail);
        }
    }

    // Before the board, because a firing is the reason some of the board's work exists.
    if !d.fired.is_empty() {
        println!("\n  horde decided");
        for f in &d.fired {
            println!("    ▸ {f}");
        }
    }

    if !d.tasks_done.is_empty() || d.tasks_added > 0 || d.tasks_open + d.tasks_claimed > 0 {
        println!("\n  board");
        for t in &d.tasks_done {
            let glyph = if t.dropped { "✕" } else { "●" };
            let owner = t.owner.clone().unwrap_or_else(|| "?".into());
            println!("    {glyph} #{:<3} {}  [{}]", t.id, t.text, owner);
            match (&t.result, t.dropped) {
                (Some(r), _) => println!("           → {r}"),
                (None, true) => println!("           → dropped, no result"),
                (None, false) => {}
            }
        }
        let mut standing = Vec::new();
        if d.tasks_added > 0 {
            standing.push(format!("{} added", d.tasks_added));
        }
        if d.tasks_open > 0 {
            standing.push(format!("{} open", d.tasks_open));
        }
        if d.tasks_claimed > 0 {
            standing.push(format!("{} claimed", d.tasks_claimed));
        }
        if !standing.is_empty() {
            println!("    {}", standing.join(", "));
        }
    }

    if !d.messages.is_empty() {
        println!("\n  bus · {}", plural(d.messages.len(), "message"));
        for m in &d.messages {
            // A glyph is enough for delivered — that is the expected case. The other two
            // are the ones you would want to act on, so they say so in words.
            let mark = match m.delivery {
                crate::proto::Delivery::Delivered => "✓",
                crate::proto::Delivery::Queued => "⧗ queued",
                crate::proto::Delivery::Dropped => "✕ dropped",
            };
            let tag = match (m.expects_reply, m.reply_to) {
                (_, Some(n)) => format!("re #{n} "),
                (true, None) => format!("ask #{} ", m.id),
                _ => String::new(),
            };
            let arrow = if m.broadcast { "→ all" } else { &format!("→ {}", m.to) };
            println!("    {mark} {tag}{} {arrow}: {}", m.from, one_line(&m.body, 60));
        }
    }

    if !d.gone.is_empty() {
        println!("\n  exited");
        for name in &d.gone {
            println!("    ✕ {name}");
        }
    }
    if !d.warnings.is_empty() {
        println!("\n  warnings");
        for w in &d.warnings {
            println!("    ! {w}");
        }
    }
}

/// `1 message` / `3 messages`. A count that says "1 messages" reads as a bug.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Collapse a message body to one line for a list. A digest is a scan, not a transcript.
fn one_line(body: &str, max: usize) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
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
    fn durations_accept_the_units_a_human_would_type() {
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1d").unwrap(), 86_400);
        // A bare number is seconds, which is what the socket API takes.
        assert_eq!(parse_duration("45").unwrap(), 45);
        let err = parse_duration("soon").unwrap_err().to_string();
        assert!(err.contains("30m"), "the error should show the format: {err}");
    }

    #[test]
    fn ago_uses_the_coarsest_unit_that_is_still_true() {
        assert_eq!(ago(0), "0s");
        assert_eq!(ago(59_000), "59s");
        assert_eq!(ago(60_000), "1m");
        assert_eq!(ago(3_600_000), "1h");
        assert_eq!(ago(90_000_000), "1d");
    }

    #[test]
    fn one_line_flattens_and_truncates_a_body() {
        assert_eq!(one_line("two\n  lines", 40), "two lines");
        let long = one_line(&"x".repeat(100), 20);
        assert_eq!(long.chars().count(), 20, "{long}");
        assert!(long.ends_with('…'));
    }

    #[test]
    fn counts_read_correctly_at_one() {
        assert_eq!(plural(1, "message"), "1 message");
        assert_eq!(plural(3, "message"), "3 messages");
    }

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
        Cli::try_parse_from(["horde", "task", "add", "write the tests"]).unwrap();
        Cli::try_parse_from(["horde", "task", "claim"]).unwrap();
        Cli::try_parse_from(["horde", "task", "done", "--result", "green"]).unwrap();
        Cli::try_parse_from(["horde", "digest"]).unwrap();
        Cli::try_parse_from(["horde", "digest", "--since", "30m", "--keep"]).unwrap();
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


/// `horde theme list` / `horde theme edit`.
///
/// Both are offline: a theme is a file on disk and a palette compiled in, so neither needs a
/// daemon. That matters for `edit` in particular -- the point is to write the file *before*
/// starting horde with it.
fn theme_cmd(cmd: ThemeCmd) -> Result<()> {
    use std::io::Write as _;
    let dir = crate::config::themes_dir();
    match cmd {
        ThemeCmd::List => {
            let builtin = crate::theme::Theme::builtin_names();
            for name in crate::theme::Theme::names() {
                let own = if builtin.contains(&name.as_str()) { "" } else { "  (yours)" };
                println!("{name}{own}");
            }
            println!("\nyour themes live in {}", dir.display());
            Ok(())
        }
        ThemeCmd::Edit { name, rename } => {
            // The configured theme is the one you are looking at, which is almost always the
            // one you want to start from.
            let from = match name {
                Some(n) => n,
                None => crate::config::Config::load().0.theme.name,
            };
            let base = crate::theme::Theme::builtin(&from).ok_or_else(|| {
                anyhow!(
                    "{from:?} is not a built-in theme. A copy has to start from one of: {}",
                    crate::theme::Theme::builtin_names().join(", ")
                )
            })?;
            let to = rename.unwrap_or_else(|| format!("{from}-mine"));
            if crate::theme::Theme::builtin(&to).is_some() {
                return Err(anyhow!(
                    "{to:?} is a built-in theme name, and a built-in always wins — \
                     pick another with --as"
                ));
            }
            let path = dir.join(format!("{to}.toml"));
            if path.exists() {
                return Err(anyhow!("{} already exists", path.display()));
            }
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;
            let mut f = std::fs::File::create(&path)
                .with_context(|| format!("writing {}", path.display()))?;
            write!(f, "{}", crate::theme::starter_file(&base))?;
            println!("wrote {}", path.display());
            println!("edit it, then set `[theme] name = \"{to}\"` in config.toml");
            Ok(())
        }
    }
}
