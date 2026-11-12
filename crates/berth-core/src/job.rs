//! Commands that run inside the UI, with their output and progress on screen.

use crate::Result;
use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{Receiver, Sender, channel},
    thread::JoinHandle,
};

pub struct Stage {
    pub title: String,
    pub command: Command,
    /// File being copied, when the command reports progress in bytes (dd).
    pub source: Option<PathBuf>,
    /// Text for the command's standard input, such as a sudo password.
    pub input: Option<String>,
}

pub struct Job {
    pub title: String,
    pub lines: Vec<String>,
    /// Progress of the current stage, 0 to 1, once the command has reported any.
    pub progress: Option<f64>,
    /// `None` while running.
    pub outcome: Option<std::result::Result<(), String>>,
    /// Shown after success.
    pub next: String,
    stages: VecDeque<Stage>,
    child: Option<Child>,
    readers: Vec<JoinHandle<()>>,
    sender: Sender<String>,
    receiver: Receiver<String>,
    total: u64,
    /// Private files the commands read; removed when the job is dropped.
    _files: Vec<tempfile::NamedTempFile>,
}

/// Progress from an output line, as a fraction: xorriso's `Writing: … 46.4%`
/// or dd's `123456 bytes (…) copied`.
pub fn progress(line: &str, total: u64) -> Option<f64> {
    if line.starts_with("xorriso : UPDATE : Writing:") {
        let percent = line.split_whitespace().find(|word| word.ends_with('%'))?;
        return Some(percent.trim_end_matches('%').parse::<f64>().ok()? / 100.0);
    }
    // curl --progress-bar: a row of # and the percentage.
    if let Some(bar) = line.trim().strip_suffix('%')
        && let Some(percent) = bar.split_whitespace().last()
        && bar
            .chars()
            .all(|ch| "# .".contains(ch) || ch.is_ascii_digit())
    {
        return Some(percent.parse::<f64>().ok()? / 100.0);
    }
    let (bytes, rest) = line.split_once(' ')?;
    if rest.starts_with("bytes") && rest.contains("copied") && total > 0 {
        return Some((bytes.parse::<u64>().ok()? as f64 / total as f64).min(1.0));
    }
    None
}

/// Forward output as lines. A carriage return ends a line too, which is how
/// progress meters redraw themselves.
fn forward(mut stream: impl Read + Send + 'static, sender: Sender<String>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        let mut line = vec![];
        while let Ok(count) = stream.read(&mut buffer)
            && count > 0
        {
            for &byte in &buffer[..count] {
                if byte == b'\n' || byte == b'\r' {
                    if !line.is_empty() {
                        let _ = sender.send(String::from_utf8_lossy(&line).into_owned());
                        line.clear();
                    }
                } else {
                    line.push(byte);
                }
            }
        }
        if !line.is_empty() {
            let _ = sender.send(String::from_utf8_lossy(&line).into_owned());
        }
    })
}

impl Job {
    pub fn start(
        stages: Vec<Stage>,
        next: &str,
        files: Vec<tempfile::NamedTempFile>,
    ) -> Result<Job> {
        let (sender, receiver) = channel();
        let mut job = Job {
            title: String::new(),
            lines: vec![],
            progress: None,
            outcome: None,
            next: next.into(),
            stages: stages.into(),
            child: None,
            readers: vec![],
            sender,
            receiver,
            total: 0,
            _files: files,
        };
        job.advance()?;
        Ok(job)
    }

    fn advance(&mut self) -> Result<()> {
        let Some(mut stage) = self.stages.pop_front() else {
            self.outcome = Some(Ok(()));
            return Ok(());
        };
        self.title = stage.title;
        self.progress = None;
        self.total = stage
            .source
            .and_then(|path| path.metadata().ok())
            .map_or(0, |metadata| metadata.len());
        let mut child = stage
            .command
            .env("PYTHONUNBUFFERED", "1")
            .stdin(if stage.input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                format!(
                    "Could not start {}: {error}",
                    stage.command.get_program().to_string_lossy()
                )
            })?;
        if let (Some(input), Some(mut stdin)) = (stage.input, child.stdin.take()) {
            let _ = stdin.write_all(input.as_bytes());
        }
        self.readers = vec![
            forward(child.stdout.take().unwrap(), self.sender.clone()),
            forward(child.stderr.take().unwrap(), self.sender.clone()),
        ];
        self.child = Some(child);
        Ok(())
    }

    fn drain(&mut self) {
        while let Ok(line) = self.receiver.try_recv() {
            let meter = progress(&line, self.total);
            // A progress meter updates its line in place rather than filling the log.
            let replaces = meter.is_some()
                && self
                    .lines
                    .last()
                    .is_some_and(|last| progress(last, self.total).is_some());
            if meter.is_some() {
                self.progress = meter;
            }
            if replaces {
                *self.lines.last_mut().unwrap() = line;
            } else {
                self.lines.push(line);
            }
        }
        let excess = self.lines.len().saturating_sub(1000);
        self.lines.drain(..excess);
    }

    /// Collect new output and move on when the current command has ended.
    pub fn tick(&mut self) {
        self.drain();
        let Some(child) = &mut self.child else {
            return;
        };
        let Ok(Some(status)) = child.try_wait() else {
            return;
        };
        self.child = None;
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        self.drain();
        if !status.success() {
            self.outcome = Some(Err(format!("{} failed ({status})", self.title)));
        } else if let Err(error) = self.advance() {
            self.outcome = Some(Err(error.to_string()));
        }
    }

    /// A finished ISO build that has not been written to a drive yet.
    pub fn offers_usb(&self, action: &str, iso: &str) -> bool {
        self.outcome == Some(Ok(())) && self.title == action && action == iso
    }

    pub fn running(&self) -> bool {
        self.outcome.is_none()
    }

    /// Ask the running command to stop. SIGTERM, so sudo passes it on.
    pub fn cancel(&mut self) {
        self.stages.clear();
        if let Some(child) = &self.child {
            let _ = Command::new("kill").arg(child.id().to_string()).status();
            self.lines.push("Cancelling…".into());
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_progress_from_xorriso_and_dd() {
        assert_eq!(progress("#########            25.0%", 0), Some(0.25));
        let line = "xorriso : UPDATE : Writing:     663552s   50.0%   fifo 100%  buf  50%  176.3xD";
        assert_eq!(progress(line, 0), Some(0.5));
        let line = "1048576 bytes (1.0 MB, 1.0 MiB) copied, 1 s, 1.0 MB/s";
        assert_eq!(progress(line, 4194304), Some(0.25));
        assert_eq!(
            progress("xorriso : UPDATE :  991 nodes read in 1 seconds", 5),
            None
        );
    }

    #[test]
    fn runs_stages_in_order_and_stops_at_a_failure() {
        let stage = |script: &str| {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            Stage {
                title: script.into(),
                command,
                source: None,
                input: None,
            }
        };
        let finish = |mut job: Job| {
            while job.running() {
                job.tick();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            job
        };
        let stages = vec![stage("echo one; printf 'a\\rb\\n' >&2"), stage("echo two")];
        let job = finish(Job::start(stages, "", vec![]).unwrap());
        assert_eq!(job.outcome, Some(Ok(())));
        assert!(job.lines.contains(&"one".to_string()) && job.lines.contains(&"b".to_string()));
        assert_eq!(job.lines.last().unwrap(), "two");

        let job =
            finish(Job::start(vec![stage("exit 3"), stage("echo never")], "", vec![]).unwrap());
        assert!(matches!(job.outcome, Some(Err(_))));
        assert!(!job.lines.contains(&"never".to_string()));
    }
}
