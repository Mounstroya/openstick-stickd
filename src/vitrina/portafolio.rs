//! Vitrina "Portafolio": quien se conecta al WiFi ve la página de portafolio del usuario.
//!
//! El stick guarda VARIOS sitios con nombre en `data_dir/sitios/<id>/`:
//!   - subidos desde el panel (archivos estáticos, servidos tal cual), o
//!   - copias offline de un enlace (bajadas con curl), cada una con su origen.
//! Uno de ellos es el ACTIVO (`activo=<id>` en config.txt) y es el que ven los invitados.
//! Si no hay activo, se muestra la landing con el enlace y su QR (si hay enlace) o
//! "aún no hay portafolio".
//!
//! Las copias NO se actualizan solas: el botón «↻ Sincronizar» las vuelve a bajar (si el stick
//! tiene internet) y solo reemplaza la anterior si salió bien. Opción por copia `en_vivo`: si
//! el invitado tiene internet (`captive::cliente_tiene_internet`), la entrada "/" redirige al
//! URL original; los recursos y quien no tenga internet siguen con la copia offline.
//!
//! Datos de cada sitio en `sitios/<id>/.info` (clave=valor; oculto, nunca se sirve).
//! Archivos en data_dir: `config.txt`, `visitas.txt`, `sitios/`.
//! Versiones anteriores usaban `sitio/` (un solo sitio subido) y `copia/` (una sola copia):
//! `migrar()` los convierte en sitios guardados al arrancar sin perder nada.

mod copia;
mod paginas;
mod visitas;

use super::{GuestReq, GuestResp, Vitrina};
use crate::util::html;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, UNIX_EPOCH};

const MB: u64 = 1024 * 1024;
const PREFIJO: &str = "/vitrina/portafolio";
/// Tope de una copia offline (además del tope total)
const MAX_COPIA: u64 = 100 * MB;

fn una_linea(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

fn escribir_atomico(p: &Path, s: &str) {
    let tmp = p.with_file_name(format!(".{}.tmp", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
    if fs::write(&tmp, s).is_ok() {
        let _ = fs::rename(&tmp, p);
    }
}

fn claves(t: &str) -> Vec<(String, String)> {
    t.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[derive(Clone, Default, Debug)]
struct Config {
    /// id del sitio que ven los invitados ("" = ninguno → landing)
    activo: String,
    /// Enlace para la landing con QR (y el que se ofrece para descargar copia)
    url: String,
    titulo: String,
    subtitulo: String,
}

impl Config {
    fn cargar(p: &Path) -> Config {
        let mut c = Config::default();
        for (k, v) in claves(&fs::read_to_string(p).unwrap_or_default()) {
            match k.as_str() {
                "activo" => c.activo = v,
                "url" => c.url = v,
                "titulo" => c.titulo = v,
                "subtitulo" => c.subtitulo = v,
                _ => {}
            }
        }
        c
    }
    fn guardar(&self, p: &Path) {
        let s = format!(
            "version=2\nactivo={}\nurl={}\ntitulo={}\nsubtitulo={}\n",
            una_linea(&self.activo),
            una_linea(&self.url),
            una_linea(&self.titulo),
            una_linea(&self.subtitulo)
        );
        escribir_atomico(p, &s);
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
enum Tipo {
    #[default]
    Subido,
    Copia,
}

/// Un sitio guardado en el stick.
#[derive(Clone, Default, Debug)]
struct Sitio {
    id: String,
    nombre: String,
    tipo: Tipo,
    /// URL de la que se hizo la copia (solo copias)
    origen: String,
    /// "18/11/2026 22:07" (hora local de la subida o la descarga)
    fecha: String,
    /// Para ordenar: segundos locales
    ts: i64,
    /// Nota de la última descarga ("3 recursos no se pudieron bajar")
    nota: String,
    /// Solo copias: si el invitado tiene internet, la entrada ("/") lo manda al sitio real
    en_vivo: bool,
    // calculados al listar
    archivos: usize,
    bytes: u64,
}

impl Sitio {
    fn cargar(dir: &Path, id: &str) -> Sitio {
        let mut s = Sitio::info(dir, id);
        let a = listar(dir);
        s.archivos = a.len();
        s.bytes = a.iter().map(|x| x.1).sum();
        s
    }
    /// Solo los datos de `.info` (sin recorrer los archivos: barato, para cada visita).
    fn info(dir: &Path, id: &str) -> Sitio {
        let mut s = Sitio { id: id.to_string(), nombre: id.to_string(), ..Default::default() };
        for (k, v) in claves(&fs::read_to_string(dir.join(".info")).unwrap_or_default()) {
            match k.as_str() {
                "nombre" if !v.trim().is_empty() => s.nombre = v,
                "tipo" => s.tipo = if v == "copia" { Tipo::Copia } else { Tipo::Subido },
                "origen" => s.origen = v,
                "fecha" => s.fecha = v,
                "ts" => s.ts = v.parse().unwrap_or(0),
                "nota" => s.nota = v,
                "en_vivo" => s.en_vivo = v == "1",
                _ => {}
            }
        }
        s
    }
    fn guardar(&self, dir: &Path) {
        let t = format!(
            "nombre={}\ntipo={}\norigen={}\nfecha={}\nts={}\nnota={}\nen_vivo={}\n",
            una_linea(&self.nombre),
            if self.tipo == Tipo::Copia { "copia" } else { "subido" },
            una_linea(&self.origen),
            una_linea(&self.fecha),
            self.ts,
            una_linea(&self.nota),
            if self.en_vivo && self.tipo == Tipo::Copia { 1 } else { 0 }
        );
        escribir_atomico(&dir.join(".info"), &t);
    }
}

/// ids de sitio: solo [a-z0-9-], 1..=40, sin guion al inicio ni al final.
fn id_valido(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

/// Convierte un nombre libre en un id ("Mi Página 2" → "mi-pagina-2").
fn slug(nombre: &str) -> String {
    let mut o = String::new();
    for c in nombre.chars().flat_map(|c| c.to_lowercase()) {
        let c = match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            c => c,
        };
        if c.is_ascii_alphanumeric() {
            o.push(c);
        } else if !o.is_empty() && !o.ends_with('-') {
            o.push('-');
        }
        if o.len() >= 36 {
            break;
        }
    }
    let o = o.trim_matches('-').to_string();
    if o.is_empty() { "sitio".into() } else { o }
}

/// Primer id libre a partir del nombre (agrega -2, -3… si ya existe).
fn id_libre(sitios: &Path, nombre: &str) -> String {
    let base = slug(nombre);
    let mut id = base.clone();
    let mut n = 2;
    while sitios.join(&id).exists() || sitios.join(format!("{id}.nueva")).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

fn mtime_local(p: &Path) -> i64 {
    let t = fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
    t.map(|d| d.as_secs() as i64).unwrap_or(0) + visitas::desfase()
}

/// Convierte el formato viejo (`sitio/` + `copia/` + `fuente=`) en sitios guardados.
/// Idempotente: si ya no hay nada viejo no hace nada. Si la config era del formato viejo,
/// queda activo lo que se mostraba antes (la copia con `fuente=url`, si no el sitio subido).
fn migrar(dir: &Path) {
    let conf_p = dir.join("config.txt");
    let texto = fs::read_to_string(&conf_p).unwrap_or_default();
    let kv = claves(&texto);
    let get = |k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone()).unwrap_or_default();
    let vieja = get("version") != "2";
    let sitios = dir.join("sitios");
    let _ = fs::create_dir_all(&sitios);
    let mut activo: Option<String> = None;

    let copia = dir.join("copia");
    if copia.join("index.html").is_file() {
        let origen = get("copia_de");
        let origen = if origen.is_empty() { get("url") } else { origen };
        let nombre = copia::Url::parse(&origen).map(|u| u.host).unwrap_or_else(|| "Copia de mi página".into());
        let id = id_libre(&sitios, &nombre);
        let ts = mtime_local(&copia.join("index.html"));
        if fs::rename(&copia, sitios.join(&id)).is_ok() {
            let s = Sitio { id: id.clone(), nombre, tipo: Tipo::Copia, origen, fecha: visitas::fecha_hora(ts), ts, ..Default::default() };
            s.guardar(&sitios.join(&id));
            crate::util::log(&format!("portafolio: copia vieja migrada como sitio «{id}»"));
            if get("fuente") == "url" {
                activo = Some(id);
            }
        }
    }
    let sitio = dir.join("sitio");
    if !entradas_visibles(&sitio).is_empty() {
        let id = id_libre(&sitios, "sitio-subido");
        let ts = mtime_local(&sitio);
        if fs::rename(&sitio, sitios.join(&id)).is_ok() {
            let s = Sitio { id: id.clone(), nombre: "Sitio subido".into(), tipo: Tipo::Subido, fecha: visitas::fecha_hora(ts), ts, ..Default::default() };
            s.guardar(&sitios.join(&id));
            crate::util::log(&format!("portafolio: sitio subido viejo migrado como «{id}»"));
            if get("fuente") != "url" {
                activo = Some(id);
            }
        }
    } else {
        let _ = fs::remove_dir(&sitio); // vacía: ya no se usa
    }
    let _ = fs::remove_dir(&copia); // solo si quedó vacía

    if vieja {
        let mut c = Config::cargar(&conf_p);
        c.activo = activo.unwrap_or_default();
        c.guardar(&conf_p);
    }
}

struct Compartido {
    dir: PathBuf,
    conf: Mutex<Config>,
    visitas: Mutex<visitas::Visitas>,
    /// Mensaje de estado de la descarga en curso / última
    estado_descarga: Mutex<String>,
    descargando: AtomicBool,
    /// id del sitio que se está descargando
    descargando_id: Mutex<String>,
    /// Serializa subidas, borrados y el cálculo del espacio usado
    subida: Mutex<()>,
    max_archivo: u64,
    max_total: u64,
    curl: String,
}

/// ¿Este invitado tiene internet? (en producción: `captive::cliente_tiene_internet`)
type InternetCliente = Box<dyn Fn(IpAddr) -> bool + Send + Sync>;
/// ¿El stick tiene internet? (en producción: `net::internet`, hace ping)
type InternetStick = Box<dyn Fn() -> bool + Send + Sync>;

pub struct Portafolio {
    s: Arc<Compartido>,
    /// Inyectables para poder probar sin red
    cliente_internet: InternetCliente,
    stick_internet: InternetStick,
}

fn bloquear<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Decodifica %XX de una ruta de URL ('+' se queda como '+').
pub(crate) fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut o = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                o.push(v);
                i += 3;
                continue;
            }
        }
        o.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&o).into_owned()
}

/// Sanea una ruta relativa de archivo subido. Solo [A-Za-z0-9._-/ ], sin absolutas, sin
/// componentes vacíos ni que empiecen con '.' (eso excluye `..` y archivos ocultos).
fn sanear_ruta(r: &str) -> Result<String, String> {
    let r = r.trim();
    let r = r.strip_prefix("./").unwrap_or(r);
    if r.is_empty() {
        return Err("falta la ruta".into());
    }
    if r.len() > 240 {
        return Err("ruta demasiado larga".into());
    }
    if r.starts_with('/') {
        return Err("no se permiten rutas absolutas".into());
    }
    if let Some(c) = r.chars().find(|c| !(c.is_ascii_alphanumeric() || "._-/ ".contains(*c))) {
        return Err(format!("carácter no permitido «{c}» (usa solo letras sin acento, números, espacio, . _ -)"));
    }
    let comps: Vec<&str> = r.split('/').collect();
    if comps.len() > 12 {
        return Err("demasiadas carpetas anidadas".into());
    }
    for c in &comps {
        if c.trim().is_empty() {
            return Err("componente de ruta vacío".into());
        }
        if c.starts_with('.') {
            return Err("no se permiten «..» ni archivos ocultos".into());
        }
    }
    Ok(comps.join("/"))
}

fn tipo_por_ext(p: &Path) -> &'static str {
    let e = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match e.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "txt" | "md" => "text/plain",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "vcf" => "text/vcard",
        _ => "application/octet-stream",
    }
}

fn es_texto(ct: &str) -> bool {
    ct.starts_with("text/") || ct.ends_with("json") || ct.ends_with("xml") || ct == "image/svg+xml"
}

