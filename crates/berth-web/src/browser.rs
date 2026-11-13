//! What the browser does for the page: its keys and mouse as the screens
//! take them, the clipboard, the token it keeps, and the canvas.

use berth_page::App;
use berth_page::device::{Answer, Device, Kept, Linked, Said, Socket};
use berth_tui::{Click, Code, Event, Key, Mouse};
use ratzilla::backend::webgl2::WebGl2BackendOptions;
use ratzilla::ratatui::{Terminal, style::Color};
use ratzilla::{CellSized, FontAtlasConfig, WebGl2Backend, WebRenderer};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{KeyboardEvent, window};

/// Where the browser keeps what the page keeps between visits.
fn item(key: Kept) -> &'static str {
    match key {
        Kept::Token => "berth.token",
        Kept::Server => "berth.server",
    }
}

/// The fonts the page draws with, the first the browser has.
const FONTS: &[&str] = &[
    "JetBrains Mono",
    "JetBrainsMono Nerd Font Mono",
    "Cascadia Mono",
    "DejaVu Sans Mono",
    "Menlo",
    "Consolas",
    "monospace",
];

pub struct Browser;

/// Let the user choose a file of this device, and hand over its name and
/// bytes once the browser has read it. Nothing happens when none is chosen.
fn choose_file(accept: &str, then: Box<dyn FnOnce(String, Vec<u8>)>) {
    let Some(element) = window()
        .and_then(|window| window.document())
        .and_then(|document| document.create_element("input").ok())
    else {
        return;
    };
    let input: web_sys::HtmlInputElement = element.unchecked_into();
    input.set_type("file");
    input.set_accept(accept);
    let chosen = input.clone();
    let changed = Closure::once_into_js(move || {
        let Some(file) = chosen.files().and_then(|files| files.get(0)) else {
            return;
        };
        let name = file.name();
        let read = Closure::once_into_js(move |buffer: JsValue| {
            then(name, js_sys::Uint8Array::new(&buffer).to_vec());
        });
        let reading: JsValue = file.array_buffer().into();
        if let Ok(then) = js_sys::Reflect::get(&reading, &"then".into()) {
            let _ = then
                .unchecked_into::<js_sys::Function>()
                .call1(&reading, &read);
        }
    });
    input.set_onchange(Some(changed.unchecked_ref()));
    input.click();
}

fn storage() -> Option<web_sys::Storage> {
    window()?.local_storage().ok()?
}

/// Whether `berth serve` gave this page itself, so that it is the server.
/// A copy hosted elsewhere, as on the documentation site, asks which server.
fn embedded() -> bool {
    window()
        .and_then(|window| window.document())
        .and_then(|document| {
            document
                .query_selector("meta[name=berth-server]")
                .ok()
                .flatten()
        })
        .and_then(|meta| meta.get_attribute("content"))
        .is_some_and(|content| content == "self")
}

/// Undo the percent-encoding of a value in an address.
fn decoded(value: &str) -> String {
    js_sys::decode_uri_component(value)
        .ok()
        .and_then(|text| text.as_string())
        .unwrap_or_else(|| value.to_owned())
}

/// What the link to this page carried. A token or a setup key is taken out
/// of the address so that it is not left in the bar or the history.
fn linked() -> Linked {
    let Some(window) = window() else {
        return Linked::default();
    };
    let location = window.location();
    let (search, hash) = (
        location.search().unwrap_or_default(),
        location.hash().unwrap_or_default(),
    );
    let parts = |text: &str| -> Vec<String> {
        text.trim_start_matches(['?', '#'])
            .split('&')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let (query, fragment) = (parts(&search), parts(&hash));
    let value = |name: &str| {
        query.iter().chain(&fragment).find_map(|part| {
            let (key, value) = part.split_once('=')?;
            (key == name && !value.is_empty()).then(|| decoded(value))
        })
    };
    let linked = Linked {
        server: value("server"),
        token: value("token"),
        setup_key: value("setup-key"),
    };
    if linked.token.is_some() || linked.setup_key.is_some() {
        let secret = |part: &str| part.starts_with("token=") || part.starts_with("setup-key=");
        let keep = |parts: &[String], mark: &str| {
            let kept: Vec<&str> = parts
                .iter()
                .filter(|part| !secret(part))
                .map(String::as_str)
                .collect();
            match kept.is_empty() {
                true => String::new(),
                false => format!("{mark}{}", kept.join("&")),
            }
        };
        let bare = format!(
            "{}{}{}",
            location.pathname().unwrap_or_default(),
            keep(&query, "?"),
            keep(&fragment, "#")
        );
        if let Ok(history) = window.history() {
            let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&bare));
        }
    }
    linked
}

