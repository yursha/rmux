use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::{forkpty, ForkptyResult};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::execvp;
use std::ffi::CString;
use std::io::{StdoutLock, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

// --- PTY forking --- //

pub fn spawn_pty(rows: u16, cols: u16) -> Result<OwnedFd, Box<dyn std::error::Error>> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    unsafe {
        match forkpty(Some(&ws), None)? {
            ForkptyResult::Parent { child: _, master } => {
                set_nonblocking(&master)?;
                Ok(master)
            }
            ForkptyResult::Child => {
                let native_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

                // Convert to CString, falling back to basic "/bin/sh" if a null byte sneaks in
                let shell =
                    CString::new(native_shell).unwrap_or_else(|_| CString::new("/bin/sh").unwrap());

                let args = [shell.clone()];
                let _ = execvp(&shell, &args);
                std::process::exit(1);
            }
        }
    }
}

// --- Terminal window size utilities --- //

pub static TERMINAL_RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_signal: libc::c_int) {
    TERMINAL_RESIZED.store(true, Ordering::Relaxed);
}

pub fn setup_sigwinch_handler() -> Result<(), Box<dyn std::error::Error>> {
    let sigwinch_action = SigAction::new(
        SigHandler::Handler(handle_sigwinch),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    unsafe {
        sigaction(Signal::SIGWINCH, &sigwinch_action)?;
    }
    Ok(())
}

pub fn get_terminal_size() -> libc::winsize {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws);
    }
    ws
}

pub fn sync_terminal_size(fd: &impl AsRawFd) {
    let mut ws = get_terminal_size();
    if ws.ws_row > 1 {
        ws.ws_row -= 1;
    }
    unsafe {
        libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &mut ws);
    }
}

// --- IO utilities --- //

pub fn set_nonblocking(fd: &impl AsFd) -> Result<(), Box<dyn std::error::Error>> {
    let flags = fcntl(fd, FcntlArg::F_GETFL)?;
    let mut oflags = OFlag::from_bits_truncate(flags);
    oflags.insert(OFlag::O_NONBLOCK);
    fcntl(fd, FcntlArg::F_SETFL(oflags))?;
    Ok(())
}

pub fn write_stdout_blocking(
    stdout: &mut StdoutLock,
    mut data: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    while !data.is_empty() {
        match stdout.write(data) {
            Ok(0) => {
                let err =
                    std::io::Error::new(std::io::ErrorKind::WriteZero, "failed to write to stdout");
                return Err(err.into());
            }
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::yield_now(),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn flush_stdout_blocking(stdout: &mut StdoutLock) -> Result<(), Box<dyn std::error::Error>> {
    while let Err(e) = stdout.flush() {
        match e.kind() {
            std::io::ErrorKind::WouldBlock => std::thread::yield_now(),
            std::io::ErrorKind::Interrupted => {}
            _ => return Err(e.into()),
        }
    }
    Ok(())
}
