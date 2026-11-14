//! berth in a browser: the hosts of the `berth serve` that served this page,
//! through its portal. The screens are berth-page's; this program gives them
//! the browser's canvas, keys, clipboard, storage and WebSocket. The page
//! keeps nothing but the token; every host is the server's.

#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
mod socket;

#[cfg(target_arch = "wasm32")]
fn main() {
    browser::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("berth-web is the page berth serve shows: building berth builds it for wasm32");
}
