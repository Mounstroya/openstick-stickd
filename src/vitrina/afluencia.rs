//! Módulo "Afluencia": estima cuántos celulares hay cerca contando sus *probe requests*
//! 802.11 (las tramas con que un teléfono pregunta "¿qué redes hay?"). `usa_ap()` = false:
//! el núcleo no levanta su AP ni portal; este módulo maneja el radio por su cuenta.
//!
//! # Captura (verificado en el UZ801, wcn36xx, kernel 6.6.0-msm8916)
//! - wlan0 en modo monitor "puro" NO sirve en este hardware: `iw` acepta el modo pero el
//!   firmware no entrega NINGUNA trama (0 tramas en 1/4/6/11, con y sin flags otherbss/control).
//! - Una interfaz monitor extra junto a wlan0 *managed* solo ve tráfico de su propia red
//!   (beacons/datos del AP de casa), ningún probe request.
//! - Con wlan0 en modo AP el firmware SÍ pasa al host los probe requests de cualquier equipo
//!   (los necesita para responder), y una interfaz monitor `mon0` sobre el mismo radio los ve
//!   con su RSSI. Por eso `start()` levanta un AP OCULTO propio con hostapd (SSID no anunciado,
//!   no responde probes, WPA2 con clave aleatoria que nadie conoce, sin IP ni DHCP; no es una
//!   conexión de NetworkManager, así que no aparece en la lista de redes) y captura en `mon0`.
//!   Limitación: solo se oye el canal del AP (configurable, 6 por defecto). Como los
//!   teléfonos barren todos los canales al buscar redes, casi todos se oyen igual; los que
//!   tienen pantalla apagada sondean con menos frecuencia.
//! - Socket AF_PACKET/SOCK_RAW/ETH_P_ALL ligado a mon0. De cada trama se parsea la cabecera
//!   radiotap (longitud, flags/FCS y potencia "dBm antenna signal" respetando la alineación
//!   de cada campo) y la cabecera 802.11; solo se usan *management / probe request*
//!   (FC = 0x40) y su dirección de origen (addr2).
//!
//! # Método (es una ESTIMACIÓN, no un conteo exacto)
//! - PRIVACIDAD: la MAC nunca se guarda ni se muestra. Se convierte al momento en un hash
//!   SipHash con una sal aleatoria de 16 bytes leída de /dev/urandom que vive SOLO en RAM y
//!   se cambia cada medianoche (hora local configurada), así que un mismo teléfono no se
//!   puede seguir entre días ni reconstruir desde los datos. En disco solo hay agregados.
//! - Ventanas de 5 min. En cada una se cuentan hashes distintos, separados en:
//!     * MAC fija (bit "localmente administrada" en 0): ≈ 1 dispositivo cada una.
//!     * MAC aleatoria (bit en 1): los teléfonos modernos cambian de MAC aleatoria muy a
//!       menudo, así que contar MACs aleatorias SOBREESTIMA. Para ellas además se calcula
//!       una "huella" de los elementos de información de la trama (tasas, capacidades
//!       HT/VHT/HE, extendidas, OUIs de vendor), que es igual para todos los equipos del
//!       mismo modelo/sistema: contar huellas distintas SUBESTIMA.
//!
//!   Estimación mostrada = fijas + huellas aleatorias (cota baja razonable); la cota alta
//!   (fijas + MACs aleatorias) también se muestra.
//! - Distancia aproximada por RSSI (el máximo visto de cada dispositivo en la ventana):
//!   cerca ≥ umbral "cerca", medio ≥ umbral "medio", lejos el resto; se ignora lo más débil
//!   que el umbral mínimo. Tramas sin dato de potencia cuentan como "lejos".
//! - "Ahora" = ventana en curso (o la anterior si la actual lleva menos de 2 min).
//! - Disco: un TSV por día en `data_dir("afluencia")` (≈15 KB/día), se borran los de más de
//!   30 días. RAM acotada: máx. `MAX_UNICOS` hashes por conjunto y 24 h de agregados.

use super::{data_dir, GuestReq, GuestResp, Vitrina};
use crate::util::{html, log, run};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hasher;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const IFACE: &str = "wlan0";
const VENTANA: i64 = 300;
const DIAS_GUARDADOS: i64 = 30;
const MAX_UNICOS: usize = 10_000;
const RADIO_LOCK: &str = "/run/stickd-radio.lock";
const MON: &str = "mon0";
const CONF_HOSTAPD: &str = "/run/stickd-afluencia-hostapd.conf";
const PID_HOSTAPD: &str = "/run/stickd-afluencia-hostapd.pid";
const ARPHRD_IEEE80211_RADIOTAP: u16 = 803;

/// Si es true no se toma /run/stickd-radio.lock (para el binario de prueba, que ya corre
/// dentro de `flock`).
pub static SIN_LOCK: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------------------
// Parser radiotap + 802.11
// ---------------------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub struct Radiotap {
    pub len: usize,
    pub rssi: Option<i8>,
    pub fcs: bool,
    pub bad_fcs: bool,
}

/// Parsea la cabecera radiotap. Solo interpreta los campos 0..=5 del primer bitmap
/// (TSFT, Flags, Rate, Channel, FHSS, dBm antenna signal), que siempre van primero.
pub fn parse_radiotap(b: &[u8]) -> Option<Radiotap> {
    if b.len() < 8 || b[0] != 0 {
        return None;
    }
    let len = u16::from_le_bytes([b[2], b[3]]) as usize;
    if len < 8 || len > b.len() {
        return None;
    }
    let present = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let mut off = 8;
    let mut p = present;
    while p & (1 << 31) != 0 {
        if off + 4 > len {
            return None;
        }
        p = u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]);
        off += 4;
    }
    // (alineación, tamaño) de cada campo, en orden de bit
    const CAMPOS: [(usize, usize); 6] = [(8, 8), (1, 1), (1, 1), (2, 4), (1, 2), (1, 1)];
    let (mut flags, mut rssi) = (0u8, None);
    for (bit, &(al, sz)) in CAMPOS.iter().enumerate() {
        if present & (1 << bit) == 0 {
            continue;
        }
        off = off.div_ceil(al) * al; // alineación relativa al inicio de la cabecera
        if off + sz > len {
            return None;
        }
        match bit {
            1 => flags = b[off],
            5 => rssi = Some(b[off] as i8).filter(|r| *r < 0),
            _ => {}
        }
        off += sz;
    }
    Some(Radiotap { len, rssi, fcs: flags & 0x10 != 0, bad_fcs: flags & 0x40 != 0 })
}

