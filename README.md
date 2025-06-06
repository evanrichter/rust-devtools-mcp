# Rust DevTools MCP Server

[中文版本 / Chinese Version](README_CN.md)

A MCP (Model Context Protocol) server that provides Rust development tools for Cursor editor.

## Project Description

This project is forked from [terhechte/cursor-rust-tools](https://github.com/terhechte/cursor-rust-tools.git) with the following improvements:

- **Removed GUI functionality**: Focus on command-line mode for simplified deployment and usage
- **Upgraded dependencies**: Updated to latest versions of dependency libraries for improved performance and stability
- **Official MCP implementation**: Replaced with official MCP `rmcp` Rust SDK to ensure protocol compatibility

## Features

### LSP Integration
- Get hover information for symbols (type, description)
- Find all references of a symbol
- Get implementation code of a symbol
- Find types by name and return hover information

### Documentation Generation
- Get documentation for crates or specific symbols (e.g., `tokio` or `tokio::spawn`)
- Generate and cache Rust documentation locally
- Convert HTML documentation to Markdown format

### Cargo Commands
- Execute `cargo test` and get output
- Execute `cargo check` and get output
- Other Cargo-related operations

## Installation

```bash
cargo install --git https://github.com/cupnfish/rust-devtools-mcp
```

## Usage

### Command Line Mode

```bash
rust-devtools-mcp --no-ui
```

### Configuration File

Configure projects in `~/.rust-devtools-mcp`:

```toml
[[projects]]
root = "/path/to/your/rust/project1"
ignore_crates = []

[[projects]]
root = "/path/to/your/rust/project2"
ignore_crates = ["large-crate-name"]
```

`ignore_crates` is an optional list of crate dependency names to exclude from documentation indexing for large dependencies.

### Cursor Configuration

1. Create `.cursor/mcp.json` file in your project root
2. Cursor will automatically detect and ask whether to enable the new MCP server
3. Check server status in Cursor settings under MCP section
4. Select Agent mode in chat, then you can request to use related tools

## Architecture

The project uses a modular design:

- `src/main.rs` - Main entry point, handles server startup and notifications
- `src/context.rs` - Global context management, project configuration and state
- `src/lsp/` - Rust Analyzer LSP integration
- `src/docs/` - Documentation generation and indexing
- `src/mcp/` - MCP server implementation
- `src/project.rs` - Project abstraction and management

## How It Works

- **LSP functionality**: Starts an independent Rust Analyzer instance to index the codebase
- **Documentation functionality**: Runs `cargo doc` and parses HTML documentation into local Markdown
- **Caching mechanism**: Documentation information is stored in the `.docs-cache` folder in the project root

## Author

cupnfish

## License

Inherits the license from the original project.