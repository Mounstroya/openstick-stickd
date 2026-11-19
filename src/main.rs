//! stickd — gestor de modos del stick OpenStick (UZ801).
//!
//! Modos de conexión (prioridad automática, menor métrica de ruta gana):
//!   1. USB   — la laptop comparte internet por el cable (métrica 50)
//!   2. Casa  — cliente de una red WiFi guardada (métrica 600, la pone NM)
//!   3. LTE   — SIM con datos (métrica 700)
//!   4. Configuración — si no hay internet en 2 min, o con el botón: hotspot
//!      "Openstick" + portal web en http://192.168.4.1
//!
//! Vitrinas (src/vitrina/): el stick crea su PROPIO WiFi abierto y atiende a quien se conecta
//! (portafolio, tarjeta, buzón) o usa el radio en monitor (afluencia). Se eligen en el panel, y
//! una de ellas puede arrancar sola cuando el stick está en una powerbank (sin PC en el USB).
//! Opcional: "internet para invitados" (src/invitados.rs) da internet limitado a quien lo pide
//! desde la vitrina, por la salida que haya (USB o LTE).
//!
//! LEDs: rojo = encendido (parpadea rápido mientras mantienes el botón)
//!       verde = fijo: conectado a WiFi · parpadeo lento: configuración · latido: vitrina
//!       azul  = fijo: internet por USB o WiFi · latido: internet por LTE · destello corto: sin internet

mod button;
mod captive;
mod invitados;
mod leds;
mod net;
mod portal;
mod util;
mod vitrina;

use leds::{Led, Leds, BLUE, GREEN, RED};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use util::log;
use vitrina::Vitrina;

const MODE_FILE: &str = "/etc/stickd/modo";
const POWERBANK_FILE: &str = "/etc/stickd/powerbank";
const NO_NET_TO_CONFIG: Duration = Duration::from_secs(120);
const CONFIG_MAX_IDLE: Duration = Duration::from_secs(600);
const CONFIG_COOLDOWN: Duration = Duration::from_secs(300);
const NO_HOST_TO_POWERBANK: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_secs(5);

pub type Vitrinas = Arc<Vec<Box<dyn Vitrina>>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Auto,
    Usb,
    Casa,
    Lte,
    /// id del módulo de vitrina
    Vitrina(&'static str),
}

