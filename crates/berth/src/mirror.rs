//! The mirror of a berth server, made while the screen is open. The server
//! may wait many seconds for a host that does not answer, and the screen
//! shows what the last mirror left until the new one is there.

use std::sync::{
    Arc,
    mpsc::{Receiver, TryRecvError, channel},
};

/// What making a mirror comes to: nothing to say, or why it failed.
pub type Made = Result<(), String>;

/// A mirror being made on a thread of its own, at most one at a time.
pub struct Mirroring {
    make: Arc<dyn Fn() -> Made + Send + Sync>,
    running: Option<Receiver<Made>>,
}

impl Mirroring {
    /// `make` is the work of one mirror.
    pub fn new(make: impl Fn() -> Made + Send + Sync + 'static) -> Mirroring {
        Mirroring {
            make: Arc::new(make),
            running: None,
        }
    }

    /// The mirror of the berth server this berth uses.
    pub fn of(paths: &berth_core::paths::Paths, server: berth_core::server::Server) -> Mirroring {
        let paths = paths.clone();
        Mirroring::new(move || {
            server
                .mirror(&paths)
                .map_err(|error| format!("the berth server at {}: {error}", server.url))
        })
    }

    /// Start a mirror, unless one is under way. Returns whether it started.
    pub fn start(&mut self) -> bool {
        if self.running.is_some() {
            return false;
        }
        let (sender, receiver) = channel();
        let make = self.make.clone();
        std::thread::spawn(move || {
            let _ = sender.send(make());
        });
        self.running = Some(receiver);
        true
    }

    pub fn running(&self) -> bool {
        self.running.is_some()
    }

    /// How the mirror ended, once, when it has.
    pub fn finished(&mut self) -> Option<Made> {
        let receiver = self.running.as_ref()?;
        let made = match receiver.try_recv() {
            Ok(made) => made,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("the mirror stopped without an answer".into()),
        };
        self.running = None;
        Some(made)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn one_mirror_runs_at_a_time_and_its_end_is_told_once() {
        let (release, wait) = channel::<()>();
        let wait = std::sync::Mutex::new(wait);
        let made = Arc::new(AtomicUsize::new(0));
        let count = made.clone();
        let mut mirroring = Mirroring::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            wait.lock()
                .unwrap()
                .recv()
                .map_err(|error| error.to_string())
        });
        assert!(mirroring.start() && mirroring.running());
        // Asking again while it runs starts nothing, and nothing has ended.
        assert!(!mirroring.start());
        assert!(mirroring.finished().is_none());
        release.send(()).unwrap();
        let ended = loop {
            if let Some(ended) = mirroring.finished() {
                break ended;
            }
            std::thread::yield_now();
        };
        assert_eq!(ended, Ok(()));
        assert!(mirroring.finished().is_none() && !mirroring.running());
        assert_eq!(made.load(Ordering::SeqCst), 1);
    }
}
