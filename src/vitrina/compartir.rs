//! Módulo "Compartir archivos": un "AirDrop universal" entre quienes estén en el WiFi del stick
//! (Android, iPhone, PC), sin instalar nada. Los archivos viajan por **WebRTC DataChannel P2P**
//! directo de un dispositivo al otro (cifrado DTLS); el stick SOLO hace de señalización:
//! empareja por un código fácil de dictar (`7-luna-perro-nube`, idea de croc) y pasa ofertas,
//! respuestas y candidatos ICE. No hay relay por el stick (decisión del dueño).
//!
//! El servidor de invitados atiende UNA petición a la vez, así que todo es polling corto con
//! respuestas inmediatas (≈0.7 s mientras se negocia, ≈2.5 s ya conectados).
//!
//! Rutas de invitado:
//!   GET  /                      la página (envía o recibe)
//!   GET  /r/<código>            la página, directo en "recibir" (lo que lleva el QR)
//!   GET  /qr.svg?c=<código>     QR de `http://192.168.4.1/r/<código>` (SVG hecho aquí)
//!   POST /api/crear             crea sala → {ok,c:código,s:sala,t:token}
//!   POST /api/unirse?c=         se une con el código → {ok,s,t}
//!   POST /api/enviar?s=&t=&tipo=&g=   cuerpo crudo: SDP (offer/answer) o candidato ICE
//!   GET  /api/recibir?s=&t=&desde=    mensajes pendientes → {ok,o,m:[[seq,tipo,g,datos]..]}
//!   POST /api/reporte?s=&t=&ev=inicio|fin&b=   estadísticas (las reporta quien recibe)
//!   POST /api/salir?s=&t=       cerrar (emisor: borra la sala; receptor: la deja libre)
//!
//! Salas: `s` es un id largo (128 bits) DERIVADO del código con una clave aleatoria de este
//! arranque (SipHash con llaves aleatorias), y cada lado tiene además su token aleatorio de
//! 128 bits (de /dev/urandom). Sin el token nadie lee ni escribe la señalización de otro.
//! Caducan a los 15 min sin receptor o sin actividad (máx. 12 h). Todo en RAM.
//!
//! Candidatos mDNS (`<uuid>.local`): Chrome/Safari ocultan la IP local tras un nombre mDNS si la
//! página no tiene permiso de cámara. Entre clientes del AP el mDNS multicast normalmente se
//! resuelve (el AP reenvía multicast entre estaciones), pero no está garantizado (Android con
//! datos móviles, AP con aislamiento, Chrome Android sin resolvedor…). Por eso, cuando llega un
//! candidato `host` con `.local`, el stick reenvía el original Y una copia con la IP que ve del
//! cliente (en el AP no hay NAT: es su IP real del WiFi). El puerto del candidato es real.
//!
//! Disco (`data_dir("compartir")`): solo `estadisticas.txt` ("AAAA-MM-DD iniciadas completas bytes").

use super::{GuestReq, GuestResp, Vitrina};
use crate::util::{form, html, json};
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod pagina;
mod palabras;

pub(crate) const IP_AP: &str = "192.168.4.1";
const ID: &str = "compartir";

const MAX_SALAS: usize = 40;
const SALAS_POR_IP: usize = 3;
const MAX_SDP: usize = 16 * 1024;
const MAX_CAND: usize = 1024;
const MAX_COLA: usize = 64;
const MAX_MSGS_SALA: u32 = 1500;
const MAX_BODY: usize = MAX_SDP + 1024;
const TTL_ESPERA: Duration = Duration::from_secs(15 * 60);
const TTL_INACTIVA: Duration = Duration::from_secs(15 * 60);
const TTL_MAX: Duration = Duration::from_secs(12 * 3600);
/// Un receptor que no pregunta en este tiempo se da por ido (otro puede tomar su lugar).
const RECEPTOR_VIVO: Duration = Duration::from_secs(20);
const VENTANA: Duration = Duration::from_secs(60);
const LIM_API: u32 = 400;
const LIM_UNIRSE: u32 = 15;
const LIM_CREAR: u32 = 10;
const MAX_IPS_LIM: usize = 1024;
const DIAS_GUARDADOS: usize = 90;
const MAX_BYTES_REPORTE: u64 = 1 << 42;

