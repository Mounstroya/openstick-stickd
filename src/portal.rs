//! Panel de administración en el puerto 80. Accesible desde el cable USB (192.168.5.x), el túnel
//! WireGuard (10.x), localhost y el hotspot de CONFIGURACIÓN (192.168.4.x, con contraseña).
//! Cuando hay una VITRINA al aire, 192.168.4.x es un WiFi abierto de desconocidos: ahí el panel
//! se bloquea (además el firewall de captive.rs redirige su puerto 80 al servidor de invitados).

use crate::captive::{self, Activa};
use crate::util::{form, html, json};
use crate::{net, save_mode, save_powerbank, save_ssid, ssid_for, Mode, Request, State, Vitrinas};
use std::io::Read;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server};

const MAX_UPLOAD: u64 = 25 * 1024 * 1024;

fn allowed(ip: Option<IpAddr>, vitrina_al_aire: bool) -> bool {
    match ip {
        Some(IpAddr::V4(v)) => {
            let o = v.octets();
            let ap = o[0] == 192 && o[1] == 168 && o[2] == 4;
            v.is_loopback() || (o[0] == 192 && o[1] == 168 && o[2] == 5) || o[0] == 10 || (ap && !vitrina_al_aire)
        }
        Some(IpAddr::V6(v)) => v.is_loopback(),
        None => false,
    }
}

pub fn spawn(state: Arc<Mutex<State>>, vit: Vitrinas, activa: Activa) {
    std::thread::spawn(move || {
        let server = match Server::http("0.0.0.0:80") {
            Ok(s) => s,
            Err(e) => {
                crate::util::log(&format!("portal: no pude abrir el puerto 80: {e}"));
                return;
            }
        };
        for mut req in server.incoming_requests() {
            let ip = req.remote_addr().map(|a| a.ip());
            if !allowed(ip, activa.lock().unwrap().is_some()) {
                let _ = req.respond(Response::from_string("prohibido").with_status_code(403));
                continue;
            }
            let ct = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Content-Type"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let url = req.url().to_string();
            let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
            let (path, query) = (path.to_string(), query.to_string());
            let is_post = *req.method() == Method::Post;
            if is_post && !same_origin(&req) {
                // anti-CSRF: una página web cualquiera no puede mandar formularios al panel
                let _ = req.respond(Response::from_string("origen no permitido").with_status_code(403));
                continue;
            }
            let mut body = Vec::new();
            if is_post {
                let lim = if path.starts_with("/vitrina/") { MAX_UPLOAD } else { 8192 };
                let _ = req.as_reader().take(lim).read_to_end(&mut body);
            }

            let resp = if path == "/api/estado" {
                Response::from_string(status_json(&state, &vit)).with_header(hdr("Content-Type", "application/json; charset=utf-8"))
            } else if let Some(rest) = path.strip_prefix("/vitrina/") {
                let (id, sub) = rest.split_once('/').unwrap_or((rest, ""));
                match vit.iter().find(|v| v.id() == id) {
                    None => Response::from_string("no existe").with_status_code(404),
                    Some(v) if is_post => {
                        let f = if ct.starts_with("application/x-www-form-urlencoded") {
                            form(&String::from_utf8_lossy(&body))
                        } else {
                            form(&query)
                        };
                        let msg = v.admin_post(sub, &f, &body);
                        if ct.starts_with("application/x-www-form-urlencoded") {
                            page(&state, &vit, &msg, Some(v.id()))
                        } else {
                            // subida por fetch(): responde texto plano para el JS del módulo
                            Response::from_string(msg).with_header(hdr("Content-Type", "text/plain; charset=utf-8"))
                        }
                    }
                    Some(v) => match v.admin_get(sub) {
                        Some(r) => captive::to_tiny(r),
                        None => Response::from_string("no encontrado").with_status_code(404),
                    },
                }
            } else if is_post {
                let msg = handle_post(&state, &vit, &path, &form(&String::from_utf8_lossy(&body)));
                page(&state, &vit, &msg, None)
            } else {
                page(&state, &vit, "", None)
            };
            let _ = req.respond(resp);
        }
    });
}

