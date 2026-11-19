//! Módulo "Tarjeta de presentación": quien se conecta al WiFi ve una tarjeta digital y puede
//! guardar el contacto con un toque (vCard 3.0 en `/contacto.vcf`) o escaneando un QR.
//!
//! Problema real que resuelve: el mini-navegador de portal cautivo (iOS "Captive Network
//! Assistant", WebView de "CaptivePortalLogin" en Android) no deja descargar archivos ni abrir
//! Contactos de forma fiable. Se detecta por User-Agent y se muestran instrucciones para salir
//! a Safari/Chrome (en http://192.168.4.1, por IP: así la petición va por el WiFi aunque el
//! teléfono use datos móviles para lo demás). El QR de la vCard (sin foto) se ofrece siempre.
//!
//! Archivos en el directorio de datos:
//!   datos.txt       clave=valor (valores con `\\` y `\n` escapados)
//!   foto.jpg|png    foto de perfil (≤ 2 MB, validada por magic bytes)
//!   descargas.txt   "AAAA-MM-DD N" — descargas de la vCard del día

use super::{GuestReq, GuestResp, Vitrina};
use crate::util::{html, run};
use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;

const IP_AP: &str = "192.168.4.1";
const COLOR_DEF: &str = "#2f6fed";
const MAX_FOTO: usize = 2 * 1024 * 1024;
/// Tope del texto del QR: por encima, la versión del QR crece tanto que muchas cámaras fallan.
const MAX_QR: usize = 700;

const CAMPOS: &[(&str, &str, usize)] = &[
    ("nombre", "Nombre", 80),
    ("apellido", "Apellido", 80),
    ("puesto", "Puesto", 120),
    ("empresa", "Empresa", 120),
    ("email", "Email", 120),
    ("web", "Sitio web", 200),
];

#[derive(Default, Clone, Debug, PartialEq)]
struct Datos {
    nombre: String,
    apellido: String,
    puesto: String,
    empresa: String,
    /// Una por línea: "etiqueta|número" o solo "número"
    telefonos: String,
    email: String,
    web: String,
    /// Una por línea: "etiqueta|url"
    redes: String,
    nota: String,
    color: String,
}

impl Datos {
    fn get(&self, k: &str) -> Option<&String> {
        Some(match k {
            "nombre" => &self.nombre,
            "apellido" => &self.apellido,
            "puesto" => &self.puesto,
            "empresa" => &self.empresa,
            "telefonos" => &self.telefonos,
            "email" => &self.email,
            "web" => &self.web,
            "redes" => &self.redes,
            "nota" => &self.nota,
            "color" => &self.color,
            _ => return None,
        })
    }
    fn get_mut(&mut self, k: &str) -> Option<&mut String> {
        Some(match k {
            "nombre" => &mut self.nombre,
            "apellido" => &mut self.apellido,
            "puesto" => &mut self.puesto,
            "empresa" => &mut self.empresa,
            "telefonos" => &mut self.telefonos,
            "email" => &mut self.email,
            "web" => &mut self.web,
            "redes" => &mut self.redes,
            "nota" => &mut self.nota,
            "color" => &mut self.color,
            _ => return None,
        })
    }
    const CLAVES: [&'static str; 10] =
        ["nombre", "apellido", "puesto", "empresa", "telefonos", "email", "web", "redes", "nota", "color"];

    fn serializar(&self) -> String {
        let mut o = String::new();
        for k in Self::CLAVES {
            let v = self.get(k).unwrap();
            o.push_str(k);
            o.push('=');
            for c in v.chars() {
                match c {
                    '\\' => o.push_str("\\\\"),
                    '\n' => o.push_str("\\n"),
                    '\r' => {}
                    c => o.push(c),
                }
            }
            o.push('\n');
        }
        o
    }

    fn parsear(s: &str) -> Datos {
        let mut d = Datos::default();
        for linea in s.lines() {
            let Some((k, v)) = linea.split_once('=') else { continue };
            let Some(campo) = d.get_mut(k.trim()) else { continue };
            let mut out = String::with_capacity(v.len());
            let mut it = v.chars();
            while let Some(c) = it.next() {
                if c == '\\' {
                    match it.next() {
                        Some('n') => out.push('\n'),
                        Some(x) => out.push(x),
                        None => {}
                    }
                } else {
                    out.push(c);
                }
            }
            *campo = out;
        }
        d
    }

    fn nombre_completo(&self) -> String {
        format!("{} {}", self.nombre.trim(), self.apellido.trim()).trim().to_string()
    }

    fn color(&self) -> &str {
        let c = self.color.trim();
        if c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|x| x.is_ascii_hexdigit()) { c } else { COLOR_DEF }
    }

    /// (etiqueta, número tal cual, número para tel:)
    fn telefonos(&self) -> Vec<(String, String, String)> {
        self.telefonos
            .lines()
            .filter_map(|l| {
                let (et, num) = match l.split_once('|') {
                    Some((e, n)) => (e.trim(), n.trim()),
                    None => ("", l.trim()),
                };
                let tel: String = num.chars().filter(|c| c.is_ascii_digit() || matches!(c, '+' | '*' | '#')).collect();
                if tel.is_empty() { None } else { Some((et.to_string(), num.to_string(), tel)) }
            })
            .take(6)
            .collect()
    }

    /// (etiqueta, url segura)
    fn redes(&self) -> Vec<(String, String)> {
        self.redes
            .lines()
            .filter_map(|l| {
                let (et, u) = match l.split_once('|') {
                    Some((e, u)) => (e.trim().to_string(), u.trim()),
                    None => (String::new(), l.trim()),
                };
                let u = url_segura(u)?;
                let et = if et.is_empty() { host_de(&u) } else { et };
                Some((et, u))
            })
            .take(12)
            .collect()
    }

