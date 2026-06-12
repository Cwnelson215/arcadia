//! Virtual gamepad injection via uinput (evdev).
//!
//! Browser Gamepad API state arrives over the data channel as [`GamepadState`]
//! and is replayed onto a uinput virtual device that advertises itself as a
//! "Microsoft X-Box 360 pad" (VID/PID 0x045e/0x028e) so SDL games auto-map it
//! from the built-in controller DB.
//!
//! Unlike keyboard/mouse (XTEST), a gamepad MUST go through uinput: games read
//! joysticks via evdev (`/dev/input/js*`, `event*`), not the X server — which is
//! why this works even under Xvfb. Requires write access to `/dev/uinput`
//! (udev rule + `input` group; see arcadia/CLAUDE.md).

use crate::input::GamepadState;
use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{
    AbsInfo, AbsoluteAxisType, AttributeSet, BusType, EventType, InputEvent, InputId, Key,
    UinputAbsSetup,
};
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tracing::{error, info, warn};

const STICK_MIN: i32 = -32768;
const STICK_MAX: i32 = 32767;
const TRIG_MAX: i32 = 255;

/// W3C standard-mapping button index → evdev key (digital buttons only;
/// triggers 6/7 and d-pad 12–15 are handled as axes).
const BTN_MAP: &[(usize, Key)] = &[
    (0, Key::BTN_SOUTH),   // A
    (1, Key::BTN_EAST),    // B
    (2, Key::BTN_WEST),    // X
    (3, Key::BTN_NORTH),   // Y
    (4, Key::BTN_TL),      // LB
    (5, Key::BTN_TR),      // RB
    (8, Key::BTN_SELECT),  // Back/View
    (9, Key::BTN_START),   // Start/Menu
    (10, Key::BTN_THUMBL), // L3
    (11, Key::BTN_THUMBR), // R3
    (16, Key::BTN_MODE),   // Guide
];

/// Blocking injector loop. The uinput device is created lazily on the first
/// gamepad event, so keyboard/mouse-only (or video-only/mobile) sessions don't
/// spawn a phantom controller. Returns when the channel closes.
pub fn run(rx: Receiver<GamepadState>) {
    let mut dev: Option<VirtualDevice> = None;
    while let Ok(state) = rx.recv() {
        if dev.is_none() {
            match build_device() {
                Ok(d) => {
                    info!("virtual gamepad created (uinput)");
                    dev = Some(d);
                }
                Err(e) => {
                    error!("cannot create uinput gamepad (is /dev/uinput writable?): {e:#}");
                    return;
                }
            }
        }
        if let Some(d) = dev.as_mut() {
            if let Err(e) = apply(d, &state) {
                warn!("gamepad inject error: {e:#}");
            }
        }
    }
    info!("gamepad injector stopped");
}

/// Headless self-test (no browser/controller needed): create the virtual pad and
/// sweep the left stick + toggle the A button for ~8 s, so `jstest`/`evtest` can
/// confirm uinput access, device recognition, and axis/button mapping. Run with
/// `arcadia --gamepad-selftest`.
pub fn selftest() -> Result<()> {
    let mut dev = build_device().context("creating virtual gamepad (is /dev/uinput writable?)")?;
    info!("gamepad selftest: device created. In another shell run: jstest /dev/input/js0");
    std::thread::sleep(Duration::from_millis(800)); // let the js node appear
    for i in 0..160 {
        let phase = (i as f32 / 160.0) * std::f32::consts::TAU;
        let a_pressed = if (i / 20) % 2 == 0 { 1.0 } else { 0.0 };
        let state = GamepadState {
            axes: [phase.sin(), phase.cos(), 0.0, 0.0],
            buttons: vec![a_pressed],
        };
        apply(&mut dev, &state)?;
        std::thread::sleep(Duration::from_millis(50));
    }
    info!("gamepad selftest done");
    Ok(())
}

fn build_device() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<Key>::new();
    for (_, k) in BTN_MAP {
        keys.insert(*k);
    }

    let stick = || AbsInfo::new(0, STICK_MIN, STICK_MAX, 16, 128, 0);
    let trig = || AbsInfo::new(0, 0, TRIG_MAX, 0, 0, 0);
    let hat = || AbsInfo::new(0, -1, 1, 0, 0, 0);

    VirtualDeviceBuilder::new()?
        .name("Microsoft X-Box 360 pad")
        .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
        .with_keys(&keys)?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_X, stick()))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_Y, stick()))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_RX, stick()))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_RY, stick()))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_Z, trig()))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_RZ, trig()))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_HAT0X, hat()))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_HAT0Y, hat()))?
        .build()
        .context("building uinput virtual gamepad")
}