/// El POST debe venir del propio panel: Origin (o Referer) con el mismo host que la petición.
/// Sin ninguno de los dos (curl, scripts locales) se acepta: los navegadores siempre mandan Origin en POST.
fn same_origin(req: &tiny_http::Request) -> bool {
    let get = |n: &'static str| req.headers().iter().find(|h| h.field.equiv(n)).map(|h| h.value.as_str().to_string());
    let host = get("Host").unwrap_or_default();
    let src = get("Origin").filter(|o| o != "null").or_else(|| get("Referer"));
    match src {
        None => get("Origin").is_none(), // Origin: null (sandbox, data:) se rechaza
        Some(o) => {
            let rest = o.split_once("://").map(|(_, r)| r).unwrap_or("");
            let o_host = rest.split('/').next().unwrap_or("");
            !host.is_empty() && o_host.eq_ignore_ascii_case(&host)
        }
    }
}

fn hdr(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn field<'a>(f: &'a [(String, String)], k: &str) -> &'a str {
    f.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("")
}

fn handle_post(state: &Arc<Mutex<State>>, vit: &Vitrinas, path: &str, f: &[(String, String)]) -> String {
    match path {
        "/modo" if field(f, "modo").trim().is_empty() => "Elige una opción de la lista antes de guardar.".into(),
        "/modo" => match Mode::parse(field(f, "modo"), vit) {
            Some(m) => {
                save_mode(m);
                let mut s = state.lock().unwrap();
                s.mode = m;
                // si está en configuración, salir primero (al salir se aplica el modo nuevo)
                s.request = Some(if s.config_mode { Request::ExitConfig } else { Request::ApplyMode });
                format!("Modo cambiado a «{}».", html(&mode_label(m, vit)))
            }
            None => "Modo no válido.".into(),
        },
        "/powerbank" => {
            let v = field(f, "valor");
            let id = vit.iter().find(|x| x.id() == v).map(|x| x.id());
            save_powerbank(id);
            let mut s = state.lock().unwrap();
            s.powerbank = id;
            s.request = Some(Request::ApplyMode);
            match id {
                Some(i) => format!("En powerbank arrancará: {}.", html(vit.iter().find(|x| x.id() == i).map(|x| x.nombre()).unwrap_or(i))),
                None => "En powerbank se queda en su modo normal.".into(),
            }
        }
        "/ssid" => {
            let id = field(f, "id");
            let ssid = field(f, "ssid").trim();
            if ssid.is_empty() || ssid.len() > 32 {
                return "El nombre de la red debe tener de 1 a 32 caracteres.".into();
            }
            match vit.iter().find(|x| x.id() == id) {
                Some(v) => {
                    save_ssid(v.id(), ssid);
                    let mut s = state.lock().unwrap();
                    if s.effective == Mode::Vitrina(v.id()) {
                        s.request = Some(Request::ApplyMode);
                    }
                    format!("La red de «{}» ahora se llama «{}».", html(v.nombre()), html(ssid))
                }
                None => "Vitrina desconocida.".into(),
            }
        }
        "/wifi" => {
            let ssid = if !field(f, "ssid_manual").is_empty() { field(f, "ssid_manual") } else { field(f, "ssid") };
            let psk = field(f, "psk");
            if ssid.is_empty() || ssid.len() > 32 {
                return "Falta el nombre de la red (máx. 32 caracteres).".into();
            }
            if !psk.is_empty() && (psk.len() < 8 || psk.len() > 63) {
                return "La contraseña WiFi debe tener de 8 a 63 caracteres.".into();
            }
            match net::add_wifi(ssid, psk) {
                Ok(name) => {
                    let mut s = state.lock().unwrap();
                    if s.config_mode {
                        s.request = Some(Request::ExitConfig);
                    }
                    format!("Red guardada como «{}». El stick saldrá del modo configuración y se conectará; este WiFi se apagará.", html(&name))
                }
                Err(e) => format!("No se pudo guardar: {}", html(&e)),
            }
        }
        "/olvidar" => {
            let name = field(f, "con");
            if net::forget_wifi(name) { format!("Red «{}» olvidada.", html(name)) } else { "No se pudo olvidar esa red.".into() }
        }
        "/config" => {
            let mut s = state.lock().unwrap();
            match field(f, "accion") {
                "entrar" => {
                    s.request = Some(Request::EnterConfig);
                    "Entrando a modo configuración: aparecerá la red «Openstick».".into()
                }
                "salir" => {
                    s.request = Some(Request::ExitConfig);
                    "Saliendo de modo configuración.".into()
                }
                _ => "Acción no válida.".into(),
            }
        }
        _ => "Ruta desconocida.".into(),
    }
}

