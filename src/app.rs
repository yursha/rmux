use polling::{Event, Events, Poller};
use std::io::{StdinLock, StdoutLock};
use std::num::NonZero;
use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::sys;

enum LoopAction {
    Continue,
    Redraw,
    Exit,
}

struct Window {
    master: OwnedFd,
    parser: vt100::Parser,
    bracketed_paste: bool,
}

pub struct MuxApp {
    windows: Vec<Window>,
    current_window_idx: usize,
    prefix_mode: bool,
    poller: Poller,
}

impl MuxApp {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let poller = Poller::new()?;
        let physical_size = sys::get_terminal_size();
        let pty_rows = if physical_size.ws_row > 1 {
            physical_size.ws_row - 1
        } else {
            physical_size.ws_row
        };
        let first_window = Window {
            master: sys::spawn_pty(pty_rows, physical_size.ws_col)?,
            parser: vt100::Parser::new(pty_rows, physical_size.ws_col, 0),
            bracketed_paste: false,
        };

        Ok(Self {
            windows: vec![first_window],
            current_window_idx: 0,
            prefix_mode: false,
            poller,
        })
    }

    pub fn run(
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

            if sys::TERMINAL_RESIZED.swap(false, Ordering::Relaxed) {
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
                    let data = &pty_buffer[..n];

                    // Scan the incoming PTY data for bracketed paste mode toggle sequences
                    if data.windows(8).any(|w| w == b"\x1b[?2004h") {
                        self.windows[win_idx].bracketed_paste = true;
                    } else if data.windows(8).any(|w| w == b"\x1b[?2004l") {
                        self.windows[win_idx].bracketed_paste = false;
                    }

                    self.windows[win_idx].parser.process(data);
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
        let host_size = sys::get_terminal_size();
        let active_rows = if host_size.ws_row > 1 {
            host_size.ws_row - 1
        } else {
            host_size.ws_row
        };

        let new_win = Window {
            master: sys::spawn_pty(active_rows, host_size.ws_col)?,
            parser: vt100::Parser::new(active_rows, host_size.ws_col, 0),
            bracketed_paste: false,
        };
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
        sys::sync_terminal_size(&self.windows[self.current_window_idx].master);
        let new_size = sys::get_terminal_size();

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
        // Sync the host terminal's bracketed paste state with the active window
        if self.windows[self.current_window_idx].bracketed_paste {
            sys::write_stdout_blocking(stdout_lock, b"\x1b[?2004h")?;
        } else {
            sys::write_stdout_blocking(stdout_lock, b"\x1b[?2004l")?;
        }

        let screen_contents = self.windows[self.current_window_idx]
            .parser
            .screen()
            .contents_formatted();

        // 1. Move cursor home safely
        sys::write_stdout_blocking(stdout_lock, b"\x1b[H")?;

        // 2. Write virtual terminal screen safely
        sys::write_stdout_blocking(stdout_lock, &screen_contents)?;

        // 3. Render status bar position safely
        let host_size = sys::get_terminal_size();
        let move_to_bottom = format!("\x1b[{};1H", host_size.ws_row);
        sys::write_stdout_blocking(stdout_lock, move_to_bottom.as_bytes())?;

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
        sys::write_stdout_blocking(stdout_lock, status_text.as_bytes())?;

        // 4. Restore the cursor to its layout position safely
        let (v_row, v_col) = self.windows[self.current_window_idx]
            .parser
            .screen()
            .cursor_position();
        let restore_cursor = format!("\x1b[{};{}H", v_row + 1, v_col + 1);
        sys::write_stdout_blocking(stdout_lock, restore_cursor.as_bytes())?;

        // 5. Final flush wrapped in a safe loop
        sys::flush_stdout_blocking(stdout_lock)?;
        Ok(())
    }
}
