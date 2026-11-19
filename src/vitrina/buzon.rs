//! Módulo "Buzón offline": una *dead drop*. Quien se conecte al WiFi abierto del stick puede
//! dejar un mensaje (y opcionalmente un archivo) sin internet. El admin los lee en el panel.
//!
//! TODO lo que llega de invitados es hostil:
//!   - textos con límite de caracteres, sin caracteres de control, y escapados al pintarlos
//!   - archivos guardados como `archivos/<id aleatorio>` (permisos 0600); el nombre original
//!     sanitizado vive solo en los metadatos. Nunca se sirven a invitados; al admin se le dan
//!     como descarga (`attachment` + `application/octet-stream` + `nosniff`)
//!   - ids validados (32 hex) antes de tocar el disco → sin path traversal
//!   - límite por IP (5 envíos / 10 min) y global (60 / 10 min), cuota total configurable
//!     (500 MB por defecto), tope de 2000 mensajes y piso de 300 MB libres en disco
//!
//! Subida: `<form method=post enctype=multipart/form-data>` normal (funciona sin JS en todos
//! los navegadores; el JS de la página solo avisa si el archivo pasa de 10 MB). Ojo: el
//! mini-navegador de portal cautivo de Android no implementa el selector de archivos, por eso
//! la página sugiere abrir http://192.168.4.1/ en el navegador normal para adjuntar.
//!
//! Disco (`data_dir("buzon")`):
//!   msgs/<id>.json      un archivo por mensaje (JSON plano hecho a mano, escritura atómica)
//!   archivos/<id>       contenido del adjunto, si hay
//!   config              muro=0|1, cuota_mb=N, visto=<último nº de mensaje leído por el admin>

use super::{GuestReq, GuestResp, Vitrina};
use crate::util::{html, json};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_MENSAJE: usize = 2000; // caracteres
const MAX_APODO: usize = 40;
const MAX_NOMBRE_ARCHIVO: usize = 100;
const MAX_BODY: usize = 10 * 1024 * 1024;
const VENTANA: Duration = Duration::from_secs(600);
const ENVIOS_POR_IP: usize = 5;
const ENVIOS_GLOBALES: usize = 60;
const MAX_MENSAJES: usize = 2000;
const CUOTA_MB_DEFECTO: u64 = 500;
const CUOTA_MB_MAX: u64 = 100_000;
const MIN_LIBRE: u64 = 300 * 1024 * 1024;
const MURO_MAX: usize = 30;
const ADMIN_MAX: usize = 100;
const MB: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
struct Adjunto {
    nombre: String,
    tam: u64,
    tipo: String,
}

#[derive(Clone, Debug, PartialEq)]
struct Msg {
    id: String,
    /// Número de secuencia (creciente; sirve para "nuevos" aunque el reloj del stick salte)
    n: u64,
    ts: u64,
    apodo: String,
    texto: String,
    adjunto: Option<Adjunto>,
}

impl Msg {
    /// Bytes que ocupa en disco (JSON + adjunto), para la cuota.
    fn bytes(&self) -> u64 {
        self.to_json().len() as u64 + self.adjunto.as_ref().map_or(0, |a| a.tam)
    }

    fn to_json(&self) -> String {
        let mut s = format!(
            "{{\"v\":1,\"id\":{},\"n\":{},\"ts\":{},\"apodo\":{},\"texto\":{}",
            json(&self.id),
            self.n,
            self.ts,
            json(&self.apodo),
            json(&self.texto)
        );
        if let Some(a) = &self.adjunto {
            s.push_str(&format!(",\"archivo\":{},\"tam\":{},\"tipo\":{}", json(&a.nombre), a.tam, json(&a.tipo)));
        }
        s.push_str("}\n");
        s
    }

    fn from_json(s: &str) -> Option<Msg> {
        let m = parse_json_plano(s)?;
        let txt = |k: &str| match m.get(k) {
            Some(Val::Txt(t)) => Some(t.clone()),
            _ => None,
        };
        let num = |k: &str| match m.get(k) {
            Some(Val::Num(n)) => Some(*n),
            _ => None,
        };
        let id = txt("id")?;
        if !id_valido(&id) {
            return None;
        }
        let adjunto = match txt("archivo") {
            Some(nombre) => Some(Adjunto { nombre, tam: num("tam")?, tipo: txt("tipo").unwrap_or_default() }),
            None => None,
        };
        Some(Msg { id, n: num("n")?, ts: num("ts").unwrap_or(0), apodo: txt("apodo").unwrap_or_default(), texto: txt("texto")?, adjunto })
    }
}

struct Config {
    muro: bool,
    cuota_mb: u64,
    visto: u64,
}

struct Estado {
    /// Ordenados por `n` ascendente
    msgs: Vec<Msg>,
    usado: u64,
    cfg: Config,
    por_ip: HashMap<IpAddr, Vec<Instant>>,
    global: Vec<Instant>,
}

pub struct Buzon {
    dir: PathBuf,
    /// Espacio libre mínimo que debe quedar en el disco (ajustable en tests)
    min_libre: u64,
    st: Mutex<Estado>,
}

impl Buzon {
    pub fn new() -> Self {
        Self::with_dir(super::data_dir("buzon"))
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(dir.join("msgs"));
        let _ = fs::create_dir_all(dir.join("archivos"));
        let cfg = leer_config(&dir.join("config"));
        let (msgs, usado) = cargar(&dir);
        Buzon { dir, min_libre: MIN_LIBRE, st: Mutex::new(Estado { msgs, usado, cfg, por_ip: HashMap::new(), global: Vec::new() }) }
    }

