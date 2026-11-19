//! Módulo "Chat local": una sala de chat entre quienes están conectados al WiFi del stick,
//! sin internet. El dueño la modera desde el panel y puede escribir como "Anfitrión".
//!
//! El servidor de invitados atiende UNA petición a la vez en un solo hilo, así que aquí no hay
//! long-polling: el cliente pregunta cada ~2 s con `GET /mensajes?desde=<seq>&r=<rev>` y la
//! respuesta es inmediata y pequeña (normalmente `{"r":..,"s":..,"n":..,"ro":0,"m":[]}`).
//!
//! Rutas de invitado:
//!   GET  /                       página (funciona sin JS: formulario normal + enlace "Actualizar")
//!   GET  /mensajes?desde=&r=     JSON con los mensajes nuevos (o todos si cambió `rev`)
//!   GET  /apodo?a=               valida un apodo (JSON)
//!   POST /enviar[?js=1]          envía (form-urlencoded: apodo, texto). Con js=1 responde JSON;
//!                                sin él redirige a "/?a=<apodo>&r=<código>#fin"
//!
//! `rev` identifica "la versión" de la lista: cambia al reiniciar el daemon, al borrar o vaciar y
//! al cambiar la bienvenida. Si el cliente trae otra `rev`, recibe la lista completa y la repinta.
//!
//! TODO lo que llega de invitados es hostil:
//!   - apodo ≤ 24 y texto ≤ 500 caracteres, una sola línea, sin caracteres de control, bidi ni
//!     invisibles, con un tope a las marcas combinantes ("zalgo"); se escapa al pintarlo en el
//!     servidor y el cliente usa solo `textContent`
//!   - apodos reservados ("admin", "stick", "anfitrión", el nombre del dueño…), comparados tras
//!     normalizar mayúsculas, acentos, "leet" (4dm1n) y signos
//!   - 10 mensajes por minuto por IP, 60 por minuto en total, sin repetir el mismo texto en 30 s
//!   - IPs silenciadas por el dueño (en memoria: un reinicio las libera)
//!   - las IPs se guardan para moderar, pero nunca se mandan a los invitados
//!
//! Disco (`data_dir("chat")`):
//!   mensajes.txt   una línea por mensaje: seq \t ts \t ip \t anfitrión(0|1) \t apodo \t texto
//!                  (con \\ \t \n escapados). Se agrega al final; cuando pasa de 1000 líneas se
//!                  reescribe (atómicamente) con los últimos 500. Borrar/vaciar también reescribe.
//!   config         bienvenida=, dueno=, solo_lectura=0|1 (valores escapados igual)

use super::{GuestReq, GuestResp, Vitrina};
use crate::util::{form, html, json};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod pagina;

const MAX_APODO: usize = 24;
const MAX_TEXTO: usize = 500;
const MAX_BIENVENIDA: usize = 300;
const MAX_DUENO: usize = 40;
const MAX_GUARDADOS: usize = 500;
const MAX_LINEAS: usize = 2 * MAX_GUARDADOS;
/// Mensajes por respuesta de /mensajes (si hay más nuevos que esto, se manda la lista completa)
const MAX_RESPUESTA: usize = 100;
const PAGINA_SIN_JS: usize = 50;
const ADMIN_MAX: usize = 100;
const VENTANA: Duration = Duration::from_secs(60);
const POR_IP: usize = 10;
const GLOBAL: usize = 60;
const CONECTADO: Duration = Duration::from_secs(60);
const MAX_VISTOS: usize = 512;
const REPETIDO_SEG: u64 = 30;
const SILENCIO_MAX_MIN: u64 = 7 * 24 * 60;
const MAX_BODY: usize = 8 * 1024;
const ANFITRION: &str = "Anfitrión";

/// Nombres que nadie puede usar tal cual (tras normalizar).
const RESERVADOS: &[&str] = &[
    "admin", "administrador", "administradora", "administrator", "stick", "openstick", "anfitrion", "anfitriona",
    "host", "moderador", "moderadora", "mod", "sistema", "system", "root", "dueno", "duena", "owner",
];
/// Nombres que no pueden aparecer en ninguna parte del apodo.
const RESERVADOS_DENTRO: &[&str] = &["admin", "anfitrion", "moderador"];

#[derive(Clone, Debug, PartialEq)]
struct Msg {
    seq: u64,
    ts: u64,
    /// IP del invitado ("" si lo escribió el anfitrión). Solo para moderar.
    ip: String,
    host: bool,
    apodo: String,
    texto: String,
}

impl Msg {
    fn linea(&self) -> String {
        format!("{}\t{}\t{}\t{}\t{}\t{}\n", self.seq, self.ts, esc(&self.ip), self.host as u8, esc(&self.apodo), esc(&self.texto))
    }

    fn de_linea(l: &str) -> Option<Msg> {
        let c: Vec<&str> = l.split('\t').collect();
        if c.len() != 6 {
            return None;
        }
        let host = match c[3] {
            "1" => true,
            "0" => false,
            _ => return None,
        };
        let m = Msg {
            seq: c[0].parse().ok()?,
            ts: c[1].parse().ok()?,
            ip: unesc(c[2]),
            host,
            apodo: unesc(c[4]),
            texto: unesc(c[5]),
        };
        (m.seq > 0 && !m.texto.is_empty()).then_some(m)
    }

