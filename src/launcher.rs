//! Game launcher — a registry of launchable games plus a single-slot process
//! supervisor.
//!
//! The signaling server exposes this over HTTP (`/api/games`, `/api/launch`,
//! `/api/stop`) so the browser can pick a game and start it on the capture
//! display *before* connecting the WebRTC stream. Only one game runs at a time:
//! launching a new one stops the current one first.
//!
//! Games are launched onto the same X display arcadia captures (`--display`,
//! default `:99`) with `PULSE_SERVER=127.0.0.1` so their audio reaches the
//! `arcadia` PulseAudio sink that `--audio pulse` records. A game can opt into a
//! nested `gamescope` wrap (per-game `gamescope = true`) for fullscreen scaling;
//! the accelerated headless Xorg makes nesting possible (Xvfb can't — it has no
//! GPU). See arcadia/CLAUDE.md for the host setup.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use tracing::{info, warn};

/// Gamescope output size when a game opts into the nested wrap. Matches the
/// capture display resolution so the encoder sees a full frame.
const GAMESCOPE_W: &str = "1280";
const GAMESCOPE_H: &str = "720";

/// One launchable entry, parsed from a `[[game]]` table in the config file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Game {
    /// Stable identifier used by `/api/launch` (e.g. "steam", "retroarch").
    pub id: String,
    /// Human label shown in the web launcher.
    pub name: String,
    /// Executable to run (looked up on `PATH`).
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for the process (optional).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables for the process (optional).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Wrap the command in a nested `gamescope ... -- <command>`.
    #[serde(default)]
    pub gamescope: bool,
    /// Extra gamescope flags, used only when `gamescope = true`.
    #[serde(default)]
    pub gamescope_args: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GamesConfig {
    #[serde(default)]
    game: Vec<Game>,
}

struct Running {
    id: String,
    child: Child,
}

/// The launcher: an immutable game list plus the one currently-running child.
pub struct Launcher {
    games: Vec<Game>,
    display: String,
    current: Mutex<Option<Running>>,
}

impl Launcher {
    /// Load the games config. A missing file is tolerated (empty list + a
    /// warning) so video-only sessions still work; a malformed file is a hard
    /// error so typos surface loudly.
    pub fn load(config_path: &str, display: &str) -> Result<Self> {
        let games = match std::fs::read_to_string(config_path) {
            Ok(text) => {
                let cfg: GamesConfig = toml::from_str(&text)
                    .with_context(|| format!("parsing games config {config_path}"))?;
                info!(
                    "loaded {} game(s) from {}",
                    cfg.game.len(),
                    config_path
                );
                cfg.game
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                warn!(
                    "games config {} not found — launcher has no games (video-only)",
                    config_path
                );
                Vec::new()
            }
            Err(e) => return Err(e).with_context(|| format!("reading games config {config_path}")),
        };

        Ok(Self {
            games,
            display: display.to_string(),
            current: Mutex::new(None),
        })
    }

    /// The configured games (for `/api/games`).
    pub fn games(&self) -> &[Game] {
        &self.games
    }

    /// Id of the currently-running game, if one is still alive.
    pub fn current_id(&self) -> Option<String> {
        let mut cur = self.current.lock().unwrap();
        if let Some(running) = cur.as_mut() {
            // Reap if it exited on its own so we don't report a dead game.
            match running.child.try_wait() {
                Ok(Some(_status)) => {
                    let id = running.id.clone();
                    info!("game '{id}' exited on its own");
                    *cur = None;
                    None
                }
                Ok(None) => Some(running.id.clone()),
                Err(_) => Some(running.id.clone()),
            }
        } else {
            None
        }
    }

    /// Launch the game with the given id, stopping any current game first.
    pub fn launch(&self, id: &str) -> Result<()> {
        let game = self
            .games
            .iter()
            .find(|g| g.id == id)
            .ok_or_else(|| anyhow!("no game with id '{id}'"))?
            .clone();

        let mut cur = self.current.lock().unwrap();
        if let Some(running) = cur.take() {
            kill_and_reap(running);
        }

        let child = self.spawn(&game).with_context(|| format!("launching '{id}'"))?;
        info!(
            "launched game '{}' ({}){}",
            game.id,
            game.command,
            if game.gamescope { " via gamescope" } else { "" }
        );
        *cur = Some(Running {
            id: game.id.clone(),
            child,
        });
        Ok(())
    }

    /// Stop the current game, if any.
    pub fn stop(&self) -> Result<()> {
        let mut cur = self.current.lock().unwrap();
        if let Some(running) = cur.take() {
            info!("stopping game '{}'", running.id);
            kill_and_reap(running);
        }
        Ok(())
    }

    fn spawn(&self, game: &Game) -> Result<Child> {
        let (program, args) = if game.gamescope {
            // gamescope -W <w> -H <h> -f [extra...] -- <command> [args...]
            let mut gs = vec![
                "-W".to_string(),
                GAMESCOPE_W.to_string(),
                "-H".to_string(),
                GAMESCOPE_H.to_string(),
                "-f".to_string(),
            ];
            gs.extend(game.gamescope_args.iter().cloned());
            gs.push("--".to_string());
            gs.push(game.command.clone());
            gs.extend(game.args.iter().cloned());
            ("gamescope".to_string(), gs)
        } else {
            (game.command.clone(), game.args.clone())
        };

        let mut cmd = Command::new(&program);
        cmd.args(&args);
        // Capture display + audio sink for the child and anything it spawns.
        cmd.env("DISPLAY", &self.display);
        cmd.env("PULSE_SERVER", "127.0.0.1");
        for (k, v) in &game.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &game.cwd {
            cmd.current_dir(cwd);
        }
        // Detach stdin; let stdout/stderr inherit so game logs land in arcadia's.
        cmd.stdin(Stdio::null());

        cmd.spawn()
            .with_context(|| format!("spawning '{program}' (is it installed and on PATH?)"))
    }
}

/// Best-effort terminate + reap so we don't leave a zombie or a stray game.
fn kill_and_reap(mut running: Running) {
    if let Err(e) = running.child.kill() {
        // ESRCH (already exited) is fine; anything else is worth a line.
        warn!("killing game '{}': {e}", running.id);
    }
    let _ = running.child.wait();
}