impl Mode {
    pub fn parse(s: &str, vit: &[Box<dyn Vitrina>]) -> Option<Mode> {
        match s.trim() {
            "auto" => Some(Mode::Auto),
            "usb" => Some(Mode::Usb),
            "casa" => Some(Mode::Casa),
            "lte" => Some(Mode::Lte),
            v => {
                let id = v.strip_prefix("vitrina:")?;
                vit.iter().find(|x| x.id() == id).map(|x| Mode::Vitrina(x.id()))
            }
        }
    }
    pub fn key(self) -> String {
        match self {
            Mode::Auto => "auto".into(),
            Mode::Usb => "usb".into(),
            Mode::Casa => "casa".into(),
            Mode::Lte => "lte".into(),
            Mode::Vitrina(id) => format!("vitrina:{id}"),
        }
    }
    fn allows_usb(self) -> bool {
        !matches!(self, Mode::Casa | Mode::Lte)
    }
    fn allows_wifi(self) -> bool {
        matches!(self, Mode::Auto | Mode::Casa)
    }
    fn allows_lte(self) -> bool {
        matches!(self, Mode::Auto | Mode::Lte | Mode::Vitrina(_))
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
    /// Modo que realmente está corriendo (puede ser la vitrina de powerbank)
    pub effective: Mode,
    pub powerbank: Option<&'static str>,
    pub host_present: bool,
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

fn read_trim(p: &str) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

pub fn save_file(p: &str, v: &str) {
    let _ = std::fs::create_dir_all("/etc/stickd");
    let _ = std::fs::write(p, v);
}

pub fn save_mode(m: Mode) {
    save_file(MODE_FILE, &m.key());
}

pub fn save_powerbank(id: Option<&str>) {
    save_file(POWERBANK_FILE, id.unwrap_or(""));
}

pub fn ssid_for(v: &dyn Vitrina) -> String {
    let s = read_trim(&format!("/etc/stickd/ssid-{}", v.id()));
    if s.is_empty() { v.ssid_por_defecto() } else { s }
}

pub fn save_ssid(id: &str, ssid: &str) {
    save_file(&format!("/etc/stickd/ssid-{id}"), ssid);
}

/// ¿Hay una computadora del otro lado del USB? (si no, es una powerbank o un cargador)
fn host_connected() -> bool {
    std::fs::read_dir("/sys/class/udc")
        .ok()
        .and_then(|mut d| d.next())
        .and_then(|e| e.ok())
        .map(|e| read_trim(&format!("{}/state", e.path().display())) == "configured")
        .unwrap_or(true)
}

/// Ajusta autoconnect de cada enlace según el modo y baja los que no corresponden.
fn apply_uplinks(m: Mode, config_mode: bool) {
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

fn vit_by_id<'a>(vit: &'a Vitrinas, id: &str) -> Option<(usize, &'a dyn Vitrina)> {
    vit.iter().enumerate().find(|(_, v)| v.id() == id).map(|(i, v)| (i, v.as_ref()))
}

fn stop_vitrina(vit: &Vitrinas, activa: &captive::Activa, id: &str) {
    if let Some((_, v)) = vit_by_id(vit, id) {
        log(&format!("deteniendo vitrina {id}"));
        *activa.lock().unwrap() = None;
        v.stop();
        if v.usa_ap() {
            captive::firewall_off();
            captive::ap_down();
        }
    }
}

fn start_vitrina(vit: &Vitrinas, activa: &captive::Activa, id: &str) {
    if let Some((i, v)) = vit_by_id(vit, id) {
        log(&format!("iniciando vitrina {id}"));
        if v.usa_ap() {
            net::wifis_down();
            let ssid = ssid_for(v);
            if captive::ap_up(&ssid) && captive::firewall_on() {
                *activa.lock().unwrap() = Some(i);
                log(&format!("vitrina {id} al aire con SSID «{ssid}»"));
            } else {
                log(&format!("no se pudo levantar el AP de la vitrina {id}"));
            }
        }
        v.start();
    }
}

fn enter_config(state: &Arc<Mutex<State>>, vit: &Vitrinas, activa: &captive::Activa) {
    log("entrando a modo configuración");
    if let Mode::Vitrina(id) = state.lock().unwrap().effective {
        stop_vitrina(vit, activa, id);
    }
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
    let mut s = state.lock().unwrap();
    s.config_mode = false;
    // vuelve a aplicar el modo efectivo (reconecta WiFi o vuelve a levantar la vitrina)
    s.request = Some(Request::ApplyMode);
}

fn hotspot_has_clients() -> bool {
    let (_, out) = util::run("ip", &["neigh", "show", "dev", "wlan0"]);
    out.lines().any(|l| l.contains("REACHABLE") || l.contains("DELAY") || l.contains("STALE"))
}

fn one_time_setup() {
    let _ = util::run("nmcli", &["con", "modify", "lte", "ipv4.route-metric", "700", "ipv6.route-metric", "700"]);
    let _ = util::run("nmcli", &["con", "modify", "hotspot", "connection.autoconnect", "no", "connection.autoconnect-priority", "0"]);
    captive::firewall_off();
    captive::ap_down();
}

/// Duerme hasta TICK, pero despierta en cuanto hay una petición (botón o panel).
fn nap(state: &Arc<Mutex<State>>) {
    let t = Instant::now();
    while t.elapsed() < TICK {
        if state.lock().unwrap().request.is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("stickd {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    log(&format!("iniciando stickd {}", env!("CARGO_PKG_VERSION")));
    one_time_setup();

    let vit: Vitrinas = Arc::new(vitrina::todas());
    let activa: captive::Activa = Arc::new(Mutex::new(None));
    let mode = Mode::parse(&read_trim(MODE_FILE), &vit).unwrap_or(Mode::Auto);
    let pb = read_trim(POWERBANK_FILE);
    let powerbank = vit.iter().find(|v| v.id() == pb).map(|v| v.id());

    let state = Arc::new(Mutex::new(State {
        mode,
        effective: mode,
        powerbank,
        host_present: true,
        config_mode: net::con_active("hotspot"),
        config_since: None,
        request: None,
        button_held: false,
        internet: false,
        uplink: String::new(),
        laptop: None,
        usb_sharing: false,
        scan: Vec::new(),
    }));

    button::spawn(state.clone());
    portal::spawn(state.clone(), vit.clone(), activa.clone());
    captive::spawn(vit.clone(), activa.clone());
    invitados::spawn();

    let mut leds = Leds::new();
    let mut applied: Option<Mode> = None;
    let mut no_net_since: Option<Instant> = None;
    let mut no_host_since: Option<Instant> = None;
    let mut last_config_exit: Option<Instant> = None;
    let mut last_wifi_try = Instant::now() - Duration::from_secs(3600);
    let mut last_lte_try = last_wifi_try;
    let mut last_ap_try = Instant::now();

    loop {
        // ¿PC o powerbank?
        let host = host_connected();
        if host {
            no_host_since = None;
        } else {
            no_host_since.get_or_insert_with(Instant::now);
        }
        let on_powerbank = no_host_since.is_some_and(|t| t.elapsed() >= NO_HOST_TO_POWERBANK);

        let (mode, req, cfg, pb) = {
            let mut s = state.lock().unwrap();
            s.host_present = host;
            (s.mode, s.request.take(), s.config_mode, s.powerbank)
        };
        let eff = match (on_powerbank, pb) {
            (true, Some(id)) if !matches!(mode, Mode::Vitrina(_)) => Mode::Vitrina(id),
            _ => mode,
        };

        match req {
            Some(Request::EnterConfig) if !cfg => enter_config(&state, &vit, &activa),
            Some(Request::ExitConfig) if cfg => {
                exit_config(&state);
                last_config_exit = Some(Instant::now());
            }
            Some(Request::ApplyMode) => applied = None,
            _ => {}
        }
        let cfg = state.lock().unwrap().config_mode;

        // Cambio de modo efectivo
        if applied != Some(eff) && !cfg {
            if let Some(Mode::Vitrina(old)) = applied {
                stop_vitrina(&vit, &activa, old);
            } else if let Mode::Vitrina(_) = eff {
                // venimos de un modo normal: nada que detener
            }
            log(&format!("aplicando modo {}", eff.key()));
            apply_uplinks(eff, cfg);
            if let Mode::Vitrina(id) = eff {
                start_vitrina(&vit, &activa, id);
            }
            applied = Some(eff);
            state.lock().unwrap().effective = eff;
        } else if cfg && applied.is_some_and(|a| matches!(a, Mode::Vitrina(_))) {
            // la vitrina se detuvo al entrar a configuración; al salir se re-aplica
            applied = None;
        } else if let Mode::Vitrina(id) = eff {
            // el AP no levantó (radio ocupado, NM lento…): reintentar cada 30 s
            let needs_ap = vit_by_id(&vit, id).is_some_and(|(_, v)| v.usa_ap());
            if needs_ap && !cfg && activa.lock().unwrap().is_none() && last_ap_try.elapsed() > Duration::from_secs(30) {
                last_ap_try = Instant::now();
                log(&format!("reintentando el AP de la vitrina {id}"));
                start_vitrina(&vit, &activa, id);
            }
        }

        // 1. USB: ¿la laptop comparte internet?
        let laptop = if eff.allows_usb() && host { net::laptop_ip() } else { None };
        let usb_ok = laptop.as_deref().map(net::usb_uplink_works).unwrap_or(false);
        net::set_usb_default(if usb_ok { laptop.as_deref() } else { None });

        // 2. WiFi casa: empujar cada minuto por si NM se rindió
        if eff.allows_wifi() && !cfg && !net::wifi_clients_up() && last_wifi_try.elapsed() > Duration::from_secs(60) {
            last_wifi_try = Instant::now();
            net::connect_wifi();
        }

        // 3. LTE
        if eff.allows_lte() && !net::con_active("lte") && last_lte_try.elapsed() > Duration::from_secs(120) {
            last_lte_try = Instant::now();
            if net::lte().sim {
                let _ = net::con_up("lte");
            }
        }

        let internet = net::internet();
        let uplink = net::uplink_dev().unwrap_or_default();
        // salida para los invitados de una vitrina: USB o LTE (el WiFi es el AP)
        invitados::set_salida(internet && !uplink.is_empty() && uplink != "wlan0" && uplink != "wg0", &uplink);

        // 4. Configuración automática si no hay internet (no en vitrina: ahí es normal)
        if internet || matches!(eff, Mode::Vitrina(_)) {
            no_net_since = None;
        } else {
            let since = *no_net_since.get_or_insert_with(Instant::now);
            let cooled = last_config_exit.is_none_or(|t| t.elapsed() > CONFIG_COOLDOWN);
            if !cfg && cooled && since.elapsed() > NO_NET_TO_CONFIG {
                enter_config(&state, &vit, &activa);
            }
        }
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
            } else if matches!(eff, Mode::Vitrina(_)) {
                Led::Heartbeat
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

        nap(&state);
    }
}
