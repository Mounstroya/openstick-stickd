//! Módulo "Portafolio" — ESQUELETO, lo implementa su agente.
use super::{GuestReq, GuestResp, Vitrina};

pub struct Portafolio;

impl Portafolio {
    pub fn new() -> Self {
        Portafolio
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
        "Tu pagina web para quien se conecte"
    }
    fn ssid_por_defecto(&self) -> String {
        "Portafolio".into()
    }
    fn guest(&self, _req: &GuestReq) -> GuestResp {
        GuestResp::html("<h1>Portafolio</h1><p>En construccion</p>")
    }
    fn admin_html(&self) -> String {
        "<p>En construccion</p>".into()
    }
    fn admin_post(&self, _accion: &str, _form: &[(String, String)], _body: &[u8]) -> String {
        "En construccion".into()
    }
}