/// ¿La ruta parece una página (sin extensión o .html)? Para decidir entre redirigir a "/" o 404.
fn parece_pagina(path: &str) -> bool {
    let ult = path.rsplit('/').next().unwrap_or("");
    match ult.rsplit_once('.') {
        None => true,
        Some((_, e)) => matches!(e.to_ascii_lowercase().as_str(), "html" | "htm" | "php" | "asp" | "aspx" | "jsp"),
    }
}

enum Busqueda {
    Archivo(PathBuf),
    /// Es carpeta pero la ruta no termina en '/': redirigir agregándola
    Carpeta,
    No,
}

/// Página por defecto de una carpeta: index.html, index.htm o el primer .html.
fn indice(dir: &Path) -> Option<PathBuf> {
    for n in ["index.html", "index.htm"] {
        let p = dir.join(n);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && matches!(p.extension().and_then(|e| e.to_str()), Some("html" | "htm")))
        .collect();
    v.sort();
    v.into_iter().next()
}

/// Busca `path` (ruta de URL, decodificada) dentro de `raiz` sin salir de ella.
fn buscar(raiz: &Path, path: &str) -> Busqueda {
    let mut p = raiz.to_path_buf();
    for c in path.split('/').filter(|c| !c.is_empty()) {
        if c.starts_with('.') || c.contains('\\') || c.contains('\0') {
            return Busqueda::No;
        }
        p.push(c);
    }
    // nunca seguir enlaces simbólicos (no los creamos, pero por si acaso)
    if fs::symlink_metadata(&p).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        return Busqueda::No;
    }
    if p.is_file() {
        return Busqueda::Archivo(p);
    }
    if p.is_dir() {
        if !path.ends_with('/') && !path.is_empty() {
            return Busqueda::Carpeta;
        }
        return indice(&p).map(Busqueda::Archivo).unwrap_or(Busqueda::No);
    }
    Busqueda::No
}

fn entradas_visibles(d: &Path) -> Vec<PathBuf> {
    fs::read_dir(d)
        .map(|r| r.flatten().filter(|e| !e.file_name().to_string_lossy().starts_with('.')).map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// Raíz web de lo subido: si no hay index.html arriba y solo hay UNA carpeta, se entra en ella
/// (pasa al subir una carpeta "misitio/…").
fn raiz_web(sitio: &Path) -> PathBuf {
    let mut r = sitio.to_path_buf();
    for _ in 0..3 {
        if r.join("index.html").is_file() {
            break;
        }
        let e = entradas_visibles(&r);
        if e.len() == 1 && e[0].is_dir() {
            r = e[0].clone();
        } else {
            break;
        }
    }
    r
}

/// Lista recursiva (ruta relativa, tamaño) ordenada.
fn listar(raiz: &Path) -> Vec<(String, u64)> {
    fn rec(base: &Path, d: &Path, out: &mut Vec<(String, u64)>, prof: u32) {
        if prof > 16 {
            return;
        }
        for p in entradas_visibles(d) {
            let Ok(m) = fs::symlink_metadata(&p) else { continue };
            if m.is_dir() {
                rec(base, &p, out, prof + 1);
            } else if m.is_file() {
                let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy().into_owned();
                out.push((rel, m.len()));
            }
        }
    }
    let mut v = Vec::new();
    rec(raiz, raiz, &mut v, 0);
    v.sort();
    v
}

fn tam_humano(b: u64) -> String {
    if b >= MB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else if b >= 1024 {
        format!("{} KB", b / 1024)
    } else {
        format!("{b} B")
    }
}

fn campo<'a>(f: &'a [(String, String)], k: &str) -> &'a str {
    f.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("")
}

fn quitar_vacias(mut d: PathBuf, tope: &Path) {
    while d != tope && d.starts_with(tope) {
        if fs::remove_dir(&d).is_err() {
            break;
        }
        if !d.pop() {
            break;
        }
    }
}

impl Default for Portafolio {
    fn default() -> Self {
        Self::new()
    }
}

impl Portafolio {
    pub fn new() -> Self {
        Self::with_dir(super::data_dir("portafolio"))
    }

    /// Constructor con carpeta de datos explícita (tests).
    pub fn with_dir(dir: PathBuf) -> Self {
        Self::con_limites(dir, 25 * MB, 300 * MB, "/usr/bin/curl")
    }

    fn con_limites(dir: PathBuf, max_archivo: u64, max_total: u64, curl: &str) -> Self {
        let _ = fs::create_dir_all(&dir);
        migrar(&dir);
        // restos de una descarga interrumpida (apagón a medio bajar)
        for e in fs::read_dir(dir.join("sitios")).map(|r| r.flatten().collect::<Vec<_>>()).unwrap_or_default() {
            if e.file_name().to_string_lossy().ends_with(".nueva") {
                let _ = fs::remove_dir_all(e.path());
            }
        }
        let conf = Config::cargar(&dir.join("config.txt"));
        let visitas = visitas::Visitas::cargar(dir.join("visitas.txt"));
        Portafolio {
            s: Arc::new(Compartido {
                conf: Mutex::new(conf),
                visitas: Mutex::new(visitas),
                estado_descarga: Mutex::new(String::new()),
                descargando: AtomicBool::new(false),
                descargando_id: Mutex::new(String::new()),
                subida: Mutex::new(()),
                max_archivo,
                max_total,
                curl: curl.to_string(),
                dir,
            }),
            cliente_internet: Box::new(crate::captive::cliente_tiene_internet),
            // en tests no se hace ping: cada prueba elige si hay internet
            stick_internet: if cfg!(test) { Box::new(|| true) } else { Box::new(crate::net::internet) },
        }
    }

    fn sitios_dir(&self) -> PathBuf {
        self.s.dir.join("sitios")
    }
    /// Carpeta de un sitio existente (None si el id no es válido o no existe).
    fn dir_sitio(&self, id: &str) -> Option<PathBuf> {
        if !id_valido(id) {
            return None;
        }
        let d = self.sitios_dir().join(id);
        let es_dir = fs::symlink_metadata(&d).map(|m| m.is_dir()).unwrap_or(false);
        es_dir.then_some(d)
    }
    fn conf(&self) -> Config {
        bloquear(&self.s.conf).clone()
    }
    fn cambiar_conf(&self, f: impl FnOnce(&mut Config)) {
        let mut c = bloquear(&self.s.conf);
        f(&mut c);
        c.guardar(&self.s.dir.join("config.txt"));
    }

    /// Sitios guardados, el más reciente primero.
    fn sitios(&self) -> Vec<Sitio> {
        let mut v: Vec<Sitio> = fs::read_dir(self.sitios_dir())
            .map(|r| r.flatten().collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| {
                let id = e.file_name().to_string_lossy().into_owned();
                let d = self.dir_sitio(&id)?;
                Some(Sitio::cargar(&d, &id))
            })
            .collect();
        v.sort_by(|a, b| b.ts.cmp(&a.ts).then(a.nombre.cmp(&b.nombre)));
        v
    }

    fn uso_total(&self) -> u64 {
        listar(&self.sitios_dir()).iter().map(|x| x.1).sum()
    }

    /// Raíz web de un sitio, si tiene algo que mostrar.
    fn raiz_de(&self, id: &str) -> Option<PathBuf> {
        let d = self.dir_sitio(id)?;
        (!entradas_visibles(&d).is_empty()).then(|| raiz_web(&d))
    }

    /// Responde una ruta (ya decodificada) de `raiz` para invitado o vista previa (sin raíz →
    /// landing o "aún no hay portafolio"). `prefijo` se antepone a las redirecciones. Devuelve
    /// también si fue una página HTML (para contar visitas).
    fn responder(&self, raiz: Option<PathBuf>, path: &str, prefijo: &str) -> (GuestResp, bool) {
        let c = self.conf();
        let path = if path.is_empty() { "/" } else { path };
        let Some(raiz) = raiz else {
            if path != "/" && !parece_pagina(path) {
                return (GuestResp::not_found(), false);
            }
            let p = if c.url.is_empty() { paginas::vacia(&c.titulo) } else { paginas::landing(&c.titulo, &c.subtitulo, &c.url) };
            let mut r = GuestResp::html(p);
            r.headers.push(("Cache-Control".into(), "no-store".into()));
            r.headers.push(("X-Content-Type-Options".into(), "nosniff".into()));
            return (r, true);
        };
        match buscar(&raiz, path) {
            Busqueda::Archivo(p) => match fs::read(&p) {
                Ok(b) => {
                    let ct = tipo_por_ext(&p);
                    let es_html = ct == "text/html";
                    let ct_full = if es_texto(ct) && std::str::from_utf8(&b).is_ok() { format!("{ct}; charset=utf-8") } else { ct.to_string() };
                    let mut r = GuestResp::bytes(&ct_full, b);
                    r.headers.push(("X-Content-Type-Options".into(), "nosniff".into()));
                    r.headers.push(("Cache-Control".into(), "no-cache".into()));
                    (r, es_html)
                }
                Err(_) => (GuestResp::not_found(), false),
            },
            Busqueda::Carpeta => (GuestResp::redirect(&format!("{prefijo}{path}/")), false),
            Busqueda::No if path != "/" && parece_pagina(path) => (GuestResp::redirect(&format!("{prefijo}/")), false),
            Busqueda::No => (GuestResp::not_found(), false),
        }
    }

    fn contar(&self, ip: IpAddr) {
        bloquear(&self.s.visitas).registrar(ip, visitas::dia_local());
    }

    fn ocupado(&self, id: &str) -> bool {
        self.s.descargando.load(Ordering::SeqCst) && *bloquear(&self.s.descargando_id) == id
    }

    // ─────────── acciones de admin ───────────

    /// Crea un sitio nuevo con `nombre`, o vacía `sitio` para reemplazarlo. Responde
    /// "OK <id>" (lo lee el JS de subida) o un mensaje de error.
    fn preparar(&self, sitio: &str, nombre: &str) -> String {
        let nombre: String = nombre.trim().chars().filter(|c| !c.is_control()).take(60).collect();
        let _g = bloquear(&self.s.subida);
        let sitios = self.sitios_dir();
        let (id, mut s) = if sitio.is_empty() {
            if nombre.is_empty() {
                return "Escribe un nombre para el sitio nuevo.".into();
            }
            let id = id_libre(&sitios, &nombre);
            if fs::create_dir_all(sitios.join(&id)).is_err() {
                return "No pude crear la carpeta del sitio.".into();
            }
            (id.clone(), Sitio { id, nombre, ..Default::default() })
        } else {
            let Some(d) = self.dir_sitio(sitio) else { return "Ese sitio ya no existe.".into() };
            if self.ocupado(sitio) {
                return "Ese sitio se está descargando; espera a que termine.".into();
            }
            let mut s = Sitio::cargar(&d, sitio);
            for p in entradas_visibles(&d) {
                let _ = if p.is_dir() { fs::remove_dir_all(&p) } else { fs::remove_file(&p) };
            }
            if !nombre.is_empty() {
                s.nombre = nombre;
            }
            s.tipo = Tipo::Subido;
            s.origen.clear();
            s.nota.clear();
            s.en_vivo = false;
            (sitio.to_string(), s)
        };
        s.ts = visitas::ahora_local();
        s.fecha = visitas::fecha_hora(s.ts);
        s.guardar(&sitios.join(&id));
        format!("OK {id}")
    }

