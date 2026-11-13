//! The screens of a host with no terminal in them: they draw into ratatui
//! frames and take keys of their own type, which berth turns a terminal's
//! events into and its page in a browser the browser's.

pub mod berth_key;
pub mod event;
pub mod fields;
pub mod host;
pub mod machine;
pub mod markdown;
pub mod page;
pub mod plugins;
pub mod provisioners;
pub mod qr;
pub mod screen;
pub mod setup;
pub mod theme;

pub use event::{Click, Code, Event, Key, Mouse};
