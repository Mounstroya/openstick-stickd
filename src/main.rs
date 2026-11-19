//! stickd — gestor de modos del stick OpenStick (UZ801).
//!
//! Prioridad automática (menor métrica de ruta gana):
//!   1. USB   — la laptop comparte internet por el cable (métrica 50)
//!   2. Casa  — cliente de una red WiFi guardada (métrica 600, la pone NM)
//!   3. LTE   — SIM con datos (métrica 700)
//!   4. Configuración — si no hay internet en 2 min, o con el botón: hotspot
//!      "Openstick" + portal web en http://192.168.4.1
//!
//! LEDs: rojo = encendido (parpadea rápido mientras mantienes el botón)
//!       verde = fijo: conectado a WiFi · parpadeo lento: modo configuración
//!       azul  = fijo: internet por USB o WiFi · latido: internet por LTE · destello corto: sin internet

mod button;
mod leds;
mod net;
mod portal;
mod util;

use leds::{Led, Leds, BLUE, GREEN, RED};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use util::log;

const MODE_FILE: &str = "/etc/stickd/modo";
const NO_NET_TO_CONFIG: Duration = Duration::from_secs(120);
const CONFIG_MAX_IDLE: Duration = Duration::from_secs(600);
const CONFIG_COOLDOWN: Duration = Duration::from_secs(300);
const TICK: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Auto,
    Usb,
    Casa,
    Lte,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim() {
            "auto" => Some(Mode::Auto),
            "usb" => Some(Mode::Usb),
            "casa" => Some(Mode::Casa),
            "lte" => Some(Mode::Lte),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Usb => "usb",
            Mode::Casa => "casa",
            Mode::Lte => "lte",
        }
    }
    fn allows_usb(self) -> bool {
        matches!(self, Mode::Auto | Mode::Usb)
    }
    fn allows_wifi(self) -> bool {
        matches!(self, Mode::Auto | Mode::Casa)
    }
    fn allows_lte(self) -> bool {
        matches!(self, Mode::Auto | Mode::Lte)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    EnterConfig,
    ExitConfig,
    ApplyMode,
}

pub struct State {
    pub mode: Mode,
    pub config_mode: bool,
    pub config_since: Option<Instant>,
    pub request: Option<Request>,
    pub button_held: bool,
    pub internet: bool,
    pub uplink: String,
    pub laptop: Option<String>,
    pub usb_sharing: bool,
    pub scan: Vec<(String, u32, String)>,
}

fn load_mode() -> Mode {
    std::fs::read_to_string(MODE_FILE).ok().and_then(|s| Mode::parse(&s)).unwrap_or(Mode::Auto)
}

pub fn save_mode(m: Mode) {
    let _ = std::fs::create_dir_all("/etc/stickd");
    let _ = std::fs::write(MODE_FILE, m.as_str());
}

/// Ajusta autoconnect de cada enlace según el modo y baja los que no corresponden.
fn apply_mode(m: Mode, config_mode: bool) {
    log(&format!("aplicando modo {}", m.as_str()));
    for (n, _) in net::saved_wifis() {
        net::set_autoconnect(&n, m.allows_wifi());
    }
    net::set_autoconnect("lte", m.allows_lte());
    if !m.allows_wifi() && !config_mode {
        net::wifis_down();
    }
    if !m.allows_lte() {
        net::con_down("lte");
    }
    if !m.allows_usb() {
        net::set_usb_default(None);
    }
    if m.allows_wifi() && !config_mode {
        net::connect_wifi();
    }
}

fn enter_config(state: &Arc<Mutex<State>>) {
    log("entrando a modo configuración");
    let found = net::scan(); // antes de levantar el AP: en modo AP no se puede escanear
    net::wifis_down();
    let up = net::con_up("hotspot");
    let mut s = state.lock().unwrap();
    s.scan = found;
    s.config_mode = up;
    s.config_since = Some(Instant::now());
    if !up {
        log("no se pudo levantar el hotspot");
    }
}

fn exit_config(state: &Arc<Mutex<State>>) {
    log("saliendo de modo configuración");
    net::con_down("hotspot");
    let mode = {
        let mut s = state.lock().unwrap();
        s.config_mode = false;
        s.mode
    };
    if mode.allows_wifi() {
        net::connect_wifi();
    }
}

fn hotspot_has_clients() -> bool {
    let (_, out) = util::run("ip", &["neigh", "show", "dev", "wlan0"]);
    out.lines().any(|l| l.contains("REACHABLE") || l.contains("DELAY") || l.contains("STALE"))
}

