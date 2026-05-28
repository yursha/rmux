use std::ffi::CString;
use std::io::{stdin, stdout, StdinLock, StdoutLock, Write};
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

enum LoopAction {
    Continue,
    Redraw,
    Exit,
}

struct Window {
    master: OwnedFd,
    parser: vt100::Parser,
}

struct MuxApp {
    windows: Vec<Window>,
    current_window_idx: usize,
    prefix_mode: bool,
    poller: Poller,
}

impl MuxApp {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let poller = Poller::new()?;
        let physical_size = get_terminal_size();
        let pty_rows = if physical_size.ws_row > 1 {
            physical_size.ws_row - 1
        } else {
            physical_size.ws_row
        };
        let first_window = spawn_window(pty_rows, physical_size.ws_col)?;

        Ok(Self {
            windows: vec![first_window],
            current_window_idx: 0,
            prefix_mode: false,
            poller,
        })
    }

    fn run(
        &mut self,
        stdin_lock: &mut StdinLock,
        stdout_lock: &mut StdoutLock,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Register initial poll handles
        unsafe {
            self.poller.add(&*stdin_lock, Event::readable(0))?;
            if !self.windows.is_empty() {
                self.poller
                    .add(&self.windows[0].master, Event::readable(1))?;
            }
        }

        let mut events = Events::with_capacity(NonZero::new(10).unwrap());
        self.draw_interface(stdout_lock)?;

        'outer: loop {
            let mut pty_changed = false;

            if TERMINAL_RESIZED.swap(false, Ordering::Relaxed) {
                self.handle_terminal_resize();
                pty_changed = true;
            }

            events.clear();
            if let Err(e) = self
                .poller
                .wait(&mut events, Some(Duration::from_millis(15)))
            {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }

            for ev in events.iter() {
                let action = match ev.key {
                    0 => self.handle_stdin(stdin_lock)?,
                    _ => self.handle_pty(ev.key - 1)?,
                };

                match action {
                    LoopAction::Exit => break 'outer,
                    LoopAction::Redraw => pty_changed = true,
                    LoopAction::Continue => {}
                }
            }

            if pty_changed {
                self.draw_interface(stdout_lock)?;
            }
        }
        Ok(())
    }

    fn handle_stdin(
        &mut self,
        stdin_lock: &mut StdinLock,
    ) -> Result<LoopAction, Box<dyn std::error::Error>> {
        let mut stdin_buffer = [0u8; 128];
        let mut action = LoopAction::Continue;

        if let Ok(n) = std::io::Read::read(stdin_lock, &mut stdin_buffer) {
            if n == 0 {
                return Ok(LoopAction::Exit);
            }

            for &byte in &stdin_buffer[..n] {
                if self.prefix_mode {
                    self.prefix_mode = false;
                    action = LoopAction::Redraw;
                    match byte {
                        b'c' => self.spawn_new_window()?,
                        b'n' => {
                            self.current_window_idx =
                                (self.current_window_idx + 1) % self.windows.len()
                        }
                        b'p' => {
                            self.current_window_idx = (self.current_window_idx + self.windows.len()
                                - 1)
                                % self.windows.len()
                        }
                        0x02 => {
                            let _ = nix::unistd::write(
                                &self.windows[self.current_window_idx].master,
                                &[0x02],
                            );
                        }
                        _ => {}
                    }
                } else if byte == 0x02 {
                    self.prefix_mode = true;
                    action = LoopAction::Redraw;
                } else {
                    let _ =
                        nix::unistd::write(&self.windows[self.current_window_idx].master, &[byte]);
                }
            }
        }
        self.poller.modify(&*stdin_lock, Event::readable(0))?;
        Ok(action)
    }

    fn handle_pty(&mut self, win_idx: usize) -> Result<LoopAction, Box<dyn std::error::Error>> {
        if win_idx >= self.windows.len() {
            return Ok(LoopAction::Continue);
        }

        let mut pty_buffer = [0u8; 4096];
        let mut pty_changed = false;

        loop {
            match nix::unistd::read(&self.windows[win_idx].master, &mut pty_buffer) {
                Ok(0) | Err(nix::errno::Errno::EIO) => {
                    if self.close_window(win_idx) {
                        return Ok(LoopAction::Exit);
                    }
                    return Ok(LoopAction::Redraw);
                }
                Ok(n) => {
                    self.windows[win_idx].parser.process(&pty_buffer[..n]);
                    if win_idx == self.current_window_idx {
                        pty_changed = true;
                    }
                }
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => break,
            }
        }

        if win_idx < self.windows.len() {
            self.poller
                .modify(&self.windows[win_idx].master, Event::readable(win_idx + 1))?;
        }

        Ok(if pty_changed {
            LoopAction::Redraw
        } else {
            LoopAction::Continue
        })
    }

    fn spawn_new_window(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let host_size = get_terminal_size();
        let active_rows = if host_size.ws_row > 1 {
            host_size.ws_row - 1
        } else {
            host_size.ws_row
        };

        let new_win = spawn_window(active_rows, host_size.ws_col)?;
        let next_token = 1 + self.windows.len();
        unsafe {
            self.poller
                .add(&new_win.master, Event::readable(next_token))?;
        }
        self.windows.push(new_win);
        self.current_window_idx = self.windows.len() - 1;
        Ok(())
    }

    fn close_window(&mut self, win_idx: usize) -> bool {
        let closed_win = self.windows.remove(win_idx);
        let _ = self.poller.delete(&closed_win.master);

        if self.windows.is_empty() {
            return true;
        }

        for (idx, win) in self.windows.iter().enumerate() {
            let _ = self.poller.modify(&win.master, Event::readable(idx + 1));
        }

        if self.current_window_idx >= self.windows.len() {
            self.current_window_idx = self.windows.len() - 1;
        }

        false
    }

    fn handle_terminal_resize(&mut self) {
        sync_terminal_size(&self.windows[self.current_window_idx].master);
        let new_size = get_terminal_size();

        let pty_rows = if new_size.ws_row > 1 {
            new_size.ws_row - 1
        } else {
            new_size.ws_row
        };
        for win in &mut self.windows {
            win.parser.screen_mut().set_size(pty_rows, new_size.ws_col);
        }
    }

    fn draw_interface(
        &self,
        stdout_lock: &mut StdoutLock,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let screen_contents = self.windows[self.current_window_idx]
            .parser
            .screen()
            .contents_formatted();

        // 1. Move cursor home safely
        write_stdout_blocking(stdout_lock, b"\x1b[H")?;

        // 2. Write virtual terminal screen safely
        write_stdout_blocking(stdout_lock, &screen_contents)?;

        // 3. Render status bar position safely
        let host_size = get_terminal_size();
        let move_to_bottom = format!("\x1b[{};1H", host_size.ws_row);
        write_stdout_blocking(stdout_lock, move_to_bottom.as_bytes())?;

        let mut dynamic_tabs = String::new();
        for (idx, _) in self.windows.iter().enumerate() {
            if idx == self.current_window_idx {
                dynamic_tabs.push_str(&format!(
                    " \x1b[48;5;240m\x1b[38;5;255m {}:bash* \x1b[48;5;236m",
                    idx
                ));
            } else {
                dynamic_tabs.push_str(&format!(" {}:bash ", idx));
            }
        }

        let prefix_badge = if self.prefix_mode {
            " \x1b[48;5;160m\x1b[38;5;255m [PREFIX] \x1b[48;5;236m"
        } else {
            ""
        };

        let status_text = format!(
            "\x1b[48;5;236m\x1b[38;5;255m 🦀 RUST-MUX |{} Tabs:{} | Grid: {}x{} \x1b[K\x1b[0m",
            prefix_badge, dynamic_tabs, host_size.ws_row, host_size.ws_col
        );
        write_stdout_blocking(stdout_lock, status_text.as_bytes())?;

        // 4. Restore the cursor to its layout position safely
        let (v_row, v_col) = self.windows[self.current_window_idx]
            .parser
            .screen()
            .cursor_position();
        let restore_cursor = format!("\x1b[{};{}H", v_row + 1, v_col + 1);
        write_stdout_blocking(stdout_lock, restore_cursor.as_bytes())?;

        // 5. Final flush wrapped in a safe loop
        flush_stdout_blocking(stdout_lock)?;
        Ok(())
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
        match forkpty(Some(&ws), None)? {
            ForkptyResult::Parent { child: _, master } => {
                let _ = set_nonblocking(&master);
                let parser = vt100::Parser::new(rows, cols, 0);
                Ok(Window { master, parser })
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

static TERMINAL_RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_signal: libc::c_int) {
    TERMINAL_RESIZED.store(true, Ordering::Relaxed);
}

fn setup_sigwinch_handler() -> Result<(), Box<dyn std::error::Error>> {
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

fn get_terminal_size() -> libc::winsize {
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

fn sync_terminal_size(master: &impl AsRawFd) {
    let mut ws = get_terminal_size();
    if ws.ws_row > 1 {
        ws.ws_row -= 1;
    }
    unsafe {
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &mut ws);
    }
}

// --- IO utilities --- //

fn set_nonblocking(fd: &impl AsFd) -> Result<(), Box<dyn std::error::Error>> {
    let flags = fcntl(fd, FcntlArg::F_GETFL)?;
    let mut oflags = OFlag::from_bits_truncate(flags);
    oflags.insert(OFlag::O_NONBLOCK);
    fcntl(fd, FcntlArg::F_SETFL(oflags))?;
    Ok(())
}

fn write_stdout_blocking(
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

fn flush_stdout_blocking(stdout: &mut StdoutLock) -> Result<(), Box<dyn std::error::Error>> {
    while let Err(e) = stdout.flush() {
        match e.kind() {
            std::io::ErrorKind::WouldBlock => std::thread::yield_now(),
            std::io::ErrorKind::Interrupted => {}
            _ => return Err(e.into()),
        }
    }
    Ok(())
}

// --- main --- //

fn main() {
    setup_sigwinch_handler().expect("Failed to set up resize handler");

    enable_raw_mode().expect("Failed to enable raw mode");
    defer! { disable_raw_mode().expect("Failed to restore terminal mode"); }

    let stdin_handle = stdin();

    let orig_flags = fcntl(&stdin_handle, FcntlArg::F_GETFL).expect("Failed to read stdin flags");
    defer! {
        fcntl(&stdin_handle, FcntlArg::F_SETFL(OFlag::from_bits_truncate(orig_flags)))
            .expect("Failed to reset stdin flags");
    }

    set_nonblocking(&stdin_handle).expect("Failed to enable non-blocking IO");

    let mut stdin_lock = stdin_handle.lock();
    let mut stdout_lock = stdout().lock();

    let mut app = MuxApp::new().unwrap();
    app.run(&mut stdin_lock, &mut stdout_lock).unwrap();
}
