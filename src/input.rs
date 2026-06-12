//! Keyboard + mouse injection via uinput (evdev).
//!
//! Browser input events arrive over a WebRTC data channel as JSON strings,
//! parse into [`InputEvent`], and are forwarded to a dedicated injector thread
//! (see [`run`]). That thread owns two uinput virtual devices — a keyboard and a
//! relative-pointer mouse — and replays each event onto them.
//!
//! **Why uinput, not XTEST (changed 2026-06-11):** XTEST injects into the X
//! server's request queue, which `ximagesrc` also serializes through for its
//! screen grabs — so a burst of input briefly delays a frame grab and the video
//! stutters. uinput goes kernel → evdev → the X server's separate input thread,
//! off the grab path, so input and capture no longer contend. This is only
//! viable on the real accelerated Xorg (Stage 4); the original Xvfb couldn't
//! read evdev devices, which is why XTEST was used through Stages 2–3.
//!
//! Requires write access to `/dev/uinput` (udev rule + `input` group; see
//! arcadia/CLAUDE.md) — the same access the gamepad ([`crate::gamepad`]) needs.

use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{
    AttributeSet, BusType, EventType, InputEvent as EvdevEvent, InputId, Key, RelativeAxisType,
};
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tracing::{error, info, warn};

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
    /// Full gamepad state snapshot (routed to the gamepad injector).
    Gamepad(GamepadState),
    /// Drop the virtual gamepad so the host sees a controller disconnect — the
    /// way out of Steam Big Picture's controller mode (routed to the gamepad
    /// injector). The next `Gamepad` event re-creates the pad.
    GamepadRelease,
    /// Adaptive-bitrate request from the browser (kbps); applied to the encoder.
    Bitrate { kbps: u32 },
    /// Capture state: the browser is (un)pointer-locked. Toggles the captured
    /// cursor's visibility server-side, so it shows only while the user is
    /// actively controlling the display.
    Capture { on: bool },
    /// Release every still-held key/button WITHOUT destroying the devices.
    /// Sent server-side at session end (the injector is now persistent, so the
    /// old "release on channel close" no longer fires per session). Not parsed
    /// from any browser message.
    ReleaseAll,
}