// ───────────────────────── estado ─────────────────────────

struct Msg {
    seq: u64,
    tipo: &'static str,
    g: u32,
    datos: String,
}

struct Lado {
    token: String,
    ultimo: Instant,
    cola: VecDeque<Msg>,
    seq: u64,
    ip: IpAddr,
}

impl Lado {
    fn nuevo(ip: IpAddr, ahora: Instant) -> Lado {
        Lado { token: aleatorio_hex(16), ultimo: ahora, cola: VecDeque::new(), seq: 0, ip }
    }
    fn meter(&mut self, tipo: &'static str, g: u32, datos: String) -> bool {
        if self.cola.len() >= MAX_COLA {
            return false;
        }
        self.seq += 1;
        self.cola.push_back(Msg { seq: self.seq, tipo, g, datos });
        true
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Fase {
    Esperando,
    Conectando,
    Transfiriendo,
    Terminada,
}

impl Fase {
    fn texto(self) -> &'static str {
        match self {
            Fase::Esperando => "esperando a quien recibe",
            Fase::Conectando => "emparejada",
            Fase::Transfiriendo => "transfiriendo",
            Fase::Terminada => "terminada",
        }
    }
}

struct Sala {
    creada: Instant,
    emisor: Lado,
    receptor: Option<Lado>,
    msgs: u32,
    fase: Fase,
    inicio: bool,
    fin: bool,
}

impl Sala {
    fn ultimo(&self) -> Instant {
        self.receptor.as_ref().map_or(self.emisor.ultimo, |r| r.ultimo.max(self.emisor.ultimo))
    }
    fn caducada(&self, ahora: Instant) -> bool {
        let edad = ahora.duration_since(self.creada);
        edad > TTL_MAX
            || (self.receptor.is_none() && self.fase == Fase::Esperando && edad > TTL_ESPERA)
            || ahora.duration_since(self.ultimo()) > TTL_INACTIVA
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Dia {
    iniciadas: u64,
    completas: u64,
    bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Lim {
    Api,
    Unirse,
    Crear,
}

struct Estado {
    salas: HashMap<String, Sala>,
    stats: BTreeMap<String, Dia>,
    limites: HashMap<(IpAddr, Lim), (Instant, u32)>,
}

pub struct Compartir {
    dir: PathBuf,
    k1: RandomState,
    k2: RandomState,
    st: Mutex<Estado>,
}

/// Errores que ve el invitado (código + texto).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fallo {
    Codigo,
    NoSala,
    Ocupada,
    Limite,
    Lleno,
    MuchasSalas,
    Invalido,
    Grande,
    Solo,
}

impl Fallo {
    fn codigo(self) -> &'static str {
        match self {
            Fallo::Codigo => "codigo",
            Fallo::NoSala => "nosala",
            Fallo::Ocupada => "ocupada",
            Fallo::Limite => "limite",
            Fallo::Lleno => "lleno",
            Fallo::MuchasSalas => "salas",
            Fallo::Invalido => "invalido",
            Fallo::Grande => "grande",
            Fallo::Solo => "solo",
        }
    }
    fn texto(self) -> &'static str {
        match self {
            Fallo::Codigo => "Ese código no existe o ya caducó. Revísalo: es un número y tres palabras, como 7-luna-perro-nube.",
            Fallo::NoSala => "La sesión terminó o caducó.",
            Fallo::Ocupada => "Alguien más ya está recibiendo con ese código.",
            Fallo::Limite => "Vas muy rápido. Espera un momento y vuelve a intentar.",
            Fallo::Lleno => "Demasiados mensajes pendientes. Espera un momento.",
            Fallo::MuchasSalas => "Hay demasiados envíos abiertos ahora. Cierra alguno o espera unos minutos.",
            Fallo::Invalido => "Petición no válida.",
            Fallo::Grande => "Mensaje demasiado grande.",
            Fallo::Solo => "Todavía no hay nadie del otro lado.",
        }
    }
    fn json(self) -> String {
        format!("{{\"ok\":0,\"e\":{},\"msg\":{}}}", json(self.codigo()), json(self.texto()))
    }
}

impl Default for Compartir {
    fn default() -> Self {
        Self::new()
    }
}

impl Compartir {
    pub fn new() -> Self {
        Self::with_dir(super::data_dir(ID))
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        let stats = leer_stats(&dir.join("estadisticas.txt"));
        Compartir {
            dir,
            k1: RandomState::new(),
            k2: RandomState::new(),
            st: Mutex::new(Estado { salas: HashMap::new(), stats, limites: HashMap::new() }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Estado> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Id de sala (128 bits en hex) derivado del código normalizado con llaves de este arranque.
    fn derivar(&self, codigo: &str) -> String {
        let h = |k: &RandomState| {
            let mut h = k.build_hasher();
            "compartir/sala".hash(&mut h);
            codigo.hash(&mut h);
            h.finish()
        };
        format!("{:016x}{:016x}", h(&self.k1), h(&self.k2))
    }

    fn limpiar(st: &mut Estado, ahora: Instant) {
        st.salas.retain(|_, s| !s.caducada(ahora));
    }

    /// Ventana fija de 60 s por (IP, tipo). `true` = permitido.
    fn permitir(st: &mut Estado, ip: IpAddr, lim: Lim, ahora: Instant) -> bool {
        if st.limites.len() >= MAX_IPS_LIM {
            st.limites.retain(|_, (t, _)| ahora.duration_since(*t) < VENTANA);
            if st.limites.len() >= MAX_IPS_LIM {
                return false;
            }
        }
        let tope = match lim {
            Lim::Api => LIM_API,
            Lim::Unirse => LIM_UNIRSE,
            Lim::Crear => LIM_CREAR,
        };
        let e = st.limites.entry((ip, lim)).or_insert((ahora, 0));
        if ahora.duration_since(e.0) >= VENTANA {
            *e = (ahora, 0);
        }
        if e.1 >= tope {
            return false;
        }
        e.1 += 1;
        true
    }

    fn crear(&self, ip: IpAddr) -> Result<String, Fallo> {
        let mut st = self.lock();
        let ahora = Instant::now();
        Self::limpiar(&mut st, ahora);
        if !Self::permitir(&mut st, ip, Lim::Crear, ahora) {
            return Err(Fallo::Limite);
        }
        if st.salas.len() >= MAX_SALAS || st.salas.values().filter(|s| s.emisor.ip == ip).count() >= SALAS_POR_IP {
            return Err(Fallo::MuchasSalas);
        }
        let (codigo, id) = loop {
            let c = palabras::generar();
            let id = self.derivar(&c);
            if !st.salas.contains_key(&id) {
                break (c, id);
            }
        };
        let emisor = Lado::nuevo(ip, ahora);
        let r = format!("{{\"ok\":1,\"c\":{},\"s\":{},\"t\":{}}}", json(&codigo), json(&id), json(&emisor.token));
        st.salas.insert(id, Sala { creada: ahora, emisor, receptor: None, msgs: 0, fase: Fase::Esperando, inicio: false, fin: false });
        Ok(r)
    }

    fn unirse(&self, ip: IpAddr, codigo: &str) -> Result<String, Fallo> {
        let mut st = self.lock();
        let ahora = Instant::now();
        Self::limpiar(&mut st, ahora);
        if !Self::permitir(&mut st, ip, Lim::Unirse, ahora) {
            return Err(Fallo::Limite);
        }
        let codigo = palabras::normalizar(codigo).ok_or(Fallo::Codigo)?;
        let id = self.derivar(&codigo);
        let sala = st.salas.get_mut(&id).ok_or(Fallo::Codigo)?;
        if sala.receptor.as_ref().is_some_and(|r| ahora.duration_since(r.ultimo) < RECEPTOR_VIVO) {
            return Err(Fallo::Ocupada);
        }
        let rec = Lado::nuevo(ip, ahora);
        let r = format!("{{\"ok\":1,\"s\":{},\"t\":{}}}", json(&id), json(&rec.token));
        sala.receptor = Some(rec);
        sala.fase = Fase::Conectando;
        // aviso al emisor: hay alguien (si ya había otro, el emisor reinicia la conexión)
        sala.emisor.cola.clear();
        sala.emisor.meter("hola", 0, String::new());
        Ok(r)
    }

    /// Busca la sala y dice qué lado es (true = emisor). Actualiza su actividad.
    fn lado<'a>(st: &'a mut Estado, s: &str, t: &str, ahora: Instant) -> Result<(&'a mut Sala, bool), Fallo> {
        let sala = st.salas.get_mut(s).ok_or(Fallo::NoSala)?;
        if !t.is_empty() && igual(&sala.emisor.token, t) {
            sala.emisor.ultimo = ahora;
            return Ok((sala, true));
        }
        if let Some(r) = sala.receptor.as_mut() {
            if !t.is_empty() && igual(&r.token, t) {
                r.ultimo = ahora;
                return Ok((sala, false));
            }
        }
        Err(Fallo::NoSala)
    }

    fn enviar(&self, ip: IpAddr, s: &str, t: &str, tipo: &str, g: &str, datos: &[u8]) -> Result<(), Fallo> {
        let mut st = self.lock();
        let ahora = Instant::now();
        if !Self::permitir(&mut st, ip, Lim::Api, ahora) {
            return Err(Fallo::Limite);
        }
        Self::limpiar(&mut st, ahora);
        let g: u32 = g.parse().map_err(|_| Fallo::Invalido)?;
        let datos = std::str::from_utf8(datos).map_err(|_| Fallo::Invalido)?;
        let (sala, es_emisor) = Self::lado(&mut st, s, t, ahora)?;
        let (tipo, salida): (&'static str, Vec<String>) = match tipo {
            "offer" | "answer" => {
                if (tipo == "offer") != es_emisor {
                    return Err(Fallo::Invalido);
                }
                if datos.len() > MAX_SDP {
                    return Err(Fallo::Grande);
                }
                let sdp = validar_sdp(datos, ip).ok_or(Fallo::Invalido)?;
                (if tipo == "offer" { "offer" } else { "answer" }, vec![sdp])
            }
            "ice" => {
                if datos.len() > MAX_CAND + 64 {
                    return Err(Fallo::Grande);
                }
                ("ice", validar_candidato(datos, ip).ok_or(Fallo::Invalido)?)
            }
            "re" if !es_emisor => ("re", vec![String::new()]),
            "bye" => ("bye", vec![String::new()]),
            _ => return Err(Fallo::Invalido),
        };
        if sala.msgs + salida.len() as u32 > MAX_MSGS_SALA {
            return Err(Fallo::Lleno);
        }
        let destino = if es_emisor { sala.receptor.as_mut().ok_or(Fallo::Solo)? } else { &mut sala.emisor };
        if destino.cola.len() + salida.len() > MAX_COLA {
            return Err(Fallo::Lleno);
        }
        let n = salida.len() as u32;
        for d in salida {
            destino.meter(tipo, g, d);
        }
        sala.msgs += n;
        Ok(())
    }

    fn recibir(&self, ip: IpAddr, s: &str, t: &str, desde: u64) -> Result<String, Fallo> {
        let mut st = self.lock();
        let ahora = Instant::now();
        if !Self::permitir(&mut st, ip, Lim::Api, ahora) {
            return Err(Fallo::Limite);
        }
        Self::limpiar(&mut st, ahora);
        let (sala, es_emisor) = Self::lado(&mut st, s, t, ahora)?;
        let otro = if es_emisor { sala.receptor.as_ref().is_some_and(|r| ahora.duration_since(r.ultimo) < RECEPTOR_VIVO) } else { true };
        let yo = if es_emisor { &mut sala.emisor } else { sala.receptor.as_mut().ok_or(Fallo::NoSala)? };
        while yo.cola.front().is_some_and(|m| m.seq <= desde) {
            yo.cola.pop_front();
        }
        let m: Vec<String> = yo.cola.iter().map(|m| format!("[{},{},{},{}]", m.seq, json(m.tipo), m.g, json(&m.datos))).collect();
        Ok(format!("{{\"ok\":1,\"o\":{},\"m\":[{}]}}", otro as u8, m.join(",")))
    }

    fn reporte(&self, ip: IpAddr, s: &str, t: &str, ev: &str, bytes: &str) -> Result<(), Fallo> {
        let mut st = self.lock();
        let ahora = Instant::now();
        if !Self::permitir(&mut st, ip, Lim::Api, ahora) {
            return Err(Fallo::Limite);
        }
        let (sala, es_emisor) = Self::lado(&mut st, s, t, ahora)?;
        if es_emisor {
            return Err(Fallo::Invalido); // solo reporta quien recibe: así no se cuenta doble
        }
        let bytes = bytes.parse::<u64>().unwrap_or(0).min(MAX_BYTES_REPORTE);
        let (ini, fin) = match ev {
            "inicio" if !sala.inicio => {
                sala.inicio = true;
                sala.fase = Fase::Transfiriendo;
                (1, 0)
            }
            "fin" if !sala.fin => {
                sala.fin = true;
                sala.fase = Fase::Terminada;
                (0, 1)
            }
            "inicio" | "fin" => return Ok(()),
            _ => return Err(Fallo::Invalido),
        };
        let d = st.stats.entry(hoy()).or_default();
        d.iniciadas += ini;
        d.completas += fin;
        if fin == 1 {
            d.bytes = d.bytes.saturating_add(bytes);
        }
        while st.stats.len() > DIAS_GUARDADOS {
            let k = st.stats.keys().next().cloned().unwrap_or_default();
            st.stats.remove(&k);
        }
        guardar_stats(&self.dir.join("estadisticas.txt"), &st.stats);
        Ok(())
    }

    fn salir(&self, s: &str, t: &str) -> Result<(), Fallo> {
        let mut st = self.lock();
        let ahora = Instant::now();
        let (sala, es_emisor) = Self::lado(&mut st, s, t, ahora)?;
        if es_emisor {
            st.salas.remove(s);
        } else {
            sala.receptor = None;
            if sala.fase != Fase::Terminada {
                sala.fase = Fase::Esperando;
            }
            sala.emisor.cola.clear();
            sala.emisor.meter("bye", 0, String::new());
        }
        Ok(())
    }

    fn hoy_dia(&self) -> Dia {
        self.lock().stats.get(&hoy()).copied().unwrap_or_default()
    }
}

fn json_resp(s: String) -> GuestResp {
    GuestResp::bytes("application/json; charset=utf-8", s.into_bytes())
}

fn ok_o(r: Result<(), Fallo>) -> GuestResp {
    json_resp(match r {
        Ok(()) => "{\"ok\":1}".into(),
        Err(f) => f.json(),
    })
}

fn es_ip(host: &str) -> bool {
    let h = host.rsplit_once(':').map_or(host, |(a, p)| if p.chars().all(|c| c.is_ascii_digit()) { a } else { host });
    h.trim_matches(['[', ']']).parse::<IpAddr>().is_ok()
}

impl Vitrina for Compartir {
    fn id(&self) -> &'static str {
        ID
    }
    fn nombre(&self) -> &'static str {
        "Compartir archivos"
    }
    fn descripcion(&self) -> &'static str {
        "Pasa archivos de cualquier tamaño entre celulares y PCs, directo y cifrado"
    }
    fn ssid_por_defecto(&self) -> String {
        "Compartir archivos".into()
    }
    fn max_body(&self) -> usize {
        MAX_BODY
    }

