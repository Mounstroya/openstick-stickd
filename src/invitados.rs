//! "Internet para invitados": en una vitrina con AP, quien se conecta puede pedir internet por
//! un rato (minutos por dispositivo, velocidad y datos limitados) desde una barra que el núcleo
//! inyecta en las páginas de la vitrina.
//!
//! Cómo funciona (todo en la tabla nft `inet stickd_captive` de captive.rs):
//!   - set `invitados` (ipv4, `flags timeout`, `counter`): una IP con acceso es un elemento con
//!     timeout = minutos concedidos; el kernel lo borra solo al expirar. El contador por elemento
//!     cuenta los bytes (subida + bajada) para el tope de datos (no hay `nft_quota` en este kernel:
//!     el tope lo vigila `tick()` cada pocos segundos).
//!   - prerouting: a los miembros NO se les secuestra DNS (va a un resolvedor real) ni HTTP (salvo
//!     el que va al propio stick, que sigue siendo la vitrina).
//!   - forward: miembros → cualquier interfaz menos el AP y wg0, nunca a redes privadas; la vuelta
//!     solo `established,related`. Velocidad por IP con sets dinámicos + `limit rate over` (este
//!     kernel no trae HTB/TBF/ingress, así que es un policía que descarta lo que sobra).
//!   - los paquetes aceptados llevan una marca; postrouting hace masquerade de lo marcado, y en las
//!     tablas `nm-shared-*` de NetworkManager (que rechazan reenviar hacia usb0) se inserta una
//!     regla "marca → accept", que solo sirve mientras nuestra tabla (que marca) esté cargada.

use crate::util::{html, log, run};
use crate::vitrina::GuestResp;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const CONF: &str = "/etc/stickd/invitados";
pub const TABLA: &str = "stickd_captive";
pub const SET: &str = "invitados";
/// Marca de los paquetes de invitados aceptados ("STKI").
pub const MARCA: &str = "0x53544b49";
const COMENTARIO: &str = "stickd-invitados";
/// Resolvedor para los invitados con acceso (el dnsmasq de NM reenvía a 127.0.0.1, que en el
/// stick no responde).
pub const DNS_REAL: &str = "1.1.1.1";
/// Gancho de PRUEBA: si existe, nombre de una interfaz extra que se trata como el AP (veth de un
/// network namespace). Solo root escribe en /run.
const PRUEBA_IF: &str = "/run/stickd/ap-prueba";
pub const RUTA: &str = "/_stick/internet";
/// Destinos que un invitado nunca puede alcanzar (laptop, túnel WireGuard, LAN, CGNAT…).
pub const PRIVADAS: &str = "0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/3";

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub activo: bool,
    pub minutos: u32,
    /// Mbps por dispositivo; 0 = sin límite
    pub baja_mbps: f64,
    pub sube_mbps: f64,
    pub max_disp: u32,
    /// MB por dispositivo; 0 = sin tope
    pub tope_mb: u64,
    /// ¿Debe ver una página de la vitrina antes de poder pedir internet?
    pub exigir_vista: bool,
    /// Horas antes de que el mismo dispositivo pueda pedir otra vez; 0 = cuando quiera
    pub horas_espera: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config { activo: false, minutos: 30, baja_mbps: 5.0, sube_mbps: 1.0, max_disp: 5, tope_mb: 0, exigir_vista: true, horas_espera: 24 }
    }
}

impl Config {
    pub fn parse(s: &str) -> Config {
        let mut c = Config::default();
        for l in s.lines() {
            let Some((k, v)) = l.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "activo" => c.activo = v == "si",
                "minutos" => c.minutos = v.parse().unwrap_or(c.minutos),
                "baja_mbps" => c.baja_mbps = v.parse().unwrap_or(c.baja_mbps),
                "sube_mbps" => c.sube_mbps = v.parse().unwrap_or(c.sube_mbps),
                "max_disp" => c.max_disp = v.parse().unwrap_or(c.max_disp),
                "tope_mb" => c.tope_mb = v.parse().unwrap_or(c.tope_mb),
                "exigir_vista" => c.exigir_vista = v == "si",
                "horas_espera" => c.horas_espera = v.parse().unwrap_or(c.horas_espera),
                _ => {}
            }
        }
        c.normalizar()
    }

    pub fn texto(&self) -> String {
        let sn = |b: bool| if b { "si" } else { "no" };
        format!(
            "activo={}\nminutos={}\nbaja_mbps={}\nsube_mbps={}\nmax_disp={}\ntope_mb={}\nexigir_vista={}\nhoras_espera={}\n",
            sn(self.activo),
            self.minutos,
            self.baja_mbps,
            self.sube_mbps,
            self.max_disp,
            self.tope_mb,
            sn(self.exigir_vista),
            self.horas_espera
        )
    }

    fn normalizar(mut self) -> Config {
        self.minutos = self.minutos.clamp(1, 24 * 60);
        let mb = |x: f64| if x.is_finite() { (x.clamp(0.0, 1000.0) * 10.0).round() / 10.0 } else { 0.0 };
        self.baja_mbps = mb(self.baja_mbps);
        self.sube_mbps = mb(self.sube_mbps);
        self.max_disp = self.max_disp.clamp(1, 200);
        self.tope_mb = self.tope_mb.min(100_000);
        self.horas_espera = self.horas_espera.min(24 * 30);
        self
    }

    /// Bytes por segundo para `limit rate over` (None = sin límite).
    pub fn bps(mbps: f64) -> Option<u64> {
        if mbps > 0.0 { Some(((mbps * 125_000.0) as u64).max(1000)) } else { None }
    }

    fn velocidad_txt(&self) -> String {
        let f = |x: f64| if x > 0.0 { format!("{} Mbps", fmt_num(x)) } else { "sin límite".into() };
        format!("bajada {} · subida {}", f(self.baja_mbps), f(self.sube_mbps))
    }
}

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 { format!("{}", x as u64) } else { format!("{x:.1}") }
}