#[derive(Debug, PartialEq)]
pub struct Probe {
    pub mac: [u8; 6],
    pub rssi: Option<i8>,
    pub huella: u64,
}

impl Probe {
    pub fn aleatoria(&self) -> bool {
        self.mac[0] & 0x02 != 0
    }
}

/// Tipo/subtipo 802.11 de una trama con radiotap (para diagnóstico): (fc byte 0).
pub fn fc0(b: &[u8]) -> Option<u8> {
    let rt = parse_radiotap(b)?;
    b.get(rt.len).copied()
}

/// Devuelve el probe request si la trama lo es (FC 0x40: versión 0, management, subtipo 4).
pub fn parse_probe(b: &[u8]) -> Option<Probe> {
    let rt = parse_radiotap(b)?;
    if rt.bad_fcs {
        return None;
    }
    let mut f = &b[rt.len..];
    if rt.fcs {
        f = &f[..f.len().checked_sub(4)?];
    }
    if f.len() < 24 || f[0] != 0x40 {
        return None;
    }
    let mac: [u8; 6] = f[10..16].try_into().ok()?;
    if mac[0] & 0x01 != 0 || mac == [0; 6] {
        return None; // multicast / basura
    }
    Some(Probe { mac, rssi: rt.rssi, huella: huella_ies(&f[24..]) })
}

/// Huella del modelo de equipo a partir de los IEs (sin SSID, canal ni contenido de vendor).
fn huella_ies(mut ies: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    while ies.len() >= 2 {
        let (id, l) = (ies[0], ies[1] as usize);
        if ies.len() < 2 + l {
            break;
        }
        let v = &ies[2..2 + l];
        match id {
            0 | 3 => {}
            221 => {
                h.write_u8(id);
                h.write(&v[..v.len().min(4)]); // OUI + tipo
            }
            1 | 50 | 45 | 127 | 191 | 255 => {
                h.write_u8(id);
                h.write(v);
            }
            _ => h.write_u8(id),
        }
        ies = &ies[2 + l..];
    }
    h.finish()
}

fn hash_mac(sal: &[u8; 16], mac: &[u8; 6]) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(sal);
    h.write(mac);
    h.finish()
}

fn sal_nueva() -> [u8; 16] {
    let mut s = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut s)).is_err() {
        // respaldo muy improbable: reloj + dirección de pila
        let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        s = (n ^ (&s as *const _ as u128)).to_le_bytes();
    }
    s
}

// ---------------------------------------------------------------------------------------
// Agregados
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Registro {
    pub t: i64,
    pub fijas: u32,
    pub aleatorias: u32,
    pub huellas: u32,
    pub cerca: u32,
    pub medio: u32,
    pub lejos: u32,
    pub probes: u32,
}

impl Registro {
    pub fn estimacion(&self) -> u32 {
        self.fijas + self.huellas
    }
    fn tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            self.t, self.fijas, self.aleatorias, self.huellas, self.cerca, self.medio, self.lejos, self.probes
        )
    }
    fn de_tsv(l: &str) -> Option<Registro> {
        let v: Vec<i64> = l.split('\t').map(|x| x.trim().parse().ok()).collect::<Option<_>>()?;
        if v.len() < 8 {
            return None;
        }
        let u = |i: usize| v[i].clamp(0, u32::MAX as i64) as u32;
        Some(Registro { t: v[0], fijas: u(1), aleatorias: u(2), huellas: u(3), cerca: u(4), medio: u(5), lejos: u(6), probes: u(7) })
    }
}

#[derive(Default)]
struct Ventana {
    inicio: i64,
    fijas: HashMap<u64, i8>,
    aleatorias: HashSet<u64>,
    huellas: HashMap<u64, i8>,
    probes: u32,
}

fn max_rssi(m: &mut HashMap<u64, i8>, k: u64, r: i8) {
    if let Some(v) = m.get_mut(&k) {
        *v = (*v).max(r);
    } else if m.len() < MAX_UNICOS {
        m.insert(k, r);
    }
}

impl Ventana {
    fn agregar(&mut self, cfg: &Config, sal: &[u8; 16], p: &Probe) {
        let r = p.rssi.unwrap_or(i8::MIN);
        if p.rssi.is_some() && r < cfg.rssi_min {
            return;
        }
        self.probes = self.probes.saturating_add(1);
        let h = hash_mac(sal, &p.mac);
        if p.aleatoria() {
            if self.aleatorias.len() < MAX_UNICOS {
                self.aleatorias.insert(h);
            }
            let mut hh = DefaultHasher::new();
            hh.write(sal);
            hh.write_u64(p.huella);
            max_rssi(&mut self.huellas, hh.finish(), r);
        } else {
            max_rssi(&mut self.fijas, h, r);
        }
    }
    fn registro(&self, cfg: &Config) -> Registro {
        let mut r = Registro {
            t: self.inicio,
            fijas: self.fijas.len() as u32,
            aleatorias: self.aleatorias.len() as u32,
            huellas: self.huellas.len() as u32,
            probes: self.probes,
            ..Default::default()
        };
        for &s in self.fijas.values().chain(self.huellas.values()) {
            if s >= cfg.rssi_cerca {
                r.cerca += 1;
            } else if s >= cfg.rssi_medio {
                r.medio += 1;
            } else {
                r.lejos += 1;
            }
        }
        r
    }
}

// ---------------------------------------------------------------------------------------
// Configuración y tiempo local
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct Config {
    /// Canal del AP oculto (el monitor solo oye ese canal; ver doc del módulo)
    canal: u8,
    rssi_min: i8,
    rssi_cerca: i8,
    rssi_medio: i8,
    /// Desfase respecto a UTC en minutos (el stick suele estar en UTC).
    utc_min: i32,
}

impl Default for Config {
    fn default() -> Self {
        Config { canal: 6, rssi_min: -95, rssi_cerca: -60, rssi_medio: -75, utc_min: offset_sistema() }
    }
}

