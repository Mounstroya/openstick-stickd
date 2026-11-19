//! HTML/CSS/JS de la página del invitado. La página funciona sin JS (lista pintada en el
//! servidor + formulario normal); con JS se vuelve chat en vivo con polling corto.
//! El JS es ES5 (sin flechas ni fetch) para los WebView viejos de Android y el CNA de iOS, y
//! pinta TODO dato con `textContent`.

use super::{ANFITRION, MAX_APODO, MAX_TEXTO, Msg, hora, tono, urlenc};
use crate::util::html;

pub(super) struct Vista<'a> {
    pub msgs: &'a [&'a Msg],
    pub apodo: &'a str,
    pub aviso: &'a str,
    pub conectados: usize,
    pub bienvenida: &'a str,
    pub solo_lectura: bool,
    pub rev: u64,
    pub seq: u64,
}

fn item(m: &Msg) -> String {
    let badge = if m.host && m.apodo != ANFITRION { format!("<span class=badge>{ANFITRION}</span>") } else { String::new() };
    format!(
        "<li class=\"m{}\" data-s={}><div class=c><b class=n style=\"--h:{}\">{}</b>{}<time data-ts={}>{}</time></div><p>{}</p></li>",
        if m.host { " host" } else { "" },
        m.seq,
        tono(&m.apodo),
        html(&m.apodo),
        badge,
        m.ts,
        hora(m.ts),
        html(&m.texto)
    )
}

fn conectados(n: usize) -> String {
    if n == 1 { "1 conectado".into() } else { format!("{n} conectados") }
}