#[derive(Clone, Debug)]
pub struct Acceso {
    pub ip: Ipv4Addr,
    pub mac: String,
    pub nombre: String,
    pub hasta: Instant,
    /// Bytes de recargas anteriores de la tabla (al recargar el contador vuelve a 0)
    pub bytes_previos: u64,
    pub bytes_nft: u64,
}

impl Acceso {
    pub fn bytes(&self) -> u64 {
        self.bytes_previos + self.bytes_nft
    }
    pub fn restante(&self, ahora: Instant) -> Duration {
        self.hasta.saturating_duration_since(ahora)
    }
}

#[derive(Debug, PartialEq)]
pub enum Rechazo {
    Apagado,
    SinAp,
    SinSalida,
    VerPrimero,
    YaUsado(Duration),
    Lleno,
}

impl Rechazo {
    fn texto(&self) -> String {
        match self {
            Rechazo::Apagado => "El internet para invitados está apagado.".into(),
            Rechazo::SinAp => "La vitrina no está al aire.".into(),
            Rechazo::SinSalida => "Ahora mismo el stick no tiene conexión a internet. Intenta más tarde.".into(),
            Rechazo::VerPrimero => "Primero echa un vistazo a la página.".into(),
            Rechazo::YaUsado(d) => {
                let m = d.as_secs().div_ceil(60);
                if m >= 120 {
                    format!("Este dispositivo ya usó su tiempo de internet. Podrás pedir otra vez en unas {} horas.", m.div_ceil(60))
                } else {
                    format!("Este dispositivo ya usó su tiempo de internet. Podrás pedir otra vez en {m} min.")
                }
            }
            Rechazo::Lleno => "Ya hay el máximo de dispositivos usando internet. Intenta en unos minutos.".into(),
        }
    }
}

pub struct Estado {
    pub cfg: Config,
    pub accesos: Vec<Acceso>,
    /// (dispositivo, cuándo pidió) para `horas_espera`
    usados: Vec<(String, Instant)>,
    vistos: Vec<(Ipv4Addr, Instant)>,
    pub salida: bool,
    pub salida_dev: String,
    pub ap: bool,
    ganchos: bool,
}

impl Estado {
    pub fn nuevo(cfg: Config) -> Estado {
        Estado { cfg, accesos: vec![], usados: vec![], vistos: vec![], salida: false, salida_dev: String::new(), ap: false, ganchos: false }
    }

    pub fn tiene(&self, ip: Ipv4Addr, ahora: Instant) -> bool {
        self.accesos.iter().any(|a| a.ip == ip && a.hasta > ahora)
    }

    fn visto(&self, ip: Ipv4Addr) -> bool {
        self.vistos.iter().any(|(v, _)| *v == ip)
    }

    /// ¿Puede este dispositivo recibir acceso ahora? (no mira si ya lo tiene)
    pub fn validar(&self, ip: Ipv4Addr, disp: &str, ahora: Instant) -> Result<(), Rechazo> {
        if !self.cfg.activo {
            return Err(Rechazo::Apagado);
        }
        if !self.ap {
            return Err(Rechazo::SinAp);
        }
        if !self.salida {
            return Err(Rechazo::SinSalida);
        }
        let espera = Duration::from_secs(self.cfg.horas_espera as u64 * 3600);
        if let Some((_, t)) = self.usados.iter().find(|(d, _)| d == disp) {
            let pasado = ahora.saturating_duration_since(*t);
            if pasado < espera {
                return Err(Rechazo::YaUsado(espera - pasado));
            }
        }
        if self.accesos.iter().filter(|a| a.hasta > ahora).count() >= self.cfg.max_disp as usize {
            return Err(Rechazo::Lleno);
        }
        if self.cfg.exigir_vista && !self.visto(ip) {
            return Err(Rechazo::VerPrimero);
        }
        Ok(())
    }

    fn registrar(&mut self, a: Acceso, disp: String, ahora: Instant) {
        self.usados.retain(|(d, _)| *d != disp);
        self.usados.push((disp, ahora));
        self.accesos.retain(|x| x.ip != a.ip);
        self.accesos.push(a);
    }

    fn podar(&mut self, ahora: Instant) {
        let espera = Duration::from_secs(self.cfg.horas_espera as u64 * 3600);
        self.usados.retain(|(_, t)| ahora.saturating_duration_since(*t) < espera);
        self.vistos.retain(|(_, t)| ahora.saturating_duration_since(*t) < Duration::from_secs(3600));
    }
}

static E: LazyLock<Mutex<Estado>> =
    LazyLock::new(|| Mutex::new(Estado::nuevo(Config::parse(&std::fs::read_to_string(CONF).unwrap_or_default()))));

fn e() -> std::sync::MutexGuard<'static, Estado> {
    E.lock().unwrap_or_else(|p| p.into_inner())
}

fn v4(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V4(v) => Some(v),
        IpAddr::V6(v) => v.to_ipv4_mapped(),
    }
}

// ---------------------------------------------------------------- API para el resto del núcleo

