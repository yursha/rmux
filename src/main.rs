use std::ffi::CString;
use std::io::{stdin, stdout, Read, Write, StdinLock, StdoutLock};
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

struct Window {
    master: OwnedFd,
    parser: vt100::Parser,
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

fn spawn_window(rows: u16, cols: u16) -> Result<Window, Box<dyn std::error::Error>> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    unsafe {
        match nix::pty::forkpty(Some(&ws), None)? {
            nix::pty::ForkptyResult::Parent { child: _, master } => {
                // safeguard from blocking on the PTY streams
                let _ = set_nonblocking(&master);

                let parser = vt100::Parser::new(rows, cols, 0);
                Ok(Window { master, parser })
            }
            nix::pty::ForkptyResult::Child => {
                // Drop the child process directly into a pristine shell
                let shell = std::ffi::CString::new("/bin/bash").unwrap();
                let args = [shell.clone()];
                let _ = nix::unistd::execvp(&shell, &args);
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
            // Subtract 1 row to reserve space for our persistent status bar!
            if ws.ws_row > 1 {
                ws.ws_row -= 1;
            }
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

/*
/// Processes incoming target application text streaming out of the PTY master channel.
fn handle_pty_data(master: &OwnedFd, parser: &mut vt100::Parser, buffer: &mut [u8]) -> Result<bool, Box<dyn std::error::Error>> {
    match nix::unistd::read(master, buffer) {
        Ok(0) => return Ok(false), // Clean EOF
        Ok(n) => {
            parser.process(&buffer[..n]);
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
*/

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set up our multiplexer state trackers
    let mut windows: Vec<Window> = Vec::new();
    let mut current_window_idx: usize = 0;
    let mut prefix_mode = false; // Tracks if Ctrl+B was just tapped
                                 
    
    let mut physical_size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut physical_size); }

    let sigwinch_action = SigAction::new(SigHandler::Handler(handle_sigwinch), SaFlags::empty(), SigSet::empty());
    unsafe { sigaction(Signal::SIGWINCH, &sigwinch_action)?; }

    // Allocate the virtual matrix grid using the isolated row count
    let pty_rows = if physical_size.ws_row > 1 { physical_size.ws_row - 1 } else { physical_size.ws_row };
    let first_window = spawn_window(pty_rows, physical_size.ws_col)?;

    /*
    let mut vt_parser = vt100::Parser::new(pty_rows, physical_size.ws_col, 0);
    let child = PtyChild::spawn_bash().expect("Failed to spawn bash");
    sync_terminal_size(&child.master);
    */

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
        poller.add(&stdin_lock, Event::readable(0))?;  // Token 1: Stdin
        poller.add(&first_window.master, Event::readable(1))?; // Token 0: PTY
    }
    windows.push(first_window);

    let mut events = Events::with_capacity(NonZero::new(10).unwrap());

    // 5. Core Multiplexer Event Loop
    'outer: loop {
        let mut pty_changed = false;