impl Config {
    fn texto(&self) -> String {
        format!(
            "canal={}\nrssi_min={}\nrssi_cerca={}\nrssi_medio={}\nutc_min={}\n",
            self.canal, self.rssi_min, self.rssi_cerca, self.rssi_medio, self.utc_min
        )
    }
    fn de_texto(s: &str) -> Config {
        let mut c = Config::default();
        for l in s.lines() {
            if let Some((k, v)) = l.split_once('=') {
                let _ = c.poner(k.trim(), v.trim());
            }
        }
        c
    }
    /// Aplica un campo validado; Err con el motivo si no es válido.
    fn poner(&mut self, k: &str, v: &str) -> Result<(), String> {
        let num = |lo: i64, hi: i64| v.parse::<i64>().ok().filter(|n| (lo..=hi).contains(n)).ok_or(format!("«{k}» fuera de rango ({lo}..{hi})"));
        match k {
            "canal" => self.canal = num(1, 13)? as u8,
            "rssi_min" => self.rssi_min = num(-120, -1)? as i8,
            "rssi_cerca" => self.rssi_cerca = num(-120, -1)? as i8,
            "rssi_medio" => self.rssi_medio = num(-120, -1)? as i8,
            "utc_min" => self.utc_min = num(-720, 840)? as i32,
            _ => {}
        }
        Ok(())
    }
}

fn offset_sistema() -> i32 {
    let t = ahora() as _; // tipo inferido (time_t; el alias está deprecado en musl)
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r solo escribe en `tm`.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    (tm.tm_gmtoff / 60) as i32
}

fn ahora() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Día local (días desde 1970-01-01) de un instante.
fn dia_local(t: i64, utc_min: i32) -> i64 {
    (t + utc_min as i64 * 60).div_euclid(86_400)
}