/// A snapshot of a browser Gamepad (W3C "standard" mapping). `axes` is
/// `[leftX, leftY, rightX, rightY]` in −1..1; `buttons` are `.value` floats
/// (digital 0/1, triggers 0..1) indexed by the standard layout.
#[derive(Debug, Clone)]
pub struct GamepadState {
    pub axes: [f32; 4],
    pub buttons: Vec<f32>,
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
            "g" => {
                let a = v.get("a")?.as_array()?;
                let mut axes = [0.0f32; 4];
                for (i, slot) in axes.iter_mut().enumerate() {
                    *slot = a.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
                }
                let buttons = v
                    .get("b")?
                    .as_array()?
                    .iter()
                    .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                    .collect();
                Some(InputEvent::Gamepad(GamepadState { axes, buttons }))
            }
            "gx" => Some(InputEvent::GamepadRelease),
            "r" => Some(InputEvent::Bitrate {
                kbps: v.get("kbps")?.as_u64()? as u32,
            }),
            "c" => Some(InputEvent::Capture {
                on: v.get("on")?.as_bool()?,
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

/// X button number (1=left,2=middle,3=right) → evdev button key.
fn x_button_to_btn(button: u8) -> Option<Key> {
    match button {
        1 => Some(Key::BTN_LEFT),
        2 => Some(Key::BTN_MIDDLE),
        3 => Some(Key::BTN_RIGHT),
        _ => None,
    }
}

/// Blocking injector loop. Owns the uinput devices, so it runs on its own
/// thread. The injector is now PERSISTENT (spawned once in `serve`), so the
/// channel only closes at process shutdown; per-session held-key cleanup arrives
/// as an explicit `ReleaseAll` event at each session end. The `release_all` on
/// channel close below is just a shutdown safety net.
pub fn run(rx: Receiver<InputEvent>) {
    let mut inj = match Injector::new() {
        Ok(i) => i,
        Err(e) => {
            error!("input injector: cannot create uinput devices (is /dev/uinput writable?): {e:#}");
            // Drain so the data-channel sender's `send` fails fast instead of blocking.
            while rx.recv().is_ok() {}
            return;
        }
    };
    info!("input injector ready (uinput keyboard + mouse)");

    while let Ok(ev) = rx.recv() {
        if let Err(e) = inj.handle(ev) {
            warn!("input inject error: {e:#}");
        }
    }

    inj.release_all();
    info!("input injector stopped");
}

struct Injector {
    keyboard: VirtualDevice,
    mouse: VirtualDevice,
    keymap: HashMap<String, Key>, // JS code -> evdev key
    held_keys: HashSet<u16>,      // evdev key codes currently pressed
    held_buttons: HashSet<u16>,   // evdev BTN_* codes currently pressed
}

impl Injector {
    fn new() -> Result<Self> {
        let keymap = build_keymap();

        let mut keys = AttributeSet::<Key>::new();
        for k in keymap.values() {
            keys.insert(*k);
        }
        let keyboard = VirtualDeviceBuilder::new()
            .context("opening /dev/uinput for keyboard")?
            .name("arcadia virtual keyboard")
            .input_id(InputId::new(BusType::BUS_USB, 0x1209, 0xa1ad, 0x0001))
            .with_keys(&keys)?
            .build()
            .context("building uinput keyboard")?;

        let mut btns = AttributeSet::<Key>::new();
        btns.insert(Key::BTN_LEFT);
        btns.insert(Key::BTN_MIDDLE);
        btns.insert(Key::BTN_RIGHT);
        let mut rels = AttributeSet::<RelativeAxisType>::new();
        rels.insert(RelativeAxisType::REL_X);
        rels.insert(RelativeAxisType::REL_Y);
        rels.insert(RelativeAxisType::REL_WHEEL);
        let mouse = VirtualDeviceBuilder::new()
            .context("opening /dev/uinput for mouse")?
            .name("arcadia virtual mouse")
            .input_id(InputId::new(BusType::BUS_USB, 0x1209, 0xa1ad, 0x0002))
            .with_keys(&btns)?
            .with_relative_axes(&rels)?
            .build()
            .context("building uinput mouse")?;

        // Let udev + the X server hot-plug the new devices before the first
        // event, so an early keypress isn't dropped.
        std::thread::sleep(Duration::from_millis(400));

        Ok(Self {
            keyboard,
            mouse,
            keymap,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
        })
    }

    fn handle(&mut self, ev: InputEvent) -> Result<()> {
        match ev {
            InputEvent::Key { code, down } => {
                let Some(&key) = self.keymap.get(&code) else {
                    warn!("unmapped key code {code}");
                    return Ok(());
                };
                self.keyboard
                    .emit(&[EvdevEvent::new(EventType::KEY, key.0, down as i32)])?;
                if down {
                    self.held_keys.insert(key.0);
                } else {
                    self.held_keys.remove(&key.0);
                }
            }
            InputEvent::Mouse { dx, dy } => {
                if dx != 0 || dy != 0 {
                    self.mouse.emit(&[
                        EvdevEvent::new(EventType::RELATIVE, RelativeAxisType::REL_X.0, dx as i32),
                        EvdevEvent::new(EventType::RELATIVE, RelativeAxisType::REL_Y.0, dy as i32),
                    ])?;
                }
            }
            InputEvent::Button { button, down } => {
                let Some(btn) = x_button_to_btn(button) else {
                    warn!("unmapped mouse button {button}");
                    return Ok(());
                };
                self.mouse
                    .emit(&[EvdevEvent::new(EventType::KEY, btn.0, down as i32)])?;
                if down {
                    self.held_buttons.insert(btn.0);
                } else {
                    self.held_buttons.remove(&btn.0);
                }
            }
            InputEvent::Wheel { dir } => {
                // evdev REL_WHEEL: +1 = up, -1 = down (opposite of the browser sign).
                let val = if dir < 0 { 1 } else { -1 };
                self.mouse.emit(&[EvdevEvent::new(
                    EventType::RELATIVE,
                    RelativeAxisType::REL_WHEEL.0,
                    val,
                )])?;
            }
            // Flush held keys/buttons at session end; keeps the devices alive.
            InputEvent::ReleaseAll => self.release_all(),
            // Gamepad/Bitrate/Capture are handled elsewhere (gamepad injector /
            // encoder / capture element), never on the keyboard/mouse path.
            InputEvent::Gamepad(_)
            | InputEvent::GamepadRelease
            | InputEvent::Bitrate { .. }
            | InputEvent::Capture { .. } => {}
        }
        Ok(())
    }

    fn release_all(&mut self) {
        let keys: Vec<u16> = self.held_keys.drain().collect();
        for code in keys {
            let _ = self
                .keyboard
                .emit(&[EvdevEvent::new(EventType::KEY, code, 0)]);
        }
        let btns: Vec<u16> = self.held_buttons.drain().collect();
        for code in btns {
            let _ = self.mouse.emit(&[EvdevEvent::new(EventType::KEY, code, 0)]);
        }
    }
}

/// Build a `KeyboardEvent.code` → evdev `Key` map for a gaming-relevant key set
/// (mirrors the coverage of the old XTEST keysym table).
fn build_keymap() -> HashMap<String, Key> {
    let mut m: HashMap<String, Key> = HashMap::new();

    // Letters KeyA..KeyZ. evdev key codes are not alphabetical, so list them.
    let letters: &[(&str, Key)] = &[
        ("KeyA", Key::KEY_A), ("KeyB", Key::KEY_B), ("KeyC", Key::KEY_C),
        ("KeyD", Key::KEY_D), ("KeyE", Key::KEY_E), ("KeyF", Key::KEY_F),
        ("KeyG", Key::KEY_G), ("KeyH", Key::KEY_H), ("KeyI", Key::KEY_I),
        ("KeyJ", Key::KEY_J), ("KeyK", Key::KEY_K), ("KeyL", Key::KEY_L),
        ("KeyM", Key::KEY_M), ("KeyN", Key::KEY_N), ("KeyO", Key::KEY_O),
        ("KeyP", Key::KEY_P), ("KeyQ", Key::KEY_Q), ("KeyR", Key::KEY_R),
        ("KeyS", Key::KEY_S), ("KeyT", Key::KEY_T), ("KeyU", Key::KEY_U),
        ("KeyV", Key::KEY_V), ("KeyW", Key::KEY_W), ("KeyX", Key::KEY_X),
        ("KeyY", Key::KEY_Y), ("KeyZ", Key::KEY_Z),
    ];
    // Top-row digits Digit0..Digit9.
    let digits: &[(&str, Key)] = &[
        ("Digit0", Key::KEY_0), ("Digit1", Key::KEY_1), ("Digit2", Key::KEY_2),
        ("Digit3", Key::KEY_3), ("Digit4", Key::KEY_4), ("Digit5", Key::KEY_5),
        ("Digit6", Key::KEY_6), ("Digit7", Key::KEY_7), ("Digit8", Key::KEY_8),
        ("Digit9", Key::KEY_9),
    ];
    // Function keys F1..F12.
    let fkeys: &[(&str, Key)] = &[
        ("F1", Key::KEY_F1), ("F2", Key::KEY_F2), ("F3", Key::KEY_F3),
        ("F4", Key::KEY_F4), ("F5", Key::KEY_F5), ("F6", Key::KEY_F6),
        ("F7", Key::KEY_F7), ("F8", Key::KEY_F8), ("F9", Key::KEY_F9),
        ("F10", Key::KEY_F10), ("F11", Key::KEY_F11), ("F12", Key::KEY_F12),
    ];
    let extra: &[(&str, Key)] = &[
        ("Space", Key::KEY_SPACE),
        ("Enter", Key::KEY_ENTER),
        ("Escape", Key::KEY_ESC),
        ("Tab", Key::KEY_TAB),
        ("Backspace", Key::KEY_BACKSPACE),
        ("Delete", Key::KEY_DELETE),
        ("ShiftLeft", Key::KEY_LEFTSHIFT),
        ("ShiftRight", Key::KEY_RIGHTSHIFT),
        ("ControlLeft", Key::KEY_LEFTCTRL),
        ("ControlRight", Key::KEY_RIGHTCTRL),
        ("AltLeft", Key::KEY_LEFTALT),
        ("AltRight", Key::KEY_RIGHTALT),
        ("ArrowUp", Key::KEY_UP),
        ("ArrowDown", Key::KEY_DOWN),
        ("ArrowLeft", Key::KEY_LEFT),
        ("ArrowRight", Key::KEY_RIGHT),
        ("Minus", Key::KEY_MINUS),
        ("Equal", Key::KEY_EQUAL),
        ("BracketLeft", Key::KEY_LEFTBRACE),
        ("BracketRight", Key::KEY_RIGHTBRACE),
        ("Backslash", Key::KEY_BACKSLASH),
        ("Semicolon", Key::KEY_SEMICOLON),
        ("Quote", Key::KEY_APOSTROPHE),
        ("Comma", Key::KEY_COMMA),
        ("Period", Key::KEY_DOT),
        ("Slash", Key::KEY_SLASH),
        ("Backquote", Key::KEY_GRAVE),
        ("CapsLock", Key::KEY_CAPSLOCK),
    ];

    for set in [letters, digits, fkeys, extra] {
        for &(code, key) in set {
            m.insert(code.to_string(), key);
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_event() {
        let e = InputEvent::from_json(r#"{"t":"k","code":"KeyW","down":true}"#).unwrap();
        match e {
            InputEvent::Key { code, down } => {
                assert_eq!(code, "KeyW");
                assert!(down);
            }
            other => panic!("expected Key, got {other:?}"),
        }
    }

    #[test]
    fn parses_mouse_move_and_clamps() {
        match InputEvent::from_json(r#"{"t":"m","dx":10,"dy":-5}"#).unwrap() {
            InputEvent::Mouse { dx, dy } => {
                assert_eq!(dx, 10);
                assert_eq!(dy, -5);
            }
            other => panic!("expected Mouse, got {other:?}"),
        }
        // Out-of-range deltas clamp into i16 rather than wrapping/panicking.
        match InputEvent::from_json(r#"{"t":"m","dx":1000000,"dy":-1000000}"#).unwrap() {
            InputEvent::Mouse { dx, dy } => {
                assert_eq!(dx, i16::MAX);
                assert_eq!(dy, i16::MIN);
            }
            other => panic!("expected Mouse, got {other:?}"),
        }
    }

    #[test]
    fn maps_js_button_to_x() {
        // {t:"b",button:0} (JS left) -> X button 1.
        match InputEvent::from_json(r#"{"t":"b","button":0,"down":true}"#).unwrap() {
            InputEvent::Button { button, down } => {
                assert_eq!(button, 1);
                assert!(down);
            }
            other => panic!("expected Button, got {other:?}"),
        }
    }

    #[test]
    fn wheel_sign_only() {
        let up = InputEvent::from_json(r#"{"t":"w","dy":-3}"#).unwrap();
        let down = InputEvent::from_json(r#"{"t":"w","dy":120}"#).unwrap();
        assert!(matches!(up, InputEvent::Wheel { dir: -1 }));
        assert!(matches!(down, InputEvent::Wheel { dir: 1 }));
    }

    #[test]
    fn gamepad_tolerates_short_and_extra_arrays() {
        // Only two axes + one button supplied: missing axes default to 0.0,
        // and an extra axis past index 3 is ignored.
        let e =
            InputEvent::from_json(r#"{"t":"g","a":[0.5,-0.5,0.0,0.0,0.9],"b":[1.0]}"#).unwrap();
        match e {
            InputEvent::Gamepad(g) => {
                assert_eq!(g.axes, [0.5, -0.5, 0.0, 0.0]);
                assert_eq!(g.buttons, vec![1.0]);
            }
            other => panic!("expected Gamepad, got {other:?}"),
        }
    }

    #[test]
    fn parses_bitrate_and_capture() {
        assert!(matches!(
            InputEvent::from_json(r#"{"t":"r","kbps":8000}"#).unwrap(),
            InputEvent::Bitrate { kbps: 8000 }
        ));
        assert!(matches!(
            InputEvent::from_json(r#"{"t":"c","on":false}"#).unwrap(),
            InputEvent::Capture { on: false }
        ));
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(InputEvent::from_json("{}").is_none());
        assert!(InputEvent::from_json("not json").is_none());
        assert!(InputEvent::from_json(r#"{"t":"zzz"}"#).is_none());
        // Missing required field for the discriminant.
        assert!(InputEvent::from_json(r#"{"t":"k","code":"KeyW"}"#).is_none());
    }

    #[test]
    fn clamp_i16_bounds() {
        assert_eq!(clamp_i16(None), 0);
        assert_eq!(clamp_i16(Some(&serde_json::json!(40000))), i16::MAX);
        assert_eq!(clamp_i16(Some(&serde_json::json!(-40000))), i16::MIN);
        assert_eq!(clamp_i16(Some(&serde_json::json!(7))), 7);
    }

    #[test]
    fn js_and_x_button_maps() {
        assert_eq!(js_button_to_x(0), 1);
        assert_eq!(js_button_to_x(1), 2);
        assert_eq!(js_button_to_x(2), 3);
        assert_eq!(js_button_to_x(5), 6);

        assert_eq!(x_button_to_btn(1), Some(Key::BTN_LEFT));
        assert_eq!(x_button_to_btn(2), Some(Key::BTN_MIDDLE));
        assert_eq!(x_button_to_btn(3), Some(Key::BTN_RIGHT));
        assert_eq!(x_button_to_btn(4), None);
    }

    #[test]
    fn keymap_has_expected_entries() {
        let m = build_keymap();
        assert_eq!(m.get("KeyW"), Some(&Key::KEY_W));
        assert_eq!(m.get("Space"), Some(&Key::KEY_SPACE));
        assert_eq!(m.get("Enter"), Some(&Key::KEY_ENTER));
        assert_eq!(m.get("F5"), Some(&Key::KEY_F5));
        assert_eq!(m.get("Digit0"), Some(&Key::KEY_0));
        assert!(!m.contains_key("NoSuchKey"));
        // 26 letters + 10 digits + 12 F-keys + 28 extras.
        assert_eq!(m.len(), 26 + 10 + 12 + 28);
    }
}