    fn subir(&self, sitio: &str, ruta: &str, body: &[u8]) -> String {
        let ruta = match sanear_ruta(ruta) {
            Ok(r) => r,
            Err(e) => return format!("No se guardó «{}»: {}.", html(ruta), html(&e)),
        };
        let _g = bloquear(&self.s.subida);
        let Some(dir) = self.dir_sitio(sitio) else { return format!("No se guardó «{}»: el sitio no existe.", html(&ruta)) };
        if self.ocupado(sitio) {
            return format!("No se guardó «{}»: ese sitio se está descargando.", html(&ruta));
        }
        if body.len() as u64 > self.s.max_archivo {
            return format!("No se guardó «{}»: pesa más de {} MB.", html(&ruta), self.s.max_archivo / MB);
        }
        let destino = dir.join(&ruta);
        let previo = fs::metadata(&destino).map(|m| m.len()).unwrap_or(0);
        let usado = self.uso_total();
        if usado - previo.min(usado) + body.len() as u64 > self.s.max_total {
            return format!("No se guardó «{}»: se pasaría del límite total de {} MB.", html(&ruta), self.s.max_total / MB);
        }
        if destino.is_dir() {
            return format!("No se guardó «{}»: ya existe una carpeta con ese nombre.", html(&ruta));
        }
        if let Some(p) = destino.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return format!("No se guardó «{}»: {}.", html(&ruta), html(&e.to_string()));
            }
        }
        let tmp = destino.with_file_name(format!(".subiendo-{}", destino.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        if let Err(e) = fs::write(&tmp, body).and_then(|_| fs::rename(&tmp, &destino)) {
            let _ = fs::remove_file(&tmp);
            return format!("No se guardó «{}»: {}.", html(&ruta), html(&e.to_string()));
        }
        format!("Archivo guardado: {} ({}).", html(&ruta), tam_humano(body.len() as u64))
    }

    fn usar(&self, sitio: &str) -> String {
        if sitio.is_empty() {
            let c = self.conf();
            self.cambiar_conf(|c| c.activo.clear());
            return if c.url.is_empty() {
                "Ya no se muestra ningún sitio: los invitados verán «aún no hay portafolio».".into()
            } else {
                "Ahora los invitados ven la página con tu enlace y su código QR.".into()
            };
        }
        let Some(d) = self.dir_sitio(sitio) else { return "Ese sitio ya no existe.".into() };
        let s = Sitio::cargar(&d, sitio);
        self.cambiar_conf(|c| c.activo = sitio.to_string());
        let aviso = if self.raiz_de(sitio).is_none() { " Ojo: todavía no tiene archivos." } else { "" };
        format!("Ahora los invitados ven «{}».{aviso}", html(&s.nombre))
    }

    fn borrar_sitio(&self, sitio: &str) -> String {
        let _g = bloquear(&self.s.subida);
        let Some(d) = self.dir_sitio(sitio) else { return "Ese sitio ya no existe.".into() };
        if self.ocupado(sitio) {
            return "Ese sitio se está descargando; espera a que termine.".into();
        }
        let s = Sitio::cargar(&d, sitio);
        if fs::remove_dir_all(&d).is_err() {
            return "No se pudo borrar.".into();
        }
        let era_activo = self.conf().activo == sitio;
        if era_activo {
            self.cambiar_conf(|c| c.activo.clear());
        }
        format!(
            "Sitio «{}» borrado.{}",
            html(&s.nombre),
            if era_activo { " Era el activo: ahora se muestra la página con el enlace y QR (o «aún no hay portafolio»)." } else { "" }
        )
    }

    fn guardar_url(&self, f: &[(String, String)]) -> Result<String, String> {
        let url = campo(f, "url").trim().to_string();
        let titulo: String = campo(f, "titulo").trim().chars().take(80).collect();
        let subtitulo: String = campo(f, "subtitulo").trim().chars().take(140).collect();
        if !url.is_empty() && (url.len() > 1000 || copia::Url::parse(&url).is_none()) {
            return Err("El enlace debe empezar con http:// o https:// (máx. 1000 caracteres).".into());
        }
        self.cambiar_conf(|c| {
            c.url = url.clone();
            c.titulo = titulo;
            c.subtitulo = subtitulo;
        });
        Ok(url)
    }

    /// Baja una copia de `url` como sitio: si ya hay una copia de ese mismo enlace (o se pasa
    /// `sitio`), la sincroniza (la reemplaza solo si la descarga salió bien); si no, crea un
    /// sitio nuevo con `nombre` (o el dominio). Si el stick no tiene internet no intenta nada.
    fn iniciar_descarga(&self, url: String, sitio: &str, nombre: &str, usar: bool, sincrono: bool) -> String {
        if url.is_empty() {
            return "Primero escribe el enlace de tu página.".into();
        }
        let Some(u) = copia::Url::parse(&url) else { return "El enlace debe empezar con http:// o https://.".into() };
        let nombre: String = nombre.trim().chars().filter(|c| !c.is_control()).take(60).collect();
        if self.s.descargando.load(Ordering::SeqCst) {
            return "Ya hay una descarga en curso; espera a que termine.".into();
        }
        if !(self.stick_internet)() {
            let m = "El stick no tiene internet ahora, así que no se intentó descargar; la copia que había sigue igual. \
Conéctalo a internet (cable USB, WiFi o LTE) y vuelve a intentar.";
            if !self.s.descargando.load(Ordering::SeqCst) {
                *bloquear(&self.s.estado_descarga) = m.into();
            }
            return m.into();
        }
        if self.s.descargando.swap(true, Ordering::SeqCst) {
            return "Ya hay una descarga en curso; espera a que termine.".into();
        }
        let (id, previo) = {
            let _g = bloquear(&self.s.subida);
            let existente = self
                .sitios()
                .into_iter()
                .find(|s| if sitio.is_empty() { s.tipo == Tipo::Copia && s.origen == url } else { s.id == sitio && s.tipo == Tipo::Copia });
            match existente {
                Some(s) => (s.id.clone(), Some(s)),
                None => (id_libre(&self.sitios_dir(), if nombre.is_empty() { &u.host } else { &nombre }), None),
            }
        };
        let usado_otros = self.uso_total() - previo.as_ref().map(|s| s.bytes).unwrap_or(0).min(self.uso_total());
        let espacio = self.s.max_total.saturating_sub(usado_otros).min(MAX_COPIA);
        if espacio < MB {
            self.s.descargando.store(false, Ordering::SeqCst);
            return format!("No hay espacio: los sitios guardados ya usan casi {} MB. Borra alguno primero.", self.s.max_total / MB);
        }
        let nombre = match &previo {
            _ if !nombre.is_empty() => nombre,
            Some(p) => p.nombre.clone(),
            None => u.host.clone(),
        };
        // sincronizar = ya existía; se conserva su opción «en vivo»
        let sincronizar = previo.is_some();
        let en_vivo = previo.as_ref().map(|p| p.en_vivo).unwrap_or(false);
        *bloquear(&self.s.descargando_id) = id.clone();
        *bloquear(&self.s.estado_descarga) =
            if sincronizar { format!("Sincronizando «{nombre}» con {url}…") } else { "Empezando la descarga…".into() };
        let s = Arc::clone(&self.s);
        let aviso_inicio = if sincronizar {
            format!("Sincronizando «{}»… El avance se ve al inicio de la sección «Portafolio».", html(&nombre))
        } else {
            "Descarga iniciada. El avance se ve al inicio de la sección «Portafolio».".into()
        };
        let trabajo = move || {
            let op = copia::Opciones {
                curl: s.curl.clone(),
                max_total: espacio,
                max_recursos: 400,
                externos: true,
                limite_tiempo: Duration::from_secs(300),
            };
            let progreso = |m: &str| *bloquear(&s.estado_descarga) = m.to_string();
            let dest = s.dir.join("sitios").join(&id);
            let r = copia::descargar(&url, &dest, &op, &progreso);
            let msg = match r {
                Ok(res) => {
                    let ts = visitas::ahora_local();
                    let fecha = visitas::fecha_hora(ts);
                    let nota = if res.fallidos > 0 { format!("{} recursos no se pudieron bajar", res.fallidos) } else { String::new() };
                    let si = Sitio { id: id.clone(), nombre: nombre.clone(), tipo: Tipo::Copia, origen: url.clone(), fecha: fecha.clone(), ts, nota: nota.clone(), en_vivo, ..Default::default() };
                    si.guardar(&dest);
                    if usar {
                        let mut c = bloquear(&s.conf);
                        c.activo = id.clone();
                        c.guardar(&s.dir.join("config.txt"));
                    }
                    format!(
                        "{} «{}»: {} archivos · {} · {}{}{}",
                        if sincronizar { "Sincronizado" } else { "Copia lista" },
                        nombre,
                        res.archivos,
                        tam_humano(res.bytes),
                        fecha,
                        if nota.is_empty() { String::new() } else { format!(" · {nota}") },
                        if usar && !sincronizar { " · ya es la que ven los invitados" } else { "" }
                    )
                }
                Err(e) if sincronizar => format!("No se pudo sincronizar «{nombre}»: {e}. Se conserva la copia anterior."),
                Err(e) => format!("Error al descargar: {e}"),
            };
            *bloquear(&s.estado_descarga) = msg;
            s.descargando.store(false, Ordering::SeqCst);
        };
        if sincrono {
            trabajo();
            html(&bloquear(&self.s.estado_descarga))
        } else {
            std::thread::spawn(trabajo);
            aviso_inicio
        }
    }

    /// Botón ↻ Sincronizar: vuelve a bajar la copia de un sitio desde su enlace de origen.
    fn sincronizar(&self, sitio: &str) -> String {
        let Some(d) = self.dir_sitio(sitio) else { return "Ese sitio ya no existe.".into() };
        let s = Sitio::info(&d, sitio);
        if s.tipo != Tipo::Copia || s.origen.is_empty() {
            return "Ese sitio no es una copia de un enlace: no hay nada que sincronizar.".into();
        }
        self.iniciar_descarga(s.origen, sitio, "", false, self.sincrono())
    }

    /// Opción por sitio copia: «Usar el sitio en vivo cuando haya internet».
    fn poner_en_vivo(&self, sitio: &str, si: bool) -> String {
        let _g = bloquear(&self.s.subida);
        let Some(d) = self.dir_sitio(sitio) else { return "Ese sitio ya no existe.".into() };
        if self.ocupado(sitio) {
            return "Ese sitio se está sincronizando; espera a que termine.".into();
        }
        let mut s = Sitio::info(&d, sitio);
        if s.tipo != Tipo::Copia || s.origen.is_empty() {
            return "Solo las copias de un enlace pueden usar el sitio en vivo.".into();
        }
        s.en_vivo = si;
        s.guardar(&d);
        if si {
            format!(
                "«{}»: los invitados con internet irán al sitio real ({}); los demás ven la copia offline.",
                html(&s.nombre),
                html(&s.origen)
            )
        } else {
            format!("«{}»: todos los invitados ven la copia offline.", html(&s.nombre))
        }
    }

    /// Si toca mandar a este invitado al sitio en vivo, el URL original. Solo en la página de
    /// entrada ("/"): los recursos (css, imágenes…) siempre salen de la copia.
    fn destino_en_vivo(&self, req: &GuestReq, path: &str) -> Option<String> {
        if path != "/" || !matches!(req.method, "GET" | "HEAD") {
            return None;
        }
        let activo = self.conf().activo;
        let d = self.dir_sitio(&activo)?;
        let s = Sitio::info(&d, &activo);
        if s.tipo != Tipo::Copia || !s.en_vivo {
            return None;
        }
        let u = copia::Url::parse(&s.origen)?;
        // al final: puede costar algo (consulta al módulo de internet para invitados)
        (self.cliente_internet)(req.client_ip).then(|| u.texto())
    }

    fn borrar_archivo(&self, sitio: &str, ruta: &str) -> String {
        let Ok(r) = sanear_ruta(ruta) else { return "Ruta no válida.".into() };
        let _g = bloquear(&self.s.subida);
        let Some(d) = self.dir_sitio(sitio) else { return "Ese sitio ya no existe.".into() };
        if self.ocupado(sitio) {
            return "Ese sitio se está descargando; espera a que termine.".into();
        }
        let p = d.join(&r);
        match fs::remove_file(&p) {
            Ok(_) => {
                if let Some(x) = p.parent() {
                    quitar_vacias(x.to_path_buf(), &d);
                }
                format!("Borrado: {}.", html(&r))
            }
            Err(_) => format!("No encontré «{}».", html(&r)),
        }
    }

    fn sincrono(&self) -> bool {
        cfg!(test)
    }
}