pub fn config() -> Config {
    e().cfg.clone()
}

/// Interfaces que cuentan como el AP de la vitrina (wlan0 + la de prueba, si hay).
pub fn ap_ifaces() -> Vec<String> {
    let mut v = vec!["wlan0".to_string()];
    let extra = std::fs::read_to_string(PRUEBA_IF).unwrap_or_default();
    let extra = extra.trim();
    if !extra.is_empty() && extra.len() <= 15 && extra.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') && extra != "wlan0" {
        v.push(extra.to_string());
    }
    v
}

/// Elementos vigentes (ip, segundos restantes) para recrear el set al recargar la tabla.
pub fn elementos() -> Vec<(Ipv4Addr, u64)> {
    let ahora = Instant::now();
    let s = e();
    if !s.cfg.activo {
        return vec![];
    }
    s.accesos.iter().filter(|a| a.hasta > ahora).map(|a| (a.ip, a.restante(ahora).as_secs().max(1))).collect()
}

/// El bucle principal avisa si el stick tiene salida a internet (USB/LTE, no el WiFi del AP).
pub fn set_salida(ok: bool, dev: &str) {
    let mut s = e();
    s.salida = ok;
    s.salida_dev = dev.to_string();
}

/// captive.rs acaba de cargar la tabla (AP al aire).
pub fn al_subir_ap() {
    let activo = {
        let mut s = e();
        s.ap = true;
        s.cfg.activo
    };
    if activo {
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        asegurar_ganchos();
    }
}

/// captive.rs va a borrar la tabla (vitrina detenida): se corta todo y se limpia.
pub fn al_bajar_ap() {
    let ganchos = {
        let mut s = e();
        s.ap = false;
        let n = s.accesos.len();
        s.accesos.clear();
        s.vistos.clear();
        if n > 0 {
            log(&format!("invitados: vitrina detenida, {n} acceso(s) cortado(s)"));
        }
        std::mem::replace(&mut s.ganchos, false)
    };
    if ganchos || nm_tiene_ganchos() {
        quitar_ganchos();
    }
}

pub fn tiene_internet(ip: IpAddr) -> bool {
    v4(ip).is_some_and(|ip| e().tiene(ip, Instant::now()))
}

/// ¿Mostrar la barra "Continuar con internet" a este cliente? Devuelve los minutos que ofrece.
pub fn ofrecer(ip: IpAddr) -> Option<u32> {
    let ip = v4(ip)?;
    let (mac, _) = dispositivo(ip);
    let ahora = Instant::now();
    let s = e();
    if s.tiene(ip, ahora) {
        return None;
    }
    match s.validar(ip, &id_disp(ip, &mac), ahora) {
        Ok(()) | Err(Rechazo::VerPrimero) => Some(s.cfg.minutos),
        Err(_) => None,
    }
}

/// El cliente vio una página HTML de la vitrina.
pub fn marcar_visto(ip: IpAddr) {
    if let Some(ip) = v4(ip) {
        let mut s = e();
        let ahora = Instant::now();
        s.vistos.retain(|(v, _)| *v != ip);
        s.vistos.push((ip, ahora));
    }
}

/// Si la sonda de portal cautivo debe ir directo a la página de internet (no se exige ver la vitrina).
pub fn sonda_directa(ip: IpAddr) -> bool {
    !config().exigir_vista && ofrecer(ip).is_some()
}

fn id_disp(ip: Ipv4Addr, mac: &str) -> String {
    if mac.is_empty() { ip.to_string() } else { mac.to_string() }
}

/// (MAC, nombre) del cliente según los leases del dnsmasq de NM, o la tabla de vecinos.
fn dispositivo(ip: Ipv4Addr) -> (String, String) {
    let ips = ip.to_string();
    let leases = std::fs::read_to_string("/var/lib/NetworkManager/dnsmasq-wlan0.leases").unwrap_or_default();
    if let Some(l) = leases.lines().find(|l| l.split_whitespace().nth(2) == Some(ips.as_str())) {
        let w: Vec<&str> = l.split_whitespace().collect();
        let nombre = w.get(3).filter(|n| **n != "*").map(|n| n.to_string()).unwrap_or_default();
        return (w.get(1).unwrap_or(&"").to_lowercase(), nombre);
    }
    let (_, out) = run("ip", &["neigh", "show", &ips]);
    let mac = out.split_whitespace().skip_while(|w| *w != "lladdr").nth(1).unwrap_or("").to_lowercase();
    (mac, String::new())
}

/// Concede acceso a `ip`. Ok(minutos restantes).
pub fn conceder(ip: Ipv4Addr) -> Result<u64, Rechazo> {
    let (mac, nombre) = dispositivo(ip);
    let disp = id_disp(ip, &mac);
    let ahora = Instant::now();
    let mut s = e();
    if let Some(a) = s.accesos.iter().find(|a| a.ip == ip && a.hasta > ahora) {
        return Ok(a.restante(ahora).as_secs().div_ceil(60));
    }
    s.validar(ip, &disp, ahora)?;
    let secs = s.cfg.minutos as u64 * 60;
    let el = format!("{{ {ip} timeout {secs}s }}");
    let (ok, _) = run("nft", &["add", "element", "inet", TABLA, SET, &el]);
    if !ok {
        log(&format!("invitados: nft no aceptó el elemento {ip}"));
        return Err(Rechazo::SinAp);
    }
    log(&format!("invitados: acceso a {ip} ({}) por {} min", if nombre.is_empty() { &disp } else { &nombre }, s.cfg.minutos));
    let a = Acceso { ip, mac, nombre, hasta: ahora + Duration::from_secs(secs), bytes_previos: 0, bytes_nft: 0 };
    s.registrar(a, disp, ahora);
    Ok(secs / 60)
}

