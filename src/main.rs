use std::ffi::CString;
use std::io::{stdin, stdout, Write, StdinLock, StdoutLock};
use std::num::NonZero;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::{forkpty, ForkptyResult};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::execvp;
use polling::{Event, Events, Poller};
use scopeguard::defer;

// Global flag modified during an OS signal interruption (SIGWINCH)
static TERMINAL_RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_signal: libc::c_int) {
    TERMINAL_RESIZED.store(true, Ordering::Relaxed);
}

pub struct PtyChild {
    pub master: OwnedFd,
    pub pid: nix::unistd::Pid,
}

impl PtyChild {
    pub fn spawn_bash() -> Result<Self, Box<dyn std::error::Error>> {
        let res = unsafe { forkpty(None, None)? };
        match res {
            ForkptyResult::Parent { child, master } => Ok(PtyChild { master, pid: child }),
            ForkptyResult::Child => {
                let bash = CString::new("bash")?;
                execvp(&bash, &[&bash])?;
                std::process::exit(1);
            }
        }
    }
}

/// Grabs the physical dimensions of the host window and applies them to the PTY.
fn sync_terminal_size(master: &impl AsRawFd) {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
            libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }
}

/// Configures a file descriptor to operate in non-blocking mode.
fn set_nonblocking(fd: &impl AsFd) -> Result<(), Box<dyn std::error::Error>> {
    let flags = fcntl(fd, FcntlArg::F_GETFL)?;
    let mut oflags = OFlag::from_bits_truncate(flags);
    oflags.insert(OFlag::O_NONBLOCK);
    fcntl(fd, FcntlArg::F_SETFL(oflags))?;
    Ok(())
}

/// Explicitly handles writing to a non-blocking stdout by yielding when buffers fill up.
fn write_stdout_blocking(stdout: &mut StdoutLock, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        match stdout.write(data) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "failed to write to stdout")),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::yield_now(),
            Err(e) => return Err(e),
        }
    }
    while let Err(e) = stdout.flush() {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            std::thread::yield_now();
        } else {
            return Err(e);
        }
    }
    Ok(())
}

/// Processes incoming target application text streaming out of the PTY master channel.
fn handle_pty_data(master: &OwnedFd, stdout: &mut StdoutLock, buffer: &mut [u8]) -> Result<bool, Box<dyn std::error::Error>> {
    match nix::unistd::read(master, buffer) {
        Ok(0) => return Ok(false), // Clean EOF
        Ok(n) => {
            if let Err(e) = write_stdout_blocking(stdout, &buffer[..n]) {
                eprintln!("\r\nStdout Write Error: {}\r\n", e);
                return Ok(false);
            }
        }
        Err(nix::errno::Errno::EAGAIN) => {}
        Err(nix::errno::Errno::EIO) => return Ok(false), // Traditional Linux PTY session termination
        Err(e) => {
            eprintln!("\r\nPTY Error: {}\r\n", e);
            return Ok(false);
        }
    }
    Ok(true)
}

/// Processes physical key presses captured from the user's host stdin terminal handle.
fn handle_stdin_data(stdin: &mut StdinLock, master: &OwnedFd, buffer: &mut [u8]) -> bool {
    match nix::unistd::read(stdin, buffer) {
        Ok(0) => {}
        Ok(n) if n > 0 => {
            if let Err(e) = nix::unistd::write(master, &buffer[..n]) {
                eprintln!("\r\nPTY Write Error: {}\r\n", e);
                return false;
            }
        }
        Ok(_) => {}
        Err(nix::errno::Errno::EAGAIN) => {}
        Err(e) => {
            eprintln!("\r\nStdin Error: {}\r\n", e);
            return false;
        }
    }
    true
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let child = PtyChild::spawn_bash().expect("Failed to spawn bash");
    
    // 1. Terminal Layout and Signal Registration
    sync_terminal_size(&child.master);
    let sigwinch_action = SigAction::new(SigHandler::Handler(handle_sigwinch), SaFlags::empty(), SigSet::empty());
    unsafe { sigaction(Signal::SIGWINCH, &sigwinch_action)?; }

    // 2. High-Performance Locked I/O Initialization
    let stdin_handle = stdin();

    // 3. Flags and State Fallbacks
    let orig_flags = fcntl(&stdin_handle, FcntlArg::F_GETFL)?;
    defer! { 
        let _ = fcntl(&stdin_handle, FcntlArg::F_SETFL(OFlag::from_bits_truncate(orig_flags))); 
    }

    set_nonblocking(&stdin_handle)?;

    enable_raw_mode()?;
    defer! { let _ = disable_raw_mode(); }

    // Now instantiate your locks. They will co-exist peacefully with the handle borrows.
    let mut stdin_lock = stdin_handle.lock();
    let mut stdout_lock = stdout().lock();

    // 4. Poller Registration
    let poller = Poller::new()?;
    unsafe {
        poller.add(&child.master, Event::readable(0))?; // Token 0: PTY
        poller.add(&stdin_lock, Event::readable(1))?;  // Token 1: Stdin
    }

    let mut buffer = [0u8; 1024];
    let mut events = Events::with_capacity(NonZero::new(10).unwrap());

    // 5. Core Multiplexer Event Loop
    'outer: loop {
        if TERMINAL_RESIZED.swap(false, Ordering::Relaxed) {
            sync_terminal_size(&child.master);
        }

        events.clear();
        if let Err(e) = poller.wait(&mut events, Some(Duration::from_millis(15))) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }

        for ev in events.iter() {
            match ev.key {
                0 => {
                    if !handle_pty_data(&child.master, &mut stdout_lock, &mut buffer)? {
                        break 'outer;
                    }
                    poller.modify(&child.master, Event::readable(0))?;
                }
                1 => {
                    if !handle_stdin_data(&mut stdin_lock, &child.master, &mut buffer) {
                        break 'outer;
                    }
                    poller.modify(&stdin_lock, Event::readable(1))?;
                }
                _ => {}
            }
        }
    }

    Ok(())
}
