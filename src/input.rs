//! Input injection via X11 XTEST.
//!
//! Browser input events arrive over a WebRTC data channel as JSON strings,
//! parse into [`InputEvent`], and are forwarded to a dedicated injector thread
//! (see [`run`]). That thread owns an `x11rb` connection to the headless display
//! (`:99`) and replays each event into the X server with the XTEST extension —
//! so every app on that display sees real key/pointer/button input.
//!
//! XTEST is used instead of uinput because Xvfb doesn't read kernel evdev
//! devices (and `/dev/uinput` is root-only); XTEST injects directly into the X
//! server with no root and no extra services.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Receiver;
use tracing::{error, info, warn};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    ConnectionExt as _, Keycode, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::NONE;

/// One input action from the browser. `t` discriminates the JSON shape:
/// `k`=key, `m`=relative mouse move, `b`=mouse button, `w`=wheel.
#[derive(Debug)]
pub enum InputEvent {
    /// `KeyboardEvent.code` (e.g. "KeyW", "Space") + pressed/released.
    Key { code: String, down: bool },
    /// Pointer Lock `movementX/Y` — relative motion.
    Mouse { dx: i16, dy: i16 },
    /// X button number (1=left, 2=middle, 3=right) + pressed/released.
    Button { button: u8, down: bool },
    /// Wheel: -1 = up, +1 = down (sign of `deltaY`).
    Wheel { dir: i8 },
}

impl InputEvent {
    pub fn from_json(s: &str) -> Option<InputEvent> {
        let v: Value = serde_json::from_str(s).ok()?;
        match v.get("t")?.as_str()? {
            "k" => Some(InputEvent::Key {
                code: v.get("code")?.as_str()?.to_string(),
                down: v.get("down")?.as_bool()?,
            }),
            "m" => Some(InputEvent::Mouse {
                dx: clamp_i16(v.get("dx")),
                dy: clamp_i16(v.get("dy")),
            }),
            "b" => Some(InputEvent::Button {
                button: js_button_to_x(v.get("button")?.as_u64()? as u8),
                down: v.get("down")?.as_bool()?,
            }),
            "w" => Some(InputEvent::Wheel {
                dir: if v.get("dy")?.as_f64()? < 0.0 { -1 } else { 1 },
            }),
            _ => None,
        }
    }
}