    fn guest(&self, req: &GuestReq) -> GuestResp {
        let q = form(req.query);
        let qv = |k: &str| q.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("");
        let ip = req.client_ip;
        let pagina_get = matches!(req.method, "GET" | "HEAD") && !req.path.starts_with("/api/") && req.path != "/qr.svg";
        // Con el DNS secuestrado puede llegar cualquier dominio: lo llevamos a la IP del stick,
        // que es lo que escribe la gente y lo que lleva el QR (y así el origen es siempre el mismo).
        if pagina_get && !es_ip(req.host) {
            let p = if req.path.starts_with("/r/") { req.path } else { "/" };
            return GuestResp::redirect(&format!("http://{IP_AP}{p}"));
        }
        let mut r = match (req.method, req.path) {
            ("GET" | "HEAD", "/") => self.pagina(req.user_agent, ""),
            ("GET" | "HEAD", p) if p.starts_with("/r/") => {
                let c = palabras::normalizar(&dec_path(&p[3..])).unwrap_or_default();
                self.pagina(req.user_agent, &c)
            }
            ("GET" | "HEAD", "/qr.svg") => match palabras::normalizar(qv("c")).and_then(|c| qr_svg(&format!("http://{IP_AP}/r/{c}"))) {
                Some(svg) => GuestResp::bytes("image/svg+xml", svg.into_bytes()),
                None => GuestResp::not_found(),
            },
            ("POST", "/api/crear") => json_resp(self.crear(ip).unwrap_or_else(Fallo::json)),
            ("POST", "/api/unirse") => json_resp(self.unirse(ip, qv("c")).unwrap_or_else(Fallo::json)),
            ("POST", "/api/enviar") => ok_o(self.enviar(ip, qv("s"), qv("t"), qv("tipo"), qv("g"), req.body)),
            ("GET", "/api/recibir") => json_resp(self.recibir(ip, qv("s"), qv("t"), qv("desde").parse().unwrap_or(0)).unwrap_or_else(Fallo::json)),
            ("POST", "/api/reporte") => ok_o(self.reporte(ip, qv("s"), qv("t"), qv("ev"), qv("b"))),
            ("POST", "/api/salir") => ok_o(self.salir(qv("s"), qv("t"))),
            ("GET" | "HEAD", "/favicon.ico" | "/robots.txt") => GuestResp::not_found(),
            ("GET" | "HEAD", _) => GuestResp::redirect("/"),
            _ => GuestResp::not_found(),
        };
        r.headers.push(("X-Content-Type-Options".into(), "nosniff".into()));
        r.headers.push(("Referrer-Policy".into(), "no-referrer".into()));
        r
    }