/// Botón ↻ Sincronizar de una copia (deshabilitado mientras haya una descarga en curso).
fn boton_sincronizar(s: &Sitio, descargando: bool, desc_id: &str) -> String {
    if s.tipo != Tipo::Copia || s.origen.is_empty() {
        return String::new();
    }
    let (txt, dis) = match (descargando, desc_id == s.id) {
        (true, true) => ("↻ Sincronizando…", " disabled"),
        (true, false) => ("↻ Sincronizar", " disabled title=\"Ya hay una descarga en curso\""),
        _ => ("↻ Sincronizar", ""),
    };
    format!(
        "<form method=post action=\"{PREFIJO}/sincronizar#v-portafolio\" class=pf-bajar data-msg=\"Sincronizando «{n}»…\"><input type=hidden name=sitio value=\"{id}\"><button class=sec title=\"Vuelve a bajar la copia desde {o}\"{dis}>{txt}</button></form>",
        n = html(&s.nombre),
        id = html(&s.id),
        o = html(&s.origen),
    )
}

/// Casilla «Usar el sitio en vivo cuando haya internet» de una copia, con su explicación.
fn opcion_en_vivo(s: &Sitio, ocupado: bool) -> String {
    if s.tipo != Tipo::Copia || s.origen.is_empty() {
        return String::new();
    }
    format!(
        "<form method=post action=\"{PREFIJO}/en_vivo#v-portafolio\" class=vivo><input type=hidden name=sitio value=\"{id}\"><input type=hidden name=valor value=\"{v}\">\
<label class=chk><input type=checkbox{chk}{dis} onchange=\"this.form.submit()\"> Usar el sitio en vivo cuando haya internet</label><noscript><button class=sec>Cambiar</button></noscript>\
<span class=mut>Si el invitado tiene internet (activaste «Internet para invitados» y él lo aceptó), al entrar va a <span class=url>{o}</span>; si no, ve esta copia offline.</span></form>",
        id = html(&s.id),
        v = if s.en_vivo { "0" } else { "1" },
        chk = if s.en_vivo { " checked" } else { "" },
        dis = if ocupado { " disabled" } else { "" },
        o = html(&s.origen),
    )
}