fn nft_quitar(ip: Ipv4Addr) {
    let _ = run("nft", &["delete", "element", "inet", TABLA, SET, &format!("{{ {ip} }}")]);
}

pub fn cortar(ip: Ipv4Addr) -> bool {
    let mut s = e();
    let antes = s.accesos.len();
    s.accesos.retain(|a| a.ip != ip);
    let hubo = s.accesos.len() != antes;
    drop(s);
    nft_quitar(ip);
    if hubo {
        log(&format!("invitados: acceso de {ip} cortado"));
    }
    hubo
}

fn cortar_todos() {
    let ips: Vec<Ipv4Addr> = e().accesos.drain(..).map(|a| a.ip).collect();
    if !ips.is_empty() {
        let _ = run("nft", &["flush", "set", "inet", TABLA, SET]);
        log(&format!("invitados: {} acceso(s) cortado(s)", ips.len()));
    }
}

/// Lee los contadores del set: Some([(ip, bytes)]) o None si la tabla no existe.
fn nft_contadores() -> Option<Vec<(Ipv4Addr, u64)>> {
    let (ok, out) = run("nft", &["list", "set", "inet", TABLA, SET]);
    ok.then(|| parse_elementos(&out))
}

pub fn parse_elementos(out: &str) -> Vec<(Ipv4Addr, u64)> {
    let Some(i) = out.find("elements = {") else { return vec![] };
    let rest = &out[i + 12..];
    let rest = &rest[..rest.find('}').unwrap_or(rest.len())];
    rest.split(',')
        .filter_map(|el| {
            let w: Vec<&str> = el.split_whitespace().collect();
            let ip: Ipv4Addr = w.first()?.parse().ok()?;
            let bytes = w.iter().position(|x| *x == "bytes").and_then(|p| w.get(p + 1)).and_then(|b| b.parse().ok()).unwrap_or(0);
            Some((ip, bytes))
        })
        .collect()
}

/// Recarga la tabla del portal (p.ej. cambió la velocidad) conservando los accesos.
fn recargar() {
    if let Some(c) = nft_contadores() {
        let mut s = e();
        for a in s.accesos.iter_mut() {
            if let Some((_, b)) = c.iter().find(|(ip, _)| *ip == a.ip) {
                a.bytes_nft = *b;
            }
            a.bytes_previos += a.bytes_nft;
            a.bytes_nft = 0;
        }
    }
    crate::captive::firewall_on();
}

/// Ciclo de mantenimiento: expiraciones, tope de datos y ganchos en las tablas de NM.
pub fn tick(vuelta: u64) {
    let ahora = Instant::now();
    let (ap, activo, hay) = {
        let mut s = e();
        s.podar(ahora);
        (s.ap, s.cfg.activo, !s.accesos.is_empty())
    };
    if !ap {
        return;
    }
    if hay {
        let cont = nft_contadores();
        let mut s = e();
        let tope_mb = s.cfg.tope_mb;
        let tope = tope_mb * 1_000_000;
        let mut fuera = vec![];
        for a in s.accesos.iter_mut() {
            let en_nft = cont.as_ref().and_then(|c| c.iter().find(|(ip, _)| *ip == a.ip));
            if let Some((_, b)) = en_nft {
                a.bytes_nft = *b;
            }
            if a.hasta <= ahora || (cont.is_some() && en_nft.is_none()) {
                log(&format!("invitados: expiró el acceso de {}", a.ip));
                fuera.push((a.ip, false));
            } else if tope > 0 && a.bytes() >= tope {
                log(&format!("invitados: {} llegó al tope de {tope_mb} MB", a.ip));
                fuera.push((a.ip, true));
            }
        }
        s.accesos.retain(|a| !fuera.iter().any(|(ip, _)| *ip == a.ip));
        drop(s);
        for (ip, quitar) in fuera {
            if quitar {
                nft_quitar(ip);
            }
        }
    }
    // NM recrea sus tablas al reconectar usb0: revisar el gancho cada ~15 s
    if activo && vuelta % 5 == 0 {
        asegurar_ganchos();
    }
}

pub fn spawn() {
    std::thread::spawn(|| {
        let mut n = 0u64;
        loop {
            std::thread::sleep(Duration::from_secs(3));
            n += 1;
            tick(n);
        }
    });
}

// ---------------------------------------------------------------- ganchos en las tablas de NM

fn tablas_nm() -> Vec<String> {
    let (_, out) = run("nft", &["list", "tables", "ip"]);
    out.lines().filter_map(|l| l.strip_prefix("table ip ")).filter(|t| t.starts_with("nm-shared-")).map(|t| t.trim().to_string()).collect()
}

fn handles_gancho(tabla: &str) -> Vec<String> {
    let (_, out) = run("nft", &["-a", "list", "chain", "ip", tabla, "filter_forward"]);
    out.lines().filter(|l| l.contains(COMENTARIO)).filter_map(|l| l.rsplit_once("# handle ").map(|(_, h)| h.trim().to_string())).collect()
}

fn nm_tiene_ganchos() -> bool {
    tablas_nm().iter().any(|t| !handles_gancho(t).is_empty())
}

