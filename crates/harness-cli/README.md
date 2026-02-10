# Rumble Harness CLI

A command-line daemon for automated GUI testing of the Rumble voice chat application. Designed for AI agents to iteratively develop and test UI changes.

## Architecture

```
┌─────────────────────────────────────────────┐
│           rumble-harness daemon             │
│  ┌───────────────────────────────────────┐  │
│  │ Unix Socket (/run/user/*/rumble-*.sock│  │
│  └───────────────────────────────────────┘  │
│                                             │
│  ┌─────────────────┐  ┌─────────────────┐   │
│  │ Server Process  │  │ Client Instances│   │
│  │ (optional)      │  │ [1] [2] [3]...  │   │
│  └─────────────────┘  └─────────────────┘   │
└─────────────────────────────────────────────┘
           ▲
           │ JSON commands over Unix socket
           ▼
┌─────────────────────────────────────────────┐
│         rumble-harness CLI                  │
│  rumble-harness client screenshot 1         │
└─────────────────────────────────────────────┘
```

## Quick Start

```bash
# Build
cargo build -p harness-cli

# One command to start everything (daemon + server + client)
rumble-harness up

# Take a screenshot
rumble-harness client screenshot 1 -o ui.png

# After making code changes, rebuild and screenshot in one command
rumble-harness iterate

# Clean teardown
rumble-harness down
```

## Quick Start (Manual)

For more control, you can start components individually:

```bash
# Start daemon (foreground for debugging)
rumble-harness daemon start

# Or in background
rumble-harness daemon start --background

# Create a client
rumble-harness client new --name test-bot

# Take a screenshot
rumble-harness client screenshot 1 --output ui.png

# Stop daemon
rumble-harness daemon stop
```

## Commands

### One-Shot Commands (Recommended for Agents)

```bash
rumble-harness up [--port 5000] [--name agent] [--screenshot ui.png]
# Start everything: daemon, server, client. Optionally take initial screenshot.

rumble-harness iterate [-c 1] [-o /tmp/ui.png] [--name agent] [--server 127.0.0.1:5000]
# Close client, rebuild egui-test, create new client, take screenshot.
# This is the core agent iteration loop command.

rumble-harness down
# Clean teardown: close all clients, stop server, stop daemon.

rumble-harness status
# Show what's running: daemon status, server status, list of clients.
```

### Daemon Management

```bash
rumble-harness daemon start [--background]  # Start the daemon
rumble-harness daemon stop                   # Stop the daemon
rumble-harness daemon status                 # Check if daemon is running
```

### Server Management

```bash
rumble-harness server start [--port 5000]    # Start Rumble server with self-signed cert
rumble-harness server stop                   # Stop the server
```

### Client Management

```bash
rumble-harness client new [--name NAME] [--server ADDR]  # Create client, returns ID
rumble-harness client list                               # List all clients
rumble-harness client close <ID>                         # Close a client
```

### Screenshots & Interaction

```bash
rumble-harness client screenshot <ID> [--output FILE]  # Take PNG screenshot
rumble-harness client click <ID> <X> <Y>               # Click at position
rumble-harness client mouse-move <ID> <X> <Y>          # Move mouse
rumble-harness client key-tap <ID> <KEY>               # Press and release key
rumble-harness client key-press <ID> <KEY>             # Press key (hold)
rumble-harness client key-release <ID> <KEY>           # Release key
rumble-harness client type <ID> "text"                 # Type text
rumble-harness client frames <ID> [COUNT]              # Run N frames (default: 1)
```

Supported keys: `space`, `enter`, `escape`, `tab`, `backspace`, `delete`, `left`, `right`, `up`, `down`, `a-z`, `0-9`, `f1-f12`

### State Inspection

```bash
rumble-harness client connected <ID>   # Check if connected (returns true/false)
rumble-harness client state <ID>       # Get backend state as JSON
```

### Widget Queries (AccessKit-based)

Find and interact with widgets by their accessible label text:

```bash
rumble-harness client has-widget <ID> "Connect"     # Check if widget exists
rumble-harness client widget-rect <ID> "Connect"    # Get widget bounding box
rumble-harness client click-widget <ID> "Connect"   # Click widget by label
rumble-harness client run <ID>                      # Run until UI settles
```

**Note**: These queries use AccessKit (accessibility) metadata. The room/user tree view does not expose AccessKit labels, so tree nodes cannot be found by label.

## Agent Iteration Workflow

### Simplified (Recommended)

```bash
# 1. Setup (one command)
rumble-harness up --screenshot /tmp/ui.png

# 2. Agent reviews screenshot at /tmp/ui.png, edits code...

# 3. Rebuild and screenshot (one command)
rumble-harness iterate -o /tmp/ui.png

# 4. Repeat steps 2-3 until done

# 5. Cleanup (one command)
rumble-harness down
```

### Manual (More Control)

```bash
# 1. Setup
rumble-harness daemon start --background
rumble-harness server start
rumble-harness client new --name agent --server 127.0.0.1:5000

# 2. Test current UI
rumble-harness client run 1                 # Wait for UI to stabilize
rumble-harness client screenshot 1 -o ui.png

# 3. Agent reviews screenshot, edits code...

# 4. Rebuild and restart client
rumble-harness client close 1
cargo build -p egui-test
rumble-harness client new --name agent --server 127.0.0.1:5000

# 5. Test again
rumble-harness client screenshot 1 -o ui-v2.png

# 6. Cleanup
rumble-harness daemon stop
```

## Protocol

The daemon communicates via JSON messages over a Unix socket (default: `$XDG_RUNTIME_DIR/rumble-harness.sock`).

### Command Format

```json
{"type": "client_new", "name": "test", "server": "127.0.0.1:5000"}
{"type": "screenshot", "id": 1, "output": "/tmp/screenshot.png"}
{"type": "click", "id": 1, "x": 100.0, "y": 200.0}
{"type": "run_frames", "id": 1, "count": 10}
```

### Response Format

```json
{"status": "ok", "data": {"type": "client_created", "id": 1}}
{"status": "ok", "data": {"type": "screenshot", "path": "/tmp/...", "width": 1280, "height": 720}}
{"status": "error", "message": "Client 1 not found"}
```

## Limitations

1. **GPU required**: The headless renderer uses wgpu for GPU-accelerated rendering. A GPU (or software rasterizer) must be available. The renderer prefers software rasterizers for consistency.

2. **Single-threaded clients**: Each client has its own tokio runtime but frames are processed synchronously.

3. **Tree view not queryable by label**: The room/user tree view (egui_ltreeview) does not currently expose AccessKit accessibility metadata. This means `get-by-label` queries won't find tree nodes. Use coordinate-based clicking or backend state inspection for tree interaction. Future work: add AccessKit support to egui_ltreeview.

## Custom Socket Path

```bash
rumble-harness --socket /tmp/my-harness.sock daemon start
rumble-harness --socket /tmp/my-harness.sock client list
```
