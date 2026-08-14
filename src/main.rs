//! horde — an agent-aware terminal multiplexer.
//!
//! One binary, three roles:
//!
//! * `horde` starts the daemon if needed, then attaches as a TUI client.
//! * `horde daemon` runs the server in the foreground.
//! * `horde <noun> <verb>` is a one-shot control call — the API agents drive themselves with.

mod cli;
mod client;
mod config;
mod daemon;
mod framing;
mod platform;
mod proto;
mod theme;

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::os::unix::process::CommandExt;

fn main() {
    if let Err(e) = real_main() {
        // `{:#}` prints the whole context chain, which is usually the actionable part.
        eprintln!("horde: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = cli::Cli::parse();

    match cli.command {
        Some(cli::Command::Daemon { import }) => {
            let (cfg, warnings) = config::Config::load();
            for w in &warnings {
                eprintln!("horde: config: {w}");
            }
            let rt = runtime()?;
            if import {
                rt.block_on(daemon::run_imported(cfg, warnings))
            } else {
                rt.block_on(daemon::run(cfg, warnings))
            }
        }
        // Everything else is a one-shot control call and needs no async runtime.
        Some(cmd) => cli::run(cmd),
        None => {
            let (cfg, warnings) = config::Config::load();
            ensure_daemon()?;
            runtime()?.block_on(client::attach(cfg, warnings))
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")
}

/// Start a daemon in the background if none is listening, then wait for its socket.
fn ensure_daemon() -> Result<()> {
    if cli::daemon_running() {
        return Ok(());
    }

    let exe = std::env::current_exe().context("cannot find the horde binary")?;
    let log = config::log_path();
    if let Some(p) = log.parent() {
        std::fs::create_dir_all(p)?;
    }
    // The daemon must outlive this process, so its output goes to the log rather than to
    // this terminal, which is about to become the TUI.
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let err = out.try_clone()?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);

    // Put the daemon in its own session, with no controlling terminal.
    //
    // This is what makes horde behave like tmux. `Command::spawn` leaves a child in the
    // parent's session and process group, so when the terminal window closes the kernel
    // sends SIGHUP to that whole group — taking the daemon, and every agent it owns, down
    // with the client. `setsid` detaches it so closing the window only ends the client.
    unsafe {
        cmd.pre_exec(|| {
            // SAFETY: async-signal-safe, and the only call made between fork and exec.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn().with_context(|| format!("could not start {} daemon", exe.display()))?;

    // Wait for it to bind. Two seconds is generous for a local socket, and failing with a
    // pointer to the log beats hanging.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if cli::daemon_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(anyhow!("daemon did not start within 2s — see {}", log.display()))
}