/// Call `then` on the outcome of a promise: what it gave, or why it failed.
fn settle(promise: &JsValue, then: impl FnOnce(Result<JsValue, String>) + 'static) {
    let then = Rc::new(RefCell::new(Some(then)));
    let failed = then.clone();
    let given = Closure::once_into_js(move |value: JsValue| {
        if let Some(then) = then.borrow_mut().take() {
            then(Ok(value));
        }
    });
    let refused = Closure::once_into_js(move |error: JsValue| {
        if let Some(then) = failed.borrow_mut().take() {
            let said = js_sys::Reflect::get(&error, &"message".into())
                .ok()
                .and_then(|message| message.as_string())
                .unwrap_or_else(|| "the browser did not send the request".into());
            then(Err(said));
        }
    });
    if let Ok(method) = js_sys::Reflect::get(promise, &"then".into()) {
        let _ = method
            .unchecked_into::<js_sys::Function>()
            .call2(promise, &given, &refused);
    }
}

/// One plain HTTP request with a bearer, and then its status and body. For
/// what the portal cannot carry because the caller has no token yet.
fn request(method: &str, url: &str, bearer: &str, then: Answer) {
    let Some(window) = window() else {
        return then(Err("there is no window".into()));
    };
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    if let Ok(headers) = web_sys::Headers::new() {
        let _ = headers.set("Authorization", &format!("Bearer {bearer}"));
        init.set_headers(&headers);
    }
    let sent: JsValue = window.fetch_with_str_and_init(url, &init).into();
    settle(&sent, move |answer| {
        let response: web_sys::Response = match answer {
            Ok(response) => response.unchecked_into(),
            Err(error) => return then(Err(error)),
        };
        let status = response.status();
        match response.text() {
            Ok(text) => settle(&text.into(), move |body| {
                then(body.map(|body| (status, body.as_string().unwrap_or_default())))
            }),
            Err(_) => then(Ok((status, String::new()))),
        }
    });
}

/// The address this page came from: `http://host:port`.
fn origin() -> String {
    window()
        .and_then(|window| window.location().origin().ok())
        .unwrap_or_default()
}

/// Where this page is: its origin and path, without what follows them.
fn page() -> String {
    window()
        .map(|window| {
            let location = window.location();
            format!(
                "{}{}",
                location.origin().unwrap_or_default(),
                location.pathname().unwrap_or_default()
            )
        })
        .unwrap_or_default()
}

impl Device for Browser {
    fn copy(&self, text: &str) -> bool {
        match window() {
            Some(window) => {
                let _ = window.navigator().clipboard().write_text(text);
                true
            }
            None => false,
        }
    }

    /// Pasting is the browser's: its `paste` event carries the text.
    fn paste(&self) -> Option<String> {
        None
    }

    fn open(&self, link: &str) -> Result<(), String> {
        let window = window().ok_or("there is no window")?;
        match window.open_with_url_and_target(link, "_blank") {
            Ok(Some(_)) => Ok(()),
            _ => Err("the browser kept the tab from opening".into()),
        }
    }

    fn choose_file(
        &self,
        accept: &str,
        then: Box<dyn FnOnce(String, Vec<u8>)>,
    ) -> Result<(), String> {
        choose_file(accept, then);
        Ok(())
    }

    fn stored(&self, key: Kept) -> Option<String> {
        storage()?.get_item(item(key)).ok()?
    }

    fn store(&self, key: Kept, value: &str) {
        if let Some(storage) = storage() {
            let _ = match value.is_empty() {
                true => storage.remove_item(item(key)),
                false => storage.set_item(item(key), value),
            };
        }
    }

