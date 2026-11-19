//! Módulo "Chat local" — ESQUELETO, lo implementa su agente.
use super::{GuestReq, GuestResp, Vitrina};

pub struct Chat;

impl Chat {
    pub fn new() -> Self {
        Chat
    }
}

impl Vitrina for Chat {
    fn id(&self) -> &'static str {
        "chat"
    }
    fn nombre(&self) -> &'static str {
        "Chat local"
    }
    fn descripcion(&self) -> &'static str {
        "Chat entre quienes se conecten al WiFi, sin internet"
    }
    fn ssid_por_defecto(&self) -> String {
        "Chat local".into()
    }
    fn guest(&self, _req: &GuestReq) -> GuestResp {
        GuestResp::html("<h1>Chat local</h1><p>En construcción</p>")
    }
    fn admin_html(&self) -> String {
        "<p>En construcción</p>".into()
    }
    fn admin_post(&self, _accion: &str, _form: &[(String, String)], _body: &[u8]) -> String {
        "En construcción".into()
    }
}
