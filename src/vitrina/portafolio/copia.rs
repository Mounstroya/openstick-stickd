//! Copia offline de una página web con `curl` (sin shell): baja el HTML principal y los
//! recursos que referencia (css/js/imágenes/fuentes, también los de CSS: `url()` e `@import`),
//! los guarda con nombres saneados y reescribe las referencias a rutas relativas locales.
//! Los enlaces relativos que no se bajaron (otras páginas, iframes) se vuelven absolutos para
//! que al menos funcionen cuando el invitado tenga datos.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const UA: &str = "Mozilla/5.0 (Linux; Android 14; Mobile) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Mobile Safari/537.36";

pub struct Opciones {
    /// Ruta del ejecutable curl (/usr/bin/curl en el stick)
    pub curl: String,
    /// Tope de bytes de toda la copia
    pub max_total: u64,
    /// Tope de recursos (además del HTML principal)
    pub max_recursos: usize,
    /// ¿Bajar también recursos de otros dominios (CDNs, Google Fonts…)?
    pub externos: bool,
    /// Tiempo total máximo; al pasarlo ya no se piden más recursos
    pub limite_tiempo: Duration,
}

pub struct Resultado {
    pub archivos: usize,
    pub bytes: u64,
    pub fallidos: usize,
}

// ───────────────────────────── URLs ─────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct Url {
    pub esquema: String,
    /// host[:puerto] en minúsculas, sin puerto por defecto
    pub host: String,
    /// Siempre empieza con '/', normalizada (sin . ni ..)
    pub ruta: String,
    pub query: Option<String>,
}

fn limpiar(s: &str) -> String {
    s.trim().chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect()
}

fn normalizar(r: &str) -> String {
    let segs: Vec<&str> = r.split('/').skip(1).collect();
    let n = segs.len();
    let mut out: Vec<&str> = Vec::new();
    for (i, s) in segs.iter().enumerate() {
        match *s {
            "." => {
                if i == n - 1 {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if i == n - 1 {
                    out.push("");
                }
            }
            x => out.push(x),
        }
    }
    format!("/{}", out.join("/"))
}

impl Url {
    pub fn parse(s: &str) -> Option<Url> {
        let s = limpiar(s);
        let (esq, resto) = s.split_once("://")?;
        let esquema = esq.to_ascii_lowercase();
        if esquema != "http" && esquema != "https" {
            return None;
        }
        let fin = resto.find(['/', '?', '#']).unwrap_or(resto.len());
        let auth = resto[..fin].rsplit('@').next().unwrap_or("");
        let mut host = auth.to_ascii_lowercase();
        let defecto = if esquema == "http" { ":80" } else { ":443" };
        if let Some(h) = host.strip_suffix(defecto) {
            host = h.to_string();
        }
        if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]_".contains(c)) {
            return None;
        }
        let resto = &resto[fin..];
        let sin_frag = resto.split('#').next().unwrap_or("");
        let (ruta, query) = match sin_frag.split_once('?') {
            Some((r, q)) => (r, Some(q.to_string())),
            None => (sin_frag, None),
        };
        let ruta = if ruta.is_empty() { "/".to_string() } else { normalizar(ruta) };
        Some(Url { esquema, host, ruta, query })
    }

    /// Resuelve una referencia (relativa o absoluta) contra esta URL. `None` si no es http(s)
    /// (data:, mailto:, javascript:, solo #fragmento…).
    pub fn resolver(&self, r: &str) -> Option<Url> {
        let r = limpiar(r);
        if r.is_empty() || r.starts_with('#') {
            return None;
        }
        if let Some(p) = r.find(':') {
            let esq = &r[..p];
            if !esq.is_empty()
                && esq.as_bytes()[0].is_ascii_alphabetic()
                && esq.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            {
                return Url::parse(&r);
            }
        }
        if r.starts_with("//") {
            return Url::parse(&format!("{}:{}", self.esquema, r));
        }
        let r = r.split('#').next().unwrap_or("");
        let (ruta, query) = match r.split_once('?') {
            Some((a, q)) => (a, Some(q.to_string())),
            None => (r, None),
        };
        let ruta = if ruta.is_empty() {
            return Some(Url { query: query.or_else(|| self.query.clone()), ..self.clone() });
        } else if ruta.starts_with('/') {
            normalizar(ruta)
        } else {
            let dir = &self.ruta[..self.ruta.rfind('/').map(|i| i + 1).unwrap_or(0)];
            normalizar(&format!("{dir}{ruta}"))
        };
        Some(Url { esquema: self.esquema.clone(), host: self.host.clone(), ruta, query })
    }

    pub fn mismo_origen(&self, o: &Url) -> bool {
        self.esquema == o.esquema && self.host == o.host
    }

    pub fn texto(&self) -> String {
        match &self.query {
            Some(q) => format!("{}://{}{}?{}", self.esquema, self.host, self.ruta, q),
            None => format!("{}://{}{}", self.esquema, self.host, self.ruta),
        }
    }
}