fn scale_stick(v: f32) -> i32 {
    let t = ((v + 1.0) / 2.0).clamp(0.0, 1.0); // -1..1 -> 0..1
    (STICK_MIN as f32 + t * (STICK_MAX - STICK_MIN) as f32).round() as i32
}

fn scale_trig(v: f32) -> i32 {
    (v.clamp(0.0, 1.0) * TRIG_MAX as f32).round() as i32
}

fn pressed(buttons: &[f32], i: usize) -> i32 {
    if buttons.get(i).copied().unwrap_or(0.0) > 0.5 {
        1
    } else {
        0
    }
}

fn apply(d: &mut VirtualDevice, s: &GamepadState) -> Result<()> {
    let abs = |code: u16, val: i32| InputEvent::new(EventType::ABSOLUTE, code, val);
    let key = |code: u16, val: i32| InputEvent::new(EventType::KEY, code, val);

    let mut ev: Vec<InputEvent> = vec![
        abs(AbsoluteAxisType::ABS_X.0, scale_stick(s.axes[0])),
        abs(AbsoluteAxisType::ABS_Y.0, scale_stick(s.axes[1])),
        abs(AbsoluteAxisType::ABS_RX.0, scale_stick(s.axes[2])),
        abs(AbsoluteAxisType::ABS_RY.0, scale_stick(s.axes[3])),
        abs(
            AbsoluteAxisType::ABS_Z.0,
            scale_trig(s.buttons.get(6).copied().unwrap_or(0.0)),
        ),
        abs(
            AbsoluteAxisType::ABS_RZ.0,
            scale_trig(s.buttons.get(7).copied().unwrap_or(0.0)),
        ),
    ];

    // D-pad (standard buttons 12=up,13=down,14=left,15=right) -> hat axes.
    let hx = pressed(&s.buttons, 15) - pressed(&s.buttons, 14);
    let hy = pressed(&s.buttons, 13) - pressed(&s.buttons, 12);
    ev.push(abs(AbsoluteAxisType::ABS_HAT0X.0, hx));
    ev.push(abs(AbsoluteAxisType::ABS_HAT0Y.0, hy));

    for (i, k) in BTN_MAP {
        ev.push(key(k.0, pressed(&s.buttons, *i)));
    }

    d.emit(&ev).context("emitting gamepad events")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_stick_endpoints_and_center() {
        assert_eq!(scale_stick(-1.0), STICK_MIN);
        assert_eq!(scale_stick(1.0), STICK_MAX);
        // Center maps to ~0 (rounding lands within one LSB of the half-range).
        assert!(scale_stick(0.0).abs() <= 1);
        // Out-of-range inputs clamp rather than overshoot the axis range.
        assert_eq!(scale_stick(-5.0), STICK_MIN);
        assert_eq!(scale_stick(5.0), STICK_MAX);
    }

    #[test]
    fn scale_trig_range() {
        assert_eq!(scale_trig(0.0), 0);
        assert_eq!(scale_trig(1.0), TRIG_MAX);
        assert_eq!(scale_trig(0.5), 128); // 0.5 * 255 = 127.5 -> 128
        assert_eq!(scale_trig(-1.0), 0); // clamps low
        assert_eq!(scale_trig(2.0), TRIG_MAX); // clamps high
    }

    #[test]
    fn pressed_threshold_at_half() {
        let buttons = [0.0, 0.4, 0.6, 1.0];
        assert_eq!(pressed(&buttons, 0), 0);
        assert_eq!(pressed(&buttons, 1), 0);
        assert_eq!(pressed(&buttons, 2), 1);
        assert_eq!(pressed(&buttons, 3), 1);
        // Missing index is treated as released, not a panic.
        assert_eq!(pressed(&buttons, 99), 0);
    }

    #[test]
    fn btn_map_covers_standard_face_and_system_buttons() {
        let indices: Vec<usize> = BTN_MAP.iter().map(|(i, _)| *i).collect();
        for i in [0, 1, 2, 3, 4, 5, 8, 9, 10, 11, 16] {
            assert!(indices.contains(&i), "BTN_MAP missing W3C index {i}");
        }
        // A/B/X/Y are the first four entries, in order.
        assert_eq!(BTN_MAP[0].1, Key::BTN_SOUTH);
        assert_eq!(BTN_MAP[1].1, Key::BTN_EAST);
    }
}