    /// `[seq,ts,"apodo","texto",tono,anfitrión]` — sin la IP.
    fn json(&self) -> String {
        format!("[{},{},{},{},{},{}]", self.seq, self.ts, json(&self.apodo), json(&self.texto), tono(&self.apodo), self.host as u8)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Config {
    bienvenida: String,
    dueno: String,
    solo_lectura: bool,
}

struct Estado {
    /// Ordenados por `seq` ascendente
    msgs: VecDeque<Msg>,
    /// Último `seq` asignado (nunca baja mientras corre el daemon)
    seq: u64,
    rev: u64,
    /// Líneas que tiene hoy `mensajes.txt` (para saber cuándo rotar)
    lineas: usize,
    cfg: Config,
    envios_ip: HashMap<IpAddr, VecDeque<Instant>>,
    envios: VecDeque<Instant>,
    vistos: HashMap<IpAddr, Instant>,
    silenciados: HashMap<IpAddr, Instant>,
}

impl Estado {
    fn visto(&mut self, ip: IpAddr, ahora: Instant) {
        if self.vistos.len() >= MAX_VISTOS && !self.vistos.contains_key(&ip) {
            self.vistos.retain(|_, t| ahora.duration_since(*t) < CONECTADO);
            if self.vistos.len() >= MAX_VISTOS {
                return;
            }
        }
        self.vistos.insert(ip, ahora);
    }

    fn conectados(&self, ahora: Instant) -> usize {
        self.vistos.values().filter(|t| ahora.duration_since(**t) < CONECTADO).count()
    }

    fn silenciado(&mut self, ip: IpAddr, ahora: Instant) -> Option<u64> {
        self.silenciados.retain(|_, hasta| *hasta > ahora);
        self.silenciados.get(&ip).map(|h| h.duration_since(ahora).as_secs().div_ceil(60).max(1))
    }
}

pub struct Chat {
    dir: PathBuf,
    st: Mutex<Estado>,
}

/// Por qué no se aceptó un envío o un apodo.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fallo {
    Vacio,
    Largo,
    SinApodo,
    ApodoLargo,
    Reservado,
    Limite,
    Flood,
    Repetido,
    Silenciado(u64),
    Lectura,
    Disco,
}

impl Fallo {
    fn codigo(self) -> &'static str {
        match self {
            Fallo::Vacio => "vacio",
            Fallo::Largo => "largo",
            Fallo::SinApodo => "apodo",
            Fallo::ApodoLargo => "apodolargo",
            Fallo::Reservado => "reservado",
            Fallo::Limite => "limite",
            Fallo::Flood => "flood",
            Fallo::Repetido => "repetido",
            Fallo::Silenciado(_) => "silenciado",
            Fallo::Lectura => "lectura",
            Fallo::Disco => "err",
        }
    }

    fn de_codigo(c: &str) -> Option<Fallo> {
        Some(match c {
            "vacio" => Fallo::Vacio,
            "largo" => Fallo::Largo,
            "apodo" => Fallo::SinApodo,
            "apodolargo" => Fallo::ApodoLargo,
            "reservado" => Fallo::Reservado,
            "limite" => Fallo::Limite,
            "flood" => Fallo::Flood,
            "repetido" => Fallo::Repetido,
            "silenciado" => Fallo::Silenciado(0),
            "lectura" => Fallo::Lectura,
            "err" => Fallo::Disco,
            _ => return None,
        })
    }

    fn texto(self) -> String {
        match self {
            Fallo::Vacio => "Escribe un mensaje.".into(),
            Fallo::Largo => format!("El mensaje es demasiado largo (máximo {MAX_TEXTO} caracteres)."),
            Fallo::SinApodo => "Elige un apodo para escribir.".into(),
            Fallo::ApodoLargo => format!("El apodo es demasiado largo (máximo {MAX_APODO} caracteres)."),
            Fallo::Reservado => "Ese apodo está reservado. Elige otro.".into(),
            Fallo::Limite => "Vas muy rápido: espera un momento antes de escribir otra vez.".into(),
            Fallo::Flood => "El chat está muy activo. Espera unos segundos.".into(),
            Fallo::Repetido => "Ya enviaste ese mismo mensaje.".into(),
            Fallo::Silenciado(0) => "El anfitrión te puso en pausa un rato.".into(),
            Fallo::Silenciado(1) => "El anfitrión te puso en pausa. Podrás escribir en 1 minuto.".into(),
            Fallo::Silenciado(m) => format!("El anfitrión te puso en pausa. Podrás escribir en {m} minutos."),
            Fallo::Lectura => "Ahora el chat es de solo lectura: solo escribe el anfitrión.".into(),
            Fallo::Disco => "No se pudo guardar el mensaje. Intenta de nuevo.".into(),
        }
    }
}

impl Chat {
    pub fn new() -> Self {
        Self::with_dir(super::data_dir("chat"))
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        let cfg = leer_config(&dir.join("config"));
        let (msgs, lineas) = cargar(&dir.join("mensajes.txt"));
        let seq = msgs.back().map_or(0, |m| m.seq);
        let rev = ahora_ts().saturating_mul(1000);
        let chat = Chat {
            dir,
            st: Mutex::new(Estado {
                msgs,
                seq,
                rev,
                lineas,
                cfg,
                envios_ip: HashMap::new(),
                envios: VecDeque::new(),
                vistos: HashMap::new(),
                silenciados: HashMap::new(),
            }),
        };
        {
            let mut st = chat.lock();
            if st.lineas > MAX_GUARDADOS {
                chat.reescribir(&mut st);
            }
        }
        chat
    }

