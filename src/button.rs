//! Botón físico "Restart" (gpio-keys, /dev/input/event1, KEY_RESTART = 408).
//! Pulsación corta (<3 s): entrar/salir del modo configuración.
//! Pulsación larga (>=8 s): reiniciar el stick.

use crate::State;
use std::fs::File;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const EV_KEY: u16 = 1;
const KEY_RESTART: u16 = 408;

pub fn spawn(state: Arc<Mutex<State>>) {
    std::thread::spawn(move || loop {
        if let Err(e) = listen(&state) {
            crate::util::log(&format!("botón: {e}; reintento en 10 s"));
        }
        std::thread::sleep(Duration::from_secs(10));
    });
}

fn find_device() -> Option<String> {
    let devs = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
    let mut in_gpio = false;
    for line in devs.lines() {
        if line.starts_with("N: Name=") {
            in_gpio = line.contains("GPIO Buttons");
        }
        if in_gpio && line.starts_with("H: Handlers=") {
            return line.split_whitespace().find(|w| w.starts_with("event")).map(|e| format!("/dev/input/{e}"));
        }
    }
    None
}

fn listen(state: &Arc<Mutex<State>>) -> std::io::Result<()> {
    let path = find_device().unwrap_or_else(|| "/dev/input/event1".into());
    let mut f = File::open(&path)?;
    // struct input_event en aarch64: timeval (16 bytes) + type u16 + code u16 + value i32 = 24
    let mut buf = [0u8; 24];
    let mut pressed: Option<Instant> = None;
    loop {
        f.read_exact(&mut buf)?;
        let typ = u16::from_ne_bytes([buf[16], buf[17]]);
        let code = u16::from_ne_bytes([buf[18], buf[19]]);
        let value = i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]);
        if typ != EV_KEY || code != KEY_RESTART {
            continue;
        }
        match value {
            1 => {
                pressed = Some(Instant::now());
                state.lock().unwrap().button_held = true;
            }
            0 => {
                let held = pressed.take().map(|t| t.elapsed()).unwrap_or_default();
                let mut s = state.lock().unwrap();
                s.button_held = false;
                if held >= Duration::from_secs(8) {
                    crate::util::log("botón largo: reiniciando");
                    let _ = crate::util::run("systemctl", &["reboot"]);
                } else if held < Duration::from_secs(3) {
                    s.request = Some(if s.config_mode { crate::Request::ExitConfig } else { crate::Request::EnterConfig });
                }
            }
            _ => {}
        }
    }
}
