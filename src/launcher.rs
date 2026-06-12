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
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use tracing::{info, warn};

/// The capture display resolution. Shared by the gamescope wrap (its `-W/-H`
/// output size) and the `fit_window` resize, so both clamp to the same frame the
/// encoder captures.
const CAPTURE_W: &str = "1280";
const CAPTURE_H: &str = "720";

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
    /// If set, after launch, resize the top-level window whose name matches this
    /// substring to the capture resolution at +0+0. Fixes apps (e.g. Steam Big
    /// Picture, which opens a fixed 1280x800 Deck-UI window) that create a window
    /// larger than the capture display — with no window manager on `:99` nothing
    /// else constrains it, so the overflow falls off the framebuffer.
    #[serde(default)]
    pub fit_window: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GamesConfig {
    #[serde(default)]
    game: Vec<Game>,
}

struct Running {
    id: String,
    child: Child,
    /// True while the process group is SIGSTOP'd by the idle spin-down.
    suspended: bool,
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
        if let Some(match_name) = game.fit_window.clone() {
            fit_window_async(self.display.clone(), match_name);
        }
        *cur = Some(Running {
            id: game.id.clone(),
            child,
            suspended: false,
        });
        Ok(())
    }

    /// Freeze the running game's process group (SIGSTOP) so it does no CPU/GPU
    /// work while no viewer is connected. Idempotent and a no-op when nothing is
    /// running. The child was started as its own process-group leader (see
    /// `spawn`), so signaling the group catches the game and its children.
    pub fn suspend(&self) {
        let mut cur = self.current.lock().unwrap();
        if let Some(r) = cur.as_mut() {
            if !r.suspended {
                // SAFETY: a plain libc signal to the child's process group.
                unsafe { libc::killpg(r.child.id() as libc::pid_t, libc::SIGSTOP) };
                r.suspended = true;
                info!("froze game '{}' (idle spin-down)", r.id);
            }
        }
    }

    /// Resume a frozen game (SIGCONT). Idempotent; a no-op if not suspended or
    /// nothing is running.
    pub fn resume(&self) {
        let mut cur = self.current.lock().unwrap();
        if let Some(r) = cur.as_mut() {
            if r.suspended {
                // SAFETY: a plain libc signal to the child's process group.
                unsafe { libc::killpg(r.child.id() as libc::pid_t, libc::SIGCONT) };
                r.suspended = false;
                info!("resumed game '{}' (viewer connected)", r.id);
            }
        }
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
                CAPTURE_W.to_string(),
                "-H".to_string(),
                CAPTURE_H.to_string(),
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
        // Own process group (pgid == child pid) so the idle spin-down can
        // SIGSTOP/SIGCONT the whole game subtree, not just the parent.
        cmd.process_group(0);

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

/// Clamp a freshly-launched game's window to the capture display.
///
/// Some apps (Steam Big Picture opens a fixed 1280x800 Deck-UI window) create a
/// window bigger than the capture framebuffer; with no window manager on `:99`,
/// the overflow falls off-screen and is never captured. This spawns a detached,
/// bounded poll loop that resizes/moves the matching top-level window to the
/// capture resolution at +0+0 via `xdotool`. CEF/Chromium apps (steamwebhelper)
/// reflow their UI to the new window size, so nothing is clipped.
///
/// The loop both waits out the app's multi-second boot and re-asserts the size a
/// few times in case the app relays out and snaps back. It is best-effort: a
/// missing window (not booted yet, or already stopped) is a no-op and the loop
/// retries, then exits on its own — it is not tied to the child's lifetime.
fn fit_window_async(disp: String, match_name: String) {
    std::thread::spawn(move || {
        // ~30s budget: 40 tries × 750ms. Cheap (one short-lived xdotool per try).
        // (`display` is a reserved tracing field name — hence `disp`.)
        info!("fit_window: watching for window matching '{match_name}' on {disp}");
        for _ in 0..40 {
            let _ = Command::new("xdotool")
                .args([
                    "search",
                    "--name",
                    &match_name,
                    "windowsize",
                    "%@",
                    CAPTURE_W,
                    CAPTURE_H,
                    "windowmove",
                    "%@",
                    "0",
                    "0",
                ])
                .env("DISPLAY", &disp)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            std::thread::sleep(std::time::Duration::from_millis(750));
        }
        info!("fit_window: done watching for '{match_name}'");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("create temp file");
        f.write_all(contents.as_bytes()).expect("write config");
        f
    }

    #[test]
    fn loads_valid_games() {
        let f = write_config(
            r#"
            [[game]]
            id = "retroarch"
            name = "RetroArch"
            command = "retroarch"
            args = ["-f"]

            [[game]]
            id = "steam"
            name = "Steam"
            command = "steam"
            gamescope = true
            fit_window = "Steam Big Picture"
            "#,
        );
        let l = Launcher::load(f.path().to_str().unwrap(), ":99").unwrap();
        let games = l.games();
        assert_eq!(games.len(), 2);

        assert_eq!(games[0].id, "retroarch");
        assert_eq!(games[0].command, "retroarch");
        assert_eq!(games[0].args, vec!["-f".to_string()]);
        assert!(!games[0].gamescope); // defaults to false
        assert_eq!(games[0].fit_window, None); // omitted -> default None

        assert_eq!(games[1].id, "steam");
        assert!(games[1].gamescope);
        assert!(games[1].args.is_empty()); // omitted -> default empty
        assert_eq!(games[1].fit_window.as_deref(), Some("Steam Big Picture"));
    }

    #[test]
    fn missing_file_is_empty_not_error() {
        // The documented video-only fallback: a missing config is tolerated.
        let l = Launcher::load("/no/such/path/games.toml", ":99").unwrap();
        assert!(l.games().is_empty());
    }

    #[test]
    fn malformed_toml_is_error() {
        let f = write_config("this is = not valid toml [[[");
        assert!(Launcher::load(f.path().to_str().unwrap(), ":99").is_err());
    }
}