    fn lock(&self) -> MutexGuard<'_, Estado> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn guardar_config(&self, c: &Config) -> bool {
        let s = format!("muro={}\ncuota_mb={}\nvisto={}\n", c.muro as u8, c.cuota_mb, c.visto);
        escribir_atomico(&self.dir.join("config"), s.as_bytes())
    }

    fn ruta_msg(&self, id: &str) -> PathBuf {
        self.dir.join("msgs").join(format!("{id}.json"))
    }

    fn ruta_archivo(&self, id: &str) -> PathBuf {
        self.dir.join("archivos").join(id)
    }

    /// Procesa un envío de invitado. Devuelve el código de aviso para "/?r=<código>".
    fn recibir(&self, req: &GuestReq) -> &'static str {
        let campos = match leer_campos(req.content_type, req.body) {
            Ok(c) => c,
            Err(Fallo::Incompleto) if req.body.len() >= MAX_BODY => return "grande",
            Err(_) => return "err",
        };
        let texto = limpiar(&String::from_utf8_lossy(&campos.mensaje), true);
        let apodo = limpiar(&String::from_utf8_lossy(&campos.apodo), false);
        if texto.chars().count() > MAX_MENSAJE || apodo.chars().count() > MAX_APODO {
            return "largo";
        }
        let archivo = campos.archivo.filter(|a| !a.datos.is_empty());
        if texto.is_empty() && archivo.is_none() {
            return "vacio";
        }

        let mut st = self.lock();
        let ahora = Instant::now();
        st.global.retain(|t| ahora.duration_since(*t) < VENTANA);
        st.por_ip.retain(|_, v| {
            v.retain(|t| ahora.duration_since(*t) < VENTANA);
            !v.is_empty()
        });
        if st.global.len() >= ENVIOS_GLOBALES || st.por_ip.get(&req.client_ip).map_or(0, |v| v.len()) >= ENVIOS_POR_IP {
            return "limite";
        }

        let id = match id_aleatorio() {
            Some(i) => i,
            None => return "err",
        };
        let msg = Msg {
            id: id.clone(),
            n: st.msgs.last().map_or(1, |m| m.n + 1),
            ts: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
            apodo,
            texto,
            adjunto: archivo.as_ref().map(|a| Adjunto {
                nombre: sanitizar_nombre(&a.nombre),
                tam: a.datos.len() as u64,
                tipo: sanitizar_tipo(a.tipo),
            }),
        };
        let tam = msg.bytes();
        if st.msgs.len() >= MAX_MENSAJES || st.usado + tam > st.cfg.cuota_mb * MB {
            return "lleno";
        }
        match libre(&self.dir) {
            Some(l) if l >= self.min_libre.saturating_add(tam) => {}
            _ => return "lleno",
        }

        if let Some(a) = &archivo {
            let ok = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(self.ruta_archivo(&id))
                .and_then(|mut f| f.write_all(a.datos).and_then(|_| f.sync_data()))
                .is_ok();
            if !ok {
                let _ = fs::remove_file(self.ruta_archivo(&id));
                return "err";
            }
        }
        // El JSON es el "commit": si no se escribe, el mensaje no existe.
        if !escribir_atomico(&self.ruta_msg(&id), msg.to_json().as_bytes()) {
            let _ = fs::remove_file(self.ruta_archivo(&id));
            return "err";
        }
        st.usado += tam;
        st.msgs.push(msg);
        st.global.push(ahora);
        st.por_ip.entry(req.client_ip).or_default().push(ahora);
        "ok"
    }

    fn pagina(&self, aviso: &str) -> String {
        let (aviso_txt, clase) = match aviso {
            "ok" => ("¡Mensaje recibido!", "ok"),
            "vacio" => ("Escribe un mensaje o adjunta un archivo.", "bad"),
            "largo" => ("El mensaje o el apodo es demasiado largo.", "bad"),
            "grande" => ("El archivo es demasiado grande (máximo 10 MB).", "bad"),
            "limite" => ("Demasiados envíos. Espera unos minutos y vuelve a intentar.", "bad"),
            "lleno" => ("El buzón está lleno por ahora. Intenta más tarde.", "bad"),
            "err" => ("No se pudo guardar el mensaje. Intenta de nuevo.", "bad"),
            _ => ("", ""),
        };
        let aviso_html = if aviso_txt.is_empty() { String::new() } else { format!("<p class=\"aviso {clase}\">{aviso_txt}</p>") };
        let muro = {
            let st = self.lock();
            if st.cfg.muro {
                let items: String = st.msgs.iter().rev().take(MURO_MAX).map(item_muro).collect();
                let items = if items.is_empty() { "<p class=mut>Todavía no hay mensajes.</p>".to_string() } else { items };
                format!("<section><h2>Muro</h2>{items}</section>")
            } else {
                String::new()
            }
        };
        format!(
            r#"<!doctype html><html lang=es><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>Buzón</title><style>
:root{{--bg:#0f1115;--card:#181b22;--tx:#e8eaf0;--mut:#8a90a0;--ac:#4f8cff;--ok:#3ecf8e;--bad:#ff5d5d}}
@media (prefers-color-scheme:light){{:root{{--bg:#f4f5f8;--card:#fff;--tx:#14161c;--mut:#5d6372}}}}
*{{box-sizing:border-box}}body{{margin:0;font:16px/1.45 system-ui,sans-serif;background:var(--bg);color:var(--tx)}}
main{{max-width:560px;margin:auto;padding:16px}}h1{{font-size:22px;margin:8px 0 4px}}h2{{font-size:17px;margin:20px 0 8px}}
form,.m{{background:var(--card);border-radius:14px;padding:16px;margin-bottom:12px}}
label{{display:block;margin-bottom:10px}}textarea,input[type=text]{{width:100%;padding:10px;border-radius:8px;border:1px solid #8884;background:var(--bg);color:var(--tx);font:inherit;margin-top:4px}}
textarea{{min-height:120px}}button{{background:var(--ac);color:#fff;border:0;border-radius:8px;padding:12px 18px;font-size:16px;width:100%}}
.mut,small{{color:var(--mut)}}.aviso{{padding:10px 12px;border-radius:10px;color:#fff}}.aviso.ok{{background:var(--ok)}}.aviso.bad{{background:var(--bad)}}
.m p{{margin:6px 0 0;white-space:pre-wrap;word-break:break-word}}
</style></head><body><main>
<h1>📮 Buzón</h1><p class=mut>Deja un mensaje o un archivo. Funciona sin internet.</p>
{aviso_html}
<form method=post action=/enviar enctype=multipart/form-data id=f>
<label>Mensaje<textarea name=mensaje maxlength={MAX_MENSAJE}></textarea></label>
<label>Apodo (opcional)<input type=text name=apodo maxlength={MAX_APODO} autocomplete=off></label>
<label>Archivo (opcional, máx. 10 MB)<input type=file name=archivo id=a></label>
<p><small>¿No puedes elegir un archivo? Abre <b>http://192.168.4.1/</b> en tu navegador normal.</small></p>
<button id=b>Enviar</button></form>
{muro}
</main><script>
(function(){{var f=document.getElementById('f'),a=document.getElementById('a'),b=document.getElementById('b');
f.onsubmit=function(){{if(a.files&&a.files[0]&&a.files[0].size>{MAX_BODY}-65536){{alert('El archivo pasa de 10 MB.');return false;}}
b.disabled=true;b.textContent='Enviando…';return true;}};}})();
</script></body></html>"#
        )
    }

    fn descargar(&self, id: &str) -> Option<GuestResp> {
        if !id_valido(id) {
            return None;
        }
        let nombre = {
            let st = self.lock();
            st.msgs.iter().find(|m| m.id == id)?.adjunto.as_ref()?.nombre.clone()
        };
        let datos = fs::read(self.ruta_archivo(id)).ok()?;
        Some(descarga("application/octet-stream", &nombre, datos))
    }

    fn exportar(&self) -> GuestResp {
        let st = self.lock();
        let mut s = String::new();
        for m in &st.msgs {
            s.push_str(&format!("#{} · {} · {}\n", m.n, fecha(m.ts), if m.apodo.is_empty() { "(sin apodo)" } else { &m.apodo }));
            if let Some(a) = &m.adjunto {
                s.push_str(&format!("[adjunto: {} · {} · id {}]\n", a.nombre, tam_humano(a.tam), m.id));
            }
            s.push_str(&m.texto);
            s.push_str("\n\n");
        }
        descarga("text/plain; charset=utf-8", "buzon.txt", s.into_bytes())
    }
}

impl Default for Buzon {
    fn default() -> Self {
        Self::new()
    }
}

impl Vitrina for Buzon {
    fn id(&self) -> &'static str {
        "buzon"
    }
    fn nombre(&self) -> &'static str {
        "Buzón offline"
    }
    fn descripcion(&self) -> &'static str {
        "Dejan mensajes y archivos sin internet"
    }
    fn ssid_por_defecto(&self) -> String {
        "Buzon".into()
    }
    fn max_body(&self) -> usize {
        MAX_BODY
    }

    fn guest(&self, req: &GuestReq) -> GuestResp {
        let mut r = match (req.method, req.path) {
            ("POST", "/enviar") => {
                let codigo = self.recibir(req);
                GuestResp::redirect(&format!("/?r={codigo}"))
            }
            ("GET" | "HEAD", "/") => {
                let aviso = crate::util::form(req.query).into_iter().find(|(k, _)| k == "r").map(|(_, v)| v).unwrap_or_default();
                GuestResp::html(self.pagina(&aviso))
            }
            ("GET" | "HEAD", "/favicon.ico" | "/robots.txt") => GuestResp::not_found(),
            ("GET" | "HEAD", _) => GuestResp::redirect("/"),
            _ => GuestResp::not_found(),
        };
        r.headers.push(("Cache-Control".into(), "no-store".into()));
        r.headers.push(("X-Content-Type-Options".into(), "nosniff".into()));
        r
    }

    fn admin_html(&self) -> String {
        let st = self.lock();
        let libre_mb = libre(&self.dir).map_or("?".into(), |l| (l / MB).to_string());
        let nuevos = st.msgs.iter().filter(|m| m.n > st.cfg.visto).count();
        let mut o = format!(
            "<p>{} mensajes · {} nuevos · usado {} de {} MB · {} MB libres en disco</p>",
            st.msgs.len(),
            nuevos,
            tam_humano(st.usado),
            st.cfg.cuota_mb,
            libre_mb
        );
        o.push_str(&format!(
            "<form method=post action=/vitrina/buzon/muro><input type=hidden name=valor value={}>\
             <p>Muro público: <b>{}</b> <button>{}</button></p></form>",
            if st.cfg.muro { "off" } else { "on" },
            if st.cfg.muro { "activado (los invitados ven los textos recientes)" } else { "desactivado" },
            if st.cfg.muro { "Desactivar" } else { "Activar" }
        ));
        o.push_str(&format!(
            "<form method=post action=/vitrina/buzon/cuota><label>Cuota total (MB)\
             <input type=text name=cuota_mb inputmode=numeric value={}></label><button>Guardar cuota</button></form>",
            st.cfg.cuota_mb
        ));
        o.push_str(
            "<p><a href=/vitrina/buzon/exportar>Descargar todos los textos (.txt)</a></p>\
             <form method=post action=/vitrina/buzon/leidos><button>Marcar todo como leído</button></form>",
        );
        if st.msgs.len() > ADMIN_MAX {
            o.push_str(&format!("<p>Mostrando los {ADMIN_MAX} más recientes de {}.</p>", st.msgs.len()));
        }
        o.push_str("<ul>");
        for m in st.msgs.iter().rev().take(ADMIN_MAX) {
            let adj = m.adjunto.as_ref().map_or(String::new(), |a| {
                format!(
                    "<br>📎 <a href=/vitrina/buzon/archivo/{}>{}</a> <small>({} · {})</small>",
                    m.id,
                    html(&a.nombre),
                    tam_humano(a.tam),
                    html(&a.tipo)
                )
            });
            o.push_str(&format!(
                "<li style=\"display:block\"><small>#{} · {}{}</small> <b>{}</b>{}\
                 <div style=\"white-space:pre-wrap;word-break:break-word\">{}</div>\
                 <form method=post action=/vitrina/buzon/borrar><input type=hidden name=id value={}><button class=link>borrar</button></form></li>",
                m.n,
                fecha(m.ts),
                if m.n > st.cfg.visto { " · <b>nuevo</b>" } else { "" },
                if m.apodo.is_empty() { "<i>anónimo</i>".to_string() } else { html(&m.apodo) },
                adj,
                html(&m.texto),
                m.id
            ));
        }
        if st.msgs.is_empty() {
            o.push_str("<li>Sin mensajes</li>");
        }
        o.push_str("</ul>");
        o.push_str(
            "<form method=post action=/vitrina/buzon/borrar_todo>\
             <label><input type=checkbox name=confirmar value=si required> Sí, borrar todo</label>\
             <button class=link>Borrar todos los mensajes y archivos</button></form>",
        );
        o
    }

    fn admin_post(&self, accion: &str, form: &[(String, String)], _body: &[u8]) -> String {
        let campo = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("");
        let mut st = self.lock();
        match accion {
            "muro" => {
                let antes = st.cfg.muro;
                st.cfg.muro = campo("valor") == "on";
                if !self.guardar_config(&st.cfg) {
                    st.cfg.muro = antes;
                    return "No se pudo guardar la configuración.".into();
                }
                if st.cfg.muro { "Muro público activado.".into() } else { "Muro público desactivado.".into() }
            }
            "cuota" => match campo("cuota_mb").trim().parse::<u64>() {
                Ok(n) if (1..=CUOTA_MB_MAX).contains(&n) => {
                    let antes = st.cfg.cuota_mb;
                    st.cfg.cuota_mb = n;
                    if !self.guardar_config(&st.cfg) {
                        st.cfg.cuota_mb = antes;
                        return "No se pudo guardar la configuración.".into();
                    }
                    format!("Cuota fijada en {n} MB.")
                }
                _ => format!("Cuota no válida (1 a {CUOTA_MB_MAX} MB)."),
            },
            "leidos" => {
                st.cfg.visto = st.msgs.last().map_or(st.cfg.visto, |m| m.n);
                self.guardar_config(&st.cfg);
                "Mensajes marcados como leídos.".into()
            }
            "borrar" => {
                let id = campo("id");
                if !id_valido(id) {
                    return "Id no válido.".into();
                }
                match st.msgs.iter().position(|m| m.id == id) {
                    Some(i) => {
                        let _ = fs::remove_file(self.ruta_archivo(id));
                        if fs::remove_file(self.ruta_msg(id)).is_err() && self.ruta_msg(id).exists() {
                            return "No se pudo borrar el mensaje.".into();
                        }
                        let m = st.msgs.remove(i);
                        st.usado = st.usado.saturating_sub(m.bytes());
                        format!("Mensaje #{} borrado.", m.n)
                    }
                    None => "Ese mensaje ya no existe.".into(),
                }
            }
            "borrar_todo" => {
                if campo("confirmar") != "si" {
                    return "Marca la casilla de confirmación para borrar todo.".into();
                }
                for sub in ["msgs", "archivos"] {
                    if let Ok(rd) = fs::read_dir(self.dir.join(sub)) {
                        for e in rd.flatten() {
                            let _ = fs::remove_file(e.path());
                        }
                    }
                }
                let n = st.msgs.len();
                // Conserva la numeración para que los "nuevos" sigan funcionando.
                st.cfg.visto = st.msgs.last().map_or(st.cfg.visto, |m| m.n).max(st.cfg.visto);
                self.guardar_config(&st.cfg);
                st.msgs.clear();
                st.usado = 0;
                format!("{n} mensajes borrados.")
            }
            _ => "Acción desconocida.".into(),
        }
    }

    fn admin_get(&self, ruta: &str) -> Option<GuestResp> {
        if let Some(id) = ruta.strip_prefix("archivo/") {
            return self.descargar(id);
        }
        match ruta {
            "exportar" => Some(self.exportar()),
            _ => None,
        }
    }

    fn resumen(&self) -> String {
        let st = self.lock();
        let nuevos = st.msgs.iter().filter(|m| m.n > st.cfg.visto).count();
        format!("{} mensajes · {} nuevos", st.msgs.len(), nuevos)
    }
}

// ───────────────────────── utilidades ─────────────────────────

fn item_muro(m: &Msg) -> String {
    let apodo = if m.apodo.is_empty() { "Anónimo".to_string() } else { html(&m.apodo) };
    let adj = if m.adjunto.is_some() { " · 📎 adjunto" } else { "" };
    let texto = if m.texto.is_empty() { String::new() } else { format!("<p>{}</p>", html(&m.texto)) };
    format!("<div class=m><b>{apodo}</b> <small>{}{adj}</small>{texto}</div>", fecha(m.ts))
}

fn descarga(tipo: &str, nombre: &str, datos: Vec<u8>) -> GuestResp {
    let ascii: String = nombre.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let mut pct = String::new();
    for b in nombre.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            pct.push(b as char);
        } else {
            pct.push_str(&format!("%{b:02X}"));
        }
    }
    let mut r = GuestResp::bytes(tipo, datos);
    r.headers.push(("Content-Disposition".into(), format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{pct}")));
    r.headers.push(("X-Content-Type-Options".into(), "nosniff".into()));
    r.headers.push(("Content-Security-Policy".into(), "sandbox; default-src 'none'".into()));
    r.headers.push(("Cache-Control".into(), "no-store".into()));
    r
}

fn id_valido(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn id_aleatorio() -> Option<String> {
    let mut b = [0u8; 16];
    fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Quita caracteres de control (salvo saltos de línea si `multilinea`), normaliza CRLF y recorta.
fn limpiar(s: &str, multilinea: bool) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let s: String = s
        .chars()
        .filter(|&c| (multilinea && c == '\n') || !(c.is_control() || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')))
        .collect();
    s.trim().to_string()
}

fn sanitizar_nombre(n: &str) -> String {
    let base = n.rsplit(['/', '\\']).next().unwrap_or("");
    let s: String = base
        .chars()
        .map(|c| if c.is_alphanumeric() || " ._-()[]+,".contains(c) { c } else { '_' })
        .take(MAX_NOMBRE_ARCHIVO)
        .collect();
    let s = s.trim_start_matches(['.', ' ']).trim_end().to_string();
    if s.is_empty() { "archivo".into() } else { s }
}

fn sanitizar_tipo(t: &[u8]) -> String {
    let t = String::from_utf8_lossy(t);
    let t: String = t.trim().chars().take(80).collect();
    if !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || "/.+-_".contains(c)) { t.to_ascii_lowercase() } else { String::new() }
}

fn tam_humano(b: u64) -> String {
    if b >= MB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else if b >= 1024 {
        format!("{:.0} KB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

#[allow(deprecated)] // libc::time_t en musl: aviso del cambio a 64 bits, ya es 64 bits en aarch64
fn fecha(ts: u64) -> String {
    let t = ts as libc::time_t;
    // SAFETY: localtime_r solo escribe en `tm`, que es nuestro.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return ts.to_string();
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

/// Bytes libres para usuarios no-root en el sistema de archivos de `p`.
fn libre(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` es una cadena C válida y `s` es memoria nuestra.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
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
    let mut c = Config { muro: false, cuota_mb: CUOTA_MB_DEFECTO, visto: 0 };
    for l in fs::read_to_string(p).unwrap_or_default().lines() {
        match l.split_once('=') {
            Some(("muro", v)) => c.muro = v.trim() == "1",
            Some(("cuota_mb", v)) => {
                if let Ok(n) = v.trim().parse::<u64>()
                    && (1..=CUOTA_MB_MAX).contains(&n)
                {
                    c.cuota_mb = n;
                }
            }
            Some(("visto", v)) => c.visto = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    c
}

/// Carga los mensajes del disco; limpia temporales y adjuntos huérfanos.
fn cargar(dir: &Path) -> (Vec<Msg>, u64) {
    let mut msgs = Vec::new();
    if let Ok(rd) = fs::read_dir(dir.join("msgs")) {
        for e in rd.flatten() {
            let p = e.path();
            match p.extension().and_then(|x| x.to_str()) {
                Some("json") => {
                    let m = fs::read_to_string(&p).ok().and_then(|s| Msg::from_json(&s));
                    match m {
                        // El id debe coincidir con el nombre del archivo.
                        Some(m) if p.file_stem().and_then(|s| s.to_str()) == Some(m.id.as_str()) => msgs.push(m),
                        _ => crate::util::log(&format!("buzon: ignoro {} (ilegible)", p.display())),
                    }
                }
                Some("tmp") => {
                    let _ = fs::remove_file(&p);
                }
                _ => {}
            }
        }
    }
    msgs.sort_by_key(|m| m.n);
    // Adjuntos: el tamaño real manda; si falta el archivo, se quita la referencia.
    for m in &mut msgs {
        if let Some(a) = &mut m.adjunto {
            match fs::metadata(dir.join("archivos").join(&m.id)) {
                Ok(md) => a.tam = md.len(),
                Err(_) => m.adjunto = None,
            }
        }
    }
    if let Ok(rd) = fs::read_dir(dir.join("archivos")) {
        for e in rd.flatten() {
            let nombre = e.file_name();
            let n = nombre.to_str().unwrap_or("");
            if !msgs.iter().any(|m| m.id == n && m.adjunto.is_some()) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    let usado = msgs.iter().map(Msg::bytes).sum();
    (msgs, usado)
}

// ───────────────────────── JSON plano ─────────────────────────

#[derive(Debug, PartialEq)]
enum Val {
    Txt(String),
    Num(u64),
}

/// Parser mínimo para un objeto JSON plano con valores string o entero no negativo
/// (lo único que escribe este módulo).
fn parse_json_plano(s: &str) -> Option<HashMap<String, Val>> {
    let mut it = s.trim().chars().peekable();
    let mut m = HashMap::new();
    let salta = |it: &mut std::iter::Peekable<std::str::Chars>| {
        while it.peek().is_some_and(|c| c.is_whitespace()) {
            it.next();
        }
    };
    fn cadena(it: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
        if it.next()? != '"' {
            return None;
        }
        let mut o = String::new();
        loop {
            match it.next()? {
                '"' => return Some(o),
                '\\' => match it.next()? {
                    '"' => o.push('"'),
                    '\\' => o.push('\\'),
                    '/' => o.push('/'),
                    'n' => o.push('\n'),
                    'r' => o.push('\r'),
                    't' => o.push('\t'),
                    'b' => o.push('\u{8}'),
                    'f' => o.push('\u{c}'),
                    'u' => {
                        let h: String = (0..4).filter_map(|_| it.next()).collect();
                        let v = u32::from_str_radix(&h, 16).ok()?;
                        o.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                    }
                    _ => return None,
                },
                c => o.push(c),
            }
        }
    }
    if it.next()? != '{' {
        return None;
    }
    salta(&mut it);
    if it.peek() == Some(&'}') {
        return Some(m);
    }
    loop {
        salta(&mut it);
        let k = cadena(&mut it)?;
        salta(&mut it);
        if it.next()? != ':' {
            return None;
        }
        salta(&mut it);
        let v = if it.peek() == Some(&'"') {
            Val::Txt(cadena(&mut it)?)
        } else {
            let mut d = String::new();
            while it.peek().is_some_and(|c| c.is_ascii_digit()) {
                d.push(it.next()?);
            }
            Val::Num(d.parse().ok()?)
        };
        m.insert(k, v);
        salta(&mut it);
        match it.next()? {
            ',' => continue,
            '}' => return Some(m),
            _ => return None,
        }
    }
}

// ───────────────────────── multipart / formulario ─────────────────────────

#[derive(Debug, PartialEq)]
enum Fallo {
    Formato,
    Incompleto,
}

struct ArchivoSubido<'a> {
    nombre: String,
    tipo: &'a [u8],
    datos: &'a [u8],
}

#[derive(Default)]
struct Campos<'a> {
    mensaje: Cow<'a, [u8]>,
    apodo: Cow<'a, [u8]>,
    archivo: Option<ArchivoSubido<'a>>,
}

fn buscar(h: &[u8], aguja: &[u8], desde: usize) -> Option<usize> {
    if desde > h.len() || aguja.is_empty() {
        return None;
    }
    let primero = aguja[0];
    let mut i = desde;
    while i + aguja.len() <= h.len() {
        i += h[i..h.len() - aguja.len() + 1].iter().position(|&b| b == primero)?;
        if &h[i..i + aguja.len()] == aguja {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parámetros de un header tipo `form-data; name="x"; filename="a;b.txt"` (valores con o sin comillas).
fn parametros(v: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut ch = v.chars().peekable();
    // salta el valor principal
    for c in ch.by_ref() {
        if c == ';' {
            break;
        }
    }
    loop {
        while ch.peek().is_some_and(|c| c.is_whitespace() || *c == ';') {
            ch.next();
        }
        let mut k = String::new();
        while let Some(&c) = ch.peek() {
            if c == '=' || c == ';' {
                break;
            }
            k.push(c);
            ch.next();
        }
        if k.is_empty() {
            return out;
        }
        let mut val = String::new();
        if ch.peek() == Some(&'=') {
            ch.next();
            if ch.peek() == Some(&'"') {
                ch.next();
                while let Some(c) = ch.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = ch.next() {
                                val.push(n)
                            }
                        }
                        c => val.push(c),
                    }
                }
            } else {
                while let Some(&c) = ch.peek() {
                    if c == ';' {
                        break;
                    }
                    val.push(c);
                    ch.next();
                }
            }
        }
        out.push((k.trim().to_ascii_lowercase(), val.trim().to_string()));
    }
}

fn leer_campos<'a>(content_type: &str, body: &'a [u8]) -> Result<Campos<'a>, Fallo> {
    let ct = content_type.trim().to_ascii_lowercase();
    if ct.starts_with("multipart/form-data") {
        let boundary = parametros(content_type).into_iter().find(|(k, _)| k == "boundary").map(|(_, v)| v).ok_or(Fallo::Formato)?;
        if boundary.is_empty() || boundary.len() > 200 {
            return Err(Fallo::Formato);
        }
        multipart(body, boundary.as_bytes())
    } else if ct.starts_with("application/x-www-form-urlencoded") {
        // Sin adjunto (navegadores que ignoren enctype). Solo los dos campos de texto.
        let s = std::str::from_utf8(body).map_err(|_| Fallo::Formato)?;
        let f = crate::util::form(s);
        let get = |k: &str| f.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()).unwrap_or_default();
        Ok(Campos { mensaje: Cow::Owned(get("mensaje").into_bytes()), apodo: Cow::Owned(get("apodo").into_bytes()), archivo: None })
    } else {
        Err(Fallo::Formato)
    }
}

fn multipart<'a>(body: &'a [u8], boundary: &[u8]) -> Result<Campos<'a>, Fallo> {
    let mut delim = b"--".to_vec();
    delim.extend_from_slice(boundary);
    let mut sep = b"\r\n".to_vec();
    sep.extend_from_slice(&delim);

    let mut c = Campos::default();
    let mut pos = buscar(body, &delim, 0).ok_or(Fallo::Incompleto)? + delim.len();
    let mut partes = 0;
    loop {
        if body.len() < pos + 2 {
            return Err(Fallo::Incompleto);
        }
        if &body[pos..pos + 2] == b"--" {
            return Ok(c);
        }
        // relleno de transporte opcional y CRLF
        while pos < body.len() && (body[pos] == b' ' || body[pos] == b'\t') {
            pos += 1;
        }
        if !body[pos..].starts_with(b"\r\n") {
            return Err(if body.len() <= pos + 1 { Fallo::Incompleto } else { Fallo::Formato });
        }
        pos += 2;
        let (cabeceras, inicio) = if body[pos..].starts_with(b"\r\n") {
            (&body[pos..pos], pos + 2)
        } else {
            let fin = buscar(body, b"\r\n\r\n", pos).ok_or(Fallo::Incompleto)?;
            (&body[pos..fin], fin + 4)
        };
        let fin = buscar(body, &sep, inicio).ok_or(Fallo::Incompleto)?;
        let datos = &body[inicio..fin];
        pos = fin + sep.len();
        partes += 1;
        if partes > 10 || cabeceras.len() > 4096 {
            return Err(Fallo::Formato);
        }

        let (mut nombre, mut archivo, mut tipo): (String, Option<String>, &'a [u8]) = (String::new(), None, b"");
        for linea in cabeceras.split(|&b| b == b'\n') {
            let linea = linea.strip_suffix(b"\r").unwrap_or(linea);
            let Some(dp) = linea.iter().position(|&b| b == b':') else { continue };
            let k = String::from_utf8_lossy(&linea[..dp]).trim().to_ascii_lowercase();
            let v = &linea[dp + 1..];
            if k == "content-disposition" {
                for (pk, pv) in parametros(&String::from_utf8_lossy(v)) {
                    match pk.as_str() {
                        "name" => nombre = pv,
                        "filename" => archivo = Some(pv),
                        _ => {}
                    }
                }
            } else if k == "content-type" {
                let v = v.trim_ascii();
                tipo = &v[..v.iter().position(|&b| b == b';').unwrap_or(v.len())];
            }
        }
        match (nombre.as_str(), archivo) {
            ("mensaje", None) if c.mensaje.is_empty() => c.mensaje = Cow::Borrowed(datos),
            ("apodo", None) if c.apodo.is_empty() => c.apodo = Cow::Borrowed(datos),
            ("archivo", Some(n)) if c.archivo.is_none() => c.archivo = Some(ArchivoSubido { nombre: n, tipo, datos }),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn tmp(nombre: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("buzon-test-{}-{nombre}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 4, n))
    }

    const B: &str = "----WebKitFormBoundaryXyZ123";

    fn cuerpo(mensaje: &str, apodo: &str, archivo: Option<(&str, &[u8])>) -> Vec<u8> {
        let mut b = Vec::new();
        for (k, v) in [("mensaje", mensaje), ("apodo", apodo)] {
            b.extend_from_slice(format!("--{B}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
        }
        let (n, d) = archivo.unwrap_or(("", b""));
        b.extend_from_slice(
            format!("--{B}\r\nContent-Disposition: form-data; name=\"archivo\"; filename=\"{n}\"\r\nContent-Type: image/png\r\n\r\n").as_bytes(),
        );
        b.extend_from_slice(d);
        b.extend_from_slice(format!("\r\n--{B}--\r\n").as_bytes());
        b
    }

    fn post(bz: &Buzon, ip_: IpAddr, body: &[u8]) -> String {
        let ct = format!("multipart/form-data; boundary={B}");
        let r = bz.guest(&GuestReq {
            method: "POST",
            path: "/enviar",
            query: "",
            host: "captive.apple.com",
            user_agent: "test",
            content_type: &ct,
            body,
            client_ip: ip_,
        });
        assert_eq!(r.status, 302);
        r.headers.iter().find(|(k, _)| k == "Location").unwrap().1.clone()
    }

    fn get(bz: &Buzon, path: &str, query: &str) -> GuestResp {
        bz.guest(&GuestReq {
            method: "GET",
            path,
            query,
            host: "x",
            user_agent: "test",
            content_type: "",
            body: b"",
            client_ip: ip(9),
        })
    }

    fn texto(r: &GuestResp) -> String {
        String::from_utf8(r.body.clone()).unwrap()
    }

    #[test]
    fn mensaje_simple_persiste() {
        let d = tmp("persiste");
        let bz = Buzon::with_dir(d.clone());
        assert_eq!(post(&bz, ip(2), &cuerpo("Hola\r\nmundo", "Ana", None)), "/?r=ok");
        assert_eq!(bz.resumen(), "1 mensajes · 1 nuevos");
        let bz2 = Buzon::with_dir(d.clone());
        assert_eq!(bz2.resumen(), "1 mensajes · 1 nuevos");
        let st = bz2.lock();
        assert_eq!(st.msgs[0].texto, "Hola\nmundo");
        assert_eq!(st.msgs[0].apodo, "Ana");
        assert!(st.msgs[0].adjunto.is_none());
    }

    #[test]
    fn archivo_sanitizado_y_descarga_admin() {
        let d = tmp("archivo");
        let bz = Buzon::with_dir(d.clone());
        let datos = b"\x89PNG\r\n--falso\r\n<script>alert(1)</script>";
        assert_eq!(post(&bz, ip(3), &cuerpo("", "", Some(("../../etc/pass%22wd<x>.png", datos)))), "/?r=ok");
        let (id, nombre) = {
            let st = bz.lock();
            (st.msgs[0].id.clone(), st.msgs[0].adjunto.as_ref().unwrap().nombre.clone())
        };
        assert!(id_valido(&id));
        assert_eq!(nombre, "pass_22wd_x_.png");
        // en disco con id aleatorio, contenido íntegro, sin permisos de ejecución
        let p = d.join("archivos").join(&id);
        assert_eq!(fs::read(&p).unwrap(), datos);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let r = bz.admin_get(&format!("archivo/{id}")).unwrap();
        assert_eq!(r.content_type, "application/octet-stream");
        assert_eq!(r.body, datos);
        let h = |k: &str| r.headers.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()).unwrap();
        assert!(h("Content-Disposition").starts_with("attachment; filename=\"pass_22wd_x_.png\""));
        assert_eq!(h("X-Content-Type-Options"), "nosniff");
        // invitados nunca lo reciben
        let r = get(&bz, &format!("/archivos/{id}"), "");
        assert_eq!(r.status, 302);
        assert!(!texto(&get(&bz, "/", "")).contains("pass_wd"));
    }

    #[test]
    fn sin_path_traversal() {
        let bz = Buzon::with_dir(tmp("traversal"));
        assert!(bz.admin_get("archivo/../../etc/passwd").is_none());
        assert!(bz.admin_get("archivo/").is_none());
        assert!(bz.admin_get(&format!("archivo/{}", "a".repeat(32))).is_none());
        assert!(bz.admin_get("otra").is_none());
        let f = vec![("id".to_string(), "../config".to_string())];
        assert_eq!(bz.admin_post("borrar", &f, b""), "Id no válido.");
    }

    #[test]
    fn escapa_en_admin_y_muro() {
        let bz = Buzon::with_dir(tmp("escape"));
        post(&bz, ip(4), &cuerpo("<script>alert('x')</script>", "<b>yo</b>", Some(("a.png", b"zz"))));
        let a = bz.admin_html();
        assert!(!a.contains("<script>") && a.contains("&lt;script&gt;"));
        assert!(!a.contains("<b>yo</b>") && a.contains("&lt;b&gt;yo&lt;/b&gt;"));
        // muro apagado: no se ven textos
        let g = texto(&get(&bz, "/", "r=ok"));
        assert!(g.contains("¡Mensaje recibido!"));
        assert!(!g.contains("script&gt;") && !g.contains("yo"));
        // muro encendido: textos escapados, adjunto solo como "📎 adjunto"
        assert_eq!(bz.admin_post("muro", &[("valor".into(), "on".into())], b""), "Muro público activado.");
        let g = texto(&get(&bz, "/", ""));
        assert!(g.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
        assert!(!g.contains("<script>alert"));
        assert!(g.contains("📎 adjunto") && !g.contains("a.png"));
        // el muro sobrevive reinicios
        let bz2 = Buzon::with_dir(bz.dir.clone());
        assert!(bz2.lock().cfg.muro);
    }

    #[test]
    fn aviso_no_refleja_query() {
        let bz = Buzon::with_dir(tmp("aviso"));
        let g = texto(&get(&bz, "/", "r=%3Cscript%3E"));
        assert_eq!(g.matches("<script>").count(), 1);
        assert!(!g.contains("&lt;script"));
    }

    #[test]
    fn limite_por_ip() {
        let bz = Buzon::with_dir(tmp("limite"));
        for _ in 0..ENVIOS_POR_IP {
            assert_eq!(post(&bz, ip(5), &cuerpo("hola", "", None)), "/?r=ok");
        }
        assert_eq!(post(&bz, ip(5), &cuerpo("hola", "", None)), "/?r=limite");
        assert_eq!(post(&bz, ip(6), &cuerpo("hola", "", None)), "/?r=ok");
    }

    #[test]
    fn cuota_y_disco() {
        let mut bz = Buzon::with_dir(tmp("cuota"));
        assert_eq!(bz.admin_post("cuota", &[("cuota_mb".into(), "1".into())], b""), "Cuota fijada en 1 MB.");
        assert!(bz.admin_post("cuota", &[("cuota_mb".into(), "0".into())], b"").starts_with("Cuota no válida"));
        let grande = vec![7u8; 1024 * 1024 + 1];
        assert_eq!(post(&bz, ip(7), &cuerpo("", "", Some(("g.bin", &grande)))), "/?r=lleno");
        assert_eq!(post(&bz, ip(7), &cuerpo("", "", Some(("p.bin", &grande[..1000])))), "/?r=ok");
        bz.min_libre = u64::MAX / 2;
        assert_eq!(post(&bz, ip(8), &cuerpo("hola", "", None)), "/?r=lleno");
        assert!(bz.admin_html().contains("usado 1"));
    }

    #[test]
    fn validaciones() {
        let bz = Buzon::with_dir(tmp("valida"));
        assert_eq!(post(&bz, ip(10), &cuerpo("  ", "x", None)), "/?r=vacio");
        assert_eq!(post(&bz, ip(10), &cuerpo(&"ñ".repeat(MAX_MENSAJE + 1), "", None)), "/?r=largo");
        assert_eq!(post(&bz, ip(10), &cuerpo(&"ñ".repeat(MAX_MENSAJE), &"é".repeat(MAX_APODO + 1), None)), "/?r=largo");
        assert_eq!(post(&bz, ip(10), &cuerpo(&"ñ".repeat(MAX_MENSAJE), &"é".repeat(MAX_APODO), None)), "/?r=ok");
        // cuerpo truncado (el núcleo corta en max_body)
        let mut c = cuerpo("hola", "", Some(("a", &[1u8; 100])));
        c.truncate(c.len() - 10);
        assert_eq!(post(&bz, ip(11), &c), "/?r=err");
        assert_eq!(post(&bz, ip(11), b"basura sin boundary"), "/?r=err");
        // caracteres de control fuera (salvo \n)
        post(&bz, ip(12), &cuerpo("a\u{0}b\u{1b}[31mc\u{202e}d\n e", "x\ny", None));
        let st = bz.lock();
        let m = st.msgs.last().unwrap();
        assert_eq!(m.texto, "ab[31mcd\n e");
        assert_eq!(m.apodo, "xy");
    }

    #[test]
    fn urlencoded_sin_archivo() {
        let bz = Buzon::with_dir(tmp("urlenc"));
        let r = bz.guest(&GuestReq {
            method: "POST",
            path: "/enviar",
            query: "",
            host: "x",
            user_agent: "t",
            content_type: "application/x-www-form-urlencoded",
            body: b"mensaje=hola+%26+adi%C3%B3s&apodo=Lu",
            client_ip: ip(13),
        });
        assert_eq!(r.headers[0].1, "/?r=ok");
        assert_eq!(bz.lock().msgs[0].texto, "hola & adiós");
    }

    #[test]
    fn borrar_y_nuevos() {
        let d = tmp("borrar");
        let bz = Buzon::with_dir(d.clone());
        post(&bz, ip(14), &cuerpo("uno", "", Some(("x.txt", b"abc"))));
        post(&bz, ip(14), &cuerpo("dos", "", None));
        assert_eq!(bz.resumen(), "2 mensajes · 2 nuevos");
        bz.admin_post("leidos", &[], b"");
        assert_eq!(bz.resumen(), "2 mensajes · 0 nuevos");
        post(&bz, ip(14), &cuerpo("tres", "", None));
        assert_eq!(bz.resumen(), "3 mensajes · 1 nuevos");
        let id = bz.lock().msgs[0].id.clone();
        assert_eq!(bz.admin_post("borrar", &[("id".into(), id.clone())], b""), "Mensaje #1 borrado.");
        assert!(!d.join("archivos").join(&id).exists());
        assert!(!d.join("msgs").join(format!("{id}.json")).exists());
        assert_eq!(bz.resumen(), "2 mensajes · 1 nuevos");
        assert!(bz.admin_post("borrar_todo", &[], b"").starts_with("Marca"));
        assert_eq!(bz.admin_post("borrar_todo", &[("confirmar".into(), "si".into())], b""), "2 mensajes borrados.");
        assert_eq!(bz.resumen(), "0 mensajes · 0 nuevos");
        assert_eq!(bz.lock().usado, 0);
        assert_eq!(Buzon::with_dir(d).resumen(), "0 mensajes · 0 nuevos");
    }

    #[test]
    fn carga_robusta() {
        let d = tmp("robusta");
        let bz = Buzon::with_dir(d.clone());
        post(&bz, ip(15), &cuerpo("ok", "", Some(("f", b"123"))));
        let id = bz.lock().msgs[0].id.clone();
        fs::write(d.join("msgs").join("roto.json"), "{no es json").unwrap();
        fs::write(d.join("msgs").join("x.tmp"), "medio").unwrap();
        fs::write(d.join("archivos").join("huerfano"), "zzz").unwrap();
        let bz2 = Buzon::with_dir(d.clone());
        assert_eq!(bz2.resumen(), "1 mensajes · 1 nuevos");
        assert!(!d.join("msgs").join("x.tmp").exists());
        assert!(!d.join("archivos").join("huerfano").exists());
        assert!(d.join("archivos").join(&id).exists());
    }

    #[test]
    fn json_ida_y_vuelta() {
        let m = Msg {
            id: "0123456789abcdef0123456789abcdef".into(),
            n: 42,
            ts: 1_700_000_000,
            apodo: "\"q\\\" \u{1}".into(),
            texto: "línea1\nlínea2\t😀 </script>".into(),
            adjunto: Some(Adjunto { nombre: "a b.txt".into(), tam: 9, tipo: "text/plain".into() }),
        };
        assert_eq!(Msg::from_json(&m.to_json()), Some(m));
    }

    #[test]
    fn multipart_con_filename_raro() {
        let body = format!(
            "preambulo\r\n--{B}\r\nContent-Disposition: form-data; name=\"archivo\"; filename=\"a;b \\\"c\\\".txt\"\r\n\r\nDATA\r\n--{B}--"
        );
        let c = multipart(body.as_bytes(), B.as_bytes()).unwrap();
        let a = c.archivo.unwrap();
        assert_eq!(a.nombre, "a;b \"c\".txt");
        assert_eq!(a.datos, b"DATA");
        assert_eq!(sanitizar_nombre(&a.nombre), "a_b _c_.txt");
    }
}