        if TERMINAL_RESIZED.swap(false, Ordering::Relaxed) {
            sync_terminal_size(&windows[current_window_idx].master);

            // Update the virtual grid to match the new physical window bounds
            let mut new_size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
            unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut new_size); }

            let pty_rows = if new_size.ws_row > 1 { new_size.ws_row - 1 } else { new_size.ws_row };
            for win in &mut windows {
                win.parser.screen_mut().set_size(pty_rows, new_size.ws_col);
            }
            pty_changed = true; // Force redraw on window resizing
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
                    let mut stdin_buffer = [0u8; 128];
                    if let Ok(n) = stdin_lock.read(&mut stdin_buffer) {
                        if n == 0 { break 'outer; }

                        let mut i = 0;

                        while i < n {
                            let byte = stdin_buffer[i];

                            if prefix_mode {
                                prefix_mode = false; // Reset mode immediately upon processing command
                                pty_changed = true; // Force redraw to clear the prefix badge
                                match byte {
                                    b'c' => { // Create a new window session
                                        let mut host_size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
                                        unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut host_size); }
                                        let active_rows = if host_size.ws_row > 1 { host_size.ws_row - 1 } else { host_size.ws_row };

                                        if let Ok(new_win) = spawn_window(active_rows, host_size.ws_col) {
                                            let next_token = 1 + windows.len();
                                            unsafe { let _ = poller.add(&new_win.master, Event::readable(next_token)); }
                                            windows.push(new_win);
                                            current_window_idx = windows.len() - 1;
                                        }
                                    }
                                    b'n' => { // Switch to Next Window
                                        current_window_idx = (current_window_idx + 1) % windows.len();
                                    }
                                    b'p' => { // Switch to Previous Window
                                        current_window_idx = (current_window_idx + windows.len() - 1) % windows.len();
                                    }
                                    0x02 => { // Send a literal Ctrl+B to the application by double-tapping it
                                        let _ = nix::unistd::write(&windows[current_window_idx].master, &[0x02]);
                                    }
                                    _ => {} // Unmapped prefix key, sink silently
                                }
                            } else {
                                if byte == 0x02 { // Caught raw Ctrl+B!
                                    prefix_mode = true;
                                    pty_changed = true; // Force immediate redraw to show [PREFIX] badge
                                } else { // Normal execution pass-through to active window
                                    let _ = nix::unistd::write(&windows[current_window_idx].master, &[byte]);
                                }
                            }
                            i += 1;
                        }
                    }
                    poller.modify(&stdin_lock, Event::readable(0))?;
                }
                _ => {
                    let win_idx = ev.key - 1;
                    if win_idx < windows.len() {
                        let mut pty_buffer = [0u8; 4096];
                        match nix::unistd::read(&windows[win_idx].master, &mut pty_buffer) {
                            Ok(0) | Err(nix::errno::Errno::EIO) => {
                                // 1. Remove the dead window from state and stop polling it
                                let closed_win = windows.remove(win_idx);
                                let _ = poller.delete(&closed_win.master);

                                // 2. If that was the last open window, exit the entire program!
                                if windows.is_empty() {
                                    break 'outer;
                                }

                                // 3. Re-index remaining windows so their poller tokens align with their new vector indices
                                for (idx, win) in windows.iter().enumerate() {
                                    let _ = poller.modify(&win.master, Event::readable(idx + 1));
                                }

                                // 4. Safeguard our focus index so it doesn't point out-of-bounds
                                if current_window_idx >= windows.len() {
                                    current_window_idx = windows.len() - 1;
                                }

                                pty_changed = true;
                                break;
                            }
                            Ok(n) => {
                                // Update the specific window parser matrix, even if running in the background!
                                windows[win_idx].parser.process(&pty_buffer[..n]);
                                if win_idx == current_window_idx {
                                    pty_changed = true; // Only flag viewport changes if it's our focused tab
                                }
                            }
                            _ => {}
                        }
                        poller.modify(&windows[win_idx].master, Event::readable(ev.key))?;
                    }
                }
            }
        }

        // Draw Frame Layer (Compositor composition)
        if pty_changed {
            let screen_contents = windows[current_window_idx].parser.screen().contents_formatted();

            // Render Step A: Draw the virtual guest terminal layer from home (1,1)
            let _ = stdout_lock.write_all(b"\x1b[H");
            let _ = write_stdout_blocking(&mut stdout_lock, &screen_contents);

            // Fetch host dimensions to pinpoint the physical window's absolute bottom edge
            let mut host_size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
            unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut host_size); }

            // Render Step B: Snap the cursor to column 1 of our reserved bottom row
            let move_to_bottom = format!("\x1b[{};1H", host_size.ws_row);
            let _ = stdout_lock.write_all(move_to_bottom.as_bytes());

            // Render Step C: Paint the styled status bar
            let mut dynamic_tabs = String::new();
            for (idx, _) in windows.iter().enumerate() {
                if idx == current_window_idx {
                    // Highlight the active tab with a lighter background and an asterisk
                    dynamic_tabs.push_str(&format!(" \x1b[48;5;240m\x1b[38;5;255m {}:bash* \x1b[48;5;236m", idx));
                } else {
                    // Regular dark background for background tabs
                    dynamic_tabs.push_str(&format!(" {}:bash ", idx));
                }
            }

            // Draw a bright red alert notice if tmux-style prefix mode is active
            let prefix_badge = if prefix_mode {
                " \x1b[48;5;160m\x1b[38;5;255m [PREFIX] \x1b[48;5;236m"
            } else {
                ""
            };

            let status_text = format!(
                "\x1b[48;5;236m\x1b[38;5;255m 🦀 RUST-MUX |{} Tabs:{} | Grid: {}x{} \x1b[K\x1b[0m",
                prefix_badge, dynamic_tabs, host_size.ws_row, host_size.ws_col
            );

            let _ = stdout_lock.write_all(status_text.as_bytes());

            // Render Step D: Put the hardware cursor back to its correct active input position
            // Note: vt100 coordinates are 0-indexed; ANSI escape sequences are 1-indexed.
            let (v_row, v_col) = windows[current_window_idx].parser.screen().cursor_position();
            let restore_cursor = format!("\x1b[{};{}H", v_row + 1, v_col + 1);
            let _ = stdout_lock.write_all(restore_cursor.as_bytes());

            let _ = stdout_lock.flush();
        }
    }

    Ok(())
}