    fn admin_html(&self) -> String {
        let mut st = self.lock();
        let ahora = Instant::now();
        Self::limpiar(&mut st, ahora);
        let d = st.stats.get(&hoy()).copied().unwrap_or_default();
        let tot = st.stats.values().fold(Dia::default(), |a, b| Dia {
            iniciadas: a.iniciadas + b.iniciadas,
            completas: a.completas + b.completas,
            bytes: a.bytes.saturating_add(b.bytes),
        });
        let mut o = format!(
            "<p>Hoy: <b>{}</b> transferencias iniciadas · <b>{}</b> completas · {} pasados.</p>\
             <p class=mut>Últimos {} días: {} iniciadas · {} completas · {}. Los cuentan los propios teléfonos al terminar; \
             los archivos nunca pasan por el stick (van directo de un dispositivo al otro por WebRTC).</p>",
            d.iniciadas,
            d.completas,
            tam(d.bytes),
            st.stats.len().max(1),
            tot.iniciadas,
            tot.completas,
            tam(tot.bytes)
        );
        o.push_str(&format!("<h3>Sesiones activas ({})</h3>", st.salas.len()));
        if st.salas.is_empty() {
            o.push_str("<p class=mut>Ninguna.</p>");
        } else {
            let mut v: Vec<&Sala> = st.salas.values().collect();
            v.sort_by_key(|s| s.creada);
            o.push_str("<ul>");
            for (i, s) in v.iter().enumerate() {
                o.push_str(&format!(
                    "<li><span>Sesión {} · {} · abierta hace {} · {} mensajes de señalización</span></li>",
                    i + 1,
                    html(s.fase.texto()),
                    hace(ahora.duration_since(s.creada)),
                    s.msgs
                ));
            }
            o.push_str("</ul>");
        }
        o.push_str(
            "<form method=post action=/vitrina/compartir/vaciar>\
             <label class=chk><input type=checkbox name=confirmar value=si required> Sí, cerrar todas las sesiones</label>\
             <button class=link>Cerrar todas las sesiones</button></form>\
             <p class=mut>Las sesiones duran 15 min sin actividad y se guardan solo en memoria. Cerrarlas corta los envíos que estén negociando; \
             una transferencia ya conectada sigue (va directo entre los teléfonos).</p>",
        );
        o
    }

