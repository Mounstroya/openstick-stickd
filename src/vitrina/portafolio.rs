//! Vitrina "Portafolio": quien se conecta al WiFi ve la página de portafolio del usuario.
//!
//! Fuentes posibles (elige el admin):
//!   - `sitio`: archivos subidos desde el panel (data_dir/sitio/), servidos tal cual.
//!   - `url`: un enlace. Si hay copia offline (data_dir/copia/, bajada con curl) se sirve esa;
//!     si no, una landing con el enlace en grande y su código QR.
//! Si la fuente elegida no tiene nada, se muestra la landing (si hay enlace) o "aún no hay
//! portafolio".
//!
//! Archivos en data_dir: `config.txt` (clave=valor), `visitas.txt`, `sitio/`, `copia/`.

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
use std::time::Duration;

const MB: u64 = 1024 * 1024;
const PREFIJO: &str = "/vitrina/portafolio";

#[derive(Clone, Copy, PartialEq, Debug, Default)]
enum Fuente {
    #[default]
    Sitio,
    Url,
}

#[derive(Clone, Default, Debug)]
struct Config {
    fuente: Fuente,
    url: String,
    titulo: String,
    subtitulo: String,
    /// Resumen de la última copia descargada ("23 archivos · 1.2 MB · 18/11/2026 14:02")
    copia_info: String,
    /// URL de la que se hizo la copia
    copia_de: String,
}

impl Config {
    fn cargar(p: &Path) -> Config {
        let mut c = Config::default();
        if let Ok(t) = fs::read_to_string(p) {
            for l in t.lines() {
                let Some((k, v)) = l.split_once('=') else { continue };
                let v = v.to_string();
                match k {
                    "fuente" => c.fuente = if v == "url" { Fuente::Url } else { Fuente::Sitio },
                    "url" => c.url = v,
                    "titulo" => c.titulo = v,
                    "subtitulo" => c.subtitulo = v,
                    "copia_info" => c.copia_info = v,
                    "copia_de" => c.copia_de = v,
                    _ => {}
                }
            }
        }
        c
    }
    fn guardar(&self, p: &Path) {
        let una = |s: &str| s.replace(['\n', '\r'], " ");
        let s = format!(
            "fuente={}\nurl={}\ntitulo={}\nsubtitulo={}\ncopia_info={}\ncopia_de={}\n",
            if self.fuente == Fuente::Url { "url" } else { "sitio" },
            una(&self.url),
            una(&self.titulo),
            una(&self.subtitulo),
            una(&self.copia_info),
            una(&self.copia_de)
        );
        let tmp = p.with_extension("tmp");
        if fs::write(&tmp, s).is_ok() {
            let _ = fs::rename(&tmp, p);
        }
    }
}

struct Compartido {
    dir: PathBuf,
    conf: Mutex<Config>,
    visitas: Mutex<visitas::Visitas>,
    /// Mensaje de estado de la descarga en curso / última
    estado_descarga: Mutex<String>,
    descargando: AtomicBool,
    /// Serializa las subidas (el cálculo del total y la escritura)
    subida: Mutex<()>,
    max_archivo: u64,
    max_total: u64,
    curl: String,
}