fn mode_label(m: Mode, vit: &Vitrinas) -> String {
    match m {
        Mode::Vitrina(id) => format!("Vitrina: {}", vit.iter().find(|v| v.id() == id).map(|v| v.nombre()).unwrap_or(id)),
        Mode::Auto => "Conexión: automático".into(),
        Mode::Usb => "Conexión: solo USB".into(),
        Mode::Casa => "Conexión: solo WiFi".into(),
        Mode::Lte => "Conexión: solo LTE".into(),
    }
}

fn status_json(state: &Arc<Mutex<State>>, vit: &Vitrinas) -> String {
    let (mode, eff, cfg, internet, uplink, laptop, sharing, host, pb) = {
        let s = state.lock().unwrap();
        (s.mode, s.effective, s.config_mode, s.internet, s.uplink.clone(), s.laptop.clone(), s.usb_sharing, s.host_present, s.powerbank)
    };
    let l = net::lte();
    let vit_res: Vec<String> = vit.iter().map(|v| format!("{}:{}", json(v.id()), json(&v.resumen()))).collect();
    format!(
        "{{\"modo\":{},\"efectivo\":{},\"configuracion\":{},\"internet\":{},\"salida\":{},\"pc_conectada\":{},\"powerbank\":{},\"laptop\":{},\"laptop_comparte\":{},\"wifi\":{},\"lte\":{{\"estado\":{},\"senal\":{},\"operador\":{},\"tecnologia\":{},\"sim\":{}}},\"wireguard\":{},\"ips\":{},\"vitrinas\":{{{}}}}}",
        json(&mode.key()),
        json(&mode_label(eff, vit)),
        cfg,
        internet,
        json(&salida_humana(&uplink, sharing)),
        host,
        json(pb.unwrap_or("")),
        json(laptop.as_deref().unwrap_or("")),
        sharing,
        json(&net::current_ssid()),
        json(&l.state),
        json(&l.signal),
        json(&l.operator),
        json(&l.tech),
        l.sim,
        json(&net::wireguard()),
        json(&net::addrs()),
        vit_res.join(",")
    )
}

fn salida_humana(uplink: &str, sharing: bool) -> String {
    match uplink {
        "usb0" if sharing => "USB (laptop)".into(),
        "wlan0" => "WiFi".into(),
        u if u.starts_with("wwan") || u.starts_with("rmnet") => "LTE".into(),
        "" => "ninguna".into(),
        u => u.into(),
    }
}