/// "AAAA-MM-DD" de un número de día (algoritmo civil de H. Hinnant).
fn fecha(dia: i64) -> String {
    let z = dia + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

fn hhmm(t: i64, utc_min: i32) -> String {
    let s = (t + utc_min as i64 * 60).rem_euclid(86_400);
    format!("{:02}:{:02}", s / 3600, s % 3600 / 60)
}

// ---------------------------------------------------------------------------------------
// Estado compartido
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct Diag {
    tramas: u64,
    mgmt: [u64; 16],
    otras: u64,
    canal: u8,
}

struct Estado {
    dir: PathBuf,
    cfg: Config,
    sal: [u8; 16],
    dia_sal: i64,
    actual: Ventana,
    /// Agregados de las últimas 24 h (para la gráfica y los totales de hoy)
    historia: VecDeque<Registro>,
    fijas_hoy: HashSet<u64>,
    activo: bool,
    error: String,
    diag: Diag,
}

impl Estado {
    fn nuevo(dir: PathBuf) -> Estado {
        let cfg = std::fs::read_to_string(dir.join("config.txt")).map(|s| Config::de_texto(&s)).unwrap_or_default();
        let t = ahora();
        let hoy = dia_local(t, cfg.utc_min);
        let mut e = Estado {
            dir,
            sal: sal_nueva(),
            dia_sal: hoy,
            actual: Ventana { inicio: t - t.rem_euclid(VENTANA), ..Default::default() },
            historia: VecDeque::new(),
            fijas_hoy: HashSet::new(),
            activo: false,
            error: String::new(),
            diag: Diag::default(),
            cfg,
        };
        e.cargar(t);
        e.podar();
        e
    }

    fn archivo(&self, dia: i64) -> PathBuf {
        self.dir.join(format!("{}.tsv", fecha(dia)))
    }

    fn cargar(&mut self, t: i64) {
        let hoy = dia_local(t, self.cfg.utc_min);
        let mut v: Vec<Registro> = [hoy - 1, hoy, hoy + 1] // +-1: por si cambió la zona horaria
            .iter()
            .filter_map(|d| std::fs::read_to_string(self.archivo(*d)).ok())
            .flat_map(|s| s.lines().filter_map(Registro::de_tsv).collect::<Vec<_>>())
            .filter(|r| r.t > t - 86_400 && r.t <= t)
            .collect();
        v.sort_by_key(|r| r.t);
        v.dedup_by_key(|r| r.t);
        self.historia = v.into_iter().collect();
    }

    /// Borra archivos de más de DIAS_GUARDADOS días.
    fn podar(&self) {
        let limite = format!("{}.tsv", fecha(dia_local(ahora(), self.cfg.utc_min) - DIAS_GUARDADOS));
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.ends_with(".tsv") && n.len() == 14 && n < limite {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    fn registrar(&mut self, p: &Probe) {
        if !p.aleatoria() && self.fijas_hoy.len() < MAX_UNICOS && p.rssi.is_none_or(|r| r >= self.cfg.rssi_min) {
            self.fijas_hoy.insert(hash_mac(&self.sal, &p.mac));
        }
        let (cfg, sal) = (&self.cfg, &self.sal);
        self.actual.agregar(cfg, sal, p);
    }

    /// Cierra la ventana en curso (si tiene algo) y la guarda.
    fn cerrar(&mut self) {
        if self.actual.probes > 0 {
            let r = self.actual.registro(&self.cfg);
            let ruta = self.archivo(dia_local(r.t, self.cfg.utc_min));
            use std::io::Write;
            let ok = std::fs::OpenOptions::new().create(true).append(true).open(&ruta).and_then(|mut f| f.write_all(r.tsv().as_bytes()));
            if let Err(e) = ok {
                log(&format!("afluencia: no pude guardar {}: {e}", ruta.display()));
            }
            self.historia.push_back(r);
        }
        while self.historia.front().is_some_and(|r| r.t <= ahora() - 86_400) || self.historia.len() > 300 {
            self.historia.pop_front();
        }
    }

    /// Cada segundo: cambio de ventana y rotación diaria de la sal.
    fn tick(&mut self, t: i64) {
        let inicio = t - t.rem_euclid(VENTANA);
        if inicio != self.actual.inicio {
            self.cerrar();
            self.actual = Ventana { inicio, ..Default::default() };
        }
        let hoy = dia_local(t, self.cfg.utc_min);
        if hoy != self.dia_sal {
            self.dia_sal = hoy;
            self.sal = sal_nueva();
            self.fijas_hoy.clear();
            self.podar();
        }
    }

    fn vivo(&self) -> Registro {
        self.actual.registro(&self.cfg)
    }

    /// Lo que se muestra como "ahora".
    fn ahora_reg(&self) -> Option<Registro> {
        if !self.activo {
            return None;
        }
        let v = self.vivo();
        let prev = self.historia.back().filter(|r| r.t == v.t - VENTANA);
        match prev {
            Some(p) if ahora() - v.t < 120 => Some(*p),
            _ => Some(v),
        }
    }

    /// Registros de hoy (hora local), incluida la ventana en curso.
    fn hoy(&self) -> Vec<Registro> {
        let hoy = dia_local(ahora(), self.cfg.utc_min);
        let mut v: Vec<Registro> = self.historia.iter().copied().filter(|r| dia_local(r.t, self.cfg.utc_min) == hoy).collect();
        if self.activo && self.actual.probes > 0 {
            v.push(self.vivo());
        }
        v
    }

    fn pico_hoy(&self) -> Option<Registro> {
        self.hoy().into_iter().filter(|r| r.estimacion() > 0).max_by_key(|r| (r.estimacion(), -r.t))
    }

    fn borrar(&mut self) {
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().ends_with(".tsv") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        self.historia.clear();
        self.fijas_hoy.clear();
        let inicio = self.actual.inicio;
        self.actual = Ventana { inicio, ..Default::default() };
        self.sal = sal_nueva();
        self.diag = Diag::default();
    }
}

// ---------------------------------------------------------------------------------------
// Radio y captura
// ---------------------------------------------------------------------------------------

/// Candado del radio compartido con otros procesos (solo durante las transiciones).
/// (el fd solo se guarda para que el candado dure hasta el `drop`)
struct RadioLock(#[allow(dead_code)] Option<OwnedFd>);

impl RadioLock {
    fn tomar() -> RadioLock {
        if SIN_LOCK.load(Ordering::Relaxed) {
            return RadioLock(None);
        }
        let c = c"/run/stickd-radio.lock";
        // SAFETY: open/flock sobre una ruta C válida; el fd queda dentro de OwnedFd.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC, 0o644) };
        if fd < 0 {
            return RadioLock(None);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        for _ in 0..100 {
            if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return RadioLock(Some(fd));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        log(&format!("afluencia: {RADIO_LOCK} ocupado 10 s; sigo sin él"));
        RadioLock(None)
    }
}

fn cmd(prog: &str, args: &[&str]) -> bool {
    let ok = run(prog, args).0;
    if !ok {
        log(&format!("afluencia: falló {prog} {}", args.join(" ")));
    }
    ok
}

fn tipo_wlan0() -> String {
    let (_, out) = run("iw", &["dev", IFACE, "info"]);
    out.lines().find_map(|l| l.trim().strip_prefix("type ").map(String::from)).unwrap_or_default()
}

fn phy() -> String {
    std::fs::read_to_string(format!("/sys/class/net/{IFACE}/phy80211/name")).map(|s| s.trim().to_string()).unwrap_or_else(|_| "phy0".into())
}

/// Mata un hostapd nuestro que haya quedado vivo (p.ej. si stickd murió sin stop()).
fn matar_hostapd_viejo() {
    let Ok(pid) = std::fs::read_to_string(PID_HOSTAPD) else { return };
    let Ok(pid) = pid.trim().parse::<i32>() else { return };
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if String::from_utf8_lossy(&cmdline).contains(CONF_HOSTAPD) {
        // SAFETY: kill(2) a un pid verificado como nuestro hostapd.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = std::fs::remove_file(PID_HOSTAPD);
}

/// AP oculto (sin SSID en beacons, no responde probes, WPA2 con clave aleatoria que nadie
/// conoce) + interfaz monitor `mon0` sobre el mismo radio.
fn levantar(canal: u8) -> Result<std::process::Child, String> {
    let _l = RadioLock::tomar();
    let _ = run("iw", &["dev", MON, "del"]);
    matar_hostapd_viejo();
    cmd("nmcli", &["dev", "set", IFACE, "managed", "no"]);
    cmd("ip", &["link", "set", IFACE, "down"]);
    let clave: String = sal_nueva().iter().map(|b| format!("{b:02x}")).collect(); // 32 caracteres (WPA: 8..63)
    let conf = format!(
        "interface={IFACE}\ndriver=nl80211\nssid=Afluencia\nhw_mode=g\nchannel={canal}\nignore_broadcast_ssid=1\nmax_num_sta=1\nwpa=2\nwpa_key_mgmt=WPA-PSK\nrsn_pairwise=CCMP\nwpa_passphrase={clave}\n"
    );
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(CONF_HOSTAPD).map_err(|e| format!("no pude escribir {CONF_HOSTAPD}: {e}"))?;
        f.write_all(conf.as_bytes()).map_err(|e| e.to_string())?;
    }
    let mut hijo = std::process::Command::new("hostapd")
        .args(["-P", PID_HOSTAPD, CONF_HOSTAPD])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("no pude lanzar hostapd: {e}"))?;
    let t = Instant::now();
    while tipo_wlan0() != "AP" {
        if t.elapsed() > Duration::from_secs(10) || hijo.try_wait().ok().flatten().is_some() {
            let _ = hijo.kill();
            let _ = hijo.wait();
            return Err("hostapd no levantó el AP".into());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    if !cmd("iw", &["phy", &phy(), "interface", "add", MON, "type", "monitor"]) || !cmd("ip", &["link", "set", MON, "up"]) {
        let _ = hijo.kill();
        let _ = hijo.wait();
        return Err("el driver no creó la interfaz monitor".into());
    }
    Ok(hijo)
}

/// Quita mon0 y el AP y devuelve wlan0 a NetworkManager como cliente (el núcleo reconecta).
pub fn restaurar(hijo: Option<std::process::Child>) {
    let _l = RadioLock::tomar();
    let _ = run("iw", &["dev", MON, "del"]);
    if let Some(mut h) = hijo {
        // SAFETY: SIGTERM a nuestro propio hijo para que hostapd limpie al salir.
        unsafe { libc::kill(h.id() as i32, libc::SIGTERM) };
        let t = Instant::now();
        while h.try_wait().ok().flatten().is_none() && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = h.kill();
        let _ = h.wait();
    }
    matar_hostapd_viejo();
    let _ = std::fs::remove_file(CONF_HOSTAPD);
    cmd("ip", &["link", "set", IFACE, "down"]);
    cmd("iw", &["dev", IFACE, "set", "type", "managed"]);
    cmd("ip", &["link", "set", IFACE, "up"]);
    cmd("nmcli", &["dev", "set", IFACE, "managed", "yes"]);
}

/// Socket AF_PACKET crudo ligado a `iface` con timeout de lectura de 200 ms.
pub fn abrir_socket(iface: &str) -> Result<OwnedFd, String> {
    let proto = (libc::ETH_P_ALL as u16).to_be();
    let nombre = std::ffi::CString::new(iface).map_err(|_| "nombre de interfaz inválido".to_string())?;
    // SAFETY: llamadas libc con estructuras inicializadas; el fd pasa a OwnedFd.
    unsafe {
        let fd = libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, proto as i32);
        if fd < 0 {
            return Err(format!("socket AF_PACKET: {}", std::io::Error::last_os_error()));
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let idx = libc::if_nametoindex(nombre.as_ptr());
        if idx == 0 {
            return Err(format!("no existe {iface}"));
        }
        let mut sll: libc::sockaddr_ll = std::mem::zeroed();
        sll.sll_family = libc::AF_PACKET as u16;
        sll.sll_protocol = proto;
        sll.sll_ifindex = idx as i32;
        if libc::bind(fd.as_raw_fd(), &sll as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_ll>() as u32) < 0 {
            return Err(format!("bind: {}", std::io::Error::last_os_error()));
        }
        let tv = libc::timeval { tv_sec: 0, tv_usec: 200_000 };
        libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const libc::c_void, std::mem::size_of::<libc::timeval>() as u32);
        Ok(fd)
    }
}

/// Lee una trama; None si se venció el timeout o no es 802.11+radiotap.
pub fn leer(fd: &OwnedFd, buf: &mut [u8]) -> Option<usize> {
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    let mut sl = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
    // SAFETY: buf y sll son válidos durante la llamada.
    let n = unsafe { libc::recvfrom(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0, &mut sll as *mut _ as *mut libc::sockaddr, &mut sl) };
    (n > 0 && sll.sll_hatype == ARPHRD_IEEE80211_RADIOTAP).then_some(n as usize)
}

struct Inner {
    st: Mutex<Estado>,
    corriendo: AtomicBool,
    hilo: Mutex<Option<JoinHandle<()>>>,
}

fn fallo(inner: &Inner, e: String) {
    log(&format!("afluencia: {e}"));
    let mut s = inner.st.lock().unwrap();
    s.error = e;
    s.activo = false;
}

fn captura(inner: Arc<Inner>) {
    let canal = inner.st.lock().unwrap().cfg.canal;
    let mut hijo = match levantar(canal) {
        Ok(h) => h,
        Err(e) => {
            fallo(&inner, e);
            restaurar(None);
            return;
        }
    };
    let fd = match abrir_socket(MON) {
        Ok(fd) => fd,
        Err(e) => {
            fallo(&inner, e);
            restaurar(Some(hijo));
            return;
        }
    };
    {
        let mut s = inner.st.lock().unwrap();
        s.activo = true;
        s.error.clear();
        s.diag.canal = canal;
    }
    log(&format!("afluencia: capturando probe requests en {MON} (AP oculto, canal {canal})"));
    let mut buf = vec![0u8; 4096];
    let mut seg = Instant::now();
    while inner.corriendo.load(Ordering::Relaxed) {
        if let Some(n) = leer(&fd, &mut buf) {
            let b = &buf[..n];
            let mut s = inner.st.lock().unwrap();
            s.diag.tramas += 1;
            match fc0(b) {
                Some(f) if f & 0x0c == 0 => s.diag.mgmt[(f >> 4) as usize] += 1,
                _ => s.diag.otras += 1,
            }
            if let Some(p) = parse_probe(b) {
                s.registrar(&p);
            }
        }
        if seg.elapsed() >= Duration::from_secs(1) {
            seg = Instant::now();
            inner.st.lock().unwrap().tick(ahora());
            if hijo.try_wait().ok().flatten().is_some() {
                fallo(&inner, "hostapd terminó inesperadamente".into());
                break;
            }
        }
    }
    drop(fd);
    {
        let mut s = inner.st.lock().unwrap();
        s.cerrar();
        let inicio = s.actual.inicio;
        s.actual = Ventana { inicio, ..Default::default() };
        s.activo = false;
    }
    restaurar(Some(hijo));
    log("afluencia: captura detenida, wlan0 vuelve a managed");
}

// ---------------------------------------------------------------------------------------
// Módulo
// ---------------------------------------------------------------------------------------

pub struct Afluencia {
    inner: Arc<Inner>,
}

impl Afluencia {
    pub fn new() -> Self {
        Self::con_dir(data_dir("afluencia"))
    }
    pub fn con_dir(dir: PathBuf) -> Self {
        Afluencia { inner: Arc::new(Inner { st: Mutex::new(Estado::nuevo(dir)), corriendo: AtomicBool::new(false), hilo: Mutex::new(None) }) }
    }

    /// Texto de diagnóstico (para el binario de prueba `examples/afluencia_prueba.rs`).
    #[allow(dead_code)]
    pub fn diagnostico(&self) -> String {
        let s = self.inner.st.lock().unwrap();
        let d = &s.diag;
        let v = s.vivo();
        format!(
            "tramas={} beacon={} probe_req={} probe_resp={} otras={} canal={} | ventana: fijas={} aleatorias={} huellas={} cerca/medio/lejos={}/{}/{} probes_validos={} | error={:?}",
            d.tramas, d.mgmt[8], d.mgmt[4], d.mgmt[5], d.otras, d.canal, v.fijas, v.aleatorias, v.huellas, v.cerca, v.medio, v.lejos, v.probes, s.error
        )
    }

    fn svg(&self, s: &Estado) -> String {
        let t = ahora();
        let off = s.cfg.utc_min as i64 * 60;
        let hora_ini = (t + off) - (t + off).rem_euclid(3600) - off; // inicio de la hora local actual (UTC)
        let mut vals = [0u32; 24];
        let regs = s.historia.iter().copied().chain((s.activo && s.actual.probes > 0).then(|| s.vivo()));
        for r in regs {
            let k = (r.t - (hora_ini - 23 * 3600)).div_euclid(3600);
            if (0..24).contains(&k) {
                vals[k as usize] = vals[k as usize].max(r.estimacion());
            }
        }
        let max = vals.iter().copied().max().unwrap_or(0).max(1);
        let tope = if max <= 5 { 5 } else { max.div_ceil(5) * 5 };
        let (x0, y0, w, h) = (28.0, 8.0, 448.0, 120.0);
        let bw = w / 24.0;
        let mut o = String::from(
            r#"<svg viewBox="0 0 480 150" width="100%" role="img" aria-label="Estimación de dispositivos por hora, últimas 24 h" style="max-width:560px;display:block;font:10px system-ui,sans-serif">"#
        );
        for (i, f) in [0.0, 0.5, 1.0].iter().enumerate() {
            let y = y0 + h - h * f;
            o += &format!(r#"<line x1="{x0}" x2="{}" y1="{y}" y2="{y}" stroke="currentColor" stroke-opacity="{}"/>"#, x0 + w, if i == 0 { 0.4 } else { 0.12 });
            o += &format!(r#"<text x="{}" y="{}" text-anchor="end" fill="currentColor" fill-opacity=".65">{}</text>"#, x0 - 4.0, y + 3.0, (tope as f64 * f).round());
        }
        for (i, &v) in vals.iter().enumerate() {
            let hh = h * v as f64 / tope as f64;
            let x = x0 + i as f64 * bw + 1.0;
            let ini = hora_ini - (23 - i as i64) * 3600;
            let etiqueta = format!("{}–{}: máx. ≈{v}", hhmm(ini, s.cfg.utc_min), hhmm(ini + 3600, s.cfg.utc_min));
            // zona de hover más grande que la barra
            o += &format!(r#"<g><title>{}</title><rect x="{x}" y="{y0}" width="{}" height="{h}" fill="transparent"/>"#, html(&etiqueta), bw - 2.0);
            if v > 0 {
                let r = (hh / 2.0).min(3.0);
                let (xb, yb, wb) = (x, y0 + h - hh, bw - 2.0);
                o += &format!(
                    r#"<path d="M{xb} {yt}V{ya}Q{xb} {yb} {xr} {yb}H{xs}Q{xe} {yb} {xe} {ya}V{yt}Z" style="fill:var(--ac,#4f8cff)"/>"#,
                    yt = y0 + h,
                    ya = yb + r,
                    xr = xb + r,
                    xs = xb + wb - r,
                    xe = xb + wb
                );
            }
            o += "</g>";
            if i % 3 == 2 || i == 23 {
                o += &format!(
                    r#"<text x="{}" y="{}" text-anchor="middle" fill="currentColor" fill-opacity=".65">{}</text>"#,
                    x + (bw - 2.0) / 2.0,
                    y0 + h + 14.0,
                    if i == 23 { "ahora".to_string() } else { hhmm(ini, s.cfg.utc_min)[..2].to_string() + "h" }
                );
            }
        }
        o + "</svg>"
    }
}

impl Default for Afluencia {
    fn default() -> Self {
        Self::new()
    }
}

impl Vitrina for Afluencia {
    fn id(&self) -> &'static str {
        "afluencia"
    }
    fn nombre(&self) -> &'static str {
        "Afluencia"
    }
    fn descripcion(&self) -> &'static str {
        "Estima cuántos celulares hay cerca (anónimo, radio en modo monitor)"
    }
    fn ssid_por_defecto(&self) -> String {
        "Afluencia".into()
    }
    fn usa_ap(&self) -> bool {
        false
    }
    fn guest(&self, _req: &GuestReq) -> GuestResp {
        GuestResp::not_found()
    }

    fn start(&self) {
        let mut h = self.inner.hilo.lock().unwrap();
        if h.as_ref().is_some_and(|j| !j.is_finished()) {
            return;
        }
        self.inner.corriendo.store(true, Ordering::Relaxed);
        let inner = self.inner.clone();
        *h = std::thread::Builder::new().name("afluencia".into()).spawn(move || captura(inner)).ok();
    }

    fn stop(&self) {
        self.inner.corriendo.store(false, Ordering::Relaxed);
        let h = self.inner.hilo.lock().unwrap().take();
        if let Some(j) = h {
            let _ = j.join(); // el hilo restaura wlan0 al salir
        }
    }

    fn resumen(&self) -> String {
        let s = self.inner.st.lock().unwrap();
        let pico = s.pico_hoy().map(|p| format!("pico {}", hhmm(p.t, s.cfg.utc_min))).unwrap_or_else(|| "sin pico hoy".into());
        match s.ahora_reg() {
            Some(r) => format!("≈{} dispositivos ahora · {pico}", r.estimacion()),
            None if !s.error.is_empty() => format!("error: {}", s.error),
            None => format!("detenido · {pico}"),
        }
    }

    fn admin_html(&self) -> String {
        let s = self.inner.st.lock().unwrap();
        let c = &s.cfg;
        let estado = if s.activo {
            format!("<span class=ok>capturando</span> · canal {}", s.diag.canal)
        } else if !s.error.is_empty() {
            format!("<span class=bad>error: {}</span>", html(&s.error))
        } else {
            "detenido (activa el modo Afluencia para capturar)".into()
        };
        let ahora_txt = match s.ahora_reg() {
            Some(r) => format!(
                "<b style=\"font-size:28px\">≈{}</b> dispositivos <small style=\"color:var(--mut)\">(rango {}–{})</small><br>cerca {} · medio {} · lejos {}<br><small style=\"color:var(--mut)\">MAC fija {} · aleatoria: {} huellas / {} MACs</small>",
                r.estimacion(),
                r.estimacion(),
                r.fijas + r.aleatorias,
                r.cerca,
                r.medio,
                r.lejos,
                r.fijas,
                r.huellas,
                r.aleatorias
            ),
            None => "—".into(),
        };
        let hoy = s.hoy();
        let (pico, prom) = match s.pico_hoy() {
            Some(p) => {
                let suma: u32 = hoy.iter().map(|r| r.estimacion()).sum();
                (format!("{} (≈{})", hhmm(p.t, c.utc_min), p.estimacion()), format!("{:.1}", suma as f64 / hoy.len().max(1) as f64))
            }
            None => ("—".into(), "—".into()),
        };
        let probes: u32 = hoy.iter().map(|r| r.probes).sum();
        let sg = if c.utc_min < 0 { '-' } else { '+' };
        format!(
            r#"<dl>
<dt>Estado</dt><dd>{estado}</dd>
<dt>Ahora</dt><dd>{ahora_txt}</dd>
<dt>Hora pico hoy</dt><dd>{pico}</dd>
<dt>Promedio hoy</dt><dd>{prom} por ventana de 5 min ({nv} ventanas)</dd>
<dt>Hoy</dt><dd>{fh} MAC fijas distintas · {probes} probe requests</dd>
<dt>Diagnóstico</dt><dd><small>{tramas} tramas · {pr} probe req · {bc} beacons · {ot} otras</small></dd>
</dl>
<h3 style="font-size:14px;margin:14px 0 4px">Últimas 24 h <small style="color:var(--mut);font-weight:normal">(máximo por hora de la estimación de 5 min, hora UTC{sg}{oh}:{om:02})</small></h3>
{svg}
<details><summary>Configuración</summary>
<form method=post action=/vitrina/afluencia/config>
<label>Canal de escucha (1–13; se aplica al reactivar el modo)<input type=text name=canal value="{canal}" inputmode=numeric></label>
<label>RSSI mínimo para contar (dBm)<input type=text name=rssi_min value="{rmin}"></label>
<label>«Cerca» desde (dBm)<input type=text name=rssi_cerca value="{rcerca}"></label>
<label>«Medio» desde (dBm)<input type=text name=rssi_medio value="{rmedio}"></label>
<label>Zona horaria (minutos respecto a UTC, p.ej. -360 = CDMX)<input type=text name=utc_min value="{utc}"></label>
<button>Guardar</button></form></details>
<details><summary>¿Cómo se cuenta?</summary><p style="color:var(--mut);font-size:14px">El stick levanta un WiFi oculto al que nadie puede conectarse y, con una interfaz en modo monitor, escucha los «probe requests» que los celulares envían al buscar redes (este chip no entrega tramas en modo monitor puro). Solo se oye el canal elegido, pero los teléfonos barren todos los canales al buscar. Las MAC nunca se guardan: se transforman en un código con una sal aleatoria que solo existe en memoria y cambia cada día. Muchos teléfonos usan MAC aleatorias que cambian seguido, así que el número es una estimación: MAC fijas + modelos distintos de equipos con MAC aleatoria (el rango alto cuenta cada MAC aleatoria). En disco solo quedan totales cada 5 min, 30 días. Mientras este modo está activo el WiFi cliente se apaga (internet solo por USB/LTE).</p></details>
<form method=post action=/vitrina/afluencia/borrar onsubmit="return confirm('¿Borrar todos los datos de afluencia?')"><button class=link>Borrar datos</button></form>"#,
            nv = hoy.len(),
            fh = s.fijas_hoy.len(),
            tramas = s.diag.tramas,
            pr = s.diag.mgmt[4],
            bc = s.diag.mgmt[8],
            ot = s.diag.otras,
            oh = c.utc_min.abs() / 60,
            om = c.utc_min.abs() % 60,
            svg = self.svg(&s),
            canal = c.canal,
            rmin = c.rssi_min,
            rcerca = c.rssi_cerca,
            rmedio = c.rssi_medio,
            utc = c.utc_min,
        )
    }

    fn admin_post(&self, accion: &str, form: &[(String, String)], _body: &[u8]) -> String {
        let mut s = self.inner.st.lock().unwrap();
        match accion {
            "config" => {
                let mut c = s.cfg.clone();
                for (k, v) in form {
                    if let Err(e) = c.poner(k, v.trim()) {
                        return html(&e);
                    }
                }
                if c.rssi_cerca <= c.rssi_medio {
                    return "«Cerca» debe ser mayor (menos negativo) que «medio».".into();
                }
                let _ = std::fs::write(s.dir.join("config.txt"), c.texto());
                s.cfg = c;
                "Configuración de afluencia guardada.".into()
            }
            "borrar" => {
                s.borrar();
                "Datos de afluencia borrados.".into()
            }
            _ => "Acción no válida.".into(),
        }
    }
}

// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Trama radiotap + probe request construida a mano.
    fn trama(radiotap: &[u8], fc: u8, mac: [u8; 6], ies: &[u8], fcs: bool) -> Vec<u8> {
        let mut v = radiotap.to_vec();
        v.extend([fc, 0x00, 0, 0]); // FC + duración
        v.extend([0xff; 6]); // addr1 broadcast
        v.extend(mac); // addr2
        v.extend([0xff; 6]); // addr3
        v.extend([0x10, 0x00]); // secuencia
        v.extend(ies);
        if fcs {
            v.extend([0xde, 0xad, 0xbe, 0xef]);
        }
        v
    }

    // Flags + Rate + Channel + dBm signal: flags@8, rate@9, channel@10 (alineado a 2), señal@14
    const RT_SIMPLE: [u8; 15] = [0, 0, 15, 0, 0x2e, 0, 0, 0, 0x00, 0x02, 0x85, 0x09, 0xa0, 0x00, 0xc4];
    const IES: [u8; 14] = [0, 0, 1, 4, 0x02, 0x04, 0x0b, 0x16, 50, 2, 0x0c, 0x12, 3, 1];

    #[test]
    fn radiotap_simple() {
        let r = parse_radiotap(&RT_SIMPLE).unwrap();
        assert_eq!(r, Radiotap { len: 15, rssi: Some(-60), fcs: false, bad_fcs: false });
    }

    #[test]
    fn radiotap_tsft_alineado_y_bitmap_extendido() {
        // present0 = TSFT|Flags|dBm + bit31 (ext); present1 = 0. TSFT va alineado a 8 → @16
        let mut rt = vec![0, 0, 0, 0, 0x23, 0, 0, 0x80, 0, 0, 0, 0];
        rt.extend([0; 4]); // relleno hasta 16
        rt.extend([1, 2, 3, 4, 5, 6, 7, 8]); // TSFT @16
        rt.push(0x10); // flags @24: FCS al final
        rt.push(0xb0u8); // señal @25 = -80
        let l = rt.len() as u8;
        rt[2] = l;
        let r = parse_radiotap(&rt).unwrap();
        assert_eq!(r, Radiotap { len: 26, rssi: Some(-80), fcs: true, bad_fcs: false });
        let mac = [0xda, 0xa1, 0x19, 1, 2, 3];
        let p = parse_probe(&trama(&rt, 0x40, mac, &IES, true)).unwrap();
        assert_eq!(p.mac, mac);
        assert_eq!(p.rssi, Some(-80));
        assert!(p.aleatoria());
    }

    #[test]
    fn probe_y_no_probe() {
        let mac = [0x3c, 0x5a, 0xb4, 9, 8, 7];
        let p = parse_probe(&trama(&RT_SIMPLE, 0x40, mac, &IES, false)).unwrap();
        assert_eq!(p.mac, mac);
        assert!(!p.aleatoria());
        assert_eq!(p.rssi, Some(-60));
        assert!(parse_probe(&trama(&RT_SIMPLE, 0x80, mac, &IES, false)).is_none()); // beacon
        assert!(parse_probe(&trama(&RT_SIMPLE, 0x50, mac, &IES, false)).is_none()); // probe resp
        assert!(parse_probe(&trama(&RT_SIMPLE, 0x40, [0x01, 0, 0x5e, 0, 0, 1], &IES, false)).is_none()); // multicast
        let mut mala = RT_SIMPLE;
        mala[8] = 0x40; // FCS incorrecta
        assert!(parse_probe(&trama(&mala, 0x40, mac, &IES, false)).is_none());
        assert!(parse_probe(&RT_SIMPLE).is_none()); // sin 802.11
        assert!(parse_probe(&[0, 0, 200, 0, 0, 0, 0, 0]).is_none()); // longitud imposible
        assert_eq!(fc0(&trama(&RT_SIMPLE, 0x80, mac, &IES, false)), Some(0x80));
    }

    #[test]
    fn huella_ignora_ssid_y_canal() {
        let a = [0, 3, b'a', b'b', b'c', 1, 2, 0x82, 0x84, 3, 1, 6];
        let b = [0, 0, 1, 2, 0x82, 0x84, 3, 1, 11];
        let c = [0, 0, 1, 2, 0x82, 0x8b, 3, 1, 11];
        assert_eq!(huella_ies(&a), huella_ies(&b));
        assert_ne!(huella_ies(&b), huella_ies(&c));
        huella_ies(&[1, 200, 3]); // IE truncado: no entra en pánico
    }

    #[test]
    fn ventana_cuenta_y_clasifica() {
        let cfg = Config { utc_min: 0, ..Default::default() };
        let sal = [7; 16];
        let mut v = Ventana::default();
        let p = |m: [u8; 6], r: i8, h: u64| Probe { mac: m, rssi: Some(r), huella: h };
        v.agregar(&cfg, &sal, &p([0x00, 1, 1, 1, 1, 1], -50, 1)); // fija, cerca
        v.agregar(&cfg, &sal, &p([0x00, 1, 1, 1, 1, 1], -80, 1)); // misma: queda el máximo
        v.agregar(&cfg, &sal, &p([0x00, 2, 2, 2, 2, 2], -70, 1)); // fija, medio
        v.agregar(&cfg, &sal, &p([0x02, 3, 3, 3, 3, 3], -85, 9)); // aleatoria, huella 9
        v.agregar(&cfg, &sal, &p([0x06, 4, 4, 4, 4, 4], -88, 9)); // aleatoria, misma huella
        v.agregar(&cfg, &sal, &p([0x0a, 5, 5, 5, 5, 5], -99, 5)); // bajo rssi_min: ignorada
        let r = v.registro(&cfg);
        assert_eq!((r.fijas, r.aleatorias, r.huellas, r.probes), (2, 2, 1, 5));
        assert_eq!((r.cerca, r.medio, r.lejos), (1, 1, 1));
        assert_eq!(r.estimacion(), 3);
        assert_eq!(Registro::de_tsv(r.tsv().trim()), Some(r));
    }

    #[test]
    fn fechas_y_horas() {
        assert_eq!(fecha(0), "1970-01-01");
        assert_eq!(fecha(dia_local(1_795_000_000, 0)), "2026-11-18");
        assert_eq!(dia_local(1_795_000_000, -360), dia_local(1_795_000_000 - 6 * 3600, 0));
        assert_eq!(hhmm(3600 * 13 + 60 * 5, -360), "07:05");
    }

    #[test]
    fn config_valida() {
        let mut c = Config::default();
        assert!(c.poner("canal", "11").is_ok());
        assert_eq!(c.canal, 11);
        assert!(c.poner("canal", "99").is_err());
        assert!(c.poner("rssi_cerca", "10").is_err());
        assert_eq!(Config::de_texto(&c.texto()), c);
    }

    #[test]
    fn persistencia_y_panel() {
        let dir = std::env::temp_dir().join(format!("afluencia-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = Afluencia::con_dir(dir.clone());
        {
            let mut s = a.inner.st.lock().unwrap();
            s.cfg.utc_min = 0;
            s.activo = true;
            let t = ahora();
            s.actual.inicio = t - t.rem_euclid(VENTANA) - VENTANA;
            s.registrar(&Probe { mac: [0, 1, 2, 3, 4, 5], rssi: Some(-55), huella: 0 });
            s.tick(t);
            assert_eq!(s.historia.len(), 1);
            // un archivo viejo que se debe podar
            std::fs::write(dir.join("2000-01-01.tsv"), "1\t1\t1\t1\t1\t1\t1\t1\n").unwrap();
            s.podar();
        }
        assert!(!dir.join("2000-01-01.tsv").exists());
        let html = a.admin_html();
        assert!(html.contains("<svg") && html.contains("Borrar datos"));
        assert!(!html.contains("00:01:02") && !html.contains("0:1:2:3"));
        assert!(a.resumen().starts_with('≈'));
        // recarga desde disco
        let b = Afluencia::con_dir(dir.clone());
        assert_eq!(b.inner.st.lock().unwrap().historia.len(), 1);
        assert!(b.admin_post("config", &[("rssi_cerca".into(), "-90".into())], &[]).contains("mayor"));
        assert!(b.admin_post("borrar", &[], &[]).contains("borrados"));
        assert!(b.inner.st.lock().unwrap().historia.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