pub struct Portafolio {
    s: Arc<Compartido>,
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
        let _ = fs::create_dir_all(dir.join("sitio"));
        let conf = Config::cargar(&dir.join("config.txt"));
        let visitas = visitas::Visitas::cargar(dir.join("visitas.txt"));
        Portafolio {
            s: Arc::new(Compartido {
                conf: Mutex::new(conf),
                visitas: Mutex::new(visitas),
                estado_descarga: Mutex::new(String::new()),
                descargando: AtomicBool::new(false),
                subida: Mutex::new(()),
                max_archivo,
                max_total,
                curl: curl.to_string(),
                dir,
            }),
        }
    }

    fn sitio(&self) -> PathBuf {
        self.s.dir.join("sitio")
    }
    fn copia(&self) -> PathBuf {
        self.s.dir.join("copia")
    }
    fn conf(&self) -> Config {
        bloquear(&self.s.conf).clone()
    }
    fn cambiar_conf(&self, f: impl FnOnce(&mut Config)) {
        let mut c = bloquear(&self.s.conf);
        f(&mut c);
        c.guardar(&self.s.dir.join("config.txt"));
    }

    fn hay_copia(&self) -> bool {
        self.copia().join("index.html").is_file()
    }
    fn hay_sitio(&self) -> bool {
        !entradas_visibles(&self.sitio()).is_empty()
    }

    /// Carpeta que se sirve ahora mismo, si hay alguna.
    fn raiz_activa(&self, c: &Config) -> Option<PathBuf> {
        match c.fuente {
            Fuente::Url if self.hay_copia() => Some(self.copia()),
            Fuente::Sitio if self.hay_sitio() => Some(raiz_web(&self.sitio())),
            _ => None,
        }
    }

    /// Responde una ruta (ya decodificada) para invitado o vista previa. `prefijo` se antepone
    /// a las redirecciones. Devuelve también si fue una página HTML (para contar visitas).
    fn responder(&self, path: &str, prefijo: &str) -> (GuestResp, bool) {
        let c = self.conf();
        let path = if path.is_empty() { "/" } else { path };
        let Some(raiz) = self.raiz_activa(&c) else {
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

    // ─────────── acciones de admin ───────────

    fn subir(&self, ruta: &str, body: &[u8]) -> String {
        let ruta = match sanear_ruta(ruta) {
            Ok(r) => r,
            Err(e) => return format!("No se guardó «{}»: {}.", html(ruta), html(&e)),
        };
        let _g = bloquear(&self.s.subida);
        if body.len() as u64 > self.s.max_archivo {
            return format!("No se guardó «{}»: pesa más de {} MB.", html(&ruta), self.s.max_archivo / MB);
        }
        let destino = self.sitio().join(&ruta);
        let previo = fs::metadata(&destino).map(|m| m.len()).unwrap_or(0);
        let usado: u64 = listar(&self.sitio()).iter().map(|x| x.1).sum();
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
        self.cambiar_conf(|c| c.fuente = Fuente::Sitio);
        format!("Archivo guardado: {} ({}).", html(&ruta), tam_humano(body.len() as u64))
    }

    fn guardar_url(&self, f: &[(String, String)]) -> Result<String, String> {
        let url = campo(f, "url").trim().to_string();
        let titulo: String = campo(f, "titulo").trim().chars().take(80).collect();
        let subtitulo: String = campo(f, "subtitulo").trim().chars().take(140).collect();
        if !url.is_empty() {
            if url.len() > 1000 || copia::Url::parse(&url).is_none() {
                return Err("El enlace debe empezar con http:// o https:// (máx. 1000 caracteres).".into());
            }
        }
        let cambio = url != self.conf().copia_de;
        if cambio && self.hay_copia() && !self.s.descargando.load(Ordering::SeqCst) {
            let _ = fs::remove_dir_all(self.copia());
        }
        self.cambiar_conf(|c| {
            if cambio {
                c.copia_info.clear();
                c.copia_de.clear();
            }
            c.url = url.clone();
            c.titulo = titulo;
            c.subtitulo = subtitulo;
            if !url.is_empty() {
                c.fuente = Fuente::Url;
            }
        });
        Ok(url)
    }

    fn iniciar_descarga(&self, url: String, sincrono: bool) -> String {
        if url.is_empty() {
            return "Primero escribe el enlace de tu página.".into();
        }
        if self.s.descargando.swap(true, Ordering::SeqCst) {
            return "Ya hay una descarga en curso.".into();
        }
        *bloquear(&self.s.estado_descarga) = "Descargando…".into();
        let s = Arc::clone(&self.s);
        let trabajo = move || {
            let op = copia::Opciones {
                curl: s.curl.clone(),
                max_total: 100 * MB,
                max_recursos: 400,
                externos: true,
                limite_tiempo: Duration::from_secs(300),
            };
            let progreso = |m: &str| *bloquear(&s.estado_descarga) = m.to_string();
            let r = copia::descargar(&url, &s.dir.join("copia"), &op, &progreso);
            let msg = match r {
                Ok(res) => {
                    let info = format!(
                        "{} archivos · {} · {}{}",
                        res.archivos,
                        tam_humano(res.bytes),
                        visitas::fecha_hora(visitas::ahora_local()),
                        if res.fallidos > 0 { format!(" · {} recursos no se pudieron bajar", res.fallidos) } else { String::new() }
                    );
                    let mut c = bloquear(&s.conf);
                    c.copia_info = info.clone();
                    c.copia_de = url.clone();
                    c.fuente = Fuente::Url;
                    c.guardar(&s.dir.join("config.txt"));
                    format!("Copia lista: {info}")
                }
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
            "Descarga iniciada; el estado se actualiza abajo. Necesita que el stick tenga internet.".into()
        }
    }

    fn borrar_archivo(&self, ruta: &str) -> String {
        let Ok(r) = sanear_ruta(ruta) else { return "Ruta no válida.".into() };
        let p = self.sitio().join(&r);
        match fs::remove_file(&p) {
            Ok(_) => {
                if let Some(d) = p.parent() {
                    quitar_vacias(d.to_path_buf(), &self.sitio());
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
        let (r, pagina) = self.responder(&path, "");
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
        if ruta == "vista" {
            return Some(GuestResp::redirect(&format!("{PREFIJO}/vista/")));
        }
        if let Some(resto) = ruta.strip_prefix("vista/") {
            let path = format!("/{}", pct_decode(resto));
            let (mut r, _) = self.responder(&path, &format!("{PREFIJO}/vista"));
            // El contenido (sobre todo una copia de un sitio ajeno) corre aislado: origen opaco,
            // sin formularios ni fetch, para que no pueda tocar el panel de admin.
            r.headers.push(("Content-Security-Policy".into(), "sandbox allow-scripts allow-popups; connect-src 'none'; form-action 'none'".into()));
            return Some(r);
        }
        // GET a una acción (p.ej. recargar tras un POST): de vuelta al panel
        if matches!(ruta, "fuente" | "url" | "descargar" | "borrar_copia" | "subir" | "borrar_sitio" | "borrar_archivo" | "reiniciar_visitas") {
            return Some(GuestResp::redirect("/"));
        }
        None
    }

    fn admin_post(&self, accion: &str, form: &[(String, String)], body: &[u8]) -> String {
        match accion {
            "subir" => self.subir(campo(form, "ruta"), body),
            "fuente" => {
                let f = if campo(form, "fuente") == "url" { Fuente::Url } else { Fuente::Sitio };
                self.cambiar_conf(|c| c.fuente = f);
                match f {
                    Fuente::Url => "Fuente activa: enlace.".into(),
                    Fuente::Sitio => "Fuente activa: archivos subidos.".into(),
                }
            }
            "url" => match self.guardar_url(form) {
                Ok(u) if u.is_empty() => "Datos guardados (sin enlace).".into(),
                Ok(_) => "Enlace guardado. Los invitados verán la landing con el QR hasta que descargues una copia.".into(),
                Err(e) => e,
            },
            "descargar" => {
                let url = if form.iter().any(|(k, _)| k == "url") {
                    match self.guardar_url(form) {
                        Ok(u) => u,
                        Err(e) => return e,
                    }
                } else {
                    self.conf().url
                };
                self.iniciar_descarga(url, self.sincrono())
            }
            "borrar_copia" => {
                if self.s.descargando.load(Ordering::SeqCst) {
                    return "Espera a que termine la descarga.".into();
                }
                let _ = fs::remove_dir_all(self.copia());
                self.cambiar_conf(|c| {
                    c.copia_info.clear();
                    c.copia_de.clear();
                });
                "Copia offline borrada; se mostrará la landing con el QR.".into()
            }
            "borrar_sitio" => {
                let _g = bloquear(&self.s.subida);
                let _ = fs::remove_dir_all(self.sitio());
                let _ = fs::create_dir_all(self.sitio());
                "Sitio borrado.".into()
            }
            "borrar_archivo" => self.borrar_archivo(campo(form, "ruta")),
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

    fn admin_html(&self) -> String {
        let c = self.conf();
        let archivos = listar(&self.sitio());
        let total: u64 = archivos.iter().map(|x| x.1).sum();
        let hay_copia = self.hay_copia();
        let descargando = self.s.descargando.load(Ordering::SeqCst);
        let estado_desc = bloquear(&self.s.estado_descarga).clone();

        let activa = match (self.raiz_activa(&c).is_some(), c.fuente) {
            (true, Fuente::Sitio) => format!("<b>Archivos subidos</b> ({} archivos, {})", archivos.len(), tam_humano(total)),
            (true, Fuente::Url) => format!("<b>Copia offline</b> de <code>{}</code>", html(&c.copia_de)),
            (false, _) if !c.url.is_empty() => format!("<b>Landing con enlace y QR</b> → <code>{}</code>", html(&c.url)),
            _ => "<b>Nada aún</b>: los invitados ven «aún no hay portafolio».".into(),
        };

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

        let mostrar = 300;
        let lista: String = archivos
            .iter()
            .take(mostrar)
            .map(|(r, t)| {
                format!(
                    "<li><span>{r}<small> · {}</small></span><form method=post action={PREFIJO}/borrar_archivo><input type=hidden name=ruta value=\"{r}\"><button class=link title=Borrar>borrar</button></form></li>",
                    tam_humano(*t),
                    r = html(r)
                )
            })
            .collect();
        let mas = if archivos.len() > mostrar { format!("<li>… y {} más</li>", archivos.len() - mostrar) } else { String::new() };

        let chk = |f: Fuente| if c.fuente == f { "checked" } else { "" };
        let copia_txt = if hay_copia {
            format!("Copia offline: {}", html(&c.copia_info))
        } else if c.url.is_empty() {
            "Sin enlace.".into()
        } else {
            "Sin copia offline: los invitados ven la landing con el enlace y su QR.".into()
        };

        format!(
            r#"<div class=pf><style>
.pf .mut{{color:var(--mut,#888);font-size:14px}}.pf code{{word-break:break-all;font-size:13px}}
.pf h3{{font-size:15px;margin:18px 0 8px}}.pf .fila{{display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin:8px 0}}
.pf .fila form{{margin:0}}.pf .sec{{background:#4f8cff22;color:var(--tx,inherit)}}.pf .peligro{{background:var(--bad,#ff5d5d)}}
.pf input[type=url],.pf input[type=text]{{width:100%;padding:10px;border-radius:8px;border:1px solid #8884;background:var(--bg,transparent);color:var(--tx,inherit);margin:4px 0 8px;font-size:16px}}
.pf input[type=file]{{max-width:100%;margin:4px 0 8px;color:var(--mut,#888)}}
.pf ul.archivos{{max-height:260px;overflow:auto;font-size:14px}}.pf ul.archivos span{{word-break:break-all}}
.pf .barras{{display:grid;grid-template-columns:repeat(7,1fr);gap:6px;height:92px;align-items:end;margin:8px 0}}
.pf .barras div{{display:flex;flex-direction:column;justify-content:flex-end;align-items:center;height:100%}}
.pf .barras i{{display:block;width:100%;max-width:28px;background:var(--ac,#4f8cff);border-radius:4px 4px 0 0}}
.pf .barras small{{font-size:11px;color:var(--mut,#888);line-height:1.2}}
.pf #pf-subida,.pf #pf-estado{{font-size:14px;margin:6px 0;white-space:pre-wrap}}
</style>
<p>Activo ahora: {activa}</p>
<div class=fila><a href="{PREFIJO}/vista/" target=_blank rel=noopener><button type=button class=sec>Vista previa ↗</button></a></div>

<h3>Visitas</h3>
<p class=mut style="margin:0">{hoy} hoy · {tot} en total (únicas por dispositivo y día)</p>
<div class=barras>{barras_html}</div>
<form method=post action={PREFIJO}/reiniciar_visitas onsubmit="return confirm('¿Poner el contador en cero?')"><button class="link">reiniciar contador</button></form>

<h3>Fuente</h3>
<form method=post action={PREFIJO}/fuente>
<label class=opt><input type=radio name=fuente value=sitio {c_sitio}><span><b>Archivos subidos</b><small>Tu sitio estático (HTML/CSS/JS/imágenes) guardado en el stick.</small></span></label>
<label class=opt><input type=radio name=fuente value=url {c_url}><span><b>Enlace</b><small>Copia offline del enlace si la descargaste; si no, una landing con el enlace y su QR.</small></span></label>
<button>Usar esta fuente</button></form>

<h3>Enlace a tu página</h3>
<form method=post action={PREFIJO}/url>
<label>Enlace<input type=url name=url value="{url}" placeholder="https://tu-sitio.com" maxlength=1000 inputmode=url autocomplete=off></label>
<label>Nombre para la landing<input type=text name=titulo value="{titulo}" placeholder="Yael Montoya" maxlength=80></label>
<label>Frase corta<input type=text name=subtitulo value="{subtitulo}" placeholder="Diseño · Desarrollo · Fotografía" maxlength=140></label>
<div class=fila><button>Guardar</button><button formaction={PREFIJO}/descargar class=sec>Guardar y descargar copia</button></div></form>
<p class=mut>{copia_txt}</p>
<div id=pf-estado data-activo={activo}>{estado}</div>
{borrar_copia}
<p class=mut>La copia baja el HTML y sus CSS, JS, imágenes y fuentes (máx. 100 MB, 400 archivos). Sitios que arman su contenido con JS desde APIs pueden verse incompletos sin internet: revisa la vista previa.</p>

<h3>Subir tu sitio</h3>
<p class=mut>Sube la carpeta de tu sitio (con <code>index.html</code>) o archivos sueltos. Nombres solo con letras sin acento, números, espacio, <code>. _ -</code>. Máx. 25 MB por archivo y 300 MB en total.</p>
<label>Carpeta<input type=file id=pf-carpeta webkitdirectory multiple></label>
<label>Archivos<input type=file id=pf-archivos multiple></label>
<div class=fila><button type=button id=pf-subir>Subir</button></div>
<div id=pf-subida></div>
<p class=mut>{n_arch} archivos · {tam_total} de 300 MB</p>
<ul class=archivos>{lista}{mas}</ul>
<form method=post action={PREFIJO}/borrar_sitio onsubmit="return confirm('¿Borrar TODOS los archivos subidos?')"><button class=peligro>Borrar sitio</button></form>
<script>(function(){{
var P='{PREFIJO}',$=function(i){{return document.getElementById(i)}};
function lista(inp,carpeta){{return Array.prototype.map.call(inp.files||[],function(f){{
var r=(carpeta&&f.webkitRelativePath)?f.webkitRelativePath:f.name;
if(carpeta&&f.webkitRelativePath){{var i=r.indexOf('/');if(i>0)r=r.slice(i+1)}}return [f,r]}})
.filter(function(x){{return !x[1].split('/').some(function(c){{return c.charAt(0)=='.'}})}})}}
$('pf-subir').onclick=async function(){{
var t=lista($('pf-carpeta'),true).concat(lista($('pf-archivos'),false)),st=$('pf-subida'),ok=0,mal=[];
if(!t.length){{st.textContent='Elige una carpeta o archivos primero.';return}}
this.disabled=true;
for(var i=0;i<t.length;i++){{var f=t[i][0],r=t[i][1];
st.textContent='Subiendo '+(i+1)+' de '+t.length+': '+r;
if(f.size>25*1024*1024){{mal.push(r+': pesa más de 25 MB');continue}}
try{{var resp=await fetch(P+'/subir?ruta='+encodeURIComponent(r),{{method:'POST',body:f}});var x=await resp.text();
if(resp.ok&&x.indexOf('Archivo guardado')>=0)ok++;else{{var m=x.match(/No se guard[^<]*/);mal.push(m?m[0]:r+': error '+resp.status)}}}}
catch(e){{mal.push(r+': '+e)}}}}
st.textContent='Listo: '+ok+' de '+t.length+' archivos subidos.'+(mal.length?'\n'+mal.join('\n'):'')+'\nRecargando…';
setTimeout(function(){{location.href='/'}},mal.length?6000:1200)}};
var e=$('pf-estado');if(e&&e.dataset.activo=='1'){{var iv=setInterval(async function(){{try{{
var x=await (await fetch(P+'/estado',{{cache:'no-store'}})).text();var a=x.split('\n');e.textContent=a.slice(1).join('\n');
if(a[0]!='1'){{clearInterval(iv);setTimeout(function(){{location.href='/'}},2500)}}}}catch(_){{}}}},2000)}}
}})();</script></div>"#,
            c_sitio = chk(Fuente::Sitio),
            c_url = chk(Fuente::Url),
            url = html(&c.url),
            titulo = html(&c.titulo),
            subtitulo = html(&c.subtitulo),
            activo = if descargando { 1 } else { 0 },
            estado = html(&estado_desc),
            borrar_copia = if hay_copia && !descargando {
                format!("<form method=post action={PREFIJO}/borrar_copia><button class=link>borrar copia offline</button></form>")
            } else {
                String::new()
            },
            n_arch = archivos.len(),
            tam_total = tam_humano(total),
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
    fn subir(p: &Portafolio, ruta: &str, b: &[u8]) -> String {
        p.admin_post("subir", &[("ruta".into(), ruta.into())], b)
    }

    #[test]
    fn sanea_rutas() {
        assert_eq!(sanear_ruta("css/estilo.css").unwrap(), "css/estilo.css");
        assert_eq!(sanear_ruta("./mi foto.jpg").unwrap(), "mi foto.jpg");
        for mala in ["", "/etc/passwd", "../x", "a/../../x", "a//b", ".env", "a/.git/config", "a\\b", "niño.jpg", "a/b\0", "x;rm"] {
            assert!(sanear_ruta(mala).is_err(), "debió rechazar {mala:?}");
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
    }

    #[test]
    fn sube_y_sirve_sitio() {
        let t = tmp("sitio");
        let p = Portafolio::with_dir(t.0.clone());
        let html_usuario = b"<html><body><h1>Hola <b>mundo</b></h1><script>1<2</script></body></html>";
        assert!(subir(&p, "index.html", html_usuario).contains("Archivo guardado"));
        assert!(subir(&p, "css/estilo.css", b"body{color:red}").contains("Archivo guardado"));
        assert!(subir(&p, "proyectos/index.html", b"<p>proy</p>").contains("Archivo guardado"));
        assert!(subir(&p, "img/mi foto.png", &[0x89, b'P', b'N', b'G', 0xff]).contains("Archivo guardado"));

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

        // traversal y ocultos
        for mala in ["/../config.txt", "/%2e%2e/config.txt", "/..%2fconfig.txt", "/.subiendo-x"] {
            let r = get(&p, mala, 1);
            assert!(r.status == 404 || r.status == 302, "{mala}: {}", r.status);
            assert!(!texto(&r).contains("fuente="), "{mala} filtró config");
        }
        assert_eq!(get(&p, "/no-existe.png", 1).status, 404);
        let r = get(&p, "/pagina/rara", 1);
        assert_eq!((r.status, header(&r, "Location")), (302, Some("/")));

        // visitas: únicas por IP (solo páginas)
        get(&p, "/", 2);
        assert!(p.resumen().starts_with("2 visitas hoy · 2 en total"), "{}", p.resumen());

        // panel: lista escapada y vista previa aislada
        let a = p.admin_html();
        assert!(a.contains("img/mi foto.png"));
        let v = p.admin_get("vista/css/estilo.css").unwrap();
        assert_eq!(v.body, b"body{color:red}");
        assert!(header(&v, "Content-Security-Policy").unwrap().contains("sandbox"));
        let v = p.admin_get("vista/proyectos").unwrap();
        assert_eq!(header(&v, "Location"), Some("/vitrina/portafolio/vista/proyectos/"));
        assert_eq!(header(&p.admin_get("vista").unwrap(), "Location"), Some("/vitrina/portafolio/vista/"));

        // borrar un archivo y el sitio
        assert!(p.admin_post("borrar_archivo", &[("ruta".into(), "proyectos/index.html".into())], &[]).contains("Borrado"));
        assert!(!t.0.join("sitio/proyectos").exists());
        p.admin_post("borrar_sitio", &[], &[]);
        assert!(texto(&get(&p, "/", 1)).contains("Aún no hay portafolio"));
    }

    #[test]
    fn carpeta_subida_con_raiz() {
        let t = tmp("raiz");
        let p = Portafolio::with_dir(t.0.clone());
        subir(&p, "misitio/index.html", b"<p>raiz</p>");
        subir(&p, "misitio/a.css", b"x");
        assert_eq!(texto(&get(&p, "/", 1)), "<p>raiz</p>");
        assert_eq!(texto(&get(&p, "/a.css", 1)), "x");
    }

    #[test]
    fn limites_y_rutas_malas() {
        let t = tmp("limites");
        let p = Portafolio::con_limites(t.0.clone(), 10, 25, "curl");
        assert!(subir(&p, "a.txt", &[b'a'; 11]).contains("pesa más"));
        assert!(subir(&p, "a.txt", &[b'a'; 10]).contains("Archivo guardado"));
        assert!(subir(&p, "b.txt", &[b'a'; 10]).contains("Archivo guardado"));
        assert!(subir(&p, "c.txt", &[b'a'; 10]).contains("límite total"));
        // reemplazar un archivo existente no cuenta doble
        assert!(subir(&p, "a.txt", &[b'b'; 10]).contains("Archivo guardado"));
        let m = subir(&p, "../<script>.txt", b"x");
        assert!(m.starts_with("No se guardó") && !m.contains("<script>"), "{m}");
        assert!(subir(&p, "/abs.txt", b"x").contains("absolutas"));
        assert!(!t.0.join("abs.txt").exists());
    }

    #[test]
    fn landing_con_qr_y_escape() {
        let t = tmp("landing");
        let p = Portafolio::with_dir(t.0.clone());
        let f = |u: &str| vec![("url".to_string(), u.to_string()), ("titulo".to_string(), "Yael <M>".to_string())];
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
        assert!(a.contains("Landing con enlace"));
        // fuente sitio sin archivos → sigue mostrando la landing
        p.admin_post("fuente", &[("fuente".into(), "sitio".into())], &[]);
        assert!(texto(&get(&p, "/", 3)).contains("<svg"));
        // y la config persiste
        let p2 = Portafolio::with_dir(t.0.clone());
        assert_eq!(p2.conf().url, mala);
        assert_eq!(p2.conf().fuente, Fuente::Sitio);
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
        let base = format!("http://127.0.0.1:{puerto}");
        let msg = p.admin_post("descargar", &[("url".into(), format!("{base}/yo")), ("titulo".into(), "".into())], &[]);
        assert!(msg.contains("Copia lista"), "{msg}");
        assert!(msg.contains("1 recursos no se pudieron"), "{msg}");

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
        assert!(p.admin_html().contains("Copia offline"));

        // falla limpia: no borra la copia buena
        let msg = p.admin_post("descargar", &[], &[]);
        assert!(msg.contains("Copia lista"), "{msg}");
        let _ = std::process::Command::new("/usr/bin/curl").args(["-s", &format!("{base}/fin")]).output();
        hilo.join().unwrap();
        let msg = p.admin_post("descargar", &[], &[]);
        assert!(msg.contains("Error al descargar"), "{msg}");
        assert!(texto(&get(&p, "/", 1)).contains("css/a.css"), "la copia anterior sigue");

        // cambiar de enlace borra la copia vieja → landing
        p.admin_post("url", &[("url".into(), "https://otro.com".into())], &[]);
        assert!(texto(&get(&p, "/", 1)).contains("<svg"));
    }
}
