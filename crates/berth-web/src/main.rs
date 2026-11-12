//! berth in a browser: the hosts of the `berth serve` that served this page,
//! through its portal. The page keeps nothing but the token; every host is
//! the server's.

#[cfg(target_arch = "wasm32")]
mod admin;
#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
mod home;
#[cfg(target_arch = "wasm32")]
mod portal;
#[cfg(target_arch = "wasm32")]
mod remote;
#[cfg(target_arch = "wasm32")]
mod setup;

#[cfg(target_arch = "wasm32")]
fn main() {
    browser::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("berth-web is the page berth serve shows: building berth builds it for wasm32");
}
