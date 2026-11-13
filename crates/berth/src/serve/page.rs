//! berth in a browser: the page built from `crates/berth-web/`, carried in the program.
//! It is served to anyone who asks, since it holds nothing: what it shows it
//! gets through the portal, with the token its user gives it.

use axum::{
    Router,
    http::{HeaderValue, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};

const BUILD: &str = env!("BERTH_WEB_BUILD");
const PAGE: &str = include_str!("../../../berth-web/index.html");
const SCRIPT: &str = include_str!(concat!(env!("OUT_DIR"), "/web/berth-web.js"));
const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/berth-web_bg.wasm"));

/// Script and wasm are asked for by the build they are of, so they can be kept.
const KEPT: &str = "public, max-age=31536000, immutable";

fn file(kind: &'static str, body: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(kind)),
            (header::CACHE_CONTROL, HeaderValue::from_static(KEPT)),
        ],
        body,
    )
        .into_response()
}

async fn page() -> Response {
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))],
        // `self`: this server is the one the page is of.
        Html(PAGE.replace("@BUILD@", BUILD).replace("@SERVER@", "self")),
    )
        .into_response()
}

pub fn router() -> Router {
    Router::new()
        .route("/", get(page))
        .route(
            "/berth-web.js",
            get(|| async { file("text/javascript; charset=utf-8", SCRIPT.as_bytes()) }),
        )
        .route(
            "/berth-web_bg.wasm",
            get(|| async { file("application/wasm", WASM) }),
        )
}
