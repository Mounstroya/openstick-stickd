//! LEDs del UZ801: rojo (power), verde (wlan), azul (wan). Solo encendido/apagado,
//! pero el kernel ofrece triggers: timer (parpadeo con delay_on/off), heartbeat, default-on.

use std::collections::HashMap;
use std::fs;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Led {
    Off,
    On,
    Blink(u32, u32), // ms encendido, ms apagado
    Heartbeat,
}

pub struct Leds {
    last: HashMap<&'static str, Led>,
}

pub const RED: &str = "red:power";
pub const GREEN: &str = "green:wlan";
pub const BLUE: &str = "blue:wan";

impl Leds {
    pub fn new() -> Self {
        Leds { last: HashMap::new() }
    }

    pub fn set(&mut self, name: &'static str, st: Led) {
        if self.last.get(name) == Some(&st) {
            return;
        }
        let base = format!("/sys/class/leds/{name}");
        let w = |f: &str, v: &str| {
            let _ = fs::write(format!("{base}/{f}"), v);
        };
        match st {
            Led::Off => {
                w("trigger", "none");
                w("brightness", "0");
            }
            Led::On => {
                w("trigger", "none");
                w("brightness", "1");
            }
            Led::Heartbeat => w("trigger", "heartbeat"),
            Led::Blink(on, off) => {
                w("trigger", "timer");
                w("delay_on", &on.to_string());
                w("delay_off", &off.to_string());
            }
        }
        self.last.insert(name, st);
    }
}