/// Las tablas `nm-shared-*` rechazan reenviar hacia/desde usb0 lo que no sea de su subred: se les
/// antepone "lo marcado por stickd → accept" (solo lo marca nuestra tabla, que ya filtró).
fn asegurar_ganchos() {
    let mut todos = true;
    for t in tablas_nm() {
        if handles_gancho(&t).is_empty() {
            let regla = format!("meta mark {MARCA} accept comment \"{COMENTARIO}\"");
            // nft une sus argumentos con espacios: la regla puede ir en uno solo
            if !run("nft", &["insert", "rule", "ip", &t, "filter_forward", &regla]).0 {
                todos = false;
                log(&format!("invitados: no pude poner el gancho en {t}"));
            }
        }
    }
    e().ganchos = todos;
}

fn quitar_ganchos() {
    for t in tablas_nm() {
        for h in handles_gancho(&t) {
            let _ = run("nft", &["delete", "rule", "ip", &t, "filter_forward", "handle", &h]);
        }
    }
}

// ---------------------------------------------------------------- respuestas para invitados

/// Respuesta de "hay internet" para las sondas de portal cautivo de un cliente con acceso.
pub fn sonda_exito(host: &str, path: &str) -> GuestResp {
    let h = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    let txt = |s: &str| GuestResp::bytes("text/plain", s.as_bytes().to_vec());
    match path {
        "/generate_204" | "/gen_204" => GuestResp { status: 204, content_type: "text/plain".into(), headers: vec![], body: vec![] },
        "/connecttest.txt" => txt("Microsoft Connect Test"),
        "/ncsi.txt" => txt("Microsoft NCSI"),
        "/success.txt" => txt("success\n"),
        "/canonical.html" => GuestResp::html(
            "<meta http-equiv=\"refresh\" content=\"0;url=https://support.mozilla.org/kb/captive-portal\"/>",
        ),
        _ if h == "nmcheck.gnome.org" => txt("NetworkManager is online\n"),
        _ => GuestResp::html("<HTML><HEAD><TITLE>Success</TITLE></HEAD><BODY>Success</BODY></HTML>"),
    }
}

/// Barra fija mínima con estilos aislados (`all:initial` + todo en línea, sin JS).
pub fn barra(minutos: u32) -> String {
    format!(
        concat!(
            "<div id=\"_stick_inet_hueco\" style=\"all:initial;display:block;height:64px\"></div>",
            "<div id=\"_stick_inet\" style=\"all:initial;position:fixed;left:0;right:0;bottom:0;z-index:2147483647;display:flex;",
            "justify-content:center;box-sizing:border-box;padding:8px 12px calc(8px + env(safe-area-inset-bottom));",
            "background:rgba(12,14,18,.92)\">",
            "<a href=\"{}\" style=\"all:initial;display:block;box-sizing:border-box;width:100%;max-width:420px;text-align:center;",
            "background:#2f6fe4;color:#fff;font:600 16px/1.25 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;",
            "padding:12px 16px;border-radius:10px;cursor:pointer\">Continuar con internet ({} min)</a></div>"
        ),
        RUTA, minutos
    )
}

fn buscar_ci(hay: &[u8], aguja: &[u8]) -> Option<usize> {
    if aguja.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - aguja.len()).rev().find(|&i| hay[i..i + aguja.len()].eq_ignore_ascii_case(aguja))
}

/// Inserta `pieza` antes del último `</body>` (o `</html>`, o al final) sin tocar lo demás.
pub fn inyectar(body: &[u8], pieza: &str) -> Vec<u8> {
    let pos = buscar_ci(body, b"</body").or_else(|| buscar_ci(body, b"</html")).unwrap_or(body.len());
    let mut v = Vec::with_capacity(body.len() + pieza.len());
    v.extend_from_slice(&body[..pos]);
    v.extend_from_slice(pieza.as_bytes());
    v.extend_from_slice(&body[pos..]);
    v
}

/// Aplica la barra a una respuesta de la vitrina si corresponde. El módulo puede evitarla con el
/// header `X-Stick-Barra: no` (se quita antes de enviar).
pub fn procesar_respuesta(mut r: GuestResp, ip: IpAddr, method: &str) -> GuestResp {
    let sin_barra = r.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("X-Stick-Barra") && v.eq_ignore_ascii_case("no"));
    r.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("X-Stick-Barra"));
    let es_html = r.status == 200 && r.content_type.to_ascii_lowercase().starts_with("text/html");
    let codificado = r.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("Content-Encoding"));
    if !es_html || method != "GET" || codificado {
        return r;
    }
    marcar_visto(ip);
    if sin_barra {
        return r;
    }
    if let Some(min) = ofrecer(ip) {
        r.body = inyectar(&r.body, &barra(min));
    }
    r
}

