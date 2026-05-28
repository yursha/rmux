mod app;
mod sys;

use std::io::{stdin, stdout};

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use scopeguard::defer;

fn main() {
    sys::setup_sigwinch_handler().expect("Failed to set up resize handler");

    enable_raw_mode().expect("Failed to enable raw mode");
    defer! { disable_raw_mode().expect("Failed to restore terminal mode"); }

    let stdin_handle = stdin();

    let orig_flags = fcntl(&stdin_handle, FcntlArg::F_GETFL).expect("Failed to read stdin flags");
    defer! {
        fcntl(&stdin_handle, FcntlArg::F_SETFL(OFlag::from_bits_truncate(orig_flags)))
            .expect("Failed to reset stdin flags");
    }

    sys::set_nonblocking(&stdin_handle).expect("Failed to enable non-blocking IO");

    let mut stdin_lock = stdin_handle.lock();
    let mut stdout_lock = stdout().lock();

    let mut app = app::MuxApp::new().unwrap();
    app.run(&mut stdin_lock, &mut stdout_lock).unwrap();
}