    fn admin_post(&self, accion: &str, form: &[(String, String)], _body: &[u8]) -> String {
        let campo = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("");
        match accion {
            "vaciar" => {
                if campo("confirmar") != "si" {
                    return "Marca la casilla para confirmar.".into();
                }
                let mut st = self.lock();
                let n = st.salas.len();
                st.salas.clear();
                format!("{n} {} cerradas.", if n == 1 { "sesión" } else { "sesiones" })
            }
            _ => "Acción desconocida.".into(),
        }
    }

    fn resumen(&self) -> String {
        let n = self.hoy_dia().iniciadas;
        format!("{n} {} hoy", if n == 1 { "transferencia" } else { "transferencias" })
    }
}

impl Compartir {
    fn pagina(&self, ua: &str, codigo: &str) -> GuestResp {
        let mut r = GuestResp::html(pagina::pagina(detectar(ua), codigo));
        r.headers.push((
            "Content-Security-Policy".into(),
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; \
             img-src 'self' data: blob:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
                .into(),
        ));
        r
    }
}

// ───────────────────────── validación SDP / ICE ─────────────────────────

fn texto_limpio(s: &str) -> bool {
    s.bytes().all(|b| b == b'\r' || b == b'\n' || (0x20..0x7f).contains(&b))
}

