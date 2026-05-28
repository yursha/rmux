Split Screens: slice your physical window coordinates in half and draw the top half of vt_parser_1 on rows 1–20, and the top half of vt_parser_2 on rows 21–40.

3. The Layout Engine (The Window Manager)
This is where you handle the "splits" and "windows."

The Task: Create a tree data structure where each node is either a Window or a Pane. When the window size changes, your layout engine must recursively calculate the new dimensions for all children.

The Strategy: Use a layout algorithm (like a binary tree of splits) to determine the coordinates of each pane.

4. The Client/Server Protocol
tmux runs as a daemon (the server) and a client connects to it via a Unix Domain Socket.

The Task: Implement a simple binary protocol or JSON-RPC over Unix Sockets (tokio::net::UnixListener).

The Strategy: Use serde for serialization. The server holds the state (the PTYs and the grid buffers); the client is just a thin wrapper that captures keyboard input and draws the server's grid to the screen.

Recommended Roadmap for a "Rust-tmux" MVP
If you want to make progress, start with this "Hello World" of multiplexing:

Phase 2: Use crossterm or ratatui to build the UI shell.

Phase 3: Integrate an ANSI parser to correctly render colors and cursors.

Phase 4: Add the socket communication so you can detach and reattach.
