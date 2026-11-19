//! Páginas propias que ve el invitado cuando no hay sitio que servir: la landing con el
//! enlace + QR, y "aún no hay portafolio". Todo dato configurable se escapa.

use crate::util::html;
use qrcode::render::svg;
use qrcode::{EcLevel, QrCode};

const ESTILO: &str = r#"
:root{--bg:#f3f4f7;--card:#ffffff;--tx:#14161c;--mut:#5f6576;--ac:#3b6cf6;--ac2:#7c4dff;--borde:#e3e5ec}
@media (prefers-color-scheme:dark){:root{--bg:#0e1014;--card:#171a21;--tx:#eceef3;--mut:#9095a5;--borde:#262a35}}
*{box-sizing:border-box}html,body{margin:0}
body{min-height:100vh;font:16px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,system-ui,sans-serif;background:var(--bg);color:var(--tx);
display:flex;align-items:center;justify-content:center;padding:24px 16px}
main{width:100%;max-width:420px;text-align:center}
.card{background:var(--card);border:1px solid var(--borde);border-radius:24px;padding:32px 22px 26px;box-shadow:0 10px 40px #0000000f}
.av{width:76px;height:76px;margin:0 auto 16px;border-radius:50%;display:flex;align-items:center;justify-content:center;
font-size:30px;font-weight:700;color:#fff;background:linear-gradient(135deg,var(--ac),var(--ac2));letter-spacing:.5px}
h1{font-size:26px;line-height:1.2;margin:0 0 6px;letter-spacing:-.3px}
.sub{color:var(--mut);margin:0 0 22px}
.btn{display:block;text-decoration:none;color:#fff;background:linear-gradient(135deg,var(--ac),var(--ac2));border-radius:14px;
padding:15px 16px;font-weight:600;font-size:17px;word-break:break-all}
.btn small{display:block;font-weight:400;opacity:.85;font-size:13px;margin-top:2px}
.qr{margin:22px auto 6px;width:min(72vw,240px);padding:12px;background:#fff;border-radius:18px;border:1px solid var(--borde)}
.qr svg{display:block;width:100%;height:auto}
.nota{color:var(--mut);font-size:14px;margin:14px 4px 0}
.copiar{margin-top:14px;display:flex;gap:8px}
.copiar input{flex:1;min-width:0;padding:10px 12px;border-radius:10px;border:1px solid var(--borde);background:var(--bg);color:var(--tx);font-size:14px}
.copiar button{border:0;border-radius:10px;padding:10px 14px;background:var(--tx);color:var(--card);font-weight:600;font-size:14px}
footer{color:var(--mut);font-size:12px;margin-top:18px;opacity:.8}
"#;

fn iniciales(t: &str) -> String {
    let s: String = t
        .split_whitespace()
        .filter(|p| p.chars().next().is_some_and(|c| c.is_alphanumeric()))
        .filter(|p| !matches!(p.to_lowercase().as_str(), "de" | "del" | "la" | "el" | "y" | "portafolio"))
        .take(2)
        .filter_map(|p| p.chars().next())
        .flat_map(|c| c.to_uppercase())
        .collect();
    if s.is_empty() { "✦".into() } else { s }
}

fn pagina(titulo: &str, cuerpo: &str) -> String {
    format!(
        "<!doctype html><html lang=es><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1,viewport-fit=cover\">\
<title>{}</title><style>{ESTILO}</style></head><body><main>{cuerpo}</main></body></html>",
        html(titulo)
    )
}

/// SVG del QR (sin la declaración XML) o vacío si el texto no cabe.
pub fn qr_svg(texto: &str) -> String {
    match QrCode::with_error_correction_level(texto.as_bytes(), EcLevel::M) {
        Ok(c) => {
            let s = c.render::<svg::Color>().min_dimensions(200, 200).quiet_zone(false).build();
            match s.find("<svg") {
                Some(i) => s[i..].to_string(),
                None => s,
            }
        }
        Err(_) => String::new(),
    }
}

/// Texto corto para el botón: dominio + ruta, sin esquema.
fn corto(url: &str) -> String {
    let s = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let s = s.trim_end_matches('/');
    if s.chars().count() > 48 { format!("{}…", s.chars().take(46).collect::<String>()) } else { s.to_string() }
}

pub fn landing(titulo: &str, subtitulo: &str, url: &str) -> String {
    let t = if titulo.trim().is_empty() { "Portafolio" } else { titulo.trim() };
    let sub = if subtitulo.trim().is_empty() { "Conoce mi trabajo" } else { subtitulo.trim() };
    let qr = qr_svg(url);
    let qr = if qr.is_empty() { String::new() } else { format!("<div class=qr role=img aria-label=\"Código QR del enlace\">{qr}</div>") };
    let cuerpo = format!(
        r#"<div class=card><div class=av>{ini}</div><h1>{t}</h1><p class=sub>{sub}</p>
<a class=btn href="{u}" target=_blank rel=noopener>Ver mi portafolio<small>{c}</small></a>
{qr}
<p class=nota>Esta red WiFi no tiene internet. Escanea el código con tu cámara o guarda el enlace para abrirlo cuando tengas datos.</p>
<div class=copiar><input id=u value="{u}" readonly aria-label="Enlace"><button type=button id=b>Copiar</button></div></div>
<footer>Compartido desde un punto WiFi local</footer>
<script>(function(){{var i=document.getElementById('u'),b=document.getElementById('b');
b.onclick=function(){{i.focus();i.select();i.setSelectionRange(0,9999);var ok=false;
try{{ok=document.execCommand('copy')}}catch(e){{}}
if(navigator.clipboard&&navigator.clipboard.writeText){{navigator.clipboard.writeText(i.value).then(function(){{b.textContent='¡Copiado!'}},function(){{}})}}
b.textContent=ok?'¡Copiado!':'Mantén presionado';}};}})();</script>"#,
        ini = html(&iniciales(t)),
        t = html(t),
        sub = html(sub),
        u = html(url),
        c = html(&corto(url)),
    );
    pagina(t, &cuerpo)
}

pub fn vacia(titulo: &str) -> String {
    let t = if titulo.trim().is_empty() { "Portafolio" } else { titulo.trim() };
    let cuerpo = format!(
        "<div class=card><div class=av>{}</div><h1>{}</h1><p class=sub>Aún no hay portafolio publicado aquí.</p>\
<p class=nota>Vuelve en un rato: el dueño de esta red todavía está preparando su página.</p></div>",
        html(&iniciales(t)),
        html(t)
    );
    pagina(t, &cuerpo)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn iniciales_bonitas() {
        assert_eq!(iniciales("Portafolio de Yael Montoya"), "YM");
        assert_eq!(iniciales("ana"), "A");
        assert_eq!(iniciales(""), "✦");
    }
    #[test]
    fn qr_es_svg() {
        let s = qr_svg("https://ejemplo.com/portafolio");
        assert!(s.starts_with("<svg") && s.ends_with("</svg>"));
    }
}