fn pagina_html(titulo: &str, cuerpo: &str) -> GuestResp {
    GuestResp::html(format!(
        r#"<!doctype html><html lang=es><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1"><meta name=color-scheme content="light dark"><title>{t}</title><style>
:root{{--bg:#f3f4f7;--card:#fff;--tx:#14161c;--mut:#555b69;--ac:#1f5fd6;--bd:#00000020;--bad:#c62828;--ok:#11804f}}
@media (prefers-color-scheme:dark){{:root{{--bg:#0f1115;--card:#181b22;--tx:#e8eaf0;--mut:#9aa0ae;--ac:#2f6fe4;--bd:#ffffff2a;--bad:#ff6b6b;--ok:#3ecf8e}}}}
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--tx);font:16px/1.5 system-ui,-apple-system,sans-serif}}
main{{max-width:460px;margin:auto;padding:24px 16px}}.card{{background:var(--card);border:1px solid var(--bd);border-radius:14px;padding:18px}}
h1{{font-size:21px;margin:0 0 12px}}ul{{padding-left:20px;margin:8px 0 16px}}li{{margin:4px 0}}.mut{{color:var(--mut);font-size:14px}}
button,a.btn{{display:block;width:100%;background:var(--ac);color:#fff;border:0;border-radius:10px;padding:14px;font:600 17px/1.2 system-ui,sans-serif;text-align:center;text-decoration:none;cursor:pointer}}
a.sec{{display:block;text-align:center;margin-top:14px;color:var(--ac)}}.bad{{color:var(--bad)}}.ok{{color:var(--ok)}}
</style></head><body><main><div class=card><h1>{t}</h1>{c}</div></main></body></html>"#,
        t = html(titulo),
        c = cuerpo
    ))
}

/// GET/POST /_stick/internet (lo atiende el núcleo antes de delegar al módulo).
pub fn pagina(ip: IpAddr, method: &str) -> GuestResp {
    // absoluto: tras conceder, el DNS del invitado ya es real y "/" sería otro sitio
    let volver = "<a class=sec href=\"http://192.168.4.1/\">Volver a la página</a>";
    let Some(ip4) = v4(ip) else { return pagina_html("Internet para invitados", "<p class=bad>Solo IPv4.</p>") };
    let ahora = Instant::now();
    if method == "POST" {
        return match conceder(ip4) {
            Ok(min) => pagina_html(
                "¡Listo, ya tienes internet!",
                &format!(
                    "<p class=ok><b>Tienes internet por {min} min.</b></p><p>Si tu teléfono sigue mostrando «Iniciar sesión» o «Usar esta red tal como está», ciérralo o elige mantener la conexión: ya estás conectado.</p><p class=mut>Al terminar el tiempo volverás a ver esta página.</p>{volver}"
                ),
            ),
            Err(Rechazo::VerPrimero) => GuestResp::redirect("http://192.168.4.1/"),
            Err(r) => pagina_html("Internet para invitados", &format!("<p class=bad>{}</p>{volver}", html(&r.texto()))),
        };
    }
    let (mac, _) = dispositivo(ip4);
    let s = e();
    if let Some(a) = s.accesos.iter().find(|a| a.ip == ip4 && a.hasta > ahora) {
        let min = a.restante(ahora).as_secs().div_ceil(60);
        return pagina_html("Ya tienes internet", &format!("<p class=ok>Te quedan <b>{min} min</b>.</p>{volver}"));
    }
    let c = s.cfg.clone();
    let check = s.validar(ip4, &id_disp(ip4, &mac), ahora);
    drop(s);
    match check {
        Ok(()) | Err(Rechazo::VerPrimero) => {
            let mut cond = format!("<li>Tiempo: <b>{} minutos</b>.</li><li>Velocidad máxima: {}.</li>", c.minutos, html(&c.velocidad_txt()));
            if c.tope_mb > 0 {
                cond += &format!("<li>Datos: hasta <b>{} MB</b>.</li>", c.tope_mb);
            }
            if c.horas_espera > 0 {
                cond += &format!("<li>Una vez por dispositivo cada {} h.</li>", c.horas_espera);
            }
            cond += "<li>Solo internet: no hay acceso a otros equipos ni redes privadas.</li><li>El dueño puede cortar el acceso en cualquier momento. Úsalo con respeto.</li>";
            pagina_html(
                "Internet para invitados",
                &format!(
                    "<p>Puedes usar internet desde este WiFi con estas condiciones:</p><ul>{cond}</ul><form method=post action=\"{RUTA}\"><button>Aceptar y conectar</button></form>{volver}"
                ),
            )
        }
        Err(r) => pagina_html("Internet para invitados", &format!("<p class=bad>{}</p>{volver}", html(&r.texto()))),
    }
}

// ---------------------------------------------------------------- panel de administración

pub fn resumen() -> String {
    let ahora = Instant::now();
    let s = e();
    if !s.cfg.activo {
        return "Apagado".into();
    }
    let n = s.accesos.iter().filter(|a| a.hasta > ahora).count();
    let base = format!("Encendido · {} de {} dispositivos · {} min", n, s.cfg.max_disp, s.cfg.minutos);
    if s.salida { base } else { format!("{base} · sin salida a internet") }
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1_000_000.0)
}

pub fn admin_html() -> String {
    let ahora = Instant::now();
    let s = e();
    let c = s.cfg.clone();
    let mut aviso = String::new();
    if !s.salida {
        aviso += "<p class=bad><b>Ahora el stick no tiene salida a internet</b> (ni SIM con datos ni internet compartido por el USB). Los invitados no verán la opción hasta que la haya.</p>";
    } else {
        aviso += &format!("<p class=mut>Salida a internet: <b>{}</b>.</p>", html(&salida_humana(&s.salida_dev)));
    }
    if !s.ap {
        aviso += "<p class=mut>Funciona solo mientras una vitrina con WiFi propio está al aire.</p>";
    }
    let mut lista = String::new();
    for a in s.accesos.iter().filter(|a| a.hasta > ahora) {
        let r = a.restante(ahora).as_secs();
        let quien = if a.nombre.is_empty() { a.ip.to_string() } else { format!("{} · {}", a.nombre, a.ip) };
        let tope = if c.tope_mb > 0 { format!(" de {} MB", c.tope_mb) } else { String::new() };
        lista += &format!(
            "<li><span>{}<small class=mut style=display:block>quedan {} min {:02} s · {}{}{}</small></span><form method=post action=/invitados/cortar onsubmit=\"return confirm('¿Cortar el internet de este dispositivo?')\"><input type=hidden name=ip value=\"{}\"><button class=link>Cortar</button></form></li>",
            html(&quien),
            r / 60,
            r % 60,
            mb(a.bytes()),
            tope,
            if a.mac.is_empty() { String::new() } else { format!(" · {}", html(&a.mac)) },
            a.ip
        );
    }
    let lista = if lista.is_empty() { "<p class=mut>Nadie tiene internet ahora.</p>".to_string() } else { format!("<ul>{lista}</ul>") };
    let usados = s.usados.len();
    drop(s);
    let chk = |b: bool| if b { " checked" } else { "" };
    format!(
        r#"{aviso}<form method=post action=/invitados>
<label class=chk style="gap:10px;align-items:center;margin-bottom:10px"><input type=checkbox name=activo value=si{act} style="width:20px;height:20px"><b>Ofrecer internet a los invitados</b></label>
<p class=mut style="margin-top:0">Se muestra una barra «Continuar con internet» en las páginas de la vitrina. Quien acepta navega con estos límites; luego vuelve al portal.</p>
<label>Minutos por dispositivo<input type=number name=minutos min=1 max=1440 value={min}></label>
<div class=row><label style=flex:1>Bajada máx. (Mbps)<input type=number name=baja min=0 max=1000 step=0.1 value={baja}></label><label style=flex:1>Subida máx. (Mbps)<input type=number name=sube min=0 max=1000 step=0.1 value={sube}></label></div>
<p class=mut style="margin:-6px 0 10px">0 = sin límite. Es un tope por dispositivo; el internet real puede ser menor.</p>
<label>Máximo de dispositivos a la vez<input type=number name=max min=1 max=200 value={max}></label>
<label>Tope de datos por dispositivo (MB, 0 = sin tope)<input type=number name=tope min=0 max=100000 value={tope}></label>
<label>Horas antes de que el mismo dispositivo pueda pedir otra vez (0 = cuando quiera)<input type=number name=espera min=0 max=720 value={espera}></label>
<label class=chk style="gap:10px;align-items:center;margin-bottom:12px"><input type=checkbox name=exigir value=si{exi} style="width:20px;height:20px">Pedir que vean la página antes de dar internet</label>
<input type=hidden name=enviado value=1><button>Guardar</button></form>
<h3>Dispositivos con internet</h3>{lista}
<form method=post action=/invitados/reiniciar><button class=sec type=submit>Olvidar quién ya usó su tiempo ({usados})</button></form>"#,
        act = chk(c.activo),
        min = c.minutos,
        baja = fmt_num(c.baja_mbps),
        sube = fmt_num(c.sube_mbps),
        max = c.max_disp,
        tope = c.tope_mb,
        espera = c.horas_espera,
        exi = chk(c.exigir_vista),
    )
}

fn salida_humana(dev: &str) -> String {
    match dev {
        "usb0" => "USB (laptop)".into(),
        d if d.starts_with("wwan") || d.starts_with("rmnet") => "LTE (SIM)".into(),
        d => d.into(),
    }
}

fn campo<'a>(f: &'a [(String, String)], k: &str) -> &'a str {
    f.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("")
}

/// POST /invitados[/accion] del panel.
pub fn admin_post(accion: &str, f: &[(String, String)]) -> String {
    match accion {
        "" => {
            let num = |k: &str, d: f64| campo(f, k).trim().replace(',', ".").parse::<f64>().ok().filter(|x| x.is_finite() && *x >= 0.0).unwrap_or(d);
            let ant = config();
            let n = Config {
                activo: campo(f, "activo") == "si",
                minutos: num("minutos", ant.minutos as f64) as u32,
                baja_mbps: num("baja", ant.baja_mbps),
                sube_mbps: num("sube", ant.sube_mbps),
                max_disp: num("max", ant.max_disp as f64) as u32,
                tope_mb: num("tope", ant.tope_mb as f64) as u64,
                exigir_vista: campo(f, "exigir") == "si",
                horas_espera: num("espera", ant.horas_espera as f64) as u32,
            }
            .normalizar();
            guardar_config(n.clone());
            if n.activo {
                format!("Internet para invitados encendido: {} min por dispositivo, {}.", n.minutos, html(&n.velocidad_txt()))
            } else {
                "Internet para invitados apagado.".into()
            }
        }
        "cortar" => match campo(f, "ip").parse::<Ipv4Addr>() {
            Ok(ip) if cortar(ip) => format!("Internet de {ip} cortado."),
            _ => "Ese dispositivo ya no tenía internet.".into(),
        },
        "reiniciar" => {
            e().usados.clear();
            "Listo: todos los dispositivos pueden volver a pedir internet.".into()
        }
        _ => "Acción desconocida.".into(),
    }
}

pub fn guardar_config(n: Config) {
    crate::save_file(CONF, &n.texto());
    let (ant, ap) = {
        let mut s = e();
        (std::mem::replace(&mut s.cfg, n.clone()), s.ap)
    };
    if !n.activo {
        cortar_todos();
    }
    if !ap {
        return;
    }
    if ant.baja_mbps != n.baja_mbps || ant.sube_mbps != n.sube_mbps || ant.activo != n.activo {
        recargar();
    }
    if n.activo {
        al_subir_ap();
    } else {
        quitar_ganchos();
        e().ganchos = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_ida_y_vuelta() {
        let c = Config { activo: true, minutos: 45, baja_mbps: 2.5, sube_mbps: 0.0, max_disp: 3, tope_mb: 200, exigir_vista: false, horas_espera: 0 };
        assert_eq!(Config::parse(&c.texto()), c);
        assert_eq!(Config::parse(""), Config::default());
        let r = Config::parse("minutos=0\nmax_disp=9999\nbaja_mbps=-3\n");
        assert_eq!((r.minutos, r.max_disp, r.baja_mbps), (1, 200, 0.0));
        assert_eq!(Config::bps(2.0), Some(250_000));
        assert_eq!(Config::bps(0.0), None);
    }

    #[test]
    fn inyecta_antes_de_body() {
        let b = inyectar(b"<html><body><p>hola</p></BODY></html>", "<i>x</i>");
        assert_eq!(b, b"<html><body><p>hola</p><i>x</i></BODY></html>");
        // el último </body> (puede haber uno dentro de un comentario o script antes)
        let b = inyectar(b"<script>'</body>'</script></body>", "X");
        assert_eq!(b, b"<script>'</body>'</script>X</body>");
        assert_eq!(inyectar(b"<p>sin cierre", "X"), b"<p>sin cierreX");
        assert_eq!(inyectar(b"<p>a</p></html>", "X"), b"<p>a</p>X</html>");
        // bytes no UTF-8 intactos
        assert_eq!(inyectar(b"\xff\xfe</body>", "X"), b"\xff\xfeX</body>");
        assert!(barra(30).contains("Continuar con internet (30 min)") && barra(30).contains(RUTA));
    }

    #[test]
    fn respuesta_no_html_intacta() {
        let r = GuestResp::bytes("image/png", vec![1, 2, 3]);
        let r = procesar_respuesta(r, "192.168.4.77".parse().unwrap(), "GET");
        assert_eq!(r.body, vec![1, 2, 3]);
        let mut r = GuestResp::html("<body></body>");
        r.headers.push(("X-Stick-Barra".into(), "no".into()));
        let r = procesar_respuesta(r, "192.168.4.78".parse().unwrap(), "GET");
        assert_eq!(r.body, b"<body></body>");
        assert!(r.headers.is_empty());
    }

    #[test]
    fn lee_elementos_nft() {
        let out = "table inet stickd_captive {\n\tset invitados {\n\t\ttype ipv4_addr\n\t\tflags timeout\n\t\tcounter\n\t\telements = { 192.168.4.50 counter packets 3 bytes 1234 timeout 30m expires 29m50s,\n\t\t\t     192.168.4.51 counter packets 0 bytes 0 timeout 1m30s expires 1m29s940ms }\n\t}\n}\n";
        assert_eq!(parse_elementos(out), vec![("192.168.4.50".parse().unwrap(), 1234), ("192.168.4.51".parse().unwrap(), 0)]);
        assert!(parse_elementos("table inet x {\n set invitados {\n type ipv4_addr\n }\n}").is_empty());
    }

    #[test]
    fn reglas_de_concesion() {
        let ahora = Instant::now();
        let mut s = Estado::nuevo(Config { activo: true, max_disp: 1, horas_espera: 1, ..Config::default() });
        let ip: Ipv4Addr = "192.168.4.20".parse().unwrap();
        assert_eq!(s.validar(ip, "aa", ahora), Err(Rechazo::SinAp));
        s.ap = true;
        assert_eq!(s.validar(ip, "aa", ahora), Err(Rechazo::SinSalida));
        s.salida = true;
        assert_eq!(s.validar(ip, "aa", ahora), Err(Rechazo::VerPrimero));
        s.vistos.push((ip, ahora));
        assert_eq!(s.validar(ip, "aa", ahora), Ok(()));
        let a = Acceso { ip, mac: "aa".into(), nombre: String::new(), hasta: ahora + Duration::from_secs(60), bytes_previos: 0, bytes_nft: 0 };
        s.registrar(a, "aa".into(), ahora);
        assert!(s.tiene(ip, ahora));
        assert!(!s.tiene(ip, ahora + Duration::from_secs(61)));
        // lleno para otro
        let otro: Ipv4Addr = "192.168.4.21".parse().unwrap();
        s.vistos.push((otro, ahora));
        assert_eq!(s.validar(otro, "bb", ahora), Err(Rechazo::Lleno));
        // el mismo dispositivo no repite antes de la espera
        s.accesos.clear();
        assert!(matches!(s.validar(ip, "aa", ahora), Err(Rechazo::YaUsado(_))));
        s.podar(ahora + Duration::from_secs(3601));
        assert_eq!(s.validar(ip, "aa", ahora + Duration::from_secs(3601)), Err(Rechazo::VerPrimero), "los vistos de hace 1 h también se olvidan");
        s.cfg.activo = false;
        assert_eq!(s.validar(ip, "aa", ahora), Err(Rechazo::Apagado));
    }

    #[test]
    fn sondas_con_exito() {
        assert_eq!(sonda_exito("connectivitycheck.gstatic.com", "/generate_204").status, 204);
        assert!(String::from_utf8(sonda_exito("captive.apple.com", "/hotspot-detect.html").body).unwrap().contains("Success"));
        assert_eq!(sonda_exito("www.msftconnecttest.com", "/connecttest.txt").body, b"Microsoft Connect Test");
    }
}