    fn embedded(&self) -> bool {
        embedded()
    }

    fn linked(&self) -> Linked {
        linked()
    }

    fn origin(&self) -> String {
        origin()
    }

    fn page(&self) -> String {
        page()
    }

    fn request(&self, method: &str, url: &str, bearer: &str, then: Answer) {
        request(method, url, bearer, then)
    }

    fn connect(
        &self,
        url: &str,
        token: &str,
        said: Box<dyn Fn(Said)>,
    ) -> Result<Box<dyn Socket>, String> {
        let socket = crate::socket::open(url, token, said)?;
        Ok(Box::new(socket))
    }
}

/// The key a `keydown` was; `None` for keys the screens have no use for.
fn key_of(event: &KeyboardEvent) -> Option<Key> {
    let mut name = event.key();
    // Option on a Mac types a character of its own, such as ® for R, where the
    // screens want the letter of the key. AltGr, which adds Ctrl, types text.
    if event.alt_key()
        && !event.ctrl_key()
        && let Some(letter) = event.code().strip_prefix("Key")
        && letter.len() == 1
        && !(name.len() == 1 && name.chars().all(|ch| ch.is_ascii_alphabetic()))
    {
        name = letter.to_lowercase();
    }
    let mut chars = name.chars();
    let code = match (chars.next(), chars.next()) {
        (Some(ch), None) => Code::Char(ch),
        _ => match name.as_str() {
            "Enter" => Code::Enter,
            "Escape" => Code::Esc,
            "Backspace" => Code::Backspace,
            "Delete" => Code::Delete,
            "Insert" => Code::Insert,
            "Tab" if event.shift_key() => Code::BackTab,
            "Tab" => Code::Tab,
            "ArrowUp" => Code::Up,
            "ArrowDown" => Code::Down,
            "ArrowLeft" => Code::Left,
            "ArrowRight" => Code::Right,
            "Home" => Code::Home,
            "End" => Code::End,
            "PageUp" => Code::PageUp,
            "PageDown" => Code::PageDown,
            other => Code::F(other.strip_prefix('F')?.parse().ok()?),
        },
    };
    Some(Key {
        code,
        ctrl: event.ctrl_key(),
        alt: event.alt_key(),
        shift: event.shift_key(),
    })
}

/// Keys whose answer is the browser's own: its developer tools, a reload,
/// full screen, and anything with the Meta key.
fn leaves_to_browser(event: &KeyboardEvent) -> bool {
    let key = event.key().to_lowercase();
    event.meta_key()
        || matches!(key.as_str(), "f5" | "f11" | "f12")
        || (event.ctrl_key() && event.shift_key() && matches!(key.as_str(), "i" | "j" | "c" | "r"))
        || (event.ctrl_key() && matches!(key.as_str(), "r" | "+" | "-" | "0"))
}

fn canvas() -> Option<web_sys::Element> {
    window()?
        .document()?
        .query_selector("#terminal canvas")
        .ok()?
}

/// Size and place the canvas inside `#terminal` for cells of `cell` CSS
/// pixels: a whole number of cells, centred. ratzilla follows on its next frame.
fn fit(cell: (f32, f32)) {
    let Some(container) = window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("terminal"))
    else {
        return;
    };
    let Some(canvas) = canvas() else {
        return;
    };
    let (width, height) = (
        container.client_width() as f32,
        container.client_height() as f32,
    );
    let whole = |space: f32, cell: f32| match cell > 0.0 {
        true => (space / cell).floor().max(1.0) * cell,
        false => space,
    };
    let (w, h) = (whole(width, cell.0), whole(height, cell.1));
    let _ = canvas.set_attribute(
        "style",
        &format!(
            "position:absolute;display:block;left:{}px;top:{}px;width:{w}px;height:{h}px",
            ((width - w) / 2.0).max(0.0).floor(),
            ((height - h) / 2.0).max(0.0).floor()
        ),
    );
}