fn page(state: &Arc<Mutex<State>>, vit: &Vitrinas, msg: &str, abrir: Option<&str>) -> Response<std::io::Cursor<Vec<u8>>> {
    let (mode, cfg, scan, pb) = {
        let s = state.lock().unwrap();
        (s.mode, s.config_mode, s.scan.clone(), s.powerbank)
    };
    let radio = |key: &str, label: &str, desc: &str| {
        let actual = mode.key() == key;
        format!(
            "<label class=opt><input type=radio name=modo value=\"{}\" {}><span><b>{}</b>{}<small>{}</small></span></label>",
            html(key),
            if actual { "checked" } else { "" },
            html(label),
            if actual { " <span class=tag>guardado</span>" } else { "" },
            html(desc)
        )
    };
    let mut r_con = String::new();
    r_con += &radio("auto", "Automático (recomendado)", "Usa lo que haya, en este orden: cable USB, WiFi guardado, LTE de la SIM.");
    r_con += &radio("usb", "Solo USB", "Solo el internet que comparte la laptop por el cable.");
    r_con += &radio("casa", "Solo WiFi", "Se conecta a tus redes WiFi guardadas. La SIM no se usa.");
    r_con += &radio("lte", "Solo LTE", "Usa los datos de la SIM. El WiFi no se usa.");
    let mut r_vit = String::new();
    let mut pb_opts = format!("<option value=\"\" {}>Nada (quedarse en su modo)</option>", if pb.is_none() { "selected" } else { "" });
    let mut vit_cards = String::new();
    for v in vit.iter() {
        r_vit += &radio(&format!("vitrina:{}", v.id()), v.nombre(), v.descripcion());
        pb_opts += &format!(
            "<option value=\"{}\" {}>{}</option>",
            html(v.id()),
            if pb == Some(v.id()) { "selected" } else { "" },
            html(v.nombre())
        );
        let ssid_form = if v.usa_ap() {
            format!(
                "<form method=post action=/ssid class=row><input type=hidden name=id value=\"{}\"><label style=flex:1>Nombre de la red WiFi que crea<input type=text name=ssid maxlength=32 value=\"{}\"></label><button>Guardar</button></form>",
                html(v.id()),
                html(&ssid_for(v.as_ref()))
            )
        } else {
            "<p class=mut>Este modo no crea WiFi: usa el radio para escuchar.</p>".into()
        };
        let al_aire = mode == Mode::Vitrina(v.id());
        vit_cards += &format!(
            "<section class=\"card plegable\" id=\"v-{id}\"><details{open}><summary><span class=st><span class=h2>{nombre}{tag}</span><small class=mut>{res}</small></span><span class=abrir></span></summary><div class=cuerpo>{ssid_form}<div class=mod>{admin}</div></div></details></section>",
            id = html(v.id()),
            open = if abrir == Some(v.id()) || v.abierta() { " open" } else { "" },
            nombre = html(v.nombre()),
            tag = if al_aire { " <span class=tag>al aire</span>" } else { "" },
            res = html(&v.resumen()),
            admin = v.admin_html(),
        );
    }
    let lado = if matches!(mode, Mode::Vitrina(_)) { "vit" } else { "con" };
    let saved: String = net::saved_wifis()
        .iter()
        .map(|(n, ssid)| {
            format!(
                "<li><span>{}</span><form method=post action=/olvidar onsubmit=\"return confirm('¿Olvidar esta red?')\"><input type=hidden name=con value=\"{}\"><button class=link>Olvidar</button></form></li>",
                html(ssid),
                html(n)
            )
        })
        .collect();
    let options: String = scan
        .iter()
        .map(|(s, sig, sec)| format!("<option value=\"{0}\">{0} — señal {1}% {2}</option>", html(s), sig, html(sec)))
        .collect();
    let html_page = format!(
        r#"<!doctype html><html lang=es><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<meta name=color-scheme content="light dark"><title>OpenStick</title><style>
:root{{--bg:#0f1115;--card:#181b22;--soft:#222631;--tx:#e8eaf0;--mut:#9aa0ae;--ac:#6aa0ff;--acb:#2f6fe4;--badb:#c93636;--ok:#3ecf8e;--bad:#ff6b6b;--bd:#ffffff2a}}
@media (prefers-color-scheme:light){{:root{{--bg:#eef0f4;--card:#fff;--soft:#f3f5f9;--tx:#14161c;--mut:#555b69;--ac:#1f5fd6;--acb:#1f5fd6;--badb:#c62828;--ok:#11804f;--bad:#c62828;--bd:#00000026}}}}
*{{box-sizing:border-box}}body{{margin:0;font:16px/1.45 system-ui,sans-serif;background:var(--bg);color:var(--tx)}}
main{{max-width:640px;margin:auto;padding:16px}}h1{{font-size:22px;margin:8px 0 16px}}h2,.h2{{font-size:17px;margin:0;font-weight:700}}
.card{{background:var(--card);border:1px solid var(--bd);border-radius:14px;padding:16px;margin-bottom:14px}}.card>h2{{margin-bottom:10px}}
.msg{{background:var(--acb);color:#fff;border-radius:10px;padding:10px 12px;margin-bottom:14px}}
dl{{display:grid;grid-template-columns:auto 1fr;gap:4px 12px;margin:0}}dt{{color:var(--mut)}}dd{{margin:0;word-break:break-word}}
.ok{{color:var(--ok)}}.bad{{color:var(--bad)}}.mut{{color:var(--mut)}}pre{{margin:0;font-size:12px;white-space:pre-wrap;color:var(--mut)}}
.opt{{display:flex;gap:10px;padding:10px;align-items:flex-start;border:1px solid var(--bd);border-radius:10px;margin:0 0 8px;cursor:pointer}}
.opt:has(input:checked){{border-color:var(--ac);background:var(--soft)}}.opt input{{margin-top:4px;width:18px;height:18px;flex:none}}.opt small{{display:block;color:var(--mut)}}
input[type=text],input[type=password],input[type=email],input[type=url],input[type=tel],input[type=number],select,textarea{{width:100%;padding:10px;border-radius:8px;border:1px solid var(--bd);background:var(--bg);color:var(--tx);margin:4px 0 10px;font-size:16px}}
label{{display:block}}label.opt,label.chk{{display:flex}}
button,a.btn{{display:inline-block;background:var(--acb);color:#fff;border:1px solid var(--acb);border-radius:8px;padding:10px 16px;font:600 15px/1.2 system-ui,sans-serif;cursor:pointer;text-decoration:none;text-align:center}}
button:disabled{{opacity:.5;cursor:not-allowed}}button:focus-visible,a.btn:focus-visible,summary:focus-visible{{outline:3px solid var(--ac);outline-offset:2px}}
button.sec,a.btn.sec{{background:transparent;color:var(--ac);border-color:var(--ac)}}button.peligro{{background:var(--badb);border-color:var(--badb)}}
button.link{{background:transparent;color:var(--bad);border:1px solid var(--bad);padding:5px 10px;font-size:14px}}
ul{{list-style:none;padding:0;margin:0 0 12px}}li{{display:flex;justify-content:space-between;align-items:center;gap:8px;padding:6px 0;border-bottom:1px solid var(--bd)}}
summary{{cursor:pointer;list-style:none;display:flex;align-items:center;gap:10px;padding:12px;border:1px solid var(--bd);border-radius:10px;background:var(--soft);user-select:none}}
summary::-webkit-details-marker{{display:none}}summary::before{{content:"▸";flex:none;width:1em;font-size:20px;line-height:1;color:var(--ac)}}
details[open]>summary::before{{content:"▾"}}summary:hover{{border-color:var(--ac)}}
summary .st{{flex:1;min-width:0}}summary .st small{{display:block;font-size:14px}}
summary .abrir{{flex:none;font-size:13px;color:var(--ac);white-space:nowrap}}summary .abrir::after{{content:"tocar para abrir"}}details[open]>summary .abrir::after{{content:"tocar para cerrar"}}
details[open]>summary{{margin-bottom:12px}}details.chica>summary{{padding:8px 10px;font-size:14px;margin-top:8px}}details.chica>summary::before{{font-size:16px}}
.card.plegable{{padding:10px}}.cuerpo{{padding:0 6px 6px}}
.tag{{display:inline-block;background:var(--ok);color:var(--card);font-size:12px;font-weight:700;border-radius:6px;padding:1px 7px;margin-left:4px;vertical-align:middle}}
.row{{display:flex;gap:10px;align-items:flex-end}}.row button{{margin-bottom:10px}}
.mod{{border-top:1px solid var(--bd);padding-top:12px}}h3{{font-size:15px;margin:14px 0 6px}}
.seg{{display:grid;grid-template-columns:1fr 1fr;border:1px solid var(--ac);border-radius:10px;overflow:hidden;margin:0 0 12px}}
.seg button{{border:0;border-radius:0;background:transparent;color:var(--ac);padding:12px 8px;font-size:16px}}.seg button[aria-pressed=true]{{background:var(--acb);color:#fff}}
.seg[hidden]{{display:none}}fieldset{{border:0;padding:0;margin:0 0 6px}}legend{{font-weight:700;padding:0;margin:6px 0 6px}}
#modo[data-js] legend{{display:none}}#modo[data-js][data-lado=con] .g-vit,#modo[data-js][data-lado=vit] .g-con{{display:none}}
fieldset>p{{margin:0 0 8px;font-size:14px}}
</style></head><body><main>
<h1>OpenStick · UZ801</h1>
{msg_html}
<section class=card><h2>Estado</h2><dl id=st><dt>Cargando…</dt><dd></dd></dl></section>
<section class=card><h2>Modo de trabajo</h2>
<form method=post action=/modo id=modo data-lado={lado}>
<div class=seg id=seg hidden><button type=button data-l=con aria-pressed=false>Conexión</button><button type=button data-l=vit aria-pressed=false>Vitrina</button></div>
<fieldset class=g-con><legend>Conexión</legend><p class=mut>El stick da internet a tu laptop. Elige de dónde lo saca:</p>{r_con}</fieldset>
<fieldset class=g-vit><legend>Vitrina</legend><p class=mut>El stick crea su propio WiFi y muestra algo a quien se conecte:</p>{r_vit}</fieldset>
<p id=modo-aviso class=bad hidden></p>
<button>Guardar modo</button></form></section>
<section class=card><h2>Al conectarlo a una powerbank</h2>
<p class=mut style="margin-top:0">Si no hay una computadora en el USB por 10 s, el stick arranca esta vitrina sola. Al volver a conectarlo a la PC regresa a su modo.</p>
<form method=post action=/powerbank class=row><select name=valor style=flex:1>{pb_opts}</select><button>Guardar</button></form></section>
<h2 style="margin:22px 0 4px">Vitrinas</h2>
<p class=mut style="margin:0 0 10px">Toca una para configurarla. Para ponerla al aire elígela arriba, en «Modo de trabajo» → «Vitrina».</p>
{vit_cards}
<section class=card><h2>Redes WiFi para tener internet</h2><ul>{saved}</ul>
<form method=post action=/wifi>
<label>Red cercana<select name=ssid><option value="">— elegir —</option>{options}</select></label>
<label>…o escribe el nombre<input type=text name=ssid_manual maxlength=32 autocomplete=off></label>
<label>Contraseña<input type=password name=psk maxlength=63></label>
<button>Guardar y conectar</button></form></section>
<section class=card><h2>Modo configuración</h2>
<p class=mut style="margin-top:0">Crea la red WiFi «Openstick» para entrar a este panel desde el celular (192.168.4.1). Atajo: pulsación corta del botón (agujerito) = entrar/salir; 8 s = reiniciar. También entra solo tras 2 min sin internet.</p>
<form method=post action=/config><input type=hidden name=accion value={accion}><button>{accion_txt}</button></form></section>
</main><script>
(function(){{var f=document.getElementById('modo'),s=document.getElementById('seg'),av=document.getElementById('modo-aviso');if(!f||!s)return;
var bs=s.querySelectorAll('button');function pon(l){{f.setAttribute('data-lado',l);av.hidden=true;
for(var i=0;i<bs.length;i++)bs[i].setAttribute('aria-pressed',bs[i].getAttribute('data-l')==l?'true':'false')}}
for(var i=0;i<bs.length;i++)bs[i].onclick=function(){{pon(this.getAttribute('data-l'))}};
f.setAttribute('data-js','');s.hidden=false;pon(f.getAttribute('data-lado'));
f.onsubmit=function(e){{var l=f.getAttribute('data-lado'),c=f.querySelector('.g-'+l+' input:checked');
if(!c){{e.preventDefault();av.textContent='Elige una opción de «'+(l=='con'?'Conexión':'Vitrina')+'» antes de guardar.';av.hidden=false}}}}}})();
async function f(){{try{{const r=await fetch('/api/estado');const s=await r.json();
const e=x=>String(x).replace(/[&<>"']/g,c=>({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}})[c]);
const row=(k,v)=>`<dt>${{k}}</dt><dd>${{v}}</dd>`;
document.getElementById('st').innerHTML=
row('Funcionando en',e(s.efectivo)+(s.configuracion?' · <b>modo configuración</b>':''))+
row('Internet',s.internet?'<span class=ok>sí</span>':'<span class=bad>no</span>')+
row('Sale por',e(s.salida))+
row('Cable USB',s.pc_conectada?(s.laptop?'PC '+e(s.laptop)+(s.laptop_comparte?' (comparte internet)':''):'PC conectada'):'powerbank / cargador')+
row('WiFi',e(s.wifi||'—'))+
row('LTE (SIM)',s.lte.sim?e(s.lte.estado)+' · señal '+e(s.lte.senal)+'% '+e(s.lte.tecnologia)+' '+e(s.lte.operador):'sin SIM')+
row('Túnel WireGuard',e(s.wireguard))+row('Direcciones IP','<pre>'+e(s.ips)+'</pre>');}}catch(_){{}}}}
f();setInterval(f,5000);
</script></body></html>"#,
        msg_html = if msg.is_empty() { String::new() } else { format!("<div class=msg role=status>{msg}</div>") },
        saved = if saved.is_empty() { "<li>Ninguna guardada</li>".into() } else { saved },
        accion = if cfg { "salir" } else { "entrar" },
        accion_txt = if cfg { "Salir de configuración" } else { "Entrar a configuración" },
    );
    Response::from_string(html_page).with_header(hdr("Content-Type", "text/html; charset=utf-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vitrina::{self, portafolio::Portafolio};

    fn estado(mode: Mode) -> Arc<Mutex<State>> {
        Arc::new(Mutex::new(State {
            mode,
            effective: mode,
            powerbank: None,
            host_present: true,
            config_mode: false,
            config_since: None,
            request: None,
            button_held: false,
            internet: true,
            uplink: "usb0".into(),
            laptop: None,
            usb_sharing: true,
            scan: vec![("Casa <5G>".into(), 80, "WPA2".into())],
        }))
    }

    fn cuerpo(r: Response<std::io::Cursor<Vec<u8>>>) -> String {
        let mut s = String::new();
        r.into_reader().read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn panel_switch_de_modo() {
        let d = std::env::temp_dir().join(format!("panel-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let vit: Vitrinas = Arc::new(vec![Box::new(Portafolio::with_dir(d.clone()))]);
        let h = cuerpo(page(&estado(Mode::Vitrina("portafolio")), &vit, "", None));
        assert!(h.contains("id=modo data-lado=vit"));
        assert!(h.contains("value=\"vitrina:portafolio\" checked"));
        assert!(h.contains("class=g-con") && h.contains("class=g-vit"), "ambos grupos están en el HTML (sin JS se ven los dos)");
        let h = cuerpo(page(&estado(Mode::Lte), &vit, "", None));
        assert!(h.contains("data-lado=con") && h.contains("value=\"lte\" checked"));
        // los SSID escaneados van escapados
        assert!(h.contains("Casa &lt;5G&gt;") && !h.contains("Casa <5G>"));
        // sección abierta tras un POST a la vitrina
        assert!(!h.contains("<details open><summary><span class=st><span class=h2>Portafolio"));
        let h = cuerpo(page(&estado(Mode::Lte), &vit, "ok", Some("portafolio")));
        assert!(h.contains("<details open><summary><span class=st><span class=h2>Portafolio"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Genera el HTML del panel para revisarlo en un navegador:
    /// PANEL_OUT=/ruta cargo test vista_panel -- --ignored
    #[test]
    #[ignore]
    fn vista_panel() {
        let out = std::path::PathBuf::from(std::env::var("PANEL_OUT").unwrap_or_else(|_| "/tmp/panel".into()));
        let d = out.join("datos");
        let _ = std::fs::remove_dir_all(&d);
        let p = Portafolio::with_dir(d.clone());
        let r = p.post("preparar", &[("sitio", ""), ("nombre", "Portafolio 2024 <viejo>")], &[]);
        let id = r.strip_prefix("OK ").unwrap().to_string();
        p.post("subir", &[("sitio", &id), ("ruta", "index.html")], b"<p>hola</p>");
        p.post("subir", &[("sitio", &id), ("ruta", "css/a.css")], b"body{}");
        let r = p.post("preparar", &[("sitio", ""), ("nombre", "mounstroya.xyz")], &[]);
        let id2 = r.strip_prefix("OK ").unwrap().to_string();
        p.post("subir", &[("sitio", &id2), ("ruta", "index.html")], b"<p>copia</p>");
        p.post("usar", &[("sitio", &id2)], &[]);
        p.post("url", &[("url", "https://mounstroya.xyz/"), ("titulo", "Yael")], &[]);
        if std::env::var("PANEL_DESCARGA").is_ok() {
            p.simular_descarga("otra", "Descargando archivo 4 de 9 · 3 archivos listos · 152 KB bajados");
        }
        let vit: Vitrinas = Arc::new(vec![
            Box::new(p),
            Box::new(vitrina::tarjeta::Tarjeta::new()),
            Box::new(vitrina::buzon::Buzon::new()),
            Box::new(vitrina::afluencia::Afluencia::new()),
        ]);
        let msg = if std::env::var("PANEL_DESCARGA").is_ok() { "Descarga iniciada." } else { "" };
        let h = cuerpo(page(&estado(Mode::Vitrina("portafolio")), &vit, msg, Some("portafolio")));
        std::fs::write(out.join("panel.html"), h).unwrap();
    }
}
