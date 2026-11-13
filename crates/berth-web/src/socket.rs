//! The browser's WebSocket under the page's portal.

use berth_page::device::{Said, Socket};
use std::rc::Rc;
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{BinaryType, MessageEvent, WebSocket};

pub struct BrowserSocket(WebSocket);

impl Drop for BrowserSocket {
    fn drop(&mut self) {
        for name in ["onopen", "onmessage", "onclose"] {
            let _ = js_sys::Reflect::set(&self.0, &name.into(), &JsValue::NULL);
        }
        let _ = self.0.close();
    }
}

impl Socket for BrowserSocket {
    fn send_text(&self, text: &str) -> bool {
        self.0.send_with_str(text).is_ok()
    }

    fn send_bytes(&self, bytes: &[u8]) -> bool {
        self.0.send_with_u8_array(bytes).is_ok()
    }
}

/// Open a WebSocket to `url`, showing the token the way a browser can: as
/// its protocol.
pub fn open(url: &str, token: &str, said: Box<dyn Fn(Said)>) -> Result<BrowserSocket, String> {
    let socket = match token.is_empty() {
        true => WebSocket::new(url),
        false => WebSocket::new_with_str(url, token),
    }
    .map_err(|_| match url.starts_with("ws://") {
        // A browser does not let a page over https open plain ws.
        true => {
            "The browser did not open it: from a page over https, use an https address.".to_owned()
        }
        false => "The browser did not open that address.".to_owned(),
    })?;
    socket.set_binary_type(BinaryType::Arraybuffer);
    let said: Rc<dyn Fn(Said)> = Rc::from(said);
    let listen = |name: &str, handler: Box<dyn Fn(JsValue) -> Option<Said>>| {
        let said = said.clone();
        let closure = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
            if let Some(what) = handler(event) {
                said(what);
            }
        });
        let _ = js_sys::Reflect::set(&socket, &name.into(), closure.as_ref());
        // The socket keeps it for as long as the page has it.
        closure.forget();
    };
    listen("onopen", Box::new(|_| Some(Said::Open)));
    listen("onclose", Box::new(|_| Some(Said::Closed)));
    listen(
        "onmessage",
        Box::new(|event| {
            let data = event.dyn_into::<MessageEvent>().ok()?.data();
            if let Some(text) = data.as_string() {
                Some(Said::Text(text))
            } else if data.is_instance_of::<js_sys::ArrayBuffer>() {
                Some(Said::Bytes(js_sys::Uint8Array::new(&data).to_vec()))
            } else {
                None
            }
        }),
    );
    Ok(BrowserSocket(socket))
}