fn fragmento(v: &str) -> &str {
    v.find('#').map(|i| &v[i..]).unwrap_or("")
}

fn es_absoluta(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    v.starts_with("http://") || v.starts_with("https://") || v.starts_with("//")
}

/// Ruta relativa para ir del archivo `desde` al archivo `hacia` (ambas relativas a la raíz).
pub fn relativa(desde: &str, hacia: &str) -> String {
    let d: Vec<&str> = desde.split('/').collect();
    let d = &d[..d.len() - 1];
    let h: Vec<&str> = hacia.split('/').collect();
    let mut c = 0;
    while c < d.len() && c + 1 < h.len() && d[c] == h[c] {
        c += 1;
    }
    let mut s = "../".repeat(d.len() - c);
    s.push_str(&h[c..].join("/"));
    s
}

// ───────────────────────────── HTML ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tipo {
    /// Atributo con un recurso a bajar (src de img/script, href de link stylesheet…)
    Recurso,
    /// Atributo con un enlace que NO se baja (a href, iframe src, form action)
    Enlace,
    Srcset,
    /// Atributo style="…"
    CssAttr,
    /// Contenido de <style>…</style> (span sin comillas, texto crudo)
    CssBloque,
    Base,
}

#[derive(Debug)]
pub struct RefHtml {
    /// Para atributos el span incluye las comillas; para CssBloque es el contenido.
    pub inicio: usize,
    pub fin: usize,
    pub tipo: Tipo,
    pub valor: String,
}

fn buscar(h: &[u8], desde: usize, pat: &[u8]) -> Option<usize> {
    if desde >= h.len() {
        return None;
    }
    h[desde..].windows(pat.len()).position(|w| w == pat).map(|p| p + desde)
}

fn buscar_ci(h: &[u8], desde: usize, pat: &[u8]) -> Option<usize> {
    if desde >= h.len() {
        return None;
    }
    h[desde..].windows(pat.len()).position(|w| w.eq_ignore_ascii_case(pat)).map(|p| p + desde)
}

fn entidades(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut o = String::with_capacity(s.len());
    let mut resto = s;
    while let Some(p) = resto.find('&') {
        o.push_str(&resto[..p]);
        resto = &resto[p..];
        let fin = resto.find(';').filter(|&f| f <= 10);
        let reemplazo = fin.and_then(|f| {
            let ent = &resto[1..f];
            let c = match ent {
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
                _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, f))
        });
        match reemplazo {
            Some((c, f)) => {
                o.push(c);
                resto = &resto[f + 1..];
            }
            None => {
                o.push('&');
                resto = &resto[1..];
            }
        }
    }
    o.push_str(resto);
    o
}

fn attr_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

fn rel_recurso(rel: &str) -> bool {
    rel.split_ascii_whitespace().any(|r| {
        matches!(
            r.to_ascii_lowercase().as_str(),
            "stylesheet" | "icon" | "shortcut" | "apple-touch-icon" | "apple-touch-icon-precomposed" | "mask-icon" | "preload" | "modulepreload"
        )
    })
}