fn clamp_i16(v: Option<&Value>) -> i16 {
    let n = v.and_then(Value::as_f64).unwrap_or(0.0);
    n.clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

/// JS `MouseEvent.button` (0=left,1=middle,2=right) → X button (1,2,3).
fn js_button_to_x(b: u8) -> u8 {
    match b {
        0 => 1,
        1 => 2,
        2 => 3,
        other => other + 1,
    }
}

/// Blocking injector loop. Owns the X connection (which is not `Send`-friendly
/// to share), so it runs on its own thread. Returns when the channel closes
/// (session ended), after releasing any still-held keys/buttons so a modifier
/// can't get stuck latched in the X server.
pub fn run(dpy: String, rx: Receiver<InputEvent>) {
    let mut inj = match Injector::connect(&dpy) {
        Ok(i) => i,
        Err(e) => {
            error!("input injector: cannot attach to X display {}: {:#}", dpy, e);
            return;
        }
    };
    info!("input injector attached to {} ({} keys mapped)", dpy, inj.keymap.len());

    while let Ok(ev) = rx.recv() {
        if let Err(e) = inj.handle(ev) {
            warn!("input inject error: {e:#}");
        }
    }

    inj.release_all();
    info!("input injector stopped");
}

struct Injector {
    conn: RustConnection,
    keymap: HashMap<String, Keycode>, // JS code -> X keycode
    held_keys: HashSet<Keycode>,
    held_buttons: HashSet<u8>,
}

impl Injector {
    fn connect(display: &str) -> Result<Self> {
        let (conn, _screen) = x11rb::connect(Some(display)).context("x11 connect")?;
        conn.extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
            .context("query XTEST")?
            .ok_or_else(|| anyhow!("XTEST extension not available on this X server"))?;
        let keymap = build_keymap(&conn).context("building keymap")?;
        Ok(Self {
            conn,
            keymap,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
        })
    }

    fn handle(&mut self, ev: InputEvent) -> Result<()> {
        match ev {
            InputEvent::Key { code, down } => {
                let Some(&kc) = self.keymap.get(&code) else {
                    warn!("unmapped key code {code}");
                    return Ok(());
                };
                self.fake(if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, kc, 0, 0)?;
                if down {
                    self.held_keys.insert(kc);
                } else {
                    self.held_keys.remove(&kc);
                }
            }
            InputEvent::Mouse { dx, dy } => {
                // detail=1 => relative motion (matches Pointer Lock deltas)
                self.conn
                    .xtest_fake_input(MOTION_NOTIFY_EVENT, 1, 0, NONE, dx, dy, 0)?;
            }
            InputEvent::Button { button, down } => {
                self.fake(
                    if down { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT },
                    button,
                    0,
                    0,
                )?;
                if down {
                    self.held_buttons.insert(button);
                } else {
                    self.held_buttons.remove(&button);
                }
            }
            InputEvent::Wheel { dir } => {
                let btn = if dir < 0 { 4 } else { 5 }; // X: 4=up, 5=down
                self.fake(BUTTON_PRESS_EVENT, btn, 0, 0)?;
                self.fake(BUTTON_RELEASE_EVENT, btn, 0, 0)?;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn.xtest_fake_input(type_, detail, 0, NONE, x, y, 0)?;
        Ok(())
    }

    fn release_all(&mut self) {
        let keys: Vec<_> = self.held_keys.drain().collect();
        for kc in keys {
            let _ = self.fake(KEY_RELEASE_EVENT, kc, 0, 0);
        }
        let btns: Vec<_> = self.held_buttons.drain().collect();
        for b in btns {
            let _ = self.fake(BUTTON_RELEASE_EVENT, b, 0, 0);
        }
        let _ = self.conn.flush();
    }
}

/// Build a `KeyboardEvent.code` → X keycode map by resolving each desired keysym
/// against the server's current keyboard mapping.
fn build_keymap(conn: &RustConnection) -> Result<HashMap<String, Keycode>> {
    let setup = conn.setup();
    let min = setup.min_keycode;
    let count = setup.max_keycode - min + 1;
    let mapping = conn.get_keyboard_mapping(min, count)?.reply()?;
    let per = mapping.keysyms_per_keycode as usize;

    // keysym -> first keycode that produces it
    let mut sym_to_code: HashMap<u32, Keycode> = HashMap::new();
    for (i, chunk) in mapping.keysyms.chunks(per).enumerate() {
        let keycode = min + i as u8;
        for &sym in chunk {
            if sym != 0 {
                sym_to_code.entry(sym).or_insert(keycode);
            }
        }
    }

    let mut map = HashMap::new();
    for (code, sym) in code_to_keysym() {
        if let Some(&kc) = sym_to_code.get(&sym) {
            map.insert(code, kc);
        } else {
            warn!("keysym {sym:#x} for {code} not in X keymap");
        }
    }
    Ok(map)
}

/// JS `KeyboardEvent.code` → X11 keysym, for a gaming-relevant key set.
fn code_to_keysym() -> Vec<(String, u32)> {
    let mut v: Vec<(String, u32)> = Vec::new();

    // Letters: KeyA..KeyZ -> keysym 'a'..'z' (0x61..0x7a)
    for i in 0..26u32 {
        v.push((format!("Key{}", (b'A' + i as u8) as char), 0x61 + i));
    }
    // Top-row digits: Digit0..Digit9 -> '0'..'9' (0x30..0x39)
    for i in 0..10u32 {
        v.push((format!("Digit{i}"), 0x30 + i));
    }
    // Function keys F1..F12 -> XK_F1..XK_F12 (0xffbe..)
    for i in 0..12u32 {
        v.push((format!("F{}", i + 1), 0xffbe + i));
    }

    let extra: &[(&str, u32)] = &[
        ("Space", 0x20),
        ("Enter", 0xff0d),
        ("Escape", 0xff1b),
        ("Tab", 0xff09),
        ("Backspace", 0xff08),
        ("Delete", 0xffff),
        ("ShiftLeft", 0xffe1),
        ("ShiftRight", 0xffe2),
        ("ControlLeft", 0xffe3),
        ("ControlRight", 0xffe4),
        ("AltLeft", 0xffe9),
        ("AltRight", 0xffea),
        ("ArrowUp", 0xff52),
        ("ArrowDown", 0xff54),
        ("ArrowLeft", 0xff51),
        ("ArrowRight", 0xff53),
        ("Minus", 0x2d),
        ("Equal", 0x3d),
        ("BracketLeft", 0x5b),
        ("BracketRight", 0x5d),
        ("Backslash", 0x5c),
        ("Semicolon", 0x3b),
        ("Quote", 0x27),
        ("Comma", 0x2c),
        ("Period", 0x2e),
        ("Slash", 0x2f),
        ("Backquote", 0x60),
        ("CapsLock", 0xffe5),
    ];
    for &(code, sym) in extra {
        v.push((code.to_string(), sym));
    }
    v
}