const CSS: &str = r#"
:root{--bg:#f4f5f8;--card:#fff;--tx:#14161c;--mut:#5d6372;--ac:#1f5fd6;--acb:#1f5fd6;--bd:#00000022;--yo:#e3ecff;--l:31%;--bad:#c62828}
@media (prefers-color-scheme:dark){:root{--bg:#0f1115;--card:#1a1d25;--tx:#e8eaf0;--mut:#9aa0ae;--ac:#6aa0ff;--acb:#2f6fe4;--bd:#ffffff22;--yo:#1d2a45;--l:72%;--bad:#ff6b6b}}
*{box-sizing:border-box}html,body{margin:0}
body{font:16px/1.4 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;background:var(--bg);color:var(--tx);min-height:100vh;display:flex;flex-direction:column}
header{position:sticky;top:0;z-index:1;background:var(--bg);border-bottom:1px solid var(--bd)}
header div,main,.caja>div,.caja>label{max-width:640px;margin-left:auto;margin-right:auto}
header div{display:flex;justify-content:space-between;align-items:baseline;gap:8px;padding:10px 16px}
h1{font-size:18px;margin:0}#con{color:var(--mut);font-size:14px;white-space:nowrap}
main{flex:1;width:100%;padding:4px 16px 8px}
.fijo{background:var(--card);border-left:4px solid var(--ac);border-radius:8px;padding:10px 12px;margin:10px 0;white-space:pre-wrap;overflow-wrap:anywhere}
ol{list-style:none;margin:0;padding:0}
.m{background:var(--card);border-radius:12px;padding:7px 11px;margin:6px 0;max-width:90%;width:fit-content;min-width:40%}
.m.yo{background:var(--yo);margin-left:auto}.m.host{border:1px solid var(--ac)}
.c{display:flex;gap:6px;align-items:baseline;font-size:14px}
.n{color:hsl(var(--h,210),65%,var(--l));font-weight:700;overflow-wrap:anywhere}.host .n{color:var(--ac)}
.badge{font-size:11px;font-weight:700;background:var(--acb);color:#fff;border-radius:6px;padding:1px 6px}
time{color:var(--mut);font-size:12px;margin-left:auto;padding-left:8px;white-space:nowrap}
.m p{margin:2px 0 0;white-space:pre-wrap;overflow-wrap:anywhere}
.caja{position:sticky;bottom:0;background:var(--bg);border-top:1px solid var(--bd);padding:10px 16px;padding-bottom:calc(10px + env(safe-area-inset-bottom))}
.fila{display:flex;gap:8px}
input[type=text]{flex:1;min-width:0;width:100%;padding:10px 12px;border-radius:10px;border:1px solid var(--bd);background:var(--card);color:var(--tx);font:inherit;font-size:16px}
input[type=text]:focus{outline:2px solid var(--ac);outline-offset:0}
button{background:var(--acb);color:#fff;border:0;border-radius:10px;padding:10px 16px;font:inherit;font-weight:600}
button:disabled{opacity:.6}button.link{background:none;color:var(--ac);padding:0 4px;font-weight:400;text-decoration:underline}
label{display:block;font-size:14px;color:var(--mut);margin-bottom:8px}label input{margin-top:4px}
#quien{font-size:14px;color:var(--mut);margin-bottom:6px}
.aviso{background:var(--bad);color:#fff;border-radius:8px;padding:8px 12px;margin:0 auto 8px;max-width:640px}
.mut,.ro{color:var(--mut)}.ro{text-align:center;font-size:14px}
#elegir h2{font-size:17px;margin:0 0 4px}#elegir p.mut{margin:0 0 10px;font-size:14px}
[hidden]{display:none!important}
"#;

const JS: &str = r#"
(function(){
var d=document,B=d.body;function $(i){return d.getElementById(i)}
var lista=$('lista'),f=$('f'),tx=$('texto'),btn=$('b'),avisoEl=$('aviso');
var rev=B.getAttribute('data-rev'),seq=+B.getAttribute('data-seq')||0,ro=B.getAttribute('data-ro')==='1';
var yo='',eligiendo=false,espera=2000,timer=0,cargando=false,enviando=false,bajarYa=false;
function leer(){var a='';try{a=localStorage.getItem('apodo')||''}catch(e){}
 if(!a){var m=d.cookie.match(/(?:^|; )apodo=([^;]*)/);if(m){try{a=decodeURIComponent(m[1])}catch(e){}}}return a}
function recordar(a){try{localStorage.setItem('apodo',a)}catch(e){}
 d.cookie='apodo='+encodeURIComponent(a)+'; max-age=31536000; path=/; SameSite=Lax'}
function dos(n){return(n<10?'0':'')+n}
function hora(ts){var t=new Date(ts*1000),h=new Date(),s=dos(t.getHours())+':'+dos(t.getMinutes());
 return t.toDateString()===h.toDateString()?s:dos(t.getDate())+'/'+dos(t.getMonth()+1)+' '+s}
function el(tag,cls,txt){var e=d.createElement(tag);if(cls)e.className=cls;if(txt!=null)e.textContent=txt;return e}
function item(m){var li=el('li','m'+(m[5]?' host':(m[2]===yo?' yo':'')));li.setAttribute('data-s',m[0]);
 var c=el('div','c'),n=el('b','n',m[2]);n.style.setProperty('--h',String(m[4]));c.appendChild(n);
 if(m[5]&&m[2]!=='Anfitrión')c.appendChild(el('span','badge','Anfitrión'));var t=el('time',null,hora(m[1]));t.setAttribute('data-ts',m[1]);c.appendChild(t);
 li.appendChild(c);li.appendChild(el('p',null,m[3]));return li}
function marcarYo(){var l=lista.children;for(var i=0;i<l.length;i++){var li=l[i];if(/host/.test(li.className))continue;
 var n=li.querySelector('.n');li.className='m'+(n&&n.textContent===yo?' yo':'')}}
function abajo(){return window.innerHeight+(window.pageYOffset||d.documentElement.scrollTop)>=d.documentElement.scrollHeight-120}
function bajar(){window.scrollTo(0,d.documentElement.scrollHeight)}
function aviso(t){avisoEl.textContent=t||'';avisoEl.hidden=!t}
function ui(){$('ro').hidden=!ro;f.hidden=ro||eligiendo||!yo;$('elegir').hidden=ro||!eligiendo;$('yo').textContent=yo}
function pintar(r){var fondo=abajo()||bajarYa;bajarYa=false;
 if(r.t){lista.textContent='';var fj=$('fijo');fj.textContent=r.b||'';fj.hidden=!r.b;seq=0}
 rev=String(r.r);var fr=d.createDocumentFragment();
 for(var i=0;i<r.m.length;i++){if(r.m[i][0]>seq)fr.appendChild(item(r.m[i]))}
 lista.appendChild(fr);seq=r.s;while(lista.children.length>300)lista.removeChild(lista.firstChild);
 $('con').textContent=r.n+(r.n==1?' conectado':' conectados');$('vacio').hidden=lista.children.length>0;
 var nro=!!r.ro;if(nro!==ro){ro=nro;ui()}if(fondo)bajar()}
function sig(){clearTimeout(timer);timer=setTimeout(poll,d.hidden?Math.max(espera,15000):espera)}
function red(ok){espera=ok?2000:Math.min(espera*2,30000);$('red').hidden=ok}
function poll(){if(cargando)return;cargando=true;clearTimeout(timer);var x=new XMLHttpRequest();
 x.open('GET','/mensajes?desde='+seq+'&r='+encodeURIComponent(rev)+'&_='+(+new Date()));x.timeout=8000;
 x.onload=function(){cargando=false;var ok=false;if(x.status===200){try{pintar(JSON.parse(x.responseText));ok=true}catch(e){}}red(ok);sig()};
 x.onerror=x.ontimeout=function(){cargando=false;red(false);sig()};x.send()}
function elegir(msg){eligiendo=true;var n=$('nuevo');n.value=yo||$('apodo').value||'';$('eaviso').textContent=msg||'';$('eaviso').hidden=!msg;ui();try{n.focus()}catch(e){}}
$('fe').onsubmit=function(e){e.preventDefault();var a=$('nuevo').value,b=$('eb');if(!a.replace(/\s+/g,''))return;b.disabled=true;
 var x=new XMLHttpRequest();x.open('GET','/apodo?a='+encodeURIComponent(a));x.timeout=8000;
 x.onload=function(){b.disabled=false;var r={};try{r=JSON.parse(x.responseText)}catch(e){}
  if(r.ok){yo=r.a;recordar(yo);eligiendo=false;$('apodo').value=yo;marcarYo();ui();bajar();try{tx.focus()}catch(e){}}
  else{$('eaviso').textContent=r.msg||'No se pudo comprobar el apodo.';$('eaviso').hidden=false}};
 x.onerror=x.ontimeout=function(){b.disabled=false;$('eaviso').textContent='Sin conexión con el stick.';$('eaviso').hidden=false};x.send()};
$('cambiar').onclick=function(){elegir('')};
f.onsubmit=function(e){e.preventDefault();var t=tx.value.replace(/^\s+|\s+$/g,'');if(!t||enviando)return;if(!yo){elegir('');return}
 enviando=true;btn.disabled=true;var x=new XMLHttpRequest();x.open('POST','/enviar?js=1');
 x.setRequestHeader('Content-Type','application/x-www-form-urlencoded');x.timeout=10000;
 x.onload=function(){enviando=false;btn.disabled=false;var r={};try{r=JSON.parse(x.responseText)}catch(e){}
  if(r.ok){tx.value='';aviso('');bajarYa=true;poll()}else{aviso(r.msg||'No se pudo enviar.');if(r.e==='apodo'||r.e==='reservado'||r.e==='apodolargo')elegir(r.msg);if(r.e==='lectura'){ro=true;ui()}}};
 x.onerror=x.ontimeout=function(){enviando=false;btn.disabled=false;aviso('Sin conexión con el stick. ¿Sigues conectado a su WiFi?')};
 x.send('apodo='+encodeURIComponent(yo)+'&texto='+encodeURIComponent(t));try{tx.focus()}catch(e){}};
d.addEventListener('visibilitychange',function(){if(!d.hidden)poll()});
// arranque: modo JS
var ts=d.getElementsByTagName('time');for(var i=0;i<ts.length;i++){var v=+ts[i].getAttribute('data-ts');if(v)ts[i].textContent=hora(v)}
$('lapodo').hidden=true;$('apodo').removeAttribute('required');$('actualizar').hidden=true;$('quien').hidden=false;
yo=leer()||$('apodo').value||'';if(yo)recordar(yo);
marcarYo();if(!yo&&!ro)elegir('');else ui();bajar();sig();
})();
"#;

pub(super) fn pagina(v: &Vista) -> String {
    let items: String = v.msgs.iter().map(|m| item(m)).collect();
    let oculto = |b: bool| if b { " hidden" } else { "" };
    let a_enc = urlenc(v.apodo);
    let mut s = String::with_capacity(8192 + items.len());
    s.push_str(
        "<!doctype html><html lang=es><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1,viewport-fit=cover\">\
         <meta name=color-scheme content=\"light dark\"><title>Chat local</title><style>",
    );
    s.push_str(CSS);
    s.push_str("</style></head>");
    s.push_str(&format!(
        "<body data-rev={} data-seq={} data-ro={}>\
         <header><div><h1>💬 Chat local</h1><span id=con>{}</span></div></header><main>\
         <div id=fijo class=fijo{}>{}</div>\
         <ol id=lista>{}</ol>\
         <p id=vacio class=mut{}>Todavía no hay mensajes. ¡Saluda!</p>\
         <p id=red class=mut hidden>Reconectando con el stick…</p>\
         <p id=actualizar class=mut><a href=\"/?a={}#fin\">Actualizar</a></p><span id=fin></span></main>\
         <form id=f class=caja method=post action=/enviar{}>\
         <p id=aviso class=aviso{}>{}</p><div id=quien hidden>Escribes como <b id=yo></b> <button type=button id=cambiar class=link>cambiar</button></div>\
         <label id=lapodo>Tu apodo<input type=text name=apodo id=apodo maxlength={MAX_APODO} value=\"{}\" autocomplete=nickname required></label>\
         <div class=fila><input type=text name=texto id=texto maxlength={MAX_TEXTO} placeholder=\"Escribe un mensaje\" autocomplete=off enterkeyhint=send required>\
         <button id=b>Enviar</button></div></form>\
         <section id=elegir class=caja hidden><div><h2>¿Cómo te llamas?</h2>\
         <p class=mut>Elige un apodo para el chat. Lo verán quienes estén conectados a este WiFi.</p>\
         <form id=fe><div class=fila><input type=text id=nuevo maxlength={MAX_APODO} autocomplete=nickname placeholder=\"Tu apodo\" enterkeyhint=go>\
         <button id=eb>Entrar</button></div></form><p id=eaviso class=aviso hidden></p></div></section>\
         <p id=ro class=\"caja ro\"{}>Solo lectura: por ahora solo escribe el anfitrión.</p><script>",
        v.rev,
        v.seq,
        v.solo_lectura as u8,
        conectados(v.conectados),
        oculto(v.bienvenida.is_empty()),
        html(v.bienvenida),
        items,
        oculto(!v.msgs.is_empty()),
        a_enc,
        oculto(v.solo_lectura),
        oculto(v.aviso.is_empty()),
        html(v.aviso),
        html(v.apodo),
        oculto(!v.solo_lectura),
    ));
    s.push_str(JS);
    s.push_str("</script></body></html>");
    s
}