pub fn extraer_html(h: &[u8]) -> Vec<RefHtml> {
    let n = h.len();
    let mut i = 0;
    let mut out = Vec::new();
    while i < n {
        if h[i] != b'<' {
            i += 1;
            continue;
        }
        if h[i..].starts_with(b"<!--") {
            i = buscar(h, i + 4, b"-->").map(|p| p + 3).unwrap_or(n);
            continue;
        }
        if i + 1 >= n || !h[i + 1].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < n && (h[j].is_ascii_alphanumeric() || h[j] == b'-' || h[j] == b':') {
            j += 1;
        }
        let tag = String::from_utf8_lossy(&h[i + 1..j]).to_ascii_lowercase();
        // (nombre, inicio, fin, valor decodificado)
        let mut attrs: Vec<(String, usize, usize, String)> = Vec::new();
        loop {
            while j < n && h[j].is_ascii_whitespace() {
                j += 1;
            }
            if j >= n {
                break;
            }
            if h[j] == b'>' {
                j += 1;
                break;
            }
            if h[j] == b'/' {
                j += 1;
                continue;
            }
            let ns = j;
            while j < n && !h[j].is_ascii_whitespace() && !b"=>/".contains(&h[j]) {
                j += 1;
            }
            if j == ns {
                j += 1;
                continue;
            }
            let nombre = String::from_utf8_lossy(&h[ns..j]).to_ascii_lowercase();
            let mut k = j;
            while k < n && h[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < n && h[k] == b'=' {
                k += 1;
                while k < n && h[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < n && (h[k] == b'"' || h[k] == b'\'') {
                    let q = h[k];
                    let vs = k + 1;
                    let ve = h[vs..].iter().position(|&c| c == q).map(|p| p + vs).unwrap_or(n);
                    let fin = (ve + 1).min(n);
                    attrs.push((nombre, k, fin, entidades(&String::from_utf8_lossy(&h[vs..ve]))));
                    j = fin;
                } else {
                    let vs = k;
                    while k < n && !h[k].is_ascii_whitespace() && h[k] != b'>' {
                        k += 1;
                    }
                    attrs.push((nombre, vs, k, entidades(&String::from_utf8_lossy(&h[vs..k]))));
                    j = k;
                }
            }
        }
        clasificar(&tag, &attrs, &mut out);
        if tag == "script" || tag == "style" {
            let cierre = format!("</{tag}");
            let fin = buscar_ci(h, j, cierre.as_bytes()).unwrap_or(n);
            if tag == "style" && fin > j {
                out.push(RefHtml { inicio: j, fin, tipo: Tipo::CssBloque, valor: String::from_utf8_lossy(&h[j..fin]).into_owned() });
            }
            i = fin.max(j);
        } else {
            i = j.max(i + 1);
        }
    }
    out
}

fn clasificar(tag: &str, attrs: &[(String, usize, usize, String)], out: &mut Vec<RefHtml>) {
    let rel = attrs.iter().find(|a| a.0 == "rel").map(|a| a.3.as_str()).unwrap_or("");
    for (nombre, ini, fin, valor) in attrs {
        let tipo = match (tag, nombre.as_str()) {
            ("base", "href") => Some(Tipo::Base),
            (_, "style") => Some(Tipo::CssAttr),
            ("img" | "script" | "source" | "audio" | "video" | "track" | "embed" | "input", "src") => Some(Tipo::Recurso),
            ("img" | "source", "srcset") | (_, "data-srcset") => Some(Tipo::Srcset),
            (_, "data-src") | ("video", "poster") | ("image" | "use" | "feimage", "href" | "xlink:href") => Some(Tipo::Recurso),
            ("link", "href") => Some(if rel_recurso(rel) { Tipo::Recurso } else { Tipo::Enlace }),
            ("a" | "area", "href") | ("iframe" | "frame", "src") | ("form", "action") => Some(Tipo::Enlace),
            _ => None,
        };
        if let Some(tipo) = tipo {
            out.push(RefHtml { inicio: *ini, fin: *fin, tipo, valor: valor.clone() });
        }
    }
}

fn srcset(s: &str) -> Vec<(&str, &str)> {
    let b = s.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < n && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= n {
            break;
        }
        let us = i;
        while i < n && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let url = &s[us..i];
        if url.ends_with(',') {
            out.push((url.trim_end_matches(','), ""));
        } else {
            let ds = i;
            while i < n && b[i] != b',' {
                i += 1;
            }
            out.push((url, s[ds..i].trim()));
        }
    }
    out
}

// ───────────────────────────── CSS ─────────────────────────────

/// Referencias en CSS: (inicio, fin, valor) del texto dentro de url(…) o @import "…".
pub fn extraer_css(c: &[u8]) -> Vec<(usize, usize, String)> {
    let n = c.len();
    let mut out = Vec::new();
    let mut i = 0;
    let cadena = |k: usize, out: &mut Vec<(usize, usize, String)>| -> Option<usize> {
        if k < n && (c[k] == b'"' || c[k] == b'\'') {
            let q = c[k];
            let vs = k + 1;
            let ve = c[vs..].iter().position(|&x| x == q || x == b'\n').map(|p| p + vs).unwrap_or(n);
            out.push((vs, ve, String::from_utf8_lossy(&c[vs..ve]).into_owned()));
            Some(ve + 1)
        } else {
            None
        }
    };
    while i < n {
        if c[i..].starts_with(b"/*") {
            i = buscar(c, i + 2, b"*/").map(|p| p + 2).unwrap_or(n);
            continue;
        }
        if i + 4 <= n && c[i..i + 4].eq_ignore_ascii_case(b"url(") {
            let mut k = i + 4;
            while k < n && c[k].is_ascii_whitespace() {
                k += 1;
            }
            if let Some(sig) = cadena(k, &mut out) {
                i = sig;
            } else {
                let vs = k;
                let mut ve = c[vs..].iter().position(|&x| x == b')').map(|p| p + vs).unwrap_or(n);
                let fin = ve;
                while ve > vs && c[ve - 1].is_ascii_whitespace() {
                    ve -= 1;
                }
                if ve > vs {
                    out.push((vs, ve, String::from_utf8_lossy(&c[vs..ve]).into_owned()));
                }
                i = fin.max(i + 1);
            }
            continue;
        }
        if i + 7 <= n && c[i..i + 7].eq_ignore_ascii_case(b"@import") {
            let mut k = i + 7;
            while k < n && c[k].is_ascii_whitespace() {
                k += 1;
            }
            i = cadena(k, &mut out).unwrap_or(k);
            continue;
        }
        i += 1;
    }
    out
}

fn aplicar(orig: &[u8], mut cambios: Vec<(usize, usize, Vec<u8>)>) -> Vec<u8> {
    cambios.sort_by_key(|c| c.0);
    let mut o = Vec::with_capacity(orig.len());
    let mut pos = 0;
    for (ini, fin, nuevo) in cambios {
        if ini < pos {
            continue;
        }
        o.extend_from_slice(&orig[pos..ini]);
        o.extend_from_slice(&nuevo);
        pos = fin;
    }
    o.extend_from_slice(&orig[pos.min(orig.len())..]);
    o
}

// ───────────────────────────── Reescritura ─────────────────────────────

type Mapa = HashMap<String, Option<String>>;

/// Nuevo valor para una referencia hallada en el archivo local `desde`. `None` = dejar igual.
fn reemplazo(valor: &str, base: &Url, mapa: &Mapa, desde: &str) -> Option<String> {
    let u = base.resolver(valor)?;
    let frag = fragmento(valor);
    match mapa.get(&u.texto()) {
        Some(Some(local)) => Some(format!("{}{}", relativa(desde, local), frag)),
        _ if es_absoluta(valor) => None,
        _ => Some(format!("{}{}", u.texto(), frag)),
    }
}

fn reescribir_css(css: &[u8], base: &Url, mapa: &Mapa, desde: &str) -> Vec<u8> {
    let cambios = extraer_css(css)
        .into_iter()
        .filter_map(|(a, b, v)| reemplazo(&v, base, mapa, desde).map(|n| (a, b, n.into_bytes())))
        .collect();
    aplicar(css, cambios)
}

fn reescribir_html(h: &[u8], refs: &[RefHtml], base: &Url, mapa: &Mapa, desde: &str) -> Vec<u8> {
    let mut cambios = Vec::new();
    for r in refs {
        let nuevo = match r.tipo {
            Tipo::Base => Some("\"./\"".to_string()),
            Tipo::Recurso | Tipo::Enlace => reemplazo(&r.valor, base, mapa, desde).map(|v| format!("\"{}\"", attr_escape(&v))),
            Tipo::Srcset => {
                let partes: Vec<String> = srcset(&r.valor)
                    .into_iter()
                    .map(|(u, d)| {
                        let u = reemplazo(u, base, mapa, desde).unwrap_or_else(|| u.to_string());
                        if d.is_empty() { u } else { format!("{u} {d}") }
                    })
                    .collect();
                Some(format!("\"{}\"", attr_escape(&partes.join(", "))))
            }
            Tipo::CssAttr => {
                let n = reescribir_css(r.valor.as_bytes(), base, mapa, desde);
                Some(format!("\"{}\"", attr_escape(&String::from_utf8_lossy(&n))))
            }
            Tipo::CssBloque => {
                cambios.push((r.inicio, r.fin, reescribir_css(&h[r.inicio..r.fin], base, mapa, desde)));
                None
            }
        };
        if let Some(n) = nuevo {
            cambios.push((r.inicio, r.fin, n.into_bytes()));
        }
    }
    aplicar(h, cambios)
}

// ───────────────────────────── Descarga ─────────────────────────────

fn ext_de_tipo(ct: &str) -> Option<&'static str> {
    let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    Some(match ct.as_str() {
        "text/css" => "css",
        "text/javascript" | "application/javascript" | "application/x-javascript" => "js",
        "text/html" => "html",
        "application/json" | "application/manifest+json" => "json",
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/avif" => "avif",
        "image/svg+xml" => "svg",
        "image/x-icon" | "image/vnd.microsoft.icon" => "ico",
        "font/woff2" | "application/font-woff2" => "woff2",
        "font/woff" | "application/font-woff" => "woff",
        "font/ttf" | "application/x-font-ttf" => "ttf",
        "font/otf" => "otf",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        _ => return None,
    })
}