    fn lock(&self) -> MutexGuard<'_, Estado> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ruta(&self) -> PathBuf {
        self.dir.join("mensajes.txt")
    }

    fn reescribir(&self, st: &mut Estado) -> bool {
        let s: String = st.msgs.iter().map(Msg::linea).collect();
        let ok = escribir_atomico(&self.ruta(), s.as_bytes());
        if ok {
            st.lineas = st.msgs.len();
        }
        ok
    }

    fn guardar_config(&self, c: &Config) -> bool {
        let s = format!("bienvenida={}\ndueno={}\nsolo_lectura={}\n", esc(&c.bienvenida), esc(&c.dueno), c.solo_lectura as u8);
        escribir_atomico(&self.dir.join("config"), s.as_bytes())
    }

    /// Agrega un mensaje ya validado: lo guarda en disco y en memoria.
    fn publicar(&self, st: &mut Estado, apodo: String, texto: String, ip: Option<IpAddr>, host: bool) -> Result<u64, Fallo> {
        let m = Msg { seq: st.seq + 1, ts: ahora_ts(), ip: ip.map_or(String::new(), |i| i.to_string()), host, apodo, texto };
        let ok = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(self.ruta())
            .and_then(|mut f| f.write_all(m.linea().as_bytes()))
            .is_ok();
        if !ok {
            return Err(Fallo::Disco);
        }
        st.seq = m.seq;
        st.lineas += 1;
        st.msgs.push_back(m);
        while st.msgs.len() > MAX_GUARDADOS {
            st.msgs.pop_front();
        }
        if st.lineas > MAX_LINEAS {
            self.reescribir(st);
        }
        Ok(st.seq)
    }

    /// Mensaje de un invitado: valida, aplica límites y lo publica.
    fn enviar(&self, ip: IpAddr, apodo: &str, texto: &str) -> Result<u64, Fallo> {
        let mut st = self.lock();
        let ahora = Instant::now();
        st.visto(ip, ahora);
        if st.cfg.solo_lectura {
            return Err(Fallo::Lectura);
        }
        if let Some(min) = st.silenciado(ip, ahora) {
            return Err(Fallo::Silenciado(min));
        }
        let apodo = validar_apodo(apodo, &st.cfg.dueno)?;
        let texto = limpiar(texto);
        if texto.is_empty() {
            return Err(Fallo::Vacio);
        }
        if texto.chars().count() > MAX_TEXTO {
            return Err(Fallo::Largo);
        }

        // límites
        while st.envios.front().is_some_and(|t| ahora.duration_since(*t) >= VENTANA) {
            st.envios.pop_front();
        }
        st.envios_ip.retain(|_, v| {
            while v.front().is_some_and(|t| ahora.duration_since(*t) >= VENTANA) {
                v.pop_front();
            }
            !v.is_empty()
        });
        if st.envios_ip.get(&ip).map_or(0, |v| v.len()) >= POR_IP {
            return Err(Fallo::Limite);
        }
        if st.envios.len() >= GLOBAL {
            return Err(Fallo::Flood);
        }
        let ip_s = ip.to_string();
        let ts = ahora_ts();
        if st.msgs.iter().rev().take(30).any(|m| m.ip == ip_s && m.texto == texto && ts.saturating_sub(m.ts) < REPETIDO_SEG) {
            return Err(Fallo::Repetido);
        }

        let seq = self.publicar(&mut st, apodo, texto, Some(ip), false)?;
        st.envios.push_back(ahora);
        st.envios_ip.entry(ip).or_default().push_back(ahora);
        Ok(seq)
    }

    /// JSON de `/mensajes`. Si el cliente trae otra `rev` (o un `desde` imposible) recibe todo.
    fn mensajes_json(&self, ip: IpAddr, desde: u64, rev: Option<u64>) -> String {
        let mut st = self.lock();
        let ahora = Instant::now();
        st.visto(ip, ahora);
        let nuevos = st.msgs.iter().rev().take_while(|m| m.seq > desde).count();
        let todo = rev != Some(st.rev) || desde > st.seq || nuevos > MAX_RESPUESTA;
        let desde_i = if todo { st.msgs.len().saturating_sub(MAX_RESPUESTA) } else { st.msgs.len() - nuevos };
        let m: Vec<String> = st.msgs.iter().skip(desde_i).map(Msg::json).collect();
        let mut s = format!("{{\"r\":{},\"s\":{},\"n\":{},\"ro\":{}", st.rev, st.seq, st.conectados(ahora), st.cfg.solo_lectura as u8);
        if todo {
            s.push_str(&format!(",\"t\":1,\"b\":{}", json(&st.cfg.bienvenida)));
        }
        s.push_str(",\"m\":[");
        s.push_str(&m.join(","));
        s.push_str("]}");
        s
    }

    fn pagina(&self, ip: IpAddr, apodo: &str, aviso: &str) -> String {
        let mut st = self.lock();
        let ahora = Instant::now();
        st.visto(ip, ahora);
        let ultimos: Vec<&Msg> = st.msgs.iter().skip(st.msgs.len().saturating_sub(PAGINA_SIN_JS)).collect();
        let aviso = Fallo::de_codigo(aviso).map(|f| f.texto()).unwrap_or_default();
        pagina::pagina(&pagina::Vista {
            msgs: &ultimos,
            apodo,
            aviso: &aviso,
            conectados: st.conectados(ahora),
            bienvenida: &st.cfg.bienvenida,
            solo_lectura: st.cfg.solo_lectura,
            rev: st.rev,
            seq: st.seq,
        })
    }

    fn exportar(&self) -> GuestResp {
        let st = self.lock();
        let mut s = String::from("Chat local\n\n");
        if !st.cfg.bienvenida.is_empty() {
            s.push_str(&format!("[Bienvenida] {}\n\n", st.cfg.bienvenida));
        }
        for m in &st.msgs {
            let quien = if m.host && m.apodo == ANFITRION {
                m.apodo.clone()
            } else if m.host {
                format!("{} ({ANFITRION})", m.apodo)
            } else { format!("{} [{}]", m.apodo, m.ip) };
            s.push_str(&format!("{} {}: {}\n", fecha(m.ts), quien, m.texto));
        }
        let mut r = GuestResp::bytes("text/plain; charset=utf-8", s.into_bytes());
        r.headers.push(("Content-Disposition".into(), "attachment; filename=\"chat.txt\"".into()));
        r.headers.push(("X-Content-Type-Options".into(), "nosniff".into()));
        r.headers.push(("Content-Security-Policy".into(), "sandbox; default-src 'none'".into()));
        r
    }
}

impl Default for Chat {
    fn default() -> Self {
        Self::new()
    }
}

fn json_resp(s: String) -> GuestResp {
    GuestResp::bytes("application/json; charset=utf-8", s.into_bytes())
}

fn fallo_json(f: Fallo) -> String {
    format!("{{\"ok\":0,\"e\":{},\"msg\":{}}}", json(f.codigo()), json(&f.texto()))
}