    fn web(&self) -> Option<String> {
        url_segura(self.web.trim())
    }

    fn email(&self) -> Option<&str> {
        let e = self.email.trim();
        let ok = e.contains('@') && !e.chars().any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '?' | '&'));
        if ok { Some(e) } else { None }
    }
}

/// Acepta http(s) o un dominio "suelto" (se le antepone https://). Rechaza javascript:, data:, etc.
fn url_segura(u: &str) -> Option<String> {
    let u = u.trim();
    if u.is_empty() || u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let low = u.to_ascii_lowercase();
    if low.starts_with("https://") || low.starts_with("http://") {
        return Some(u.to_string());
    }
    if low.contains(':') && !low.split('/').next().unwrap_or("").contains('.') {
        return None; // otro esquema (javascript:, data:, intent:…)
    }
    if low.split('/').next().unwrap_or("").contains('.') && !low.contains(':') {
        return Some(format!("https://{u}"));
    }
    None
}

fn host_de(u: &str) -> String {
    let s = u.split("://").nth(1).unwrap_or(u);
    let h = s.split('/').next().unwrap_or(s);
    h.trim_start_matches("www.").to_string()
}

fn truncar(s: &str, max: usize) -> String {
    s.chars().filter(|c| *c != '\r').take(max).collect()
}

// ---------------------------------------------------------------------------------------------
// vCard
// ---------------------------------------------------------------------------------------------

/// Escapa un valor de texto de vCard 3.0 (RFC 2426 §4): `\` `,` `;` y saltos de línea.
fn vesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            ',' => o.push_str("\\,"),
            ';' => o.push_str("\\;"),
            '\n' => o.push_str("\\n"),
            '\r' => {}
            c if c.is_control() => {}
            c => o.push(c),
        }
    }
    o
}

/// Valor URI: sin escapes de texto, solo se quitan caracteres de control.
fn vuri(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Pliega una línea lógica a 75 octetos (RFC 2425 §5.8.1) sin partir caracteres UTF-8.
fn plegar(linea: &str, out: &mut String) {
    let mut ancho = 0;
    let mut limite = 75;
    for c in linea.chars() {
        let n = c.len_utf8();
        if ancho + n > limite {
            out.push_str("\r\n ");
            ancho = 1;
            limite = 75;
        }
        out.push(c);
        ancho += n;
    }
    out.push_str("\r\n");
}

fn tipo_tel(et: &str) -> &'static str {
    let e = et.to_lowercase();
    if e.contains("móvil") || e.contains("movil") || e.contains("cel") || e.contains("whats") || e.contains("mobile") {
        "CELL"
    } else if e.contains("ofi") || e.contains("trab") || e.contains("work") {
        "WORK"
    } else if e.contains("casa") || e.contains("home") {
        "HOME"
    } else if et.is_empty() {
        "CELL"
    } else {
        "VOICE"
    }
}

/// Líneas lógicas (sin plegar) de la vCard. `compacta` = para el QR: sin foto ni etiquetas Apple.
fn vcard_lineas(d: &Datos, compacta: bool, con_nota: bool, con_redes: bool, max_tels: usize) -> Vec<String> {
    let mut l = vec!["BEGIN:VCARD".to_string(), "VERSION:3.0".to_string()];
    let fn_ = d.nombre_completo();
    l.push(format!("N:{};{};;;", vesc(d.apellido.trim()), vesc(d.nombre.trim())));
    l.push(format!("FN:{}", vesc(if fn_.is_empty() { d.empresa.trim() } else { &fn_ })));
    if !d.empresa.trim().is_empty() {
        l.push(format!("ORG:{}", vesc(d.empresa.trim())));
    }
    if !d.puesto.trim().is_empty() {
        l.push(format!("TITLE:{}", vesc(d.puesto.trim())));
    }
    for (et, _, tel) in d.telefonos().into_iter().take(max_tels) {
        l.push(format!("TEL;TYPE={}:{}", tipo_tel(&et), tel));
    }
    if let Some(e) = d.email() {
        l.push(format!("EMAIL;TYPE=INTERNET:{}", vesc(e)));
    }
    if let Some(w) = d.web() {
        l.push(format!("URL:{}", vuri(&w)));
    }
    if con_redes {
        for (i, (et, u)) in d.redes().into_iter().enumerate() {
            if compacta {
                l.push(format!("URL:{}", vuri(&u)));
            } else {
                // Agrupación itemN: iOS/macOS muestran la etiqueta; Android conserva la URL.
                l.push(format!("item{}.URL:{}", i + 1, vuri(&u)));
                l.push(format!("item{}.X-ABLabel:{}", i + 1, vesc(&et)));
            }
        }
    }
    if con_nota && !d.nota.trim().is_empty() {
        l.push(format!("NOTE:{}", vesc(d.nota.trim())));
    }
    l
}

