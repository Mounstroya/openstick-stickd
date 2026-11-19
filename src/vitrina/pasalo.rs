//! Módulo "Chat local" — ESQUELETO, lo implementa su agente.
use super::{GuestReq, GuestResp, Vitrina};

pub struct Pasalo;

impl Pasalo {
    pub fn new() -> Self {
        Pasalo
    }
}

impl Vitrina for Pasalo {
    fn id(&self) -> &'static str {
        "pasalo"
    }
    fn nombre(&self) -> &'static str {
        "Pásalo"
    }
    fn descripcion(&self) -> &'static str {
        "Pasa archivos de cualquier tamaño entre celulares, directo y cifrado"
    }
    fn ssid_por_defecto(&self) -> String {
        "Pasalo".into()
    }
    fn guest(&self, _req: &GuestReq) -> GuestResp {
        GuestResp::html("<h1>Pásalo</h1><p>En construcción</p>")
    }
    fn admin_html(&self) -> String {
        "<p>En construcción</p>".into()
    }
    fn admin_post(&self, _accion: &str, _form: &[(String, String)], _body: &[u8]) -> String {
        "En construcción".into()
    }
}