impl Vitrina for Chat {
    fn id(&self) -> &'static str {
        "chat"
    }
    fn nombre(&self) -> &'static str {
        "Chat local"
    }
    fn descripcion(&self) -> &'static str {
        "Chat entre quienes se conecten al WiFi, sin internet"
    }
    fn ssid_por_defecto(&self) -> String {
        "Chat local".into()
    }
    fn max_body(&self) -> usize {
        MAX_BODY
    }

    fn guest(&self, req: &GuestReq) -> GuestResp {
        let q = form(req.query);
        let qv = |k: &str| q.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("");
        let mut r = match (req.method, req.path) {
            ("GET" | "HEAD", "/") => {
                let mut r = GuestResp::html(self.pagina(req.client_ip, &limpiar(qv("a")), qv("r")));
                r.headers.push((
                    "Content-Security-Policy".into(),
                    "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; \
                     img-src 'self' data:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
                        .into(),
                ));
                r
            }
            ("GET" | "HEAD", "/mensajes") => {
                let desde = qv("desde").parse().unwrap_or(0);
                json_resp(self.mensajes_json(req.client_ip, desde, qv("r").parse().ok()))
            }
            ("GET" | "HEAD", "/apodo") => {
                let dueno = self.lock().cfg.dueno.clone();
                json_resp(match validar_apodo(qv("a"), &dueno) {
                    Ok(a) => format!("{{\"ok\":1,\"a\":{},\"h\":{}}}", json(&a), tono(&a)),
                    Err(f) => fallo_json(f),
                })
            }
            ("POST", "/enviar") => {
                let f = form(&String::from_utf8_lossy(req.body));
                let fv = |k: &str| f.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("");
                let res = self.enviar(req.client_ip, fv("apodo"), fv("texto"));
                if qv("js") == "1" {
                    json_resp(match res {
                        Ok(seq) => format!("{{\"ok\":1,\"s\":{seq}}}"),
                        Err(e) => fallo_json(e),
                    })
                } else {
                    let apodo = limpiar(fv("apodo"));
                    let codigo = res.err().map_or("ok", Fallo::codigo);
                    let mut r = GuestResp::redirect(&format!("/?a={}&r={codigo}#fin", urlenc(&apodo)));
                    if !apodo.is_empty() && apodo.chars().count() <= MAX_APODO {
                        r.headers.push(("Set-Cookie".into(), format!("apodo={}; Max-Age=31536000; Path=/; SameSite=Lax", urlenc(&apodo))));
                    }
                    r
                }
            }
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
        st.silenciados.retain(|_, h| *h > ahora);
        let hoy_n = mensajes_hoy(&st.msgs);
        let mut o = format!(
            "<p>{} mensajes guardados · {} hoy · {} conectados ahora</p>",
            st.msgs.len(),
            hoy_n,
            st.conectados(ahora)
        );
        o.push_str(
            "<form method=post action=/vitrina/chat/escribir><label>Escribir en el chat (sale con tu nombre y el distintivo <b>Anfitrión</b>)\
             <input type=text name=texto maxlength=500 required autocomplete=off></label><button>Enviar</button> \
             <button formaction=/vitrina/chat/actualizar formnovalidate class=sec>Actualizar conversación</button></form>",
        );

        o.push_str("<h3>Conversación reciente</h3>");
        if st.msgs.len() > ADMIN_MAX {
            o.push_str(&format!("<p class=mut>Los {ADMIN_MAX} más recientes de {} (el más nuevo arriba).</p>", st.msgs.len()));
        }
        o.push_str("<ul>");
        for m in st.msgs.iter().rev().take(ADMIN_MAX) {
            let quien = if m.host {
                format!("<b>{}</b> <span class=tag>Anfitrión</span>", html(&m.apodo))
            } else {
                format!("<b>{}</b> <small class=mut>{}</small>", html(&m.apodo), html(&m.ip))
            };
            let silenciar = if m.host || m.ip.is_empty() {
                String::new()
            } else {
                format!(
                    "<form method=post action=/vitrina/chat/silenciar style=\"display:inline\"><input type=hidden name=ip value=\"{}\">\
                     <button class=link name=minutos value=15>silenciar 15 min</button> <button class=link name=minutos value=1440>silenciar 1 día</button></form>",
                    html(&m.ip)
                )
            };
            o.push_str(&format!(
                "<li style=\"display:block\"><small class=mut>{}</small> {}\
                 <div style=\"white-space:pre-wrap;overflow-wrap:anywhere\">{}</div>\
                 <form method=post action=/vitrina/chat/borrar style=\"display:inline\"><input type=hidden name=seq value={}>\
                 <button class=link>borrar</button></form> {}</li>",
                fecha(m.ts),
                quien,
                html(&m.texto),
                m.seq,
                silenciar
            ));
        }
        if st.msgs.is_empty() {
            o.push_str("<li>Todavía no hay mensajes</li>");
        }
        o.push_str("</ul>");

        if !st.silenciados.is_empty() {
            o.push_str("<h3>Silenciados</h3><ul>");
            let mut s: Vec<(IpAddr, u64)> = st.silenciados.iter().map(|(ip, h)| (*ip, h.duration_since(ahora).as_secs().div_ceil(60))).collect();
            s.sort();
            for (ip, min) in s {
                o.push_str(&format!(
                    "<li><span>{} · quedan {} min</span><form method=post action=/vitrina/chat/quitar_silencio>\
                     <input type=hidden name=ip value=\"{}\"><button class=link>quitar</button></form></li>",
                    html(&ip.to_string()),
                    min,
                    html(&ip.to_string())
                ));
            }
            o.push_str("</ul>");
        }

        o.push_str(&format!(
            "<h3>Ajustes</h3><form method=post action=/vitrina/chat/lectura><input type=hidden name=valor value={}>\
             <p>Solo lectura (solo escribe el anfitrión): <b>{}</b> <button>{}</button></p></form>",
            if st.cfg.solo_lectura { "off" } else { "on" },
            if st.cfg.solo_lectura { "activado" } else { "desactivado" },
            if st.cfg.solo_lectura { "Desactivar" } else { "Activar" }
        ));
        o.push_str(&format!(
            "<form method=post action=/vitrina/chat/config>\
             <label>Mensaje fijado de bienvenida (opcional, máx. {MAX_BIENVENIDA})<textarea name=bienvenida maxlength={MAX_BIENVENIDA} rows=3>{}</textarea></label>\
             <label>Tu nombre (nadie podrá usarlo como apodo)<input type=text name=dueno maxlength={MAX_DUENO} value=\"{}\"></label>\
             <button>Guardar ajustes</button></form>",
            html(&st.cfg.bienvenida),
            html(&st.cfg.dueno)
        ));
        o.push_str(
            "<p><a href=/vitrina/chat/exportar>Descargar la conversación (.txt)</a></p>\
             <form method=post action=/vitrina/chat/vaciar>\
             <label class=chk><input type=checkbox name=confirmar value=si required> Sí, borrar todos los mensajes</label>\
             <button class=link>Vaciar el chat</button></form>",
        );
        o
    }

    fn admin_post(&self, accion: &str, form: &[(String, String)], _body: &[u8]) -> String {
        let campo = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("");
        let mut st = self.lock();
        match accion {
            "actualizar" => "Conversación actualizada.".into(),
            "escribir" => {
                let texto = limpiar(campo("texto"));
                if texto.is_empty() {
                    return "Escribe un mensaje.".into();
                }
                if texto.chars().count() > MAX_TEXTO {
                    return format!("El mensaje es demasiado largo (máximo {MAX_TEXTO} caracteres).");
                }
                let apodo = if st.cfg.dueno.is_empty() { ANFITRION.to_string() } else { st.cfg.dueno.clone() };
                match self.publicar(&mut st, apodo, texto, None, true) {
                    Ok(_) => "Mensaje enviado como Anfitrión.".into(),
                    Err(f) => f.texto(),
                }
            }
            "borrar" => {
                let Ok(seq) = campo("seq").parse::<u64>() else { return "Mensaje no válido.".into() };
                let Some(i) = st.msgs.iter().position(|m| m.seq == seq) else { return "Ese mensaje ya no existe.".into() };
                let m = st.msgs.remove(i);
                if !self.reescribir(&mut st) {
                    if let Some(m) = m {
                        st.msgs.insert(i, m);
                    }
                    return "No se pudo borrar (error de disco).".into();
                }
                st.rev += 1;
                "Mensaje borrado.".into()
            }
            "vaciar" => {
                if campo("confirmar") != "si" {
                    return "Marca la casilla para confirmar.".into();
                }
                let antes = std::mem::take(&mut st.msgs);
                if !self.reescribir(&mut st) {
                    st.msgs = antes;
                    return "No se pudo vaciar (error de disco).".into();
                }
                st.rev += 1;
                "Chat vaciado.".into()
            }
            "silenciar" => {
                let Ok(ip) = campo("ip").parse::<IpAddr>() else { return "IP no válida.".into() };
                let min = campo("minutos").parse::<u64>().unwrap_or(15).clamp(1, SILENCIO_MAX_MIN);
                st.silenciados.insert(ip, Instant::now() + Duration::from_secs(min * 60));
                format!("{ip} silenciada por {}.", duracion(min))
            }
            "quitar_silencio" => {
                let Ok(ip) = campo("ip").parse::<IpAddr>() else { return "IP no válida.".into() };
                if st.silenciados.remove(&ip).is_some() { format!("{ip} ya puede escribir.") } else { "Esa IP no estaba silenciada.".into() }
            }
            "lectura" => {
                let mut c = st.cfg.clone();
                c.solo_lectura = campo("valor") == "on";
                if !self.guardar_config(&c) {
                    return "No se pudo guardar la configuración.".into();
                }
                st.cfg = c;
                if st.cfg.solo_lectura { "Solo lectura activado: solo tú puedes escribir.".into() } else { "Solo lectura desactivado.".into() }
            }
            "config" => {
                let bienvenida = limpiar_multilinea(campo("bienvenida"));
                let dueno = limpiar(campo("dueno"));
                if bienvenida.chars().count() > MAX_BIENVENIDA {
                    return format!("La bienvenida es demasiado larga (máximo {MAX_BIENVENIDA} caracteres).");
                }
                if dueno.chars().count() > MAX_DUENO {
                    return format!("El nombre es demasiado largo (máximo {MAX_DUENO} caracteres).");
                }
                let c = Config { bienvenida, dueno, solo_lectura: st.cfg.solo_lectura };
                if !self.guardar_config(&c) {
                    return "No se pudo guardar la configuración.".into();
                }
                if c.bienvenida != st.cfg.bienvenida {
                    st.rev += 1;
                }
                st.cfg = c;
                "Ajustes guardados.".into()
            }
            _ => "Acción desconocida.".into(),
        }
    }

    fn admin_get(&self, ruta: &str) -> Option<GuestResp> {
        match ruta {
            "exportar" => Some(self.exportar()),
            _ => None,
        }
    }

    fn resumen(&self) -> String {
        let st = self.lock();
        let n = mensajes_hoy(&st.msgs);
        let c = st.conectados(Instant::now());
        format!("{} {} hoy · {} {}", n, if n == 1 { "mensaje" } else { "mensajes" }, c, if c == 1 { "conectado" } else { "conectados" })
    }
}