fn sanear_comp(s: &str) -> String {
    let mut o: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .take(80)
        .collect();
    if o.starts_with('.') {
        o.insert(0, '_');
    }
    if o.is_empty() {
        o.push('_');
    }
    o
}

fn nombre_local(u: &Url, origen: &Url, ct: &str, usados: &mut HashSet<String>) -> String {
    let mut comps: Vec<String> = Vec::new();
    if !u.mismo_origen(origen) {
        comps.push("_ext".into());
        comps.push(sanear_comp(&u.host));
    }
    let fijos = comps.len();
    comps.extend(u.ruta.split('/').filter(|s| !s.is_empty()).map(|s| sanear_comp(&super::pct_decode(s))));
    if u.ruta.ends_with('/') || comps.len() == fijos {
        comps.push("index".into());
    }
    let ultimo = comps.last_mut().unwrap();
    if let Some(e) = ext_de_tipo(ct) {
        let actual = ultimo.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase()).unwrap_or_default();
        let equivale = actual == e || (e == "jpg" && actual == "jpeg") || (e == "html" && actual == "htm") || (e == "js" && actual == "mjs");
        if !equivale {
            ultimo.push('.');
            ultimo.push_str(e);
        }
    }
    let base = comps.join("/");
    let mut cand = base.clone();
    let mut k = 2;
    while usados.contains(&cand) {
        cand = match base.rsplit_once('.') {
            Some((a, e)) if !e.contains('/') => format!("{a}-{k}.{e}"),
            _ => format!("{base}-{k}"),
        };
        k += 1;
    }
    usados.insert(cand.clone());
    cand
}