impl Vitrina for Portafolio {
    fn id(&self) -> &'static str {
        "portafolio"
    }
    fn nombre(&self) -> &'static str {
        "Portafolio"
    }
    fn descripcion(&self) -> &'static str {
        "Tu página web (subida o copia de un enlace) para quien se conecte"
    }
    fn ssid_por_defecto(&self) -> String {
        "Portafolio de Yael".into()
    }

    fn guest(&self, req: &GuestReq) -> GuestResp {
        let path = pct_decode(req.path);
        let path = if path.is_empty() { "/".to_string() } else { path };
        if let Some(url) = self.destino_en_vivo(req, &path) {
            self.contar(req.client_ip);
            let mut r = GuestResp::redirect(&url);
            r.headers.push(("Cache-Control".into(), "no-store".into()));
            return r;
        }
        let raiz = self.raiz_de(&self.conf().activo);
        let (r, pagina) = self.responder(raiz, &path, "");
        if pagina && r.status == 200 {
            self.contar(req.client_ip);
        }
        r
    }

    fn admin_get(&self, ruta: &str) -> Option<GuestResp> {
        let ruta = ruta.split('?').next().unwrap_or("").trim_start_matches('/');
        if ruta == "estado" {
            let t = bloquear(&self.s.estado_descarga).clone();
            let activo = self.s.descargando.load(Ordering::SeqCst);
            let mut r = GuestResp::bytes("text/plain; charset=utf-8", format!("{}\n{t}", if activo { 1 } else { 0 }).into_bytes());
            r.headers.push(("Cache-Control".into(), "no-store".into()));
            return Some(r);
        }
        // vista/… = lo que ven los invitados ahora; ver/<id>/… = un sitio guardado;
        // ver/-/ = la landing con el enlace y QR
        let (raiz, prefijo, resto) = if ruta == "vista" || ruta == "ver" {
            return Some(GuestResp::redirect(&format!("{PREFIJO}/vista/")));
        } else if let Some(resto) = ruta.strip_prefix("vista/") {
            (self.raiz_de(&self.conf().activo), format!("{PREFIJO}/vista"), resto)
        } else if let Some(r) = ruta.strip_prefix("ver/") {
            let (id, resto) = r.split_once('/').unwrap_or((r, ""));
            if !r.contains('/') {
                return Some(GuestResp::redirect(&format!("{PREFIJO}/ver/{}/", if id_valido(id) { id } else { "-" })));
            }
            if id == "-" {
                (None, format!("{PREFIJO}/ver/-"), resto)
            } else {
                self.dir_sitio(id)?;
                (self.raiz_de(id), format!("{PREFIJO}/ver/{id}"), resto)
            }
        } else {
            // GET a una acción (p.ej. recargar tras un POST): de vuelta al panel
            if matches!(ruta, "url" | "descargar" | "sincronizar" | "en_vivo" | "preparar" | "subir" | "usar" | "borrar" | "borrar_archivo" | "reiniciar_visitas") {
                return Some(GuestResp::redirect("/"));
            }
            return None;
        };
        let path = format!("/{}", pct_decode(resto));
        let (mut r, _) = self.responder(raiz, &path, &prefijo);
        // El contenido (sobre todo una copia de un sitio ajeno) corre aislado: origen opaco,
        // sin formularios ni fetch, para que no pueda tocar el panel de admin.
        r.headers.push(("Content-Security-Policy".into(), "sandbox allow-scripts allow-popups; connect-src 'none'; form-action 'none'".into()));
        Some(r)
    }

    fn admin_post(&self, accion: &str, form: &[(String, String)], body: &[u8]) -> String {
        match accion {
            "preparar" => self.preparar(campo(form, "sitio"), campo(form, "nombre")),
            "subir" => self.subir(campo(form, "sitio"), campo(form, "ruta"), body),
            "usar" => self.usar(campo(form, "sitio")),
            "borrar" => self.borrar_sitio(campo(form, "sitio")),
            "url" => match self.guardar_url(form) {
                Ok(u) if u.is_empty() => "Datos guardados (sin enlace).".into(),
                Ok(_) => "Enlace guardado.".into(),
                Err(e) => e,
            },
            "sincronizar" => self.sincronizar(campo(form, "sitio")),
            "en_vivo" => self.poner_en_vivo(campo(form, "sitio"), campo(form, "valor") == "1"),
            "descargar" => {
                let usar = campo(form, "usar") == "1";
                let sitio = campo(form, "sitio");
                if !sitio.is_empty() {
                    // formato viejo del botón «Volver a descargar»: es lo mismo que sincronizar
                    return self.sincronizar(sitio);
                }
                let url = if form.iter().any(|(k, _)| k == "url") {
                    match self.guardar_url(form) {
                        Ok(u) => u,
                        Err(e) => return e,
                    }
                } else {
                    self.conf().url
                };
                self.iniciar_descarga(url, "", campo(form, "nombre"), usar, self.sincrono())
            }
            "borrar_archivo" => self.borrar_archivo(campo(form, "sitio"), campo(form, "ruta")),
            "reiniciar_visitas" => {
                bloquear(&self.s.visitas).reiniciar();
                "Contador de visitas en cero.".into()
            }
            _ => "Acción desconocida.".into(),
        }
    }

    fn resumen(&self) -> String {
        let v = bloquear(&self.s.visitas);
        let hoy = v.de_hoy(visitas::dia_local());
        format!("{hoy} {} hoy · {} en total", if hoy == 1 { "visita" } else { "visitas" }, v.total)
    }

    fn abierta(&self) -> bool {
        self.s.descargando.load(Ordering::SeqCst)
    }

    fn admin_html(&self) -> String {
        let c = self.conf();
        let sitios = self.sitios();
        let usado: u64 = sitios.iter().map(|s| s.bytes).sum();
        let descargando = self.s.descargando.load(Ordering::SeqCst);
        let desc_id = bloquear(&self.s.descargando_id).clone();
        let estado_desc = bloquear(&self.s.estado_descarga).clone();
        let activo = sitios.iter().find(|s| s.id == c.activo && self.raiz_de(&s.id).is_some());

        let ahora = match activo {
            Some(s) if s.tipo == Tipo::Copia => format!(
                "<b>{}</b> <span class=mut>(copia de <span class=url>{}</span> · sincronizada {}{})</span>",
                html(&s.nombre),
                html(&s.origen),
                if s.fecha.is_empty() { "sin fecha".into() } else { html(&s.fecha) },
                if s.en_vivo { " · en vivo para quien tenga internet" } else { "" }
            ),
            Some(s) => format!("<b>{}</b> <span class=mut>(sitio subido)</span>", html(&s.nombre)),
            None if !c.url.is_empty() => format!("<b>Página con tu enlace y código QR</b> <span class=mut>({})</span>", html(&c.url)),
            None => "<b>Nada todavía</b>: ven «aún no hay portafolio».".into(),
        };
        let sinc_activo = activo.map(|s| boton_sincronizar(s, descargando, &desc_id)).unwrap_or_default();

        // ── visitas ──
        let (dia, hoy, tot, hist) = {
            let v = bloquear(&self.s.visitas);
            let d = visitas::dia_local();
            (d, v.de_hoy(d), v.total, v.historial.clone())
        };
        let mut barras: Vec<(i64, u64)> = hist.into_iter().filter(|(x, _)| *x > dia - 7 && *x < dia).collect();
        barras.push((dia, hoy));
        let maxb = barras.iter().map(|b| b.1).max().unwrap_or(1).max(1);
        let barras_html: String = (0..7)
            .map(|k| {
                let d = dia - 6 + k;
                let n = barras.iter().find(|b| b.0 == d).map(|b| b.1).unwrap_or(0);
                let (_, m, dd) = visitas::civil(d);
                format!(
                    "<div title=\"{n} visitas\"><i style=\"height:{}%\"></i><small>{n}</small><small>{dd}/{m}</small></div>",
                    (n * 100 / maxb).max(2)
                )
            })
            .collect();

        // ── lista de sitios ──
        let mut lista = String::new();
        for s in &sitios {
            let es_activo = activo.map(|a| a.id == s.id).unwrap_or(false);
            let ocupado = descargando && desc_id == s.id;
            let id = html(&s.id);
            let tipo = match s.tipo {
                Tipo::Copia => format!("Copia de <span class=url>{}</span>", html(&s.origen)),
                Tipo::Subido => "Sitio subido".into(),
            };
            let detalles = format!(
                "{tipo} · {} · {} {} · {}{}",
                tam_humano(s.bytes),
                s.archivos,
                if s.archivos == 1 { "archivo" } else { "archivos" },
                if s.fecha.is_empty() { "sin fecha".into() } else { html(&s.fecha) },
                if s.nota.is_empty() { String::new() } else { format!(" · {}", html(&s.nota)) }
            );
            let usar = if es_activo {
                String::new()
            } else {
                format!("<form method=post action={PREFIJO}/usar><input type=hidden name=sitio value=\"{id}\"><button>Usar este</button></form>")
            };
            let actualizar = boton_sincronizar(s, descargando, &desc_id);
            let vivo = opcion_en_vivo(s, ocupado);
            let borrar = if ocupado {
                "<span class=mut>descargando…</span>".to_string()
            } else {
                format!(
                    "<form method=post action={PREFIJO}/borrar onsubmit=\"return confirm('¿Borrar este sitio del stick? No se puede deshacer.')\"><input type=hidden name=sitio value=\"{id}\"><button class=peligro>Borrar</button></form>"
                )
            };
            let archivos = listar(&self.sitios_dir().join(&s.id));
            let mostrar = 300;
            let filas: String = archivos
                .iter()
                .take(mostrar)
                .map(|(r, t)| {
                    let quitar = if s.tipo == Tipo::Subido {
                        format!(
                            "<form method=post action={PREFIJO}/borrar_archivo><input type=hidden name=sitio value=\"{id}\"><input type=hidden name=ruta value=\"{r}\"><button class=link>quitar</button></form>",
                            r = html(r)
                        )
                    } else {
                        String::new()
                    };
                    format!("<li><span>{}<small> · {}</small></span>{quitar}</li>", html(r), tam_humano(*t))
                })
                .collect();
            let mas = if archivos.len() > mostrar { format!("<li>… y {} más</li>", archivos.len() - mostrar) } else { String::new() };
            lista += &format!(
                r#"<li class="sitio{cls}"><div class=nom><b>{nombre}</b>{tag}</div><div class=mut>{detalles}</div>
<div class=fila>{usar}<a class="btn sec" href="{PREFIJO}/ver/{id}/" target=_blank rel=noopener>Vista previa</a>{actualizar}{borrar}</div>{vivo}
<details class=chica><summary>Ver archivos ({n})</summary><ul class=archivos>{filas}{mas}</ul></details></li>"#,
                cls = if es_activo { " act" } else { "" },
                nombre = html(&s.nombre),
                tag = if es_activo { " <span class=tag>ACTIVO · lo ven los invitados</span>" } else { "" },
                n = archivos.len(),
            );
        }
        // la landing también se puede elegir
        if !c.url.is_empty() || activo.is_none() {
            let es = activo.is_none();
            lista += &format!(
                r#"<li class="sitio{cls}"><div class=nom><b>{nom}</b>{tag}</div><div class=mut>{det}</div><div class=fila>{usar}<a class="btn sec" href="{PREFIJO}/ver/-/" target=_blank rel=noopener>Vista previa</a></div></li>"#,
                cls = if es { " act" } else { "" },
                nom = if c.url.is_empty() { "Sin sitio («aún no hay portafolio»)" } else { "Página con tu enlace y código QR" },
                tag = if es { " <span class=tag>ACTIVO · lo ven los invitados</span>" } else { "" },
                det = if c.url.is_empty() { "Se muestra cuando no eliges ningún sitio. Escribe tu enlace abajo para que muestre un QR.".into() } else { format!("No es una copia: solo el enlace <span class=url>{}</span> en grande y su QR, para abrirlo con datos.", html(&c.url)) },
                usar = if es { String::new() } else { format!("<form method=post action={PREFIJO}/usar><input type=hidden name=sitio value=\"\"><button>Usar esta</button></form>") },
            );
        }

        let destinos: String = sitios
            .iter()
            .filter(|s| !(descargando && desc_id == s.id))
            .map(|s| format!("<option value=\"{}\">Reemplazar «{}»</option>", html(&s.id), html(&s.nombre)))
            .collect();

        format!(
            r#"<div class=pf><style>
.pf .url{{word-break:break-all}}.pf h3{{font-size:16px;margin:22px 0 8px;padding-top:14px;border-top:1px solid var(--bd)}}
.pf .fila{{display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin:10px 0 4px}}.pf .fila form{{margin:0}}
.pf .ahora{{background:var(--soft);border-radius:10px;padding:10px 12px;margin:0 0 6px}}
.pf ul.sitios>li{{display:block;border:1px solid var(--bd);border-radius:10px;padding:10px 12px;margin-bottom:10px}}
.pf ul.sitios>li.act{{border:2px solid var(--ok)}}.pf .nom{{font-size:16px;word-break:break-word}}
.pf .tag{{display:inline-block;background:var(--ok);color:#06210f;font-size:12px;font-weight:700;border-radius:6px;padding:1px 7px;margin-left:4px;vertical-align:middle}}
.pf .mut{{font-size:14px}}.pf ul.archivos{{max-height:240px;overflow:auto;font-size:14px;margin:6px 0 0}}.pf ul.archivos span{{word-break:break-all}}
.pf .barras{{display:grid;grid-template-columns:repeat(7,1fr);gap:6px;height:92px;align-items:end;margin:8px 0}}
.pf .barras div{{display:flex;flex-direction:column;justify-content:flex-end;align-items:center;height:100%}}
.pf .barras i{{display:block;width:100%;max-width:28px;background:var(--ac);border-radius:4px 4px 0 0}}
.pf .barras small{{font-size:11px;color:var(--mut);line-height:1.2}}
.pf .progreso{{border:1px solid var(--bd);border-radius:10px;padding:10px 12px;margin:10px 0}}.pf .progreso[hidden]{{display:none}}
.pf progress{{width:100%;height:14px;margin:0 0 6px}}.pf #pf-subida,.pf #pf-estado{{font-size:14px;white-space:pre-wrap;word-break:break-word}}
.pf .chk{{display:flex;gap:8px;align-items:center;margin:4px 0 10px}}.pf form.vivo{{margin:6px 0 2px}}.pf form.vivo .chk{{margin:0 0 2px}}.pf input[type=file]{{max-width:100%;margin:4px 0 10px}}
</style>
<div class=ahora>Los invitados ven ahora: {ahora}<div class=fila><a class="btn sec" href="{PREFIJO}/vista/" target=_blank rel=noopener>Ver lo que ven ↗</a>{sinc_activo}</div></div>
<div id=pf-progreso class=progreso data-activo={activo_n}{oculto}>
<progress id=pf-barra{oculto_barra}></progress>
<div id=pf-estado role=status aria-live=polite>{estado}</div></div>

<h3>Sitios guardados en el stick</h3>
<p class=mut style="margin-top:0">{n_sitios} · usan {usado} de {max_total} MB. Toca «Usar este» para elegir cuál ven los invitados.</p>
<ul class=sitios>{lista}</ul>

<h3>Copiar una página web al stick</h3>
<p class=mut style="margin-top:0">El stick baja la página con sus imágenes y estilos para mostrarla sin internet. Cada copia se guarda como un sitio más; no se actualiza sola: usa «↻ Sincronizar» cuando cambies tu página.</p>
<form method=post action="{PREFIJO}/descargar#v-portafolio" class=pf-bajar>
<label>Enlace de tu página<input type=url name=url value="{url}" placeholder="https://tu-sitio.com" maxlength=1000 inputmode=url autocomplete=off></label>
<label>Nombre de la copia (opcional)<input type=text name=nombre maxlength=60 placeholder="si lo dejas vacío, se usa el dominio"></label>
<label class=chk><input type=checkbox name=usar value=1 checked> Mostrarla a los invitados al terminar</label>
<details class=chica><summary>Página con enlace y QR (cuando no hay sitio)</summary>
<p class=mut>Si eliges «Página con tu enlace y código QR», los invitados ven esto con el enlace de arriba.</p>
<label>Tu nombre<input type=text name=titulo value="{titulo}" placeholder="Yael Montoya" maxlength=80></label>
<label>Frase corta<input type=text name=subtitulo value="{subtitulo}" placeholder="Diseño · Desarrollo · Fotografía" maxlength=140></label>
</details>
<div class=fila><button {dis_bajar}>Guardar y descargar copia</button><button formaction={PREFIJO}/url class=sec>Solo guardar el enlace</button></div></form>
<p class=mut>Máx. 100 MB y 400 archivos por copia; tarda de segundos a unos minutos. Las páginas que arman su contenido con JavaScript desde internet pueden verse incompletas: revisa la vista previa.</p>

<h3>Subir un sitio desde esta computadora</h3>
<p class=mut style="margin-top:0">Sube la carpeta de tu sitio (con <code>index.html</code>) o archivos sueltos. Nombres de archivo solo con letras sin acento, números, espacio, <code>. _ -</code>. Máx. 25 MB por archivo y 300 MB entre todos los sitios.</p>
<label>Guardar como<select id=pf-destino><option value="">Sitio nuevo</option>{destinos}</select></label>
<label id=pf-nombre-l>Nombre del sitio nuevo<input type=text id=pf-nombre maxlength=60 placeholder="Mi portafolio"></label>
<label>Carpeta<input type=file id=pf-carpeta webkitdirectory multiple></label>
<label>…o archivos sueltos<input type=file id=pf-archivos multiple></label>
<label class=chk><input type=checkbox id=pf-usar checked> Mostrarlo a los invitados al terminar</label>
<div class=fila><button type=button id=pf-subir>Subir</button></div>
<div id=pf-subida role=status aria-live=polite></div>

<h3>Visitas</h3>
<p class=mut style="margin:0">{hoy} hoy · {tot} en total (cada celular cuenta una vez por día)</p>
<div class=barras>{barras_html}</div>
<form method=post action={PREFIJO}/reiniciar_visitas onsubmit="return confirm('¿Poner el contador en cero?')"><button class=link>Reiniciar contador</button></form>
<script>(function(){{
var P='{PREFIJO}',$=function(i){{return document.getElementById(i)}},enc=encodeURIComponent;
var de=$('pf-destino');de.onchange=function(){{$('pf-nombre-l').hidden=!!de.value}};
function lista(inp,carpeta){{return Array.prototype.map.call(inp.files||[],function(f){{
var r=(carpeta&&f.webkitRelativePath)?f.webkitRelativePath:f.name;
if(carpeta&&f.webkitRelativePath){{var i=r.indexOf('/');if(i>0)r=r.slice(i+1)}}return [f,r]}})
.filter(function(x){{return !x[1].split('/').some(function(c){{return c.charAt(0)=='.'}})}})}}
$('pf-subir').onclick=async function(){{
var t=lista($('pf-carpeta'),true).concat(lista($('pf-archivos'),false)),st=$('pf-subida'),ok=0,mal=[],b=this;
var dest=de.value,nom=$('pf-nombre').value.trim();
if(!t.length){{st.textContent='Elige una carpeta o archivos primero.';return}}
if(!dest&&!nom){{st.textContent='Escribe un nombre para el sitio nuevo.';$('pf-nombre').focus();return}}
if(dest&&!confirm('Se borrarán los archivos que tenía ese sitio y se pondrán los nuevos. ¿Seguir?'))return;
b.disabled=true;st.textContent='Preparando…';
try{{var rp=await fetch(P+'/preparar?sitio='+enc(dest)+'&nombre='+enc(nom),{{method:'POST'}});var x=await rp.text()}}catch(e){{x=String(e)}}
if(x.indexOf('OK ')!=0){{st.textContent=x;b.disabled=false;return}}
var id=x.slice(3).trim();
for(var i=0;i<t.length;i++){{var f=t[i][0],r=t[i][1];
st.textContent='Subiendo '+(i+1)+' de '+t.length+': '+r;
if(f.size>25*1024*1024){{mal.push(r+': pesa más de 25 MB');continue}}
try{{var resp=await fetch(P+'/subir?sitio='+enc(id)+'&ruta='+enc(r),{{method:'POST',body:f}});var y=await resp.text();
if(resp.ok&&y.indexOf('Archivo guardado')>=0)ok++;else{{var m=y.match(/No se guard[^<]*/);mal.push(m?m[0]:r+': error '+resp.status)}}}}
catch(e){{mal.push(r+': '+e)}}}}
if(ok&&$('pf-usar').checked){{try{{await fetch(P+'/usar?sitio='+enc(id),{{method:'POST'}})}}catch(_){{}}}}
st.textContent='Listo: '+ok+' de '+t.length+' archivos subidos.'+(mal.length?'\n'+mal.join('\n'):'')+'\nRecargando…';
setTimeout(function(){{location.href='/#v-portafolio'}},mal.length?6000:1200)}};
var pg=$('pf-progreso'),e=$('pf-estado'),barra=$('pf-barra');
Array.prototype.forEach.call(document.querySelectorAll('form.pf-bajar'),function(fm){{fm.addEventListener('submit',function(ev){{
if(ev.submitter&&ev.submitter.getAttribute('formaction'))return;
pg.hidden=false;barra.hidden=false;e.textContent=fm.dataset.msg||'Iniciando la descarga…';pg.scrollIntoView({{block:'nearest'}})}})}});
if(pg.dataset.activo=='1'){{var iv=setInterval(async function(){{try{{
var x=await (await fetch(P+'/estado',{{cache:'no-store'}})).text();var a=x.split('\n');e.textContent=a.slice(1).join('\n');
if(a[0]!='1'){{clearInterval(iv);barra.hidden=true;setTimeout(function(){{location.href='/#v-portafolio'}},2500)}}}}catch(_){{}}}},1500)}}
}})();</script></div>"#,
            n_sitios = match sitios.len() {
                0 => "Aún no hay sitios guardados".to_string(),
                1 => "1 sitio".to_string(),
                n => format!("{n} sitios"),
            },
            usado = tam_humano(usado),
            max_total = self.s.max_total / MB,
            url = html(&c.url),
            titulo = html(&c.titulo),
            subtitulo = html(&c.subtitulo),
            dis_bajar = if descargando { "disabled title=\"Ya hay una descarga en curso\"" } else { "" },
            activo_n = if descargando { 1 } else { 0 },
            oculto = if descargando || !estado_desc.is_empty() { "" } else { " hidden" },
            oculto_barra = if descargando { "" } else { " hidden" },
            estado = html(&estado_desc),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(nombre: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("pf-test-{}-{nombre}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 4, n))
    }
    fn get(p: &Portafolio, path: &str, n: u8) -> GuestResp {
        p.guest(&GuestReq {
            method: "GET",
            path,
            query: "",
            host: "cualquier.com",
            user_agent: "CaptiveNetworkSupport",
            content_type: "",
            body: &[],
            client_ip: ip(n),
        })
    }
    fn texto(r: &GuestResp) -> String {
        String::from_utf8_lossy(&r.body).into_owned()
    }
    fn header<'a>(r: &'a GuestResp, k: &str) -> Option<&'a str> {
        r.headers.iter().find(|h| h.0.eq_ignore_ascii_case(k)).map(|h| h.1.as_str())
    }
    fn kv(pares: &[(&str, &str)]) -> Vec<(String, String)> {
        pares.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }
    /// Crea un sitio nuevo y devuelve su id
    fn nuevo(p: &Portafolio, nombre: &str) -> String {
        let r = p.admin_post("preparar", &kv(&[("sitio", ""), ("nombre", nombre)]), &[]);
        r.strip_prefix("OK ").unwrap_or_else(|| panic!("{r}")).to_string()
    }
    fn subir(p: &Portafolio, id: &str, ruta: &str, b: &[u8]) -> String {
        p.admin_post("subir", &kv(&[("sitio", id), ("ruta", ruta)]), b)
    }
    fn usar(p: &Portafolio, id: &str) -> String {
        p.admin_post("usar", &kv(&[("sitio", id)]), &[])
    }

    #[test]
    fn sanea_rutas() {
        assert_eq!(sanear_ruta("css/estilo.css").unwrap(), "css/estilo.css");
        assert_eq!(sanear_ruta("./mi foto.jpg").unwrap(), "mi foto.jpg");
        for mala in ["", "/etc/passwd", "../x", "a/../../x", "a//b", ".env", "a/.git/config", "a\\b", "niño.jpg", "a/b\0", "x;rm", ".info"] {
            assert!(sanear_ruta(mala).is_err(), "debió rechazar {mala:?}");
        }
    }

    #[test]
    fn ids_de_sitio() {
        assert_eq!(slug("Mi Página  #2"), "mi-pagina-2");
        assert_eq!(slug("mounstroya.xyz"), "mounstroya-xyz");
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("¡¡!!"), "sitio");
        for bueno in ["a", "mi-sitio-2"] {
            assert!(id_valido(bueno));
        }
        for malo in ["", "-", "-a", "a-", "A", "a.b", "..", "a/b", "sitio.nueva", &"x".repeat(41)] {
            assert!(!id_valido(malo), "{malo}");
        }
    }

    #[test]
    fn decodifica_rutas() {
        assert_eq!(pct_decode("/mi%20foto.jpg"), "/mi foto.jpg");
        assert_eq!(pct_decode("/a+b%2"), "/a+b%2");
        assert_eq!(pct_decode("%2e%2e/x"), "../x");
    }

    #[test]
    fn sin_nada_muestra_vacia() {
        let t = tmp("vacia");
        let p = Portafolio::with_dir(t.0.clone());
        let r = get(&p, "/", 1);
        assert_eq!(r.status, 200);
        assert!(texto(&r).contains("Aún no hay portafolio"));
        assert_eq!(get(&p, "/logo.png", 1).status, 404);
        assert!(p.resumen().starts_with("1 visita hoy"), "{}", p.resumen());
        assert!(p.admin_html().contains("Aún no hay sitios guardados"));
    }

    #[test]
    fn sube_y_sirve_sitio() {
        let t = tmp("sitio");
        let p = Portafolio::with_dir(t.0.clone());
        let id = nuevo(&p, "Mi sitio");
        assert_eq!(id, "mi-sitio");
        let html_usuario = b"<html><body><h1>Hola <b>mundo</b></h1><script>1<2</script></body></html>";
        assert!(subir(&p, &id, "index.html", html_usuario).contains("Archivo guardado"));
        assert!(subir(&p, &id, "css/estilo.css", b"body{color:red}").contains("Archivo guardado"));
        assert!(subir(&p, &id, "proyectos/index.html", b"<p>proy</p>").contains("Archivo guardado"));
        assert!(subir(&p, &id, "img/mi foto.png", &[0x89, b'P', b'N', b'G', 0xff]).contains("Archivo guardado"));
        assert!(subir(&p, "no-existe", "a.txt", b"x").contains("no existe"));
        assert!(subir(&p, "../x", "a.txt", b"x").contains("no existe"));

        // subido pero aún no activo: se sigue viendo «aún no hay»
        assert!(texto(&get(&p, "/", 1)).contains("Aún no hay portafolio"));
        assert!(usar(&p, &id).contains("Mi sitio"));

        let r = get(&p, "/", 1);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, html_usuario, "el HTML del usuario se sirve sin escapar");
        assert_eq!(r.content_type, "text/html; charset=utf-8");
        assert_eq!(header(&r, "X-Content-Type-Options"), Some("nosniff"));

        let r = get(&p, "/css/estilo.css", 1);
        assert_eq!(r.content_type, "text/css; charset=utf-8");
        let r = get(&p, "/img/mi%20foto.png", 1);
        assert_eq!(r.content_type, "image/png");
        assert_eq!(r.body.len(), 5);

        let r = get(&p, "/proyectos", 1);
        assert_eq!((r.status, header(&r, "Location")), (302, Some("/proyectos/")));
        assert_eq!(texto(&get(&p, "/proyectos/", 1)), "<p>proy</p>");

        // traversal y ocultos (.info del sitio nunca se sirve)
        for mala in ["/../config.txt", "/%2e%2e/config.txt", "/..%2fconfig.txt", "/.subiendo-x", "/.info", "/%2einfo"] {
            let r = get(&p, mala, 1);
            assert!(r.status == 404 || r.status == 302, "{mala}: {}", r.status);
            assert!(!texto(&r).contains("activo=") && !texto(&r).contains("nombre="), "{mala} filtró datos");
        }
        assert_eq!(get(&p, "/no-existe.png", 1).status, 404);
        let r = get(&p, "/pagina/rara", 1);
        assert_eq!((r.status, header(&r, "Location")), (302, Some("/")));

        // visitas: únicas por IP (solo páginas)
        get(&p, "/", 2);
        assert!(p.resumen().starts_with("2 visitas hoy · 2 en total"), "{}", p.resumen());

        // panel: lista escapada y vistas previas aisladas
        let a = p.admin_html();
        assert!(a.contains("img/mi foto.png"));
        let v = p.admin_get("vista/css/estilo.css").unwrap();
        assert_eq!(v.body, b"body{color:red}");
        assert!(header(&v, "Content-Security-Policy").unwrap().contains("sandbox"));
        let v = p.admin_get("vista/proyectos").unwrap();
        assert_eq!(header(&v, "Location"), Some("/vitrina/portafolio/vista/proyectos/"));
        assert_eq!(header(&p.admin_get("vista").unwrap(), "Location"), Some("/vitrina/portafolio/vista/"));
        let v = p.admin_get("ver/mi-sitio/proyectos").unwrap();
        assert_eq!(header(&v, "Location"), Some("/vitrina/portafolio/ver/mi-sitio/proyectos/"));
        let v = p.admin_get("ver/mi-sitio/css/estilo.css").unwrap();
        assert!(header(&v, "Content-Security-Policy").unwrap().contains("sandbox"));
        assert!(p.admin_get("ver/../config.txt").is_none() || p.admin_get("ver/../config.txt").unwrap().status != 200);
        assert!(p.admin_get("ver/otro/").is_none());
        let v = p.admin_get("ver/-/").unwrap();
        assert!(texto(&v).contains("Aún no hay portafolio") && header(&v, "Content-Security-Policy").is_some());

        // borrar un archivo y luego el sitio (activo → vuelve a «aún no hay»)
        assert!(p.admin_post("borrar_archivo", &kv(&[("sitio", &id), ("ruta", "proyectos/index.html")]), &[]).contains("Borrado"));
        assert!(!t.0.join("sitios/mi-sitio/proyectos").exists());
        assert!(p.admin_post("borrar", &kv(&[("sitio", &id)]), &[]).contains("Era el activo"));
        assert!(!t.0.join("sitios/mi-sitio").exists());
        assert!(texto(&get(&p, "/", 1)).contains("Aún no hay portafolio"));
        assert_eq!(p.conf().activo, "");
    }

    #[test]
    fn varios_sitios_y_lista() {
        let t = tmp("varios");
        let p = Portafolio::with_dir(t.0.clone());
        let a = nuevo(&p, "Portafolio <2024>");
        let b = nuevo(&p, "Portafolio <2024>");
        assert_eq!((a.as_str(), b.as_str()), ("portafolio-2024", "portafolio-2024-2"));
        subir(&p, &a, "index.html", b"<p>A</p>");
        subir(&p, &b, "index.html", b"<p>B</p>");
        subir(&p, &b, "x.css", b"xx");
        usar(&p, &a);
        assert_eq!(texto(&get(&p, "/", 1)), "<p>A</p>");
        usar(&p, &b);
        assert_eq!(texto(&get(&p, "/", 1)), "<p>B</p>");
        // vista previa de uno que NO es el activo
        assert_eq!(texto(&p.admin_get(&format!("ver/{a}/")).unwrap()), "<p>A</p>");

        let l = p.sitios();
        assert_eq!(l.len(), 2);
        let sb = l.iter().find(|s| s.id == b).unwrap();
        assert_eq!((sb.archivos, sb.bytes, sb.tipo), (2, 10, Tipo::Subido));
        assert_eq!(sb.nombre, "Portafolio <2024>");

        let h = p.admin_html();
        assert!(!h.contains("Portafolio <2024>"), "el nombre va escapado");
        assert!(h.contains("Portafolio &lt;2024&gt;"));
        assert_eq!(h.matches("ACTIVO").count(), 1);
        assert!(h.contains("2 sitios"));
        assert!(h.contains(&format!("/ver/{a}/")) && h.contains("Usar este") && h.contains("Borrar"));
        assert!(h.contains("Reemplazar «Portafolio &lt;2024&gt;»"));

        // reemplazar: vacía el sitio y conserva el id (sigue activo)
        assert_eq!(p.admin_post("preparar", &kv(&[("sitio", &b), ("nombre", "Nuevo B")]), &[]), format!("OK {b}"));
        assert!(!t.0.join("sitios").join(&b).join("x.css").exists());
        subir(&p, &b, "index.html", b"<p>B2</p>");
        assert_eq!(texto(&get(&p, "/", 1)), "<p>B2</p>");
        assert_eq!(p.sitios().iter().find(|s| s.id == b).unwrap().nombre, "Nuevo B");
        assert!(p.admin_post("preparar", &kv(&[("sitio", "no-hay")]), &[]).contains("ya no existe"));
        assert!(p.admin_post("preparar", &kv(&[("sitio", ""), ("nombre", "  ")]), &[]).contains("nombre"));

        // persistencia
        let p2 = Portafolio::with_dir(t.0.clone());
        assert_eq!(p2.conf().activo, b);
        assert_eq!(p2.sitios().len(), 2);
        // usar «ninguno»
        assert!(usar(&p2, "").contains("aún no hay"));
        assert!(texto(&get(&p2, "/", 1)).contains("Aún no hay portafolio"));
        assert!(usar(&p2, "../etc").contains("ya no existe"));
    }

    #[test]
    fn migra_formato_viejo() {
        // lo que hay hoy en el stick: copia de un enlace activa + sitio/ vacío
        let t = tmp("migra");
        fs::write(
            t.0.join("config.txt"),
            "fuente=url\nurl=https://mounstroya.xyz/\ntitulo=\nsubtitulo=\ncopia_info=7 archivos · 159 KB · 18/11/2026 22:07\ncopia_de=https://mounstroya.xyz/\n",
        )
        .unwrap();
        fs::create_dir_all(t.0.join("copia/imagenes")).unwrap();
        fs::write(t.0.join("copia/index.html"), "<p>mounstroya</p>").unwrap();
        fs::write(t.0.join("copia/imagenes/a.png"), [1, 2, 3]).unwrap();
        fs::create_dir_all(t.0.join("sitio")).unwrap();
        fs::write(t.0.join("visitas.txt"), "").unwrap();

        let p = Portafolio::with_dir(t.0.clone());
        assert_eq!(texto(&get(&p, "/", 1)), "<p>mounstroya</p>", "debe seguir sirviendo la copia");
        assert_eq!(get(&p, "/imagenes/a.png", 1).body, vec![1, 2, 3]);
        let l = p.sitios();
        assert_eq!(l.len(), 1);
        let s = &l[0];
        assert_eq!((s.id.as_str(), s.nombre.as_str(), s.tipo, s.origen.as_str()), ("mounstroya-xyz", "mounstroya.xyz", Tipo::Copia, "https://mounstroya.xyz/"));
        assert_eq!(s.archivos, 2);
        assert!(!s.fecha.is_empty());
        assert_eq!(p.conf().activo, "mounstroya-xyz");
        assert_eq!(p.conf().url, "https://mounstroya.xyz/", "el enlace de la landing se conserva");
        assert!(!t.0.join("copia").exists() && !t.0.join("sitio").exists());
        let conf = fs::read_to_string(t.0.join("config.txt")).unwrap();
        assert!(conf.contains("version=2") && conf.contains("activo=mounstroya-xyz"), "{conf}");
        assert!(p.admin_html().contains("ACTIVO"));

        // idempotente: arrancar de nuevo no cambia nada
        drop(p);
        let p = Portafolio::with_dir(t.0.clone());
        assert_eq!(p.sitios().len(), 1);
        assert_eq!(p.conf().activo, "mounstroya-xyz");
        assert_eq!(texto(&get(&p, "/", 1)), "<p>mounstroya</p>");
    }

    #[test]
    fn migra_sitio_subido_y_copia() {
        let t = tmp("migra2");
        fs::write(t.0.join("config.txt"), "fuente=sitio\nurl=https://a.com/\ncopia_de=https://a.com/\n").unwrap();
        fs::create_dir_all(t.0.join("copia")).unwrap();
        fs::write(t.0.join("copia/index.html"), "<p>copia</p>").unwrap();
        fs::create_dir_all(t.0.join("sitio/misitio")).unwrap();
        fs::write(t.0.join("sitio/misitio/index.html"), "<p>subido</p>").unwrap();
        let p = Portafolio::with_dir(t.0.clone());
        assert_eq!(p.sitios().len(), 2);
        assert_eq!(p.conf().activo, "sitio-subido");
        assert_eq!(texto(&get(&p, "/", 1)), "<p>subido</p>");
        usar(&p, "a-com");
        assert_eq!(texto(&get(&p, "/", 1)), "<p>copia</p>");
    }

    #[test]
    fn migra_enlace_sin_copia() {
        let t = tmp("migra3");
        fs::write(t.0.join("config.txt"), "fuente=url\nurl=https://b.com/\ntitulo=Yo\n").unwrap();
        fs::create_dir_all(t.0.join("sitio")).unwrap();
        let p = Portafolio::with_dir(t.0.clone());
        assert!(p.sitios().is_empty());
        assert_eq!(p.conf().activo, "");
        assert!(texto(&get(&p, "/", 1)).contains("<svg"), "landing con QR");
    }

    #[test]
    fn carpeta_subida_con_raiz() {
        let t = tmp("raiz");
        let p = Portafolio::with_dir(t.0.clone());
        let id = nuevo(&p, "raiz");
        subir(&p, &id, "misitio/index.html", b"<p>raiz</p>");
        subir(&p, &id, "misitio/a.css", b"x");
        usar(&p, &id);
        assert_eq!(texto(&get(&p, "/", 1)), "<p>raiz</p>");
        assert_eq!(texto(&get(&p, "/a.css", 1)), "x");
    }

    #[test]
    fn limites_y_rutas_malas() {
        let t = tmp("limites");
        let p = Portafolio::con_limites(t.0.clone(), 10, 25, "curl");
        let a = nuevo(&p, "a");
        let b = nuevo(&p, "b");
        assert!(subir(&p, &a, "a.txt", &[b'a'; 11]).contains("pesa más"));
        assert!(subir(&p, &a, "a.txt", &[b'a'; 10]).contains("Archivo guardado"));
        // el límite total cuenta TODOS los sitios
        assert!(subir(&p, &b, "b.txt", &[b'a'; 10]).contains("Archivo guardado"));
        assert!(subir(&p, &b, "c.txt", &[b'a'; 10]).contains("límite total"));
        // reemplazar un archivo existente no cuenta doble
        assert!(subir(&p, &a, "a.txt", &[b'b'; 10]).contains("Archivo guardado"));
        let m = subir(&p, &a, "../<script>.txt", b"x");
        assert!(m.starts_with("No se guardó") && !m.contains("<script>"), "{m}");
        assert!(subir(&p, &a, "/abs.txt", b"x").contains("absolutas"));
        assert!(subir(&p, &a, ".info", b"nombre=x").starts_with("No se guardó"));
        assert!(!t.0.join("abs.txt").exists());
    }

    #[test]
    fn landing_con_qr_y_escape() {
        let t = tmp("landing");
        let p = Portafolio::with_dir(t.0.clone());
        let f = |u: &str| kv(&[("url", u), ("titulo", "Yael <M>")]);
        assert!(p.admin_post("url", &f("javascript:alert(1)"), &[]).contains("http"));
        let mala = "https://ejemplo.com/\"><script>alert(1)</script>";
        assert!(p.admin_post("url", &f(mala), &[]).contains("Enlace guardado"));
        let r = get(&p, "/", 3);
        let s = texto(&r);
        assert!(s.contains("<svg") && s.contains("Yael &lt;M&gt;"));
        assert!(!s.contains("<script>alert(1)"), "la URL debe ir escapada");
        assert!(s.contains("&lt;script&gt;alert(1)"));
        let a = p.admin_html();
        assert!(!a.contains("<script>alert(1)") && !a.contains("Yael <M>"));
        assert!(a.contains("Página con tu enlace y código QR"));
        // la config persiste
        let p2 = Portafolio::with_dir(t.0.clone());
        assert_eq!(p2.conf().url, mala);
        assert!(texto(&p2.admin_get("ver/-/").unwrap()).contains("<svg"));
    }

    #[test]
    fn descarga_copia_offline() {
        if !Path::new("/usr/bin/curl").exists() {
            eprintln!("sin curl, me salto la prueba");
            return;
        }
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let puerto = server.server_addr().to_ip().unwrap().port();
        let hilo = std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                let (ct, body): (&str, Vec<u8>) = match url.split('?').next().unwrap() {
                    "/yo/" => (
                        "text/html",
                        format!(
                            r#"<html><head><link rel=stylesheet href="../css/a.css?v=3"><link rel=icon href="/favicon.ico"></head>
<body><img src="img/foto.jpg" srcset="img/foto.jpg 1x, /img/grande.jpg 2x"><a href="otra.html">otra</a>
<a href="http://127.0.0.1:{p}/yo/">yo</a><script src="/js/app.js"></script><img src="/no-existe.png"></body></html>"#,
                            p = puerto
                        )
                        .into_bytes(),
                    ),
                    "/yo" => ("text/html", b"redir".to_vec()),
                    "/css/a.css" => ("text/css", b"@import 'b.css'; body{background:url(../img/fondo.png)}".to_vec()),
                    "/css/b.css" => ("text/css", b"h1{color:red}".to_vec()),
                    "/js/app.js" => ("application/javascript", b"console.log(1)".to_vec()),
                    "/favicon.ico" => ("image/x-icon", vec![1, 2, 3]),
                    "/img/fondo.png" => ("image/png", vec![1, 2, 3]),
                    "/yo/img/foto.jpg" | "/img/grande.jpg" => ("image/jpeg", vec![1, 2, 3]),
                    "/fin" => {
                        let _ = req.respond(tiny_http::Response::from_string("ok"));
                        break;
                    }
                    _ => {
                        let _ = req.respond(tiny_http::Response::from_string("no").with_status_code(404));
                        continue;
                    }
                };
                let mut resp = tiny_http::Response::from_data(body)
                    .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], ct.as_bytes()).unwrap());
                if url == "/yo" {
                    resp = resp
                        .with_status_code(301)
                        .with_header(tiny_http::Header::from_bytes(&b"Location"[..], &b"/yo/"[..]).unwrap());
                }
                let _ = req.respond(resp);
            }
        });

        let t = tmp("copia");
        let p = Portafolio::con_limites(t.0.clone(), 25 * MB, 300 * MB, "/usr/bin/curl");
        // un sitio subido activo de antes
        let sub = nuevo(&p, "subido");
        subir(&p, &sub, "index.html", b"<p>subido</p>");
        usar(&p, &sub);

        let base = format!("http://127.0.0.1:{puerto}");
        let msg = p.admin_post("descargar", &kv(&[("url", &format!("{base}/yo")), ("titulo", ""), ("nombre", "Mi copia"), ("usar", "1")]), &[]);
        assert!(msg.contains("Copia lista") && msg.contains("Mi copia"), "{msg}");
        assert!(msg.contains("1 recursos no se pudieron"), "{msg}");
        assert_eq!(p.conf().activo, "mi-copia");
        let l = p.sitios();
        assert_eq!(l.len(), 2, "la copia es un sitio más; el subido sigue");
        let c = l.iter().find(|s| s.id == "mi-copia").unwrap();
        assert_eq!((c.tipo, c.origen.as_str()), (Tipo::Copia, format!("{base}/yo").as_str()));

        let idx = texto(&get(&p, "/", 1));
        assert!(idx.contains(r#"href="css/a.css""#), "{idx}");
        assert!(idx.contains(r#"href="favicon.ico""#), "{idx}");
        assert!(idx.contains(r#"src="yo/img/foto.jpg""#), "{idx}");
        assert!(idx.contains(r#"srcset="yo/img/foto.jpg 1x, img/grande.jpg 2x""#), "{idx}");
        assert!(idx.contains(&format!(r#"href="{base}/yo/otra.html""#)), "enlace relativo → absoluto: {idx}");
        assert!(idx.contains(r#"href="index.html">yo"#), "enlace a sí misma → local: {idx}");
        assert!(idx.contains(&format!(r#"src="{base}/no-existe.png""#)), "{idx}");
        let css = texto(&get(&p, "/css/a.css", 1));
        assert_eq!(css, "@import 'b.css'; body{background:url(../img/fondo.png)}");
        assert_eq!(texto(&get(&p, "/css/b.css", 1)), "h1{color:red}");
        assert_eq!(get(&p, "/js/app.js", 1).content_type, "text/javascript; charset=utf-8");
        assert_eq!(get(&p, "/img/fondo.png", 1).body, vec![1, 2, 3]);
        let h = p.admin_html();
        assert!(h.contains("Copia de") && h.contains("↻ Sincronizar"));
        assert!(h.contains("/sincronizar#v-portafolio") && h.contains("Usar el sitio en vivo cuando haya internet"));
        assert!(h.contains("data-activo=0"));
        let e = texto(&p.admin_get("estado").unwrap());
        assert!(e.starts_with("0\nCopia lista"), "{e}");

        // ↻ Sincronizar: vuelve a bajar el mismo sitio (no crea otro) y conserva «en vivo»
        assert!(p.admin_post("en_vivo", &kv(&[("sitio", "mi-copia"), ("valor", "1")]), &[]).contains("sitio real"));
        let msg = p.admin_post("sincronizar", &kv(&[("sitio", "mi-copia")]), &[]);
        assert!(msg.starts_with("Sincronizado «Mi copia»: 8 archivos · 415 B · "), "{msg}");
        assert!(msg.contains(&visitas::fecha_hora(visitas::ahora_local())[..10]), "lleva la fecha: {msg}");
        let c = p.sitios().into_iter().find(|s| s.id == "mi-copia").unwrap();
        assert!(c.en_vivo && c.tipo == Tipo::Copia, "la opción en vivo sobrevive a la sincronización");
        let e = texto(&p.admin_get("estado").unwrap());
        assert!(e.starts_with("0\nSincronizado"), "{e}");
        // el botón viejo «Volver a descargar» hace lo mismo
        let msg = p.admin_post("descargar", &kv(&[("sitio", "mi-copia"), ("usar", "1")]), &[]);
        assert!(msg.contains("Sincronizado"), "{msg}");
        let msg = p.admin_post("descargar", &kv(&[("url", &format!("{base}/yo")), ("usar", "0")]), &[]);
        assert!(msg.contains("Sincronizado") && msg.contains("Mi copia"), "{msg}");
        assert!(p.admin_post("sincronizar", &kv(&[("sitio", &sub)]), &[]).contains("no es una copia"));
        assert_eq!(p.sitios().len(), 2);
        // la de un sitio subido no se puede «volver a descargar»
        assert!(p.admin_post("descargar", &kv(&[("sitio", &sub)]), &[]).contains("no es una copia"));

        // falla limpia: no borra la copia buena
        let _ = std::process::Command::new("/usr/bin/curl").args(["-s", &format!("{base}/fin")]).output();
        hilo.join().unwrap();
        let msg = p.admin_post("sincronizar", &kv(&[("sitio", "mi-copia")]), &[]);
        assert!(msg.contains("No se pudo sincronizar") && msg.contains("Se conserva la copia anterior"), "{msg}");
        assert!(texto(&get(&p, "/", 1)).contains("css/a.css"), "la copia anterior sigue");
        assert!(p.sitios().into_iter().find(|s| s.id == "mi-copia").unwrap().en_vivo);
        assert_eq!(p.sitios().len(), 2);
        // una URL nueva que falla no deja un sitio vacío
        let msg = p.admin_post("descargar", &kv(&[("url", &format!("{base}/otra")), ("usar", "1")]), &[]);
        assert!(msg.contains("Error al descargar"), "{msg}");
        assert_eq!(p.sitios().len(), 2);
        assert_eq!(p.conf().activo, "mi-copia");
    }

    /// Crea a mano una copia ya descargada de `origen` (sin red) y devuelve su id.
    fn copia_falsa(p: &Portafolio, id: &str, nombre: &str, origen: &str) -> String {
        let d = p.sitios_dir().join(id);
        fs::create_dir_all(d.join("css")).unwrap();
        fs::write(d.join("index.html"), format!("<p>copia {id}</p>")).unwrap();
        fs::write(d.join("css/a.css"), "a{}").unwrap();
        let s = Sitio { id: id.into(), nombre: nombre.into(), tipo: Tipo::Copia, origen: origen.into(), fecha: "01/11/2026 10:00".into(), ts: 1, ..Default::default() };
        s.guardar(&d);
        id.to_string()
    }

    #[test]
    fn boton_sincronizar_en_lista_y_arriba() {
        let t = tmp("boton");
        let p = Portafolio::with_dir(t.0.clone());
        let c = copia_falsa(&p, "mi-web", "Mi web", "https://mi-web.com/");
        let sub = nuevo(&p, "Subido");
        subir(&p, &sub, "index.html", b"<p>s</p>");
        usar(&p, &sub);
        let h = p.admin_html();
        // solo la copia tiene botón (en su fila); arriba no, porque el activo es un sitio subido
        assert_eq!(h.matches("↻ Sincronizar</button>").count(), 1, "{h}");
        assert!(h.contains("name=sitio value=\"mi-web\"><button class=sec title=\"Vuelve a bajar la copia desde https://mi-web.com/\">↻ Sincronizar"));
        assert!(!h.contains("Volver a descargar"));
        // con la copia activa, el botón aparece también arriba, junto al sitio activo
        usar(&p, &c);
        let h = p.admin_html();
        assert_eq!(h.matches("↻ Sincronizar</button>").count(), 2);
        let arriba = &h[h.find("Los invitados ven ahora").unwrap()..h.find("Sitios guardados en el stick").unwrap()];
        assert!(arriba.contains("↻ Sincronizar") && arriba.contains("sincronizada 01/11/2026 10:00"), "{arriba}");
        assert!(arriba.contains("id=pf-progreso"), "la barra de progreso queda a la vista, arriba");
        // durante una descarga: deshabilitado, y en el sitio que se baja dice «Sincronizando…»
        p.simular_descarga(&c, "Descargando archivo 2 de 5");
        let h = p.admin_html();
        assert!(h.contains("disabled>↻ Sincronizando…</button>") && h.contains("data-activo=1"));
        assert!(p.admin_post("sincronizar", &kv(&[("sitio", &c)]), &[]).contains("Ya hay una descarga"));
        assert!(p.admin_post("en_vivo", &kv(&[("sitio", &c), ("valor", "1")]), &[]).contains("espera"));
        // ids malos o sitios que no son copia
        assert!(p.admin_post("sincronizar", &kv(&[("sitio", "../x")]), &[]).contains("ya no existe"));
        assert!(p.admin_post("sincronizar", &kv(&[("sitio", &sub)]), &[]).contains("no es una copia"));
        assert_eq!(header(&p.admin_get("sincronizar").unwrap(), "Location"), Some("/"));
    }

    #[test]
    fn sin_internet_no_intenta_sincronizar() {
        let t = tmp("sinred");
        // curl inexistente: si se intentara bajar, saldría «no pude ejecutar curl»
        let mut p = Portafolio::con_limites(t.0.clone(), 25 * MB, 300 * MB, "/no/existe/curl");
        let pings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = Arc::clone(&pings);
        p.stick_internet = Box::new(move || {
            n.fetch_add(1, Ordering::SeqCst);
            false
        });
        let c = copia_falsa(&p, "mi-web", "Mi web", "https://mi-web.com/");
        usar(&p, &c);
        let antes = p.sitios();
        let msg = p.admin_post("sincronizar", &kv(&[("sitio", &c)]), &[]);
        assert!(msg.starts_with("El stick no tiene internet ahora") && msg.contains("no se intentó"), "{msg}");
        assert!(!msg.contains("curl") && !msg.contains("Error"), "{msg}");
        assert_eq!(pings.load(Ordering::SeqCst), 1);
        assert!(!p.s.descargando.load(Ordering::SeqCst));
        assert!(!t.0.join("sitios/mi-web.nueva").exists(), "ni siquiera crea la carpeta temporal");
        assert_eq!(texto(&get(&p, "/", 1)), "<p>copia mi-web</p>", "la copia sigue intacta");
        let despues = p.sitios();
        assert_eq!((despues[0].archivos, &despues[0].fecha), (antes[0].archivos, &antes[0].fecha));
        // el aviso queda junto a la barra de progreso
        assert!(texto(&p.admin_get("estado").unwrap()).starts_with("0\nEl stick no tiene internet"));
        assert!(p.admin_html().contains("El stick no tiene internet ahora"));
        // también al copiar un enlace nuevo
        let msg = p.admin_post("descargar", &kv(&[("url", "https://otra.com/"), ("nombre", "Otra")]), &[]);
        assert!(msg.contains("no tiene internet"), "{msg}");
        assert_eq!(p.sitios().len(), 1);
        // con internet sí intenta (y aquí falla porque no hay curl): conserva la copia
        p.stick_internet = Box::new(|| true);
        let msg = p.admin_post("sincronizar", &kv(&[("sitio", &c)]), &[]);
        assert!(msg.contains("No se pudo sincronizar «Mi web»") && msg.contains("curl"), "{msg}");
        assert_eq!(texto(&get(&p, "/", 1)), "<p>copia mi-web</p>");
    }

    #[test]
    fn en_vivo_redirige_solo_la_entrada_si_el_invitado_tiene_internet() {
        let t = tmp("envivo");
        let mut p = Portafolio::with_dir(t.0.clone());
        // simulado: solo el invitado .7 aceptó «Internet para invitados»
        p.cliente_internet = Box::new(|ip| ip == IpAddr::V4(Ipv4Addr::new(192, 168, 4, 7)));
        let c = copia_falsa(&p, "mi-web", "Mi web", "https://mi-web.com/hola?x=1");
        usar(&p, &c);
        // apagada por defecto: todos ven la copia
        assert!(!p.sitios()[0].en_vivo);
        assert_eq!(texto(&get(&p, "/", 7)), "<p>copia mi-web</p>");
        assert!(p.admin_html().contains("Usar el sitio en vivo cuando haya internet"));
        assert!(!p.admin_html().contains("type=checkbox checked onchange"));

        assert!(p.admin_post("en_vivo", &kv(&[("sitio", &c), ("valor", "1")]), &[]).contains("sitio real"));
        assert!(p.admin_html().contains("type=checkbox checked onchange"));
        assert!(p.admin_html().contains("en vivo para quien tenga internet"));
        // con internet: la entrada va al sitio real
        for entrada in ["/", ""] {
            let r = get(&p, entrada, 7);
            assert_eq!((r.status, header(&r, "Location")), (302, Some("https://mi-web.com/hola?x=1")), "{entrada:?}");
            assert_eq!(header(&r, "Cache-Control"), Some("no-store"));
        }
        // …pero los recursos y otras rutas siguen saliendo de la copia
        let r = get(&p, "/css/a.css", 7);
        assert_eq!((r.status, texto(&r).as_str()), (200, "a{}"));
        assert_eq!(texto(&get(&p, "/index.html", 7)), "<p>copia mi-web</p>");
        // sin internet: la copia offline
        assert_eq!(texto(&get(&p, "/", 8)), "<p>copia mi-web</p>");
        // la vista previa del panel nunca redirige
        assert_eq!(p.admin_get("vista/").unwrap().status, 200);
        // la visita redirigida cuenta
        assert!(p.resumen().starts_with("2 visitas hoy"), "{}", p.resumen());
        // POST a "/" no se redirige
        let r = p.guest(&GuestReq { method: "POST", path: "/", query: "", host: "x", user_agent: "", content_type: "", body: &[], client_ip: ip(7) });
        assert_eq!(r.status, 200);

        // persiste y se puede apagar
        let mut p2 = Portafolio::with_dir(t.0.clone());
        p2.cliente_internet = Box::new(|_| true);
        assert_eq!(get(&p2, "/", 9).status, 302);
        assert!(p2.admin_post("en_vivo", &kv(&[("sitio", &c), ("valor", "0")]), &[]).contains("copia offline"));
        assert_eq!(texto(&get(&p2, "/", 9)), "<p>copia mi-web</p>");

        // solo aplica al sitio ACTIVO y solo a copias
        p2.admin_post("en_vivo", &kv(&[("sitio", &c), ("valor", "1")]), &[]);
        let sub = nuevo(&p2, "Subido");
        subir(&p2, &sub, "index.html", b"<p>s</p>");
        usar(&p2, &sub);
        assert_eq!(texto(&get(&p2, "/", 9)), "<p>s</p>");
        assert!(p2.admin_post("en_vivo", &kv(&[("sitio", &sub), ("valor", "1")]), &[]).contains("Solo las copias"));
        assert!(!fs::read_to_string(t.0.join("sitios/subido/.info")).unwrap().contains("en_vivo=1"));
        // reemplazar una copia por archivos subidos le quita «en vivo»
        usar(&p2, &c);
        assert_eq!(get(&p2, "/", 9).status, 302);
        assert!(p2.admin_post("preparar", &kv(&[("sitio", &c)]), &[]).starts_with("OK"));
        subir(&p2, &c, "index.html", b"<p>nuevo</p>");
        assert_eq!(texto(&get(&p2, "/", 9)), "<p>nuevo</p>");
    }
}

#[cfg(test)]
impl Portafolio {
    /// Solo para la vista previa del panel en tests: finge una descarga en curso.
    pub(crate) fn simular_descarga(&self, id: &str, texto: &str) {
        self.s.descargando.store(true, Ordering::SeqCst);
        *bloquear(&self.s.descargando_id) = id.to_string();
        *bloquear(&self.s.estado_descarga) = texto.to_string();
    }
    pub(crate) fn post(&self, accion: &str, f: &[(&str, &str)], body: &[u8]) -> String {
        let f: Vec<(String, String)> = f.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        self.admin_post(accion, &f, body)
    }
}
