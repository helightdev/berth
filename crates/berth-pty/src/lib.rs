//! A command on a pseudo-terminal of its own.

use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::process::CommandExt,
    process::{Child, Command},
};

fn window(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// A pseudo-terminal: our end, and the end commands get.
pub fn open(rows: u16, cols: u16) -> io::Result<(File, OwnedFd)> {
    let (mut ours, mut theirs) = (0, 0);
    // SAFETY: openpty fills the two descriptors, which are owned from here on.
    unsafe {
        let size = window(rows, cols);
        if libc::openpty(
            &mut ours,
            &mut theirs,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        libc::fcntl(ours, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(theirs, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok((File::from_raw_fd(ours), OwnedFd::from_raw_fd(theirs)))
    }
}

/// Make `command` run on the terminal whose far end is `theirs`: it becomes
/// the leader of a new session with that terminal as its own, so programs that
/// open /dev/tty reach it.
pub fn attach(command: &mut Command, theirs: &OwnedFd) -> io::Result<()> {
    command
        .stdin(theirs.try_clone()?)
        .stdout(theirs.try_clone()?)
        .stderr(theirs.try_clone()?)
        .env("TERM", "xterm-256color");
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Start `command` on a new pseudo-terminal. Returns our end of it.
pub fn spawn(mut command: Command, rows: u16, cols: u16) -> io::Result<(File, Child)> {
    let (ours, theirs) = open(rows, cols)?;
    attach(&mut command, &theirs)?;
    let child = command.spawn()?;
    // The command holds the other end now; ours reports the end when it closes.
    drop(command);
    Ok((ours, child))
}

/// Give the terminal a new size; the command is told by the system.
pub fn resize(terminal: &File, rows: u16, cols: u16) {
    // SAFETY: the descriptor is open for as long as `terminal` is.
    unsafe {
        libc::ioctl(terminal.as_raw_fd(), libc::TIOCSWINSZ, &window(rows, cols));
    }
}