// ───────────────────────── utilidades ─────────────────────────

fn ahora_ts() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn local(ts: u64) -> Option<libc::tm> {
    let t = ts as _; // time_t inferido (el alias está deprecado en musl)
    // SAFETY: localtime_r solo escribe en `tm`, que es nuestro.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() { None } else { Some(tm) }
}

fn dia(ts: u64) -> Option<(i32, i32)> {
    local(ts).map(|tm| (tm.tm_year, tm.tm_yday))
}

fn fecha(ts: u64) -> String {
    match local(ts) {
        Some(tm) => format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min),
        None => ts.to_string(),
    }
}

/// "HH:MM" si es de hoy, "DD/MM HH:MM" si no (hora del stick; el JS la repinta con la del teléfono).
fn hora(ts: u64) -> String {
    match local(ts) {
        Some(tm) if dia(ts) == dia(ahora_ts()) => format!("{:02}:{:02}", tm.tm_hour, tm.tm_min),
        Some(tm) => format!("{:02}/{:02} {:02}:{:02}", tm.tm_mday, tm.tm_mon + 1, tm.tm_hour, tm.tm_min),
        None => String::new(),
    }
}

fn mensajes_hoy(msgs: &VecDeque<Msg>) -> usize {
    let hoy = dia(ahora_ts());
    msgs.iter().rev().take_while(|m| dia(m.ts) == hoy).count()
}

fn duracion(min: u64) -> String {
    match min {
        1 => "1 minuto".into(),
        m if m < 60 => format!("{m} minutos"),
        60 => "1 hora".into(),
        m if m < 1440 => format!("{} horas", m / 60),
        1440 => "1 día".into(),
        m => format!("{} días", m / 1440),
    }
}

/// Tono (0–359) estable para el color del apodo.
fn tono(apodo: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in apodo.to_lowercase().bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h % 360
}

fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200b}' | '\u{200c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}' | '\u{206a}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{061c}'
            | '\u{00ad}' | '\u{180e}' | '\u{115f}' | '\u{1160}' | '\u{3164}' | '\u{ffa0}')
}

fn combinante(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036f}' | '\u{0483}'..='\u{0489}' | '\u{1ab0}'..='\u{1aff}' | '\u{1dc0}'..='\u{1dff}' | '\u{20d0}'..='\u{20ff}' | '\u{fe20}'..='\u{fe2f}')
}

/// Una línea: quita control/bidi/invisibles (conserva ZWJ para emojis), saltos → espacio,
/// colapsa espacios y deja como mucho 2 marcas combinantes seguidas.
fn limpiar(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut comb = 0;
    let mut espacio = false;
    for c in s.chars() {
        let c = if matches!(c, '\n' | '\r' | '\t' | '\u{2028}' | '\u{2029}' | '\u{85}') { ' ' } else { c };
        if invisible(c) {
            continue;
        }
        if combinante(c) {
            comb += 1;
            if comb > 2 {
                continue;
            }
        } else {
            comb = 0;
        }
        if c.is_whitespace() {
            if !espacio && !o.is_empty() {
                o.push(' ');
            }
            espacio = true;
            continue;
        }
        espacio = false;
        o.push(c);
    }
    o.truncate(o.trim_end().len());
    o
}

/// Como `limpiar` pero conserva hasta 2 saltos de línea seguidos (bienvenida del dueño).
fn limpiar_multilinea(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let lineas: Vec<String> = s.split('\n').map(limpiar).collect();
    let mut o: Vec<String> = Vec::new();
    for l in lineas {
        if l.is_empty() && o.last().is_some_and(|p| p.is_empty()) {
            continue;
        }
        o.push(l);
    }
    o.join("\n").trim().to_string()
}

/// Minúsculas, sin acentos, "leet" a letras y solo [a-z0-9].
fn normalizar(s: &str) -> String {
    let mut o = String::new();
    for c in s.to_lowercase().chars() {
        let c = match c {
            'á' | 'à' | 'ä' | 'â' | 'ã' | 'å' | 'ā' | '4' | '@' | 'а' => 'a',
            'é' | 'è' | 'ë' | 'ê' | 'ē' | '3' | 'е' => 'e',
            'í' | 'ì' | 'ï' | 'î' | 'ī' | '1' | '!' | '|' | 'ı' | 'і' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' | 'õ' | 'ō' | '0' | 'о' => 'o',
            'ú' | 'ù' | 'ü' | 'û' | 'ū' => 'u',
            'ñ' => 'n',
            'ç' | 'с' => 'c',
            '5' | '$' => 's',
            '7' => 't',
            'р' => 'p',
            'х' => 'x',
            c => c,
        };
        if c.is_ascii_alphanumeric() {
            o.push(c);
        }
    }
    o
}

fn reservado(apodo: &str, dueno: &str) -> bool {
    let n = normalizar(apodo);
    if n.is_empty() {
        return false;
    }
    // "l" e "i" se confunden a simple vista (admln, stlck)
    let n_il = n.replace('l', "i");
    let choca = |p: &str| n == p || n_il == p.replace('l', "i");
    if RESERVADOS.iter().any(|r| choca(r)) || RESERVADOS_DENTRO.iter().any(|r| n.contains(r) || n_il.contains(&r.replace('l', "i"))) {
        return true;
    }
    let d = normalizar(dueno);
    if d.len() >= 3 && (n.contains(&d) || n_il.contains(&d.replace('l', "i"))) {
        return true;
    }
    dueno.split_whitespace().map(normalizar).filter(|p| p.len() >= 3).any(|p| n.contains(&p) || n_il.contains(&p.replace('l', "i")))
}

fn validar_apodo(raw: &str, dueno: &str) -> Result<String, Fallo> {
    let a = limpiar(raw);
    if a.is_empty() {
        return Err(Fallo::SinApodo);
    }
    if a.chars().count() > MAX_APODO {
        return Err(Fallo::ApodoLargo);
    }
    if reservado(&a, dueno) {
        return Err(Fallo::Reservado);
    }
    Ok(a)
}

fn urlenc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('t') => o.push('\t'),
            Some('n') => o.push('\n'),
            Some('r') => o.push('\r'),
            Some(c) => o.push(c),
            None => o.push('\\'),
        }
    }
    o
}

fn escribir_atomico(p: &Path, datos: &[u8]) -> bool {
    let tmp = p.with_extension("tmp");
    let ok = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(datos).and_then(|_| f.sync_data()))
        .and_then(|_| fs::rename(&tmp, p))
        .is_ok();
    if !ok {
        let _ = fs::remove_file(&tmp);
    }
    ok
}

fn leer_config(p: &Path) -> Config {
    let mut c = Config::default();
    if let Ok(s) = fs::read_to_string(p) {
        for l in s.lines() {
            let Some((k, v)) = l.split_once('=') else { continue };
            match k {
                "bienvenida" => c.bienvenida = unesc(v).chars().take(MAX_BIENVENIDA).collect(),
                "dueno" => c.dueno = unesc(v).chars().take(MAX_DUENO).collect(),
                "solo_lectura" => c.solo_lectura = v == "1",
                _ => {}
            }
        }
    }
    c
}