/// Candidato "candidate:..." válido. Si es `host` con dirección mDNS `.local`, devuelve
/// además una copia con la IP que el stick ve del cliente.
fn reescribir_candidato(c: &str, ip: IpAddr) -> Option<(String, Option<String>)> {
    if c.len() > MAX_CAND || !c.starts_with("candidate:") || !c.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return None;
    }
    let f: Vec<&str> = c.split(' ').collect();
    if f.len() < 8 || f[6] != "typ" || !f[1].chars().all(|x| x.is_ascii_digit()) || f[5].parse::<u16>().is_err() {
        return None;
    }
    let transp = f[2].to_ascii_lowercase();
    if transp != "udp" && transp != "tcp" {
        return None;
    }
    let mdns = f[4].len() <= 80 && f[4].to_ascii_lowercase().ends_with(".local") && f[7] == "host";
    let copia = (mdns && transp == "udp").then(|| {
        let mut g: Vec<String> = f.iter().map(|s| s.to_string()).collect();
        g[4] = ip.to_string();
        g.join(" ")
    });
    Some((c.to_string(), copia))
}

/// Cuerpo de "ice": `candidato \n sdpMid \n sdpMLineIndex`.
fn validar_candidato(datos: &str, ip: IpAddr) -> Option<Vec<String>> {
    let mut l = datos.split('\n');
    let c = l.next()?.trim_end_matches('\r');
    let mid = l.next().unwrap_or("").trim_end_matches('\r');
    let idx = l.next().unwrap_or("0").trim_end_matches('\r');
    if l.next().is_some() || mid.len() > 32 || !mid.chars().all(|x| x.is_ascii_alphanumeric() || x == '_' || x == '-') {
        return None;
    }
    let idx: u8 = idx.parse().ok()?;
    let (orig, copia) = reescribir_candidato(c, ip)?;
    let mut v = vec![format!("{orig}\n{mid}\n{idx}")];
    if let Some(cp) = copia {
        v.push(format!("{cp}\n{mid}\n{idx}"));
    }
    Some(v)
}