fn one_time_setup() {
    // LTE detrás de WiFi y USB; DNS de respaldo para cuando el único enlace sea la laptop
    let _ = util::run("nmcli", &["con", "modify", "lte", "ipv4.route-metric", "700", "ipv6.route-metric", "700"]);
    let _ = util::run("nmcli", &["con", "modify", "hotspot", "connection.autoconnect", "no"]);
}

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("stickd {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    log(&format!("iniciando stickd {}", env!("CARGO_PKG_VERSION")));
    one_time_setup();

    let state = Arc::new(Mutex::new(State {
        mode: load_mode(),
        config_mode: net::con_active("hotspot"),
        config_since: None,
        request: Some(Request::ApplyMode),
        button_held: false,
        internet: false,
        uplink: String::new(),
        laptop: None,
        usb_sharing: false,
        scan: Vec::new(),
    }));

    button::spawn(state.clone());
    portal::spawn(state.clone());

    let mut leds = Leds::new();
    let mut no_net_since: Option<Instant> = None;
    let mut last_config_exit: Option<Instant> = None;
    let mut last_wifi_try = Instant::now() - Duration::from_secs(3600);
    let mut last_lte_try = last_wifi_try;

    loop {
        let (mode, req, cfg) = {
            let mut s = state.lock().unwrap();
            (s.mode, s.request.take(), s.config_mode)
        };

        match req {
            Some(Request::EnterConfig) if !cfg => enter_config(&state),
            Some(Request::ExitConfig) if cfg => {
                exit_config(&state);
                last_config_exit = Some(Instant::now());
            }
            Some(Request::ApplyMode) => apply_mode(mode, cfg),
            _ => {}
        }
        let cfg = state.lock().unwrap().config_mode;

        // 1. USB: ¿la laptop comparte internet?
        let laptop = if mode.allows_usb() { net::laptop_ip() } else { None };
        let usb_ok = laptop.as_deref().map(net::usb_uplink_works).unwrap_or(false);
        net::set_usb_default(if usb_ok { laptop.as_deref() } else { None });

        // 2. WiFi casa: NM reconecta solo, pero empujamos cada minuto por si se rindió
        if mode.allows_wifi() && !cfg && !net::wifi_clients_up() && last_wifi_try.elapsed() > Duration::from_secs(60) {
            last_wifi_try = Instant::now();
            net::connect_wifi();
        }

        // 3. LTE: si el modo lo permite y no está activo, reintentar cada 2 min
        if mode.allows_lte() && !net::con_active("lte") && last_lte_try.elapsed() > Duration::from_secs(120) {
            last_lte_try = Instant::now();
            if net::lte().sim {
                let _ = net::con_up("lte");
            }
        }

        // Estado de internet
        let internet = net::internet();
        let uplink = net::uplink_dev().unwrap_or_default();

        // 4. Configuración automática si no hay internet
        if internet {
            no_net_since = None;
        } else {
            let since = *no_net_since.get_or_insert_with(Instant::now);
            let cooled = last_config_exit.is_none_or(|t| t.elapsed() > CONFIG_COOLDOWN);
            if !cfg && cooled && since.elapsed() > NO_NET_TO_CONFIG {
                enter_config(&state);
            }
        }
        // Salida automática si nadie usa el portal en 10 min (y reintenta las redes)
        let (cfg, since) = {
            let s = state.lock().unwrap();
            (s.config_mode, s.config_since)
        };
        if cfg && since.is_some_and(|t| t.elapsed() > CONFIG_MAX_IDLE) && !hotspot_has_clients() {
            exit_config(&state);
            last_config_exit = Some(Instant::now());
        }

        // LEDs
        let (cfg, held) = {
            let mut s = state.lock().unwrap();
            s.internet = internet;
            s.uplink = uplink.clone();
            s.laptop = laptop.clone();
            s.usb_sharing = usb_ok;
            (s.config_mode, s.button_held)
        };
        leds.set(RED, if held { Led::Blink(80, 80) } else { Led::On });
        leds.set(
            GREEN,
            if cfg {
                Led::Blink(500, 500)
            } else if net::wifi_clients_up() {
                Led::On
            } else {
                Led::Off
            },
        );
        leds.set(
            BLUE,
            if !internet {
                Led::Blink(100, 1900)
            } else if uplink.starts_with("wwan") || uplink.starts_with("rmnet") {
                Led::Heartbeat
            } else {
                Led::On
            },
        );

        std::thread::sleep(TICK);
    }
}