/// Lee `mensajes.txt`: (últimos MAX_GUARDADOS mensajes, líneas del archivo).
fn cargar(p: &Path) -> (VecDeque<Msg>, usize) {
    let mut msgs = VecDeque::new();
    let mut lineas = 0;
    let Ok(s) = fs::read_to_string(p) else { return (msgs, 0) };
    let mut ultimo = 0;
    for l in s.lines() {
        lineas += 1;
        // seq estrictamente creciente: una línea corrupta o repetida no rompe el orden
        if let Some(m) = Msg::de_linea(l).filter(|m| m.seq > ultimo) {
            ultimo = m.seq;
            msgs.push_back(m);
            if msgs.len() > MAX_GUARDADOS {
                msgs.pop_front();
            }
        }
    }
    (msgs, lineas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn tmp(n: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("stickd-chat-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 4, n))
    }

    fn get<'a>(path: &'a str, query: &'a str, ip_: IpAddr) -> GuestReq<'a> {
        GuestReq { method: "GET", path, query, host: "192.168.4.1", user_agent: "", content_type: "", body: b"", client_ip: ip_ }
    }

    fn post<'a>(path: &'a str, query: &'a str, body: &'a [u8], ip_: IpAddr) -> GuestReq<'a> {
        GuestReq {
            method: "POST",
            path,
            query,
            host: "192.168.4.1",
            user_agent: "",
            content_type: "application/x-www-form-urlencoded",
            body,
            client_ip: ip_,
        }
    }

    fn texto(r: &GuestResp) -> String {
        String::from_utf8(r.body.clone()).unwrap()
    }

    fn kv(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn limpia_texto_hostil() {
        assert_eq!(limpiar("  hola\u{202e}  mundo\n\t!\u{0007}\u{200b} "), "hola mundo !");
        // ZWJ se queda (emojis compuestos)
        assert_eq!(limpiar("👨\u{200d}👩"), "👨\u{200d}👩");
        // zalgo: máximo 2 marcas combinantes seguidas
        assert_eq!(limpiar("a\u{0301}\u{0301}\u{0301}\u{0301}b").chars().count(), 4);
        assert_eq!(limpiar_multilinea("hola\r\n\r\n\r\n\u{202e}chau"), "hola\n\nchau");
    }

    #[test]
    fn apodos_reservados() {
        for a in ["admin", "Admin", "ADMIN 1", "4dm1n", "Ádmín", "a.d.m.i.n", "admln", "Stick", "st1ck", "Anfitrión", "el_admin_real", "Yael", "yaél_m", "montoya"] {
            assert!(reservado(a, "Yael Montoya"), "{a} debería estar reservado");
        }
        for a in ["Ana", "Pepe 🌮", "Rafael", "🦊", "lipstick fan"] {
            assert!(!reservado(a, "Yael Montoya"), "{a} debería poderse usar");
        }
        assert_eq!(validar_apodo("  ", ""), Err(Fallo::SinApodo));
        assert_eq!(validar_apodo(&"x".repeat(25), ""), Err(Fallo::ApodoLargo));
        assert_eq!(validar_apodo(&"ñ".repeat(24), "").unwrap().chars().count(), 24);
    }

    #[test]
    fn linea_ida_y_vuelta() {
        let m = Msg { seq: 7, ts: 123, ip: "192.168.4.9".into(), host: false, apodo: "a\\b\tc".into(), texto: "x\ny\\n".into() };
        assert_eq!(Msg::de_linea(m.linea().trim_end_matches('\n')), Some(m));
        assert_eq!(Msg::de_linea("basura"), None);
        assert_eq!(Msg::de_linea("1\t2\t\t5\ta\tb"), None);
    }

    #[test]
    fn enviar_y_recibir_json() {
        let d = tmp("json");
        let c = Chat::with_dir(d.clone());
        let r = c.guest(&post("/enviar", "js=1", b"apodo=Ana&texto=hola+%3Cb%3E%22", ip(10)));
        assert!(texto(&r).starts_with("{\"ok\":1"), "{}", texto(&r));
        let rev = c.lock().rev;
        let r = c.guest(&get("/mensajes", "desde=0", ip(11)));
        let s = texto(&r);
        assert!(r.content_type.starts_with("application/json"));
        assert!(s.contains("\"t\":1") && s.contains("\"Ana\"") && s.contains("\"hola <b>\\\"\""), "{s}");
        assert!(!s.contains("192.168"), "no debe filtrar IPs: {s}");
        assert!(s.contains("\"n\":2"), "2 conectados: {s}");
        // incremental: con la rev correcta y al día, lista vacía
        let q = format!("desde=1&r={rev}");
        let s = texto(&c.guest(&get("/mensajes", &q, ip(11))));
        assert!(s.ends_with("\"m\":[]}") && !s.contains("\"t\":1"), "{s}");
        c.enviar(ip(11), "Beto", "qué tal").unwrap();
        let s = texto(&c.guest(&get("/mensajes", &q, ip(11))));
        assert!(s.contains("Beto") && !s.contains("Ana"), "{s}");
        // rev vieja → todo
        let s = texto(&c.guest(&get("/mensajes", "desde=2&r=5", ip(11))));
        assert!(s.contains("\"t\":1") && s.contains("Ana") && s.contains("Beto"), "{s}");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn pagina_escapa_y_funciona_sin_js() {
        let d = tmp("html");
        let c = Chat::with_dir(d.clone());
        let r = c.guest(&post("/enviar", "", b"apodo=%3Cimg+src%3Dx%3E&texto=%3Cscript%3Ealert(1)%3C%2Fscript%3E", ip(10)));
        assert_eq!(r.status, 302);
        let loc = &r.headers.iter().find(|(k, _)| k == "Location").unwrap().1;
        assert!(loc.starts_with("/?a=%3Cimg%20src%3Dx%3E&r=ok"), "{loc}");
        assert!(r.headers.iter().any(|(k, v)| k == "Set-Cookie" && v.starts_with("apodo=%3Cimg")));
        let s = texto(&c.guest(&get("/", "a=%3Cimg+src%3Dx%3E&r=limite", ip(10))));
        assert!(s.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(s.contains("value=\"&lt;img src=x&gt;\""));
        assert!(!s.contains("<img src=x>") && !s.contains("<script>alert"));
        assert!(s.contains("Vas muy rápido"));
        assert!(s.contains("<form") && s.contains("action=/enviar"));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn limites() {
        let d = tmp("limites");
        let c = Chat::with_dir(d.clone());
        for i in 0..POR_IP {
            c.enviar(ip(10), "Ana", &format!("m{i}")).unwrap();
        }
        assert_eq!(c.enviar(ip(10), "Ana", "otro"), Err(Fallo::Limite));
        assert_eq!(c.enviar(ip(11), "Beto", "hola"), Ok(POR_IP as u64 + 1));
        assert_eq!(c.enviar(ip(11), "Beto", "hola"), Err(Fallo::Repetido));
        assert_eq!(c.enviar(ip(11), "admin", "hola"), Err(Fallo::Reservado));
        assert_eq!(c.enviar(ip(11), "Beto", &"x".repeat(501)), Err(Fallo::Largo));
        assert_eq!(c.enviar(ip(11), "Beto", " \u{202e} "), Err(Fallo::Vacio));
        // anti-flood global
        let mut n = c.lock().envios.len();
        let mut k = 20;
        while n < GLOBAL {
            c.enviar(ip(k), "Otro", "x").unwrap();
            n += 1;
            k += 1;
        }
        assert_eq!(c.enviar(ip(250), "Nuevo", "x"), Err(Fallo::Flood));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn moderacion() {
        let d = tmp("moder");
        let c = Chat::with_dir(d.clone());
        c.enviar(ip(10), "Ana", "uno").unwrap();
        c.enviar(ip(11), "Beto", "feo").unwrap();
        let rev = c.lock().rev;
        assert_eq!(c.admin_post("borrar", &kv(&[("seq", "2")]), b""), "Mensaje borrado.");
        assert_eq!(c.lock().rev, rev + 1);
        assert!(!fs::read_to_string(d.join("mensajes.txt")).unwrap().contains("feo"));

        c.admin_post("silenciar", &kv(&[("ip", "192.168.4.11"), ("minutos", "15")]), b"");
        assert_eq!(c.enviar(ip(11), "Beto", "otra vez"), Err(Fallo::Silenciado(15)));
        assert!(c.admin_html().contains("Silenciados"));
        c.admin_post("quitar_silencio", &kv(&[("ip", "192.168.4.11")]), b"");
        assert!(c.enviar(ip(11), "Beto", "perdón").is_ok());

        c.admin_post("lectura", &kv(&[("valor", "on")]), b"");
        assert_eq!(c.enviar(ip(10), "Ana", "hola"), Err(Fallo::Lectura));
        assert_eq!(c.admin_post("escribir", &kv(&[("texto", "Anuncio <importante>")]), b""), "Mensaje enviado como Anfitrión.");
        let s = texto(&c.guest(&get("/mensajes", "desde=0", ip(10))));
        assert!(s.contains("\"ro\":1") && s.contains("\"Anfitrión\",\"Anuncio <importante>\""), "{s}");
        assert!(s.contains(",1]]"), "el último es del anfitrión: {s}");
        let admin = c.admin_html();
        assert!(admin.contains("Anuncio &lt;importante&gt;") && admin.contains("<span class=tag>Anfitrión</span>"));

        c.admin_post("config", &kv(&[("bienvenida", "Hola <todos>\r\nsean amables"), ("dueno", "Yael")]), b"");
        assert_eq!(c.enviar(ip(10), "yael", "x"), Err(Fallo::Lectura));
        c.admin_post("lectura", &kv(&[("valor", "off")]), b"");
        assert_eq!(c.enviar(ip(10), "YAEL", "x"), Err(Fallo::Reservado));
        let p = texto(&c.guest(&get("/", "", ip(10))));
        assert!(p.contains("Hola &lt;todos&gt;\nsean amables"));
        assert!(!p.contains("<span class=badge>"), "sin distintivo repetido si el apodo ya es Anfitrión");
        c.admin_post("escribir", &kv(&[("texto", "soy yo")]), b"");
        let p = texto(&c.guest(&get("/", "", ip(10))));
        assert!(p.contains(">Yael</b><span class=badge>Anfitrión</span>"), "{p}");

        let exp = c.admin_get("exportar").unwrap();
        let e = texto(&exp);
        assert!(e.contains("Ana [192.168.4.10]: uno") && e.contains(" Anfitrión: Anuncio <importante>"), "{e}");
        assert!(exp.headers.iter().any(|(k, v)| k == "Content-Disposition" && v.contains("chat.txt")));

        assert_eq!(c.admin_post("vaciar", &[], b""), "Marca la casilla para confirmar.");
        assert_eq!(c.admin_post("vaciar", &kv(&[("confirmar", "si")]), b""), "Chat vaciado.");
        assert!(c.lock().msgs.is_empty());
        // el seq no se reutiliza
        assert_eq!(c.enviar(ip(12), "Caro", "nuevo"), Ok(6));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn persistencia_y_rotacion() {
        let d = tmp("rot");
        {
            let c = Chat::with_dir(d.clone());
            let mut st = c.lock();
            for i in 0..(MAX_LINEAS + 5) {
                c.publicar(&mut st, "Ana".into(), format!("m{i}"), Some(ip(10)), false).unwrap();
            }
            assert_eq!(st.msgs.len(), MAX_GUARDADOS);
            assert!(st.lineas <= MAX_LINEAS);
            drop(st);
            c.admin_post("config", &kv(&[("bienvenida", "hola\\n"), ("dueno", "Yael")]), b"");
        }
        let lineas = fs::read_to_string(d.join("mensajes.txt")).unwrap().lines().count();
        assert!(lineas <= MAX_LINEAS, "{lineas}");
        let c = Chat::with_dir(d.clone());
        let st = c.lock();
        assert_eq!(st.msgs.len(), MAX_GUARDADOS);
        assert_eq!(st.seq, (MAX_LINEAS + 5) as u64);
        assert_eq!(st.msgs.back().unwrap().texto, format!("m{}", MAX_LINEAS + 4));
        assert_eq!(st.cfg.bienvenida, "hola\\n");
        assert_eq!(st.cfg.dueno, "Yael");
        assert!(st.lineas <= MAX_GUARDADOS);
        drop(st);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn apodo_y_resumen() {
        let d = tmp("apodo");
        let c = Chat::with_dir(d.clone());
        let s = texto(&c.guest(&get("/apodo", "a=+Ana%E2%80%AE+", ip(10))));
        assert!(s.starts_with("{\"ok\":1,\"a\":\"Ana\""), "{s}");
        let s = texto(&c.guest(&get("/apodo", "a=Stick", ip(10))));
        assert!(s.contains("\"e\":\"reservado\""), "{s}");
        c.enviar(ip(10), "Ana", "hola").unwrap();
        assert_eq!(c.resumen(), "1 mensaje hoy · 1 conectado");
        c.guest(&get("/", "", ip(11)));
        assert_eq!(c.resumen(), "1 mensaje hoy · 2 conectados");
        let _ = fs::remove_dir_all(d);
    }
}
