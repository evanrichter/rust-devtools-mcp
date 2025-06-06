# Rust DevTools MCP Server

[中文版本 / Chinese Version](README_CN.md)

A MCP (Model Context Protocol) server that provides Rust development tools for Cursor editor.

## Project Description

This project is forked from [terhechte/cursor-rust-tools](https://github.com/terhechte/cursor-rust-tools.git) with the following improvements:

- **Removed GUI functionality**: Focus on command-line mode for simplified deployment and usage
- **Upgraded dependencies**: Updated to latest versions of dependency libraries for improved performance and stability
- **Official MCP implementation**: Replaced with official MCP `rmcp` Rust SDK to ensure protocol compatibility
- **Config hot-reloading**: Added configuration watcher for live updates without restart
- **Multi-transport support**: Added SSE and Streamable HTTP transport options
- **Enhanced project management**: Improved project tracking and indexing notifications

## Features

### LSP Integration
- Get hover information for symbols (type, description)
- Find all references of a symbol
- Get implementation code of a symbol
- Resolve symbols by name
- Real-time indexing progress tracking

### Cargo Commands
- Execute `cargo test` with backtrace support
- Execute `cargo check` with error filtering
- Stream test output directly to client

### Project Management
- Add/remove projects from workspace
- List active projects and their indexing status
- Automatic configuration persistence
- Project discovery by file path

## Installation

```bash
cargo install --git https://github.com/cupnfish/rust-devtools-mcp
```

## Usage

### Command Line Mode

```bash
rust-devtools-mcp serve --port 4000
```

### Project Management

```bash
# Add project
rust-devtools-mcp projects add /path/to/project

# Remove project
rust-devtools-mcp projects remove /path/to/project

# List projects
rust-devtools-mcp projects list
```

### Configuration File

Configure projects in `~/.rust-devtools-mcp.toml`:

```toml
[projects]
"/path/to/project1" = { root = "/path/to/project1", ignore_crates = [] }
"/path/to/project2" = { root = "/path/to/project2", ignore_crates = ["large-crate"] }
```

`ignore_crates` is an optional list of crate dependency names to exclude from analysis.

### Cursor Configuration

1. The server will print its MCP configuration when started
2. Create `.cursor/mcp.json` file using the provided configuration
3. Cursor will automatically detect and enable the MCP server
4. Check server status in Cursor settings under MCP section
5. Select Agent mode in chat to access development tools

## Architecture

The project uses a modular design:

- `src/main.rs` - Main entry point, CLI handling and server startup
- `src/context.rs` - Global context management, project state, and notifications
- `src/cargo_remote.rs` - Cargo command execution and output parsing
- `src/config_watcher.rs` - Config file monitoring and hot reloading
- `src/lsp/` - Rust Analyzer LSP integration
- `src/mcp/` - MCP server implementation with SSE/HTTP transports
- `src/project.rs` - Project abstraction and URI handling

## How It Works

- **LSP functionality**: Manages independent Rust Analyzer instances per project
- **Project tracking**: Uses DashMap for concurrent project access
- **Config management**: Automatically saves/loads project configuration
- **Notification system**: Provides real-time updates on indexing and tool usage
- **Multi-transport**: Supports Stdio, SSE, and Streamable HTTP transports

## Author

cupnfish

## License

Inherits the license from the original project.