fn vcard(d: &Datos, foto: Option<(&'static str, &[u8])>) -> String {
    let mut out = String::new();
    for linea in vcard_lineas(d, false, true, true, usize::MAX) {
        plegar(&linea, &mut out);
    }
    if let Some((tipo, bytes)) = foto {
        plegar(&format!("PHOTO;ENCODING=b;TYPE={tipo}:{}", base64(bytes)), &mut out);
    }
    plegar("END:VCARD", &mut out);
    out
}

/// Texto para el QR: vCard sin foto, recortando nota → redes → teléfonos extra hasta caber.
fn vcard_qr(d: &Datos) -> String {
    let intentos = [(true, true, 6), (false, true, 6), (false, false, 6), (false, false, 1)];
    let mut txt = String::new();
    for (nota, redes, tels) in intentos {
        let mut l = vcard_lineas(d, true, nota, redes, tels);
        l.push("END:VCARD".into());
        txt = l.join("\r\n");
        if txt.len() <= MAX_QR {
            break;
        }
    }
    txt
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity(b.len().div_ceil(3) * 4);
    for ch in b.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        o.push(T[(n >> 18) as usize & 63] as char);
        o.push(T[(n >> 12) as usize & 63] as char);
        o.push(if ch.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        o.push(if ch.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    o
}

// ---------------------------------------------------------------------------------------------
// QR → SVG (negro sobre blanco con zona de silencio: lo que mejor leen las cámaras)
// ---------------------------------------------------------------------------------------------

fn qr_svg(texto: &str) -> Option<String> {
    use qrcode::{Color, EcLevel, QrCode};
    let ec = if texto.len() <= 400 { EcLevel::M } else { EcLevel::L };
    let code = QrCode::with_error_correction_level(texto.as_bytes(), ec).ok()?;
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
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {t} {t}" shape-rendering="crispEdges" role="img" aria-label="Código QR del contacto"><rect width="{t}" height="{t}" fill="#fff"/><path d="{p}" fill="#000"/></svg>"##
    ))
}

// ---------------------------------------------------------------------------------------------
// Detección del mini-navegador de portal cautivo
// ---------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Clone, Copy)]
enum Nav {
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

// ---------------------------------------------------------------------------------------------
// Fecha local (para el contador diario)
// ---------------------------------------------------------------------------------------------

fn hoy() -> String {
    let (ok, s) = run("date", &["+%F"]);
    let s = s.trim();
    if ok && s.len() == 10 {
        return s.to_string();
    }
    // Respaldo: UTC calculado a mano (algoritmo civil_from_days de H. Hinnant)
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let z = (secs / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------------------------------------
// Módulo
// ---------------------------------------------------------------------------------------------

struct Stats {
    dia: String,
    n: u32,
    /// IPs que ya descargaron hoy (en memoria: iOS/Android a veces piden el archivo 2 veces)
    ips: HashSet<IpAddr>,
}

pub struct Tarjeta {
    dir: PathBuf,
    stats: Mutex<Stats>,
}

impl Default for Tarjeta {
    fn default() -> Self {
        Self::new()
    }
}

impl Tarjeta {
    pub fn new() -> Self {
        Self::with_dir(super::data_dir("tarjeta"))
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let txt = std::fs::read_to_string(dir.join("descargas.txt")).unwrap_or_default();
        let mut it = txt.split_whitespace();
        let dia = it.next().unwrap_or("").to_string();
        let n = it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        Tarjeta { dir, stats: Mutex::new(Stats { dia, n, ips: HashSet::new() }) }
    }

    fn datos(&self) -> Datos {
        let mut d = Datos::parsear(&std::fs::read_to_string(self.dir.join("datos.txt")).unwrap_or_default());
        if d.nombre.trim().is_empty() && d.apellido.trim().is_empty() && d.empresa.trim().is_empty() {
            d.nombre = "Yael".into();
        }
        d
    }

    fn guardar(&self, d: &Datos) -> std::io::Result<()> {
        let tmp = self.dir.join("datos.txt.tmp");
        std::fs::write(&tmp, d.serializar())?;
        std::fs::rename(tmp, self.dir.join("datos.txt"))
    }

    fn foto(&self) -> Option<(&'static str, Vec<u8>)> {
        for (ext, tipo) in [("jpg", "JPEG"), ("png", "PNG")] {
            if let Ok(b) = std::fs::read(self.dir.join(format!("foto.{ext}"))) {
                return Some((tipo, b));
            }
        }
        None
    }

    fn hay_foto(&self) -> bool {
        self.dir.join("foto.jpg").exists() || self.dir.join("foto.png").exists()
    }

    fn resp_foto(&self) -> Option<GuestResp> {
        let (tipo, b) = self.foto()?;
        let ct = if tipo == "PNG" { "image/png" } else { "image/jpeg" };
        let mut r = GuestResp::bytes(ct, b);
        r.headers.push(("Cache-Control".into(), "no-cache".into()));
        Some(r)
    }

    fn contar(&self, ip: IpAddr) {
        let dia = hoy();
        let mut s = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        if s.dia != dia {
            s.dia = dia;
            s.n = 0;
            s.ips.clear();
        }
        if s.ips.insert(ip) {
            s.n += 1;
            let _ = std::fs::write(self.dir.join("descargas.txt"), format!("{} {}\n", s.dia, s.n));
        }
    }

    fn resp_vcf(&self) -> GuestResp {
        let d = self.datos();
        let foto = self.foto();
        let txt = vcard(&d, foto.as_ref().map(|(t, b)| (*t, b.as_slice())));
        let base: String = d
            .nombre_completo()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else if c == ' ' { '_' } else { '-' })
            .collect::<String>()
            .trim_matches(|c| c == '-' || c == '_')
            .to_string();
        let base = if base.is_empty() { "contacto".to_string() } else { base };
        let mut r = GuestResp::bytes("text/vcard; charset=utf-8", txt.into_bytes());
        r.headers.push(("Content-Disposition".into(), format!("attachment; filename=\"{base}.vcf\"")));
        r.headers.push(("Cache-Control".into(), "no-store".into()));
        r
    }

    /// Página de la tarjeta. `foto_url`: dónde pide la foto (invitado vs. vista previa del admin).
    fn pagina(&self, nav: Nav, foto_url: &str, vcf_url: &str) -> String {
        let d = self.datos();
        let ac = d.color();
        let on = texto_sobre(ac);
        let nombre = d.nombre_completo();
        let rol = [d.puesto.trim(), d.empresa.trim()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" · ");

        let avatar = if self.hay_foto() {
            format!(r#"<img class=av src="{}" alt="">"#, html(foto_url))
        } else {
            let ini: String = [d.nombre.trim(), d.apellido.trim()].iter().filter_map(|s| s.chars().next()).flat_map(|c| c.to_uppercase()).collect();
            format!(r#"<div class="av ini" aria-hidden=true>{}</div>"#, html(&ini))
        };

        // Acciones rápidas
        let tels = d.telefonos();
        let mut acts = String::new();
        if let Some((_, _, t)) = tels.first() {
            acts.push_str(&format!(r#"<a href="tel:{}">{}<span>Llamar</span></a>"#, html(t), ICO_TEL));
        }
        if let Some(e) = d.email() {
            acts.push_str(&format!(r#"<a href="mailto:{}">{}<span>Correo</span></a>"#, html(e), ICO_MAIL));
        }
        if let Some(w) = d.web() {
            acts.push_str(&format!(r#"<a href="{}" rel=noopener>{}<span>Web</span></a>"#, html(&w), ICO_WEB));
        }

        // Lista de datos (texto visible y seleccionable: sirve también para copiar a mano)
        let mut filas = String::new();
        for (et, num, t) in &tels {
            let et = if et.is_empty() { "Teléfono" } else { et };
            filas.push_str(&format!(r#"<a class=fila href="tel:{}"><small>{}</small><b>{}</b></a>"#, html(t), html(et), html(num)));
        }
        if let Some(e) = d.email() {
            filas.push_str(&format!(r#"<a class=fila href="mailto:{0}"><small>Correo</small><b>{0}</b></a>"#, html(e)));
        }
        if let Some(w) = d.web() {
            filas.push_str(&format!(r#"<a class=fila href="{}" rel=noopener><small>Sitio web</small><b>{}</b></a>"#, html(&w), html(&host_de(&w))));
        }
        let redes: String = d
            .redes()
            .iter()
            .map(|(et, u)| format!(r#"<a class=chip href="{}" rel=noopener>{}</a>"#, html(u), html(et)))
            .collect();
        let redes = if redes.is_empty() { String::new() } else { format!("<div class=chips>{redes}</div>") };
        let bio = if d.nota.trim().is_empty() { String::new() } else { format!("<p class=bio>{}</p>", html(d.nota.trim())) };

        let ip = IP_AP;
        let (aviso, boton) = match nav {
            Nav::AppleCna => (
                format!(
                    r#"<section class=aviso id=guardar><h2>Para guardar el contacto en tu iPhone</h2>
<p>Esta ventanita de "inicio de sesión WiFi" no permite descargar archivos ni abrir Contactos.</p>
<ol><li>Toca <b>Cancelar</b> (arriba a la derecha) y elige <b>Usar sin Internet</b>.</li>
<li>Abre <b>Safari</b> y escribe:<span class=ip>{ip}</span></li>
<li>Toca <b>Guardar contacto</b> y luego <b>Crear contacto nuevo</b>.</li></ol>
<p class=mut>¿Tienes otro teléfono a la mano? Escanea el código QR de abajo con su cámara.</p></section>"#
                ),
                format!(r#"<a class="save sec" href="{}">Intentar descargar aquí</a>"#, html(vcf_url)),
            ),
            Nav::AndroidWv => (
                format!(
                    r#"<section class=aviso><h2>Si el botón no hace nada</h2>
<p>La ventana de "Iniciar sesión en la red" de Android puede bloquear la descarga.</p>
<ol><li>Toca <b>⋮</b> (arriba a la derecha) → <b>Usar esta red tal como está</b>.<br>Si pregunta si quieres mantener la conexión sin Internet, di <b>Sí</b>.</li>
<li>Abre <b>Chrome</b> y escribe:<span class=ip>{ip}</span></li>
<li>Toca <b>Guardar contacto</b> y abre el archivo descargado; elige <b>Contactos</b>.</li></ol>
<p class=mut>Si Chrome no carga la página, apaga un momento los datos móviles: Android manda el tráfico por la SIM cuando el WiFi no tiene Internet.</p></section>"#
                ),
                format!(r#"<a class=save href="{}">{}Guardar contacto</a>"#, html(vcf_url), ICO_SAVE),
            ),
            Nav::Normal => (
                String::new(),
                format!(r#"<a class=save href="{}">{}Guardar contacto</a>"#, html(vcf_url), ICO_SAVE),
            ),
        };
        let (aviso_arriba, aviso_abajo) = if nav == Nav::AppleCna { (aviso, String::new()) } else { (String::new(), aviso) };

        let qr = qr_svg(&vcard_qr(&d)).unwrap_or_default();
        let titulo = if nombre.is_empty() { "Contacto".to_string() } else { nombre.clone() };

        format!(
            r#"<!doctype html><html lang=es><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1,viewport-fit=cover">
<meta name=theme-color content="{ac}"><title>{titulo_h}</title><style>
:root{{--ac:{ac};--on:{on};--bg:#eef0f4;--card:#fff;--tx:#15171c;--mut:#626878;--line:rgba(0,0,0,.07);--warn:#fff6df;--warnb:#f0c75e}}
@media (prefers-color-scheme:dark){{:root{{--bg:#0c0e12;--card:#171a21;--tx:#eceef3;--mut:#9aa0ad;--line:rgba(255,255,255,.08);--warn:#2a2412;--warnb:#7a6320}}}}
*{{box-sizing:border-box}}html{{-webkit-text-size-adjust:100%}}
body{{margin:0;background:var(--bg);color:var(--tx);font:16px/1.45 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,system-ui,sans-serif}}
main{{max-width:440px;margin:0 auto;padding:16px 16px 40px}}
.tj{{background:var(--card);border-radius:24px;overflow:hidden;box-shadow:0 12px 32px rgba(0,0,0,.12)}}
.band{{height:112px;background:var(--ac);background:linear-gradient(135deg,var(--ac),rgba(0,0,0,.35)),var(--ac);background-blend-mode:normal,normal}}
.av{{display:block;width:116px;height:116px;border-radius:50%;margin:-62px auto 0;border:4px solid var(--card);object-fit:cover;background:var(--ac)}}
.ini{{color:var(--on);font:600 42px/108px -apple-system,system-ui,sans-serif;text-align:center;letter-spacing:.02em}}
.who{{text-align:center;padding:12px 22px 0}}h1{{font-size:27px;line-height:1.15;margin:0 0 4px;letter-spacing:-.015em}}
.rol{{color:var(--mut);margin:0}}.bio{{margin:12px 22px 0;text-align:center;white-space:pre-line}}
.acts{{display:flex;gap:10px;justify-content:center;padding:18px 16px 6px}}
.acts a{{flex:1;max-width:112px;display:flex;flex-direction:column;align-items:center;gap:4px;padding:12px 4px 10px;border-radius:16px;background:var(--line);color:var(--tx);text-decoration:none;font-size:13px}}
.acts svg{{width:24px;height:24px;stroke:var(--ac)}}
.save{{display:flex;gap:10px;align-items:center;justify-content:center;margin:14px 16px 6px;padding:17px;border-radius:16px;background:var(--ac);color:var(--on);font-weight:650;font-size:18px;text-decoration:none;box-shadow:0 6px 16px rgba(0,0,0,.15)}}
.save svg{{width:22px;height:22px;stroke:var(--on)}}.save.sec{{background:none;color:var(--mut);box-shadow:none;font-size:15px;font-weight:500;text-decoration:underline;padding:8px}}
.datos{{padding:10px 0 6px}}.fila{{display:block;padding:10px 22px;border-top:1px solid var(--line);color:var(--tx);text-decoration:none}}
.fila small{{display:block;color:var(--mut);font-size:12px}}.fila b{{font-weight:500;word-break:break-word}}
.chips{{display:flex;flex-wrap:wrap;gap:8px;padding:12px 18px 20px;justify-content:center}}
.chip{{padding:8px 14px;border-radius:999px;border:1px solid var(--line);background:var(--line);color:var(--tx);text-decoration:none;font-size:14px}}
.box{{background:var(--card);border-radius:20px;padding:18px;margin-top:14px;text-align:center}}
.box h2,.aviso h2{{font-size:17px;margin:0 0 6px}}.mut{{color:var(--mut);font-size:14px}}
.qr{{width:min(240px,70vw);margin:10px auto 4px;background:#fff;border-radius:12px;padding:6px}}.qr svg{{display:block;width:100%;height:auto}}
.aviso{{background:var(--warn);border:1px solid var(--warnb);border-radius:18px;padding:16px 18px;margin:0 0 14px}}
.aviso p{{margin:0 0 8px}}.aviso ol{{margin:6px 0 10px;padding-left:20px}}.aviso li{{margin:6px 0}}
.ip{{display:block;margin:6px 0;padding:8px 12px;border-radius:10px;background:var(--card);font:600 22px/1.2 ui-monospace,Menlo,monospace;letter-spacing:.03em;text-align:center;user-select:all;-webkit-user-select:all}}
footer{{text-align:center;color:var(--mut);font-size:12px;margin-top:18px}}
</style></head><body><main>
{aviso_arriba}<article class=tj><div class=band></div>{avatar}
<div class=who><h1>{nombre_h}</h1>{rol_h}</div>{bio}
<nav class=acts>{acts}</nav>{boton}
<div class=datos>{filas}</div>{redes}</article>
{aviso_abajo}<section class=box><h2>Escanéalo con la cámara</h2>
<p class=mut style="margin:0">Desde otro teléfono: apunta la cámara al código y toca el aviso para agregar el contacto.</p>
<div class=qr>{qr}</div></section>
<footer>Red local sin Internet · {ip}</footer>
</main></body></html>"#,
            titulo_h = html(&titulo),
            nombre_h = html(&titulo),
            rol_h = if rol.is_empty() { String::new() } else { format!("<p class=rol>{}</p>", html(&rol)) },
        )
    }
}

/// Color de texto legible sobre el acento (luminancia relativa WCAG).
fn texto_sobre(hex: &str) -> &'static str {
    let c = |i: usize| {
        let v = u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f64 / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    let l = 0.2126 * c(1) + 0.7152 * c(3) + 0.0722 * c(5);
    if l > 0.4 { "#111" } else { "#fff" }
}

const ICO_TEL: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 16.9v3a2 2 0 0 1-2.2 2 19.8 19.8 0 0 1-8.6-3.1 19.5 19.5 0 0 1-6-6A19.8 19.8 0 0 1 2.1 4.2 2 2 0 0 1 4.1 2h3a2 2 0 0 1 2 1.7c.1.9.4 1.8.7 2.7a2 2 0 0 1-.5 2.1L8 9.8a16 16 0 0 0 6 6l1.3-1.3a2 2 0 0 1 2.1-.4c.9.3 1.8.6 2.7.7a2 2 0 0 1 1.7 2z"/></svg>"#;
const ICO_MAIL: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="4" width="20" height="16" rx="2"/><path d="m22 7-10 6L2 7"/></svg>"#;
const ICO_WEB: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M2 12h20M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg>"#;
const ICO_SAVE: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M19 8v6M22 11h-6"/></svg>"#;

impl Vitrina for Tarjeta {
    fn id(&self) -> &'static str {
        "tarjeta"
    }
    fn nombre(&self) -> &'static str {
        "Tarjeta de presentacion"
    }
    fn descripcion(&self) -> &'static str {
        "Guardan tu contacto con un toque"
    }
    fn ssid_por_defecto(&self) -> String {
        let d = Datos::parsear(&std::fs::read_to_string(self.dir.join("datos.txt")).unwrap_or_default());
        let n = d.nombre.trim();
        if n.is_empty() {
            return "Contacto de Yael".into();
        }
        let mut s = String::from("Contacto de ");
        for c in n.chars() {
            if s.len() + c.len_utf8() > 32 {
                break;
            }
            s.push(c);
        }
        s
    }

    fn guest(&self, req: &GuestReq) -> GuestResp {
        match req.path {
            "/contacto.vcf" => {
                self.contar(req.client_ip);
                self.resp_vcf()
            }
            "/foto" => self.resp_foto().unwrap_or_else(GuestResp::not_found),
            "/" | "/index.html" => {
                let nav = if req.query.split('&').any(|p| p == "nav=normal") { Nav::Normal } else { detectar(req.user_agent) };
                let mut r = GuestResp::html(self.pagina(nav, "/foto", "/contacto.vcf"));
                r.headers.push(("Cache-Control".into(), "no-store".into()));
                r
            }
            p if p.rsplit('/').next().is_some_and(|f| f.contains('.')) => GuestResp::not_found(),
            _ => GuestResp::redirect(&format!("http://{IP_AP}/")),
        }
    }

    fn admin_html(&self) -> String {
        let d = Datos::parsear(&std::fs::read_to_string(self.dir.join("datos.txt")).unwrap_or_default());
        let mut campos = String::new();
        for (k, etiqueta, max) in CAMPOS {
            campos.push_str(&format!(
                r#"<label>{etiqueta}<input type=text name={k} maxlength={max} value="{}" autocomplete=off></label>"#,
                html(d.get(k).unwrap())
            ));
        }
        let foto = if self.hay_foto() {
            r#"<img src="/vitrina/tarjeta/foto" alt="" style="width:72px;height:72px;border-radius:50%;object-fit:cover;vertical-align:middle;margin-right:10px">
<form method=post action=/vitrina/tarjeta/borrar_foto style="display:inline"><button class=link>quitar foto</button></form>"#
        } else {
            r#"<span style="color:var(--mut)">Sin foto (se muestran tus iniciales)</span>"#
        };
        format!(
            r#"<style>.tj-a textarea{{width:100%;padding:10px;border-radius:8px;border:1px solid #3334;background:var(--bg);color:var(--tx);margin:4px 0 10px;font:15px/1.4 system-ui,sans-serif}}
.tj-a small{{display:block;color:var(--mut);margin-top:-6px;margin-bottom:10px}}.tj-a .fila{{display:flex;align-items:center;gap:10px;margin:4px 0 12px}}</style>
<div class=tj-a>
<p style="margin-top:0;color:var(--mut)">{resumen} · <a href="/vitrina/tarjeta/vista" target=_blank style="color:var(--ac)">Ver vista previa</a></p>
<h3 style="font-size:15px;margin:0 0 6px">Foto</h3>
<div class=fila>{foto}</div>
<input type=file id=tj-foto accept="image/jpeg,image/png"> <span id=tj-st style="color:var(--mut)"></span>
<small style="margin-top:4px">JPG o PNG, máximo 2 MB. Mejor cuadrada y ~400 px: viaja dentro del contacto.</small>
<form method=post action=/vitrina/tarjeta/guardar>
{campos}
<label>Teléfonos<textarea name=telefonos rows=3 maxlength=400 placeholder="Móvil|+52 55 1234 5678">{tels}</textarea></label>
<small>Uno por línea: <code>etiqueta|número</code> (Móvil, Oficina, Casa, WhatsApp…)</small>
<label>Redes y enlaces<textarea name=redes rows=4 maxlength=1500 placeholder="LinkedIn|https://linkedin.com/in/usuario&#10;GitHub|https://github.com/usuario">{redes}</textarea></label>
<small>Uno por línea: <code>etiqueta|url</code></small>
<label>Nota / bio corta<textarea name=nota rows=3 maxlength=300>{nota}</textarea></label>
<label>Color de acento <input type=color name=color value="{color}" style="vertical-align:middle;width:56px;height:34px;border:0;background:none"></label>
<p><button>Guardar tarjeta</button></p></form></div>
<script>(function(){{var i=document.getElementById('tj-foto'),s=document.getElementById('tj-st');
i.onchange=function(){{var f=i.files[0];if(!f)return;if(f.size>2097152){{s.textContent='La foto pesa más de 2 MB';return}}
s.textContent='Subiendo…';fetch('/vitrina/tarjeta/foto',{{method:'POST',body:f}}).then(function(r){{return r.text()}})
.then(function(){{location.reload()}}).catch(function(){{s.textContent='Error al subir'}})}}}})();</script>"#,
            resumen = html(&self.resumen()),
            tels = html(&d.telefonos),
            redes = html(&d.redes),
            nota = html(&d.nota),
            color = html(d.color()),
        )
    }

    fn admin_post(&self, accion: &str, form: &[(String, String)], body: &[u8]) -> String {
        match accion {
            "guardar" => {
                let mut d = Datos::default();
                for (k, v) in form {
                    let max = match k.as_str() {
                        "telefonos" => 400,
                        "redes" => 1500,
                        "nota" => 300,
                        "color" => 7,
                        k => CAMPOS.iter().find(|c| c.0 == k).map(|c| c.2).unwrap_or(0),
                    };
                    if let Some(campo) = d.get_mut(k) {
                        let mut v = truncar(v, max);
                        if !matches!(k.as_str(), "telefonos" | "redes" | "nota") {
                            v = v.replace('\n', " ");
                        }
                        *campo = v.trim().to_string();
                    }
                }
                if d.color() != d.color.as_str() {
                    d.color = COLOR_DEF.into();
                }
                match self.guardar(&d) {
                    Ok(()) => "Tarjeta guardada".into(),
                    Err(e) => format!("No se pudo guardar la tarjeta: {e}"),
                }
            }
            "foto" => {
                if body.is_empty() {
                    return "No llegó ninguna foto".into();
                }
                if body.len() > MAX_FOTO {
                    return "La foto pesa más de 2 MB".into();
                }
                let ext = if body.starts_with(&[0xFF, 0xD8, 0xFF]) {
                    "jpg"
                } else if body.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
                    "png"
                } else {
                    return "Formato no válido: sube un JPG o PNG".into();
                };
                let otra = if ext == "jpg" { "png" } else { "jpg" };
                let tmp = self.dir.join("foto.tmp");
                let r = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, self.dir.join(format!("foto.{ext}"))));
                match r {
                    Ok(()) => {
                        let _ = std::fs::remove_file(self.dir.join(format!("foto.{otra}")));
                        "Foto actualizada".into()
                    }
                    Err(e) => format!("No se pudo guardar la foto: {e}"),
                }
            }
            "borrar_foto" => {
                let _ = std::fs::remove_file(self.dir.join("foto.jpg"));
                let _ = std::fs::remove_file(self.dir.join("foto.png"));
                "Foto eliminada".into()
            }
            _ => "Acción desconocida".into(),
        }
    }

    fn admin_get(&self, ruta: &str) -> Option<GuestResp> {
        match ruta {
            "vista" => Some(GuestResp::html(self.pagina(Nav::Normal, "/vitrina/tarjeta/foto", "/vitrina/tarjeta/contacto.vcf"))),
            "foto" => self.resp_foto(),
            "contacto.vcf" => Some(self.resp_vcf()), // descarga de prueba: no cuenta
            _ => None,
        }
    }

    fn resumen(&self) -> String {
        let dia = hoy();
        let s = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        let n = if s.dia == dia { s.n } else { 0 };
        if n == 1 { "1 contacto guardado hoy".into() } else { format!("{n} contactos guardados hoy") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn tmp(nombre: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("stickd-tarjeta-{}-{nombre}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn req<'a>(path: &'a str, ua: &'a str, ip: u8) -> GuestReq<'a> {
        GuestReq {
            method: "GET",
            path,
            query: "",
            host: "captive.apple.com",
            user_agent: ua,
            content_type: "",
            body: b"",
            client_ip: IpAddr::V4(Ipv4Addr::new(192, 168, 4, ip)),
        }
    }

    fn f(pares: &[(&str, &str)]) -> Vec<(String, String)> {
        pares.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    const UA_IOS_CNA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148";
    const UA_IOS_SAFARI: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
    const UA_AND_WV: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 7 Build/AP2A; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/125.0.0.0 Mobile Safari/537.36";
    const UA_AND_CHROME: &str = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Mobile Safari/537.36";

    #[test]
    fn escapes_vcard() {
        assert_eq!(vesc("a,b;c\\d\ne\r"), "a\\,b\\;c\\\\d\\ne");
    }

    #[test]
    fn plegado_75_octetos() {
        let mut o = String::new();
        let larga = format!("NOTE:{}", "ñ".repeat(100));
        plegar(&larga, &mut o);
        for l in o.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(l.len() <= 75, "línea de {} octetos", l.len());
        }
        let unida = o.trim_end_matches("\r\n").replace("\r\n ", "");
        assert_eq!(unida, larga);
    }

    #[test]
    fn base64_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn detecta_navegador() {
        assert_eq!(detectar(UA_IOS_CNA), Nav::AppleCna);
        assert_eq!(detectar("CaptiveNetworkSupport-481.0.1 wispr"), Nav::AppleCna);
        assert_eq!(detectar(UA_IOS_SAFARI), Nav::Normal);
        assert_eq!(detectar(UA_AND_WV), Nav::AndroidWv);
        assert_eq!(detectar(UA_AND_CHROME), Nav::Normal);
    }

    #[test]
    fn urls() {
        assert_eq!(url_segura("javascript:alert(1)"), None);
        assert_eq!(url_segura("data:text/html,x"), None);
        assert_eq!(url_segura("github.com/yael").as_deref(), Some("https://github.com/yael"));
        assert_eq!(url_segura("https://x.com/a?b=1").as_deref(), Some("https://x.com/a?b=1"));
    }

    #[test]
    fn guardar_y_vcard() {
        let t = Tarjeta::with_dir(tmp("vcard"));
        let m = t.admin_post(
            "guardar",
            &f(&[
                ("nombre", "Yael"),
                ("apellido", "Montoya; Pérez"),
                ("empresa", "ACME, S.A."),
                ("puesto", "Ingeniera"),
                ("telefonos", "Móvil|+52 (55) 1234-5678\r\nOficina|55 0000 1111"),
                ("email", "yael@example.com"),
                ("web", "example.com"),
                ("redes", "GitHub|https://github.com/yael\nMalo|javascript:alert(1)"),
                ("nota", "Hola\\mundo\nlínea 2"),
                ("color", "red"),
            ]),
            b"",
        );
        assert_eq!(m, "Tarjeta guardada");
        let d = t.datos();
        assert_eq!(d.nota, "Hola\\mundo\nlínea 2");
        assert_eq!(d.color, COLOR_DEF);
        let r = t.guest(&req("/contacto.vcf", UA_IOS_SAFARI, 10));
        assert_eq!(r.content_type, "text/vcard; charset=utf-8");
        assert!(r.headers.iter().any(|(k, v)| k == "Content-Disposition" && v.contains("attachment") && v.contains(".vcf")));
        let v = String::from_utf8(r.body).unwrap();
        assert!(v.starts_with("BEGIN:VCARD\r\nVERSION:3.0\r\n"));
        assert!(v.ends_with("END:VCARD\r\n"));
        assert!(!v.replace("\r\n", "").contains('\n'), "solo CRLF");
        assert!(v.contains("N:Montoya\\; Pérez;Yael;;;\r\n"));
        assert!(v.contains("FN:Yael Montoya\\; Pérez\r\n"));
        assert!(v.contains("ORG:ACME\\, S.A.\r\n"));
        assert!(v.contains("TEL;TYPE=CELL:+525512345678\r\n"));
        assert!(v.contains("TEL;TYPE=WORK:5500001111\r\n"));
        assert!(v.contains("URL:https://example.com\r\n"));
        assert!(v.contains("item1.URL:https://github.com/yael"));
        assert!(!v.contains("javascript"));
        assert!(v.contains("NOTE:Hola\\\\mundo\\nlínea 2\r\n"));
        assert_eq!(t.resumen(), "1 contacto guardado hoy");
        t.guest(&req("/contacto.vcf", UA_IOS_SAFARI, 10)); // misma IP: no cuenta doble
        t.guest(&req("/contacto.vcf", UA_AND_CHROME, 11));
        assert_eq!(t.resumen(), "2 contactos guardados hoy");
        // persiste
        assert_eq!(Tarjeta::with_dir(t.dir.clone()).resumen(), "2 contactos guardados hoy");
        assert_eq!(t.ssid_por_defecto(), "Contacto de Yael");
    }

    #[test]
    fn foto_valida_y_en_vcard() {
        let t = Tarjeta::with_dir(tmp("foto"));
        assert!(t.admin_post("foto", &[], b"GIF89a....").contains("no válido"));
        assert!(t.admin_post("foto", &[], &vec![0xFF; MAX_FOTO + 1]).contains("2 MB"));
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpg.extend(std::iter::repeat_n(7u8, 300));
        assert_eq!(t.admin_post("foto", &[], &jpg), "Foto actualizada");
        let v = String::from_utf8(t.guest(&req("/contacto.vcf", "", 1)).body).unwrap();
        assert!(v.contains("PHOTO;ENCODING=b;TYPE=JPEG:/9j/4"));
        for l in v.split("\r\n") {
            assert!(l.len() <= 75);
        }
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        assert_eq!(t.admin_post("foto", &[], &png), "Foto actualizada");
        assert!(!t.dir.join("foto.jpg").exists());
        assert_eq!(t.guest(&req("/foto", "", 1)).content_type, "image/png");
        assert_eq!(t.admin_get("foto").unwrap().content_type, "image/png");
        t.admin_post("borrar_foto", &[], b"");
        assert_eq!(t.guest(&req("/foto", "", 1)).status, 404);
    }

    #[test]
    fn html_escapado_y_flujos() {
        let t = Tarjeta::with_dir(tmp("html"));
        t.admin_post(
            "guardar",
            &f(&[("nombre", "<script>x</script>"), ("nota", "\"><img onerror=1>"), ("telefonos", "55 1234"), ("redes", "<b>|https://a.com/\"x")]),
            b"",
        );
        let cna = String::from_utf8(t.guest(&req("/", UA_IOS_CNA, 2)).body).unwrap();
        assert!(!cna.contains("<script>x"));
        assert!(cna.contains("&lt;script&gt;"));
        assert!(!cna.contains("<img onerror"));
        assert!(!cna.contains("<b>|"));
        assert!(cna.contains("Usar sin Internet"));
        assert!(cna.contains("192.168.4.1"));
        assert!(cna.contains("<svg"), "QR siempre presente");
        let and = String::from_utf8(t.guest(&req("/", UA_AND_WV, 2)).body).unwrap();
        assert!(and.contains("Usar esta red tal como está"));
        let normal = String::from_utf8(t.guest(&req("/", UA_IOS_SAFARI, 2)).body).unwrap();
        assert!(!normal.contains("Usar sin Internet"));
        assert!(normal.contains("href=\"/contacto.vcf\""));
        let adm = t.admin_html();
        assert!(!adm.contains("<script>x"));
        assert!(t.admin_get("vista").unwrap().body.len() > 1000);
        assert_eq!(t.guest(&req("/generate_204x", "", 2)).status, 302);
        assert_eq!(t.guest(&req("/favicon.ico", "", 2)).status, 404);
        assert_eq!(t.ssid_por_defecto(), "Contacto de <script>x</script>");
    }

    #[test]
    fn qr_cabe() {
        let mut d = Datos { nombre: "Yael".into(), apellido: "M".into(), nota: "x".repeat(2000), ..Default::default() };
        d.redes = (0..12).map(|i| format!("R{i}|https://example.com/perfil/{i}")).collect::<Vec<_>>().join("\n");
        let q = vcard_qr(&d);
        assert!(q.len() <= MAX_QR);
        assert!(!q.contains("NOTE") && !q.contains("PHOTO"));
        assert!(qr_svg(&q).is_some());
    }
}