fn listen<E>(target: &web_sys::EventTarget, name: &str, handler: impl FnMut(E) + 'static)
where
    E: wasm_bindgen::convert::FromWasmAbi + 'static,
{
    let closure = Closure::<dyn FnMut(E)>::new(handler);
    let _ = target.add_event_listener_with_callback(name, closure.as_ref().unchecked_ref());
    // The page keeps its listeners for as long as it is open.
    closure.forget();
}

/// The cell of the canvas a mouse event is over.
fn cell_of(event: &web_sys::MouseEvent, cell: (f32, f32)) -> Option<(u16, u16)> {
    let area = canvas()?.get_bounding_client_rect();
    let (x, y) = (
        event.client_x() as f64 - area.left(),
        event.client_y() as f64 - area.top(),
    );
    let inside = x >= 0.0 && y >= 0.0 && x < area.width() && y < area.height();
    (inside && cell.0 > 0.0 && cell.1 > 0.0)
        .then(|| ((x / cell.0 as f64) as u16, (y / cell.1 as f64) as u16))
}

pub fn start() {
    console_error_panic_hook::set_once();
    berth_page::device::install(Browser);
    let Some(window) = window() else {
        return;
    };
    let app = Rc::new(RefCell::new(App::new()));

    let keys = app.clone();
    listen(&window, "keydown", move |event: KeyboardEvent| {
        // Pasting is the browser's: a prevented key would not give its text.
        if leaves_to_browser(&event) || (event.ctrl_key() && event.key().eq_ignore_ascii_case("v"))
        {
            return;
        }
        let Some(key) = key_of(&event) else {
            return;
        };
        event.prevent_default();
        keys.borrow_mut().event(Event::Key(key));
    });
    let pasted = app.clone();
    listen(&window, "paste", move |event: web_sys::ClipboardEvent| {
        if let Some(text) = event
            .clipboard_data()
            .and_then(|data| data.get_data("text").ok())
        {
            event.prevent_default();
            pasted.borrow_mut().event(Event::Paste(text));
        }
    });

    let container = window
        .document()
        .and_then(|document| document.get_element_by_id("terminal"));
    let (width, height) = container.as_ref().map_or((960, 600), |container| {
        (
            container.client_width().max(320) as u32,
            container.client_height().max(200) as u32,
        )
    });
    // The browser draws the glyphs, so box drawing and arrows come out of
    // whatever monospace font it has.
    let backend = WebGl2Backend::new_with_options(
        WebGl2BackendOptions::new()
            .grid_id("terminal")
            .size((width, height))
            .disable_auto_css_resize()
            .canvas_padding_color(Color::Rgb(13, 17, 28))
            .font_atlas_config(FontAtlasConfig::dynamic(FONTS, 15.0)),
    )
    .expect("the page has a #terminal element and WebGL2");
    let cell = backend.cell_size_css_px();
    fit(cell);

    if let Some(container) = &container {
        for name in ["mousedown", "wheel"] {
            let app = app.clone();
            listen(container, name, move |event: web_sys::MouseEvent| {
                let Some((column, row)) = cell_of(&event, cell) else {
                    return;
                };
                let click = match event.dyn_ref::<web_sys::WheelEvent>() {
                    Some(wheel) if wheel.delta_y() < 0.0 => Click::ScrollUp,
                    Some(wheel) if wheel.delta_y() > 0.0 => Click::ScrollDown,
                    Some(_) => return,
                    None => match event.button() {
                        0 => Click::Left,
                        2 => Click::Right,
                        _ => return,
                    },
                };
                // The page's own wheel and selection are not wanted over the screen.
                event.prevent_default();
                app.borrow_mut()
                    .event(Event::Mouse(Mouse { click, column, row }));
            });
        }
        // A right click copies a value here; it is not the browser's menu.
        listen(container, "contextmenu", |event: web_sys::Event| {
            event.prevent_default()
        });
        // The container's size is watched itself: a zoom or a font that loads
        // late changes it without a `resize`.
        let resized = Closure::<dyn FnMut()>::new(move || fit(cell)).into_js_value();
        if let Ok(observer) = web_sys::ResizeObserver::new(resized.unchecked_ref()) {
            observer.observe(container);
        }
    }

    let terminal = Terminal::new(backend).expect("the terminal starts");
    terminal.draw_web(move |frame| {
        let mut app = app.borrow_mut();
        app.tick();
        app.draw(frame);
    });
}