/// SDP con forma razonable; a cada `a=candidate` mDNS le agrega su copia con IP.
fn validar_sdp(sdp: &str, ip: IpAddr) -> Option<String> {
    if !sdp.starts_with("v=0") || !texto_limpio(sdp) {
        return None;
    }
    let mut o = String::with_capacity(sdp.len() + 256);
    for l in sdp.split('\n') {
        let l = l.trim_end_matches('\r');
        if l.is_empty() {
            continue;
        }
        if l.len() < 2 || l.as_bytes()[1] != b'=' {
            return None;
        }
        o.push_str(l);
        o.push_str("\r\n");
        if let Some(c) = l.strip_prefix("a=") {
            if c.starts_with("candidate:") {
                if let Some((_, Some(cp))) = reescribir_candidato(c, ip) {
                    o.push_str("a=");
                    o.push_str(&cp);
                    o.push_str("\r\n");
                }
            }
        }
    }
    (o.len() <= MAX_SDP + 1024).then_some(o)
}

// ───────────────────────── utilidades ─────────────────────────

/// Comparación de tokens en tiempo constante.
fn igual(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn aleatorio(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    let ok = fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_ok();
    if !ok {
        // respaldo (no debería pasar en Linux): SipHash con llaves aleatorias del proceso
        let rs = RandomState::new();
        for (i, ch) in b.chunks_mut(8).enumerate() {
            let mut h = rs.build_hasher();
            (i, SystemTime::now()).hash(&mut h);
            for (j, x) in ch.iter_mut().enumerate() {
                *x = (h.finish() >> (8 * j)) as u8;
            }
        }
    }
    b
}

fn aleatorio_hex(n: usize) -> String {
    aleatorio(n).iter().map(|b| format!("{b:02x}")).collect()
}

fn dec_path(s: &str) -> String {
    form(&format!("x={}", s.replace('+', "%2B"))).pop().map(|(_, v)| v).unwrap_or_default()
}

fn tam(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

fn hace(d: Duration) -> String {
    let m = d.as_secs() / 60;
    if m == 0 { format!("{} s", d.as_secs()) } else { format!("{m} min") }
}

fn hoy() -> String {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let t = ts as _; // time_t inferido (el alias está deprecado en musl)
    // SAFETY: localtime_r solo escribe en `tm`, que es nuestro.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return "0000-00-00".into();
    }
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

fn leer_stats(p: &Path) -> BTreeMap<String, Dia> {
    let mut m = BTreeMap::new();
    for l in fs::read_to_string(p).unwrap_or_default().lines() {
        let c: Vec<&str> = l.split(' ').collect();
        if c.len() == 4 && c[0].len() == 10 {
            if let (Ok(i), Ok(k), Ok(b)) = (c[1].parse(), c[2].parse(), c[3].parse()) {
                m.insert(c[0].to_string(), Dia { iniciadas: i, completas: k, bytes: b });
            }
        }
    }
    m
}

fn guardar_stats(p: &Path, m: &BTreeMap<String, Dia>) {
    let s: String = m.iter().map(|(k, d)| format!("{k} {} {} {}\n", d.iniciadas, d.completas, d.bytes)).collect();
    let tmp = p.with_extension("tmp");
    if fs::write(&tmp, s).is_ok() {
        let _ = fs::rename(&tmp, p);
    }
}

fn qr_svg(texto: &str) -> Option<String> {
    use qrcode::{Color, EcLevel, QrCode};
    let code = QrCode::with_error_correction_level(texto.as_bytes(), EcLevel::M).ok()?;
    let w = code.width();
    let colores = code.to_colors();
    let q = 4;
    let mut p = String::new();
    for y in 0..w {
        let mut x = 0;
        while x < w {
            if colores[y * w + x] == Color::Dark {
                let ini = x;
                while x < w && colores[y * w + x] == Color::Dark {
                    x += 1;
                }
                p.push_str(&format!("M{} {}h{}v1h-{}z", ini + q, y + q, x - ini, x - ini));
            } else {
                x += 1;
            }
        }
    }
    let t = w + 2 * q;
    Some(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {t} {t}" shape-rendering="crispEdges" role="img" aria-label="Código QR para recibir"><rect width="{t}" height="{t}" fill="#fff"/><path d="{p}" fill="#000"/></svg>"##
    ))
}

/// Mini-navegador de portal cautivo (mismo criterio que la tarjeta).
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) enum Nav {
    /// Captive Network Assistant de iOS/iPadOS/macOS: WebKit sin "Safari" en el UA.
    AppleCna,
    /// WebView de la app CaptivePortalLogin de Android (token "; wv)").
    AndroidWv,
    Normal,
}

fn detectar(ua: &str) -> Nav {
    if ua.contains("CaptiveNetworkSupport") {
        return Nav::AppleCna;
    }
    let apple = ["iPhone", "iPad", "iPod", "Macintosh"].iter().any(|x| ua.contains(x));
    if apple && !ua.contains("Android") && ua.contains("AppleWebKit") && !ua.contains("Safari") {
        return Nav::AppleCna;
    }
    if ua.contains("Android") && (ua.contains("; wv)") || ua.contains(" wv)")) {
        return Nav::AndroidWv;
    }
    Nav::Normal
}

#[cfg(test)]
mod tests;