fn es_css(ct: &str, local: &str) -> bool {
    ct.to_ascii_lowercase().contains("text/css") || (ct.is_empty() && local.ends_with(".css"))
}

/// Baja una URL a `archivo`. Devuelve (URL efectiva tras redirecciones, content-type).
fn bajar(op: &Opciones, url: &str, archivo: &Path) -> Result<(String, String), String> {
    let out = Command::new(&op.curl)
        .args(["-sS", "-L", "--compressed", "--max-filesize", "30M", "--max-time", "60", "--max-redirs", "10"])
        .args(["--proto", "=http,https", "--proto-redir", "=http,https", "-A", UA, "-o"])
        .arg(archivo)
        .args(["-w", "%{url_effective}\\n%{content_type}\\n%{http_code}", "--url", url])
        .output()
        .map_err(|e| format!("no pude ejecutar curl: {e}"))?;
    if !out.status.success() {
        let _ = fs::remove_file(archivo);
        let motivo = match out.status.code() {
            Some(6) | Some(7) => "sin conexión o el dominio no existe".to_string(),
            Some(28) => "tardó demasiado".to_string(),
            Some(63) => "archivo de más de 30 MB".to_string(),
            _ => String::from_utf8_lossy(&out.stderr).trim().chars().take(160).collect(),
        };
        return Err(motivo);
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut l = s.lines();
    let efectiva = l.next().unwrap_or(url).to_string();
    let ct = l.next().unwrap_or("").to_string();
    let codigo: u16 = l.next().unwrap_or("0").trim().parse().unwrap_or(0);
    if codigo >= 400 {
        let _ = fs::remove_file(archivo);
        return Err(format!("el servidor respondió HTTP {codigo}"));
    }
    Ok((efectiva, ct))
}

fn encolar(u: Url, prof: u8, op: &Opciones, origen: &Url, mapa: &mut Mapa, cola: &mut VecDeque<(Url, u8)>) {
    if !op.externos && !u.mismo_origen(origen) {
        return;
    }
    let k = u.texto();
    if !mapa.contains_key(&k) {
        mapa.insert(k, None);
        cola.push_back((u, prof));
    }
}

/// Descarga `url` en `destino` (lo reemplaza solo si todo salió bien).
pub fn descargar(url: &str, destino: &Path, op: &Opciones, progreso: &dyn Fn(&str)) -> Result<Resultado, String> {
    let tmp = destino.with_extension("nueva");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| format!("no pude crear la carpeta: {e}"))?;
    let r = descargar_en(url, &tmp, op, progreso);
    match r {
        Ok(res) => {
            let _ = fs::remove_dir_all(destino);
            fs::rename(&tmp, destino).map_err(|e| format!("no pude guardar la copia: {e}"))?;
            Ok(res)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

fn descargar_en(url: &str, tmp: &Path, op: &Opciones, progreso: &dyn Fn(&str)) -> Result<Resultado, String> {
    let inicio = Instant::now();
    let base0 = Url::parse(url).ok_or("el enlace no es una URL http(s) válida")?;
    progreso("Descargando la página principal…");
    let idx = tmp.join("index.html");
    let (efectiva, ct) = bajar(op, &base0.texto(), &idx)?;
    if !ct.is_empty() && !ct.to_ascii_lowercase().contains("html") {
        return Err(format!("el enlace no es una página HTML ({ct})"));
    }
    let html = fs::read(&idx).map_err(|e| e.to_string())?;
    let origen = Url::parse(&efectiva).unwrap_or_else(|| base0.clone());
    let refs = extraer_html(&html);
    let mut base = origen.clone();
    if let Some(b) = refs.iter().find(|r| r.tipo == Tipo::Base) {
        if let Some(u) = origen.resolver(&b.valor) {
            base = u;
        }
    }

    let mut mapa: Mapa = HashMap::new();
    mapa.insert(origen.texto(), Some("index.html".into()));
    mapa.insert(base0.texto(), Some("index.html".into()));
    let mut cola = VecDeque::new();
    for r in &refs {
        match r.tipo {
            Tipo::Recurso => {
                if let Some(u) = base.resolver(&r.valor) {
                    encolar(u, 0, op, &origen, &mut mapa, &mut cola);
                }
            }
            Tipo::Srcset => {
                for (u, _) in srcset(&r.valor) {
                    if let Some(u) = base.resolver(u) {
                        encolar(u, 0, op, &origen, &mut mapa, &mut cola);
                    }
                }
            }
            Tipo::CssAttr | Tipo::CssBloque => {
                for (_, _, v) in extraer_css(r.valor.as_bytes()) {
                    if let Some(u) = base.resolver(&v) {
                        encolar(u, 0, op, &origen, &mut mapa, &mut cola);
                    }
                }
            }
            Tipo::Enlace | Tipo::Base => {}
        }
    }

    let mut usados: HashSet<String> = HashSet::from(["index.html".to_string()]);
    let mut css_docs: Vec<(String, Url, Vec<u8>)> = Vec::new();
    let mut total = html.len() as u64;
    let (mut n, mut ok, mut fallidos) = (0usize, 0usize, 0usize);
    let parcial = tmp.join(".bajando");
    while let Some((u, prof)) = cola.pop_front() {
        if n >= op.max_recursos || total >= op.max_total || inicio.elapsed() > op.limite_tiempo {
            fallidos += 1 + cola.len();
            break;
        }
        n += 1;
        progreso(&format!("Descargando recurso {} de {}…", n, n + cola.len()));
        let Ok((efect, ct)) = bajar(op, &u.texto(), &parcial) else {
            fallidos += 1;
            continue;
        };
        let tam = fs::metadata(&parcial).map(|m| m.len()).unwrap_or(0);
        if total + tam > op.max_total {
            let _ = fs::remove_file(&parcial);
            fallidos += 1;
            continue;
        }
        let local = nombre_local(&u, &origen, &ct, &mut usados);
        let dest = tmp.join(&local);
        let movido = dest.parent().map(|p| fs::create_dir_all(p).is_ok()).unwrap_or(false) && fs::rename(&parcial, &dest).is_ok();
        if !movido {
            let _ = fs::remove_file(&parcial);
            fallidos += 1;
            continue;
        }
        total += tam;
        ok += 1;
        mapa.insert(u.texto(), Some(local.clone()));
        if es_css(&ct, &local) {
            let bytes = fs::read(&dest).unwrap_or_default();
            let ubase = Url::parse(&efect).unwrap_or_else(|| u.clone());
            if prof < 4 {
                for (_, _, v) in extraer_css(&bytes) {
                    if let Some(x) = ubase.resolver(&v) {
                        encolar(x, prof + 1, op, &origen, &mut mapa, &mut cola);
                    }
                }
            }
            css_docs.push((local, ubase, bytes));
        }
    }
    let _ = fs::remove_file(&parcial);

    progreso("Reescribiendo rutas…");
    let nuevo = reescribir_html(&html, &refs, &base, &mapa, "index.html");
    fs::write(&idx, nuevo).map_err(|e| e.to_string())?;
    for (local, ubase, bytes) in css_docs {
        let n = reescribir_css(&bytes, &ubase, &mapa, &local);
        let _ = fs::write(tmp.join(&local), n);
    }
    Ok(Resultado { archivos: ok + 1, bytes: total, fallidos })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resuelve_urls() {
        let b = Url::parse("HTTPS://Ejemplo.com:443/a/b/pagina.html?x=1#top").unwrap();
        assert_eq!(b.host, "ejemplo.com");
        assert_eq!(b.ruta, "/a/b/pagina.html");
        assert_eq!(b.resolver("../css/s.css?v=2#f").unwrap().texto(), "https://ejemplo.com/a/css/s.css?v=2");
        assert_eq!(b.resolver("/img/x.png").unwrap().texto(), "https://ejemplo.com/img/x.png");
        assert_eq!(b.resolver("//cdn.net/x.js").unwrap().texto(), "https://cdn.net/x.js");
        assert_eq!(b.resolver("?p=2").unwrap().texto(), "https://ejemplo.com/a/b/pagina.html?p=2");
        assert_eq!(b.resolver("../../../../x").unwrap().ruta, "/x");
        assert!(b.resolver("data:image/png;base64,AAA").is_none());
        assert!(b.resolver("mailto:yo@x.com").is_none());
        assert!(b.resolver("javascript:alert(1)").is_none());
        assert!(b.resolver("#seccion").is_none());
        assert!(Url::parse("file:///etc/passwd").is_none());
        assert!(Url::parse("http://").is_none());
    }

    #[test]
    fn rutas_relativas() {
        assert_eq!(relativa("index.html", "css/a.css"), "css/a.css");
        assert_eq!(relativa("css/a.css", "img/b.png"), "../img/b.png");
        assert_eq!(relativa("css/a.css", "css/f/x.woff"), "f/x.woff");
        assert_eq!(relativa("_ext/cdn/x/y.css", "index.html"), "../../../index.html");
    }

    #[test]
    fn extrae_y_reescribe() {
        let h = br#"<html><head><link rel="stylesheet" href="css/a.css"><link rel=canonical href=/yo>
<style>body{background:url('img/f.png')}</style><!-- <img src="no.png"> --></head>
<body><img src=foto.jpg srcset="foto.jpg 1x, foto@2x.jpg 2x" style="background:url(x.png)">
<a href="otra.html?a=1&amp;b=2">otra</a><script>var s="<img src='js.png'>";</script></body></html>"#;
        let refs = extraer_html(h);
        let tipos: Vec<(Tipo, &str)> = refs.iter().map(|r| (r.tipo, r.valor.as_str())).collect();
        assert!(tipos.contains(&(Tipo::Recurso, "css/a.css")));
        assert!(tipos.contains(&(Tipo::Enlace, "/yo")));
        assert!(tipos.contains(&(Tipo::Recurso, "foto.jpg")));
        assert!(tipos.contains(&(Tipo::Enlace, "otra.html?a=1&b=2")));
        assert!(!tipos.iter().any(|t| t.1.contains("no.png") || t.1.contains("js.png")));
        let base = Url::parse("http://e.com/p/").unwrap();
        let mut mapa: Mapa = HashMap::new();
        mapa.insert("http://e.com/p/css/a.css".into(), Some("p/css/a.css".into()));
        mapa.insert("http://e.com/p/img/f.png".into(), Some("p/img/f.png".into()));
        let o = String::from_utf8(reescribir_html(h, &refs, &base, &mapa, "index.html")).unwrap();
        assert!(o.contains(r#"href="p/css/a.css""#), "{o}");
        assert!(o.contains("url('p/img/f.png')"), "{o}");
        assert!(o.contains(r#"href="http://e.com/p/otra.html?a=1&amp;b=2""#), "{o}");
        assert!(o.contains(r#"srcset="http://e.com/p/foto.jpg 1x, http://e.com/p/foto@2x.jpg 2x""#), "{o}");
        assert!(o.contains("<img src='js.png'>"), "el JS no se toca: {o}");
    }

    #[test]
    fn css_urls() {
        let c = br#"@import "b.css"; /* url(no.png) */ a{src:url( "f.woff2" ) format("woff2"), url(g.ttf)} b{x:url(data:x)}"#;
        let v: Vec<String> = extraer_css(c).into_iter().map(|x| x.2).collect();
        assert_eq!(v, vec!["b.css", "f.woff2", "g.ttf", "data:x"]);
    }

    #[test]
    fn nombres_locales() {
        let o = Url::parse("https://e.com/").unwrap();
        let mut u = HashSet::from(["index.html".to_string()]);
        let f = Url::parse("https://fonts.googleapis.com/css2?family=Inter").unwrap();
        assert_eq!(nombre_local(&f, &o, "text/css; charset=utf-8", &mut u), "_ext/fonts.googleapis.com/css2.css");
        let a = Url::parse("https://e.com/s.css?v=1").unwrap();
        let b = Url::parse("https://e.com/s.css?v=2").unwrap();
        assert_eq!(nombre_local(&a, &o, "text/css", &mut u), "s.css");
        assert_eq!(nombre_local(&b, &o, "text/css", &mut u), "s-2.css");
        let r = Url::parse("https://e.com/").unwrap();
        assert_eq!(nombre_local(&r, &o, "text/html", &mut u), "index-2.html");
        let x = Url::parse("https://e.com/mi%20foto/.oculto").unwrap();
        assert_eq!(nombre_local(&x, &o, "", &mut u), "mi_foto/_.oculto");
    }
}
