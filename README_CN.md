# Rust DevTools MCP Server

[English Version / 英文版本](README.md)

一个为 Cursor 编辑器提供 Rust 开发工具的 MCP (Model Context Protocol) 服务器。

## 项目说明

本项目 fork 自 [terhechte/cursor-rust-tools](https://github.com/terhechte/cursor-rust-tools.git)，并进行了以下改进：

- **移除 GUI 功能**：专注于命令行模式，简化部署和使用
- **升级依赖**：更新到最新版本的依赖库，提升性能和稳定性
- **使用官方 MCP 实现**：替换为 MCP 官方的 `rmcp` Rust SDK，确保协议兼容性
- **配置热重载**：添加配置监视器实现无需重启的实时更新
- **多传输支持**：新增 SSE 和 Streamable HTTP 传输选项
- **增强项目管理**：改进项目跟踪和索引通知

## 功能特性

### LSP 集成
- 获取符号的悬停信息（类型、描述）
- 查找符号的所有引用
- 获取符号的实现代码
- 按名称解析符号
- 实时跟踪索引进度

### Cargo 命令
- 执行带回溯支持的 `cargo test`
- 执行带错误过滤的 `cargo check`
- 将测试输出直接流式传输到客户端

### 项目管理
- 向工作区添加/移除项目
- 列出活动项目及其索引状态
- 自动配置持久化
- 通过文件路径发现项目

## 安装

```bash
cargo install --git https://github.com/cupnfish/rust-devtools-mcp
```

## 使用

### 命令行模式

```bash
rust-devtools-mcp serve --port 4000
```

### 项目管理

```bash
# 添加项目
rust-devtools-mcp projects add /path/to/project

# 移除项目
rust-devtools-mcp projects remove /path/to/project

# 列出项目
rust-devtools-mcp projects list
```

### 配置文件

在 `~/.rust-devtools-mcp.toml` 中配置项目：

```toml
[projects]
"/path/to/project1" = { root = "/path/to/project1", ignore_crates = [] }
"/path/to/project2" = { root = "/path/to/project2", ignore_crates = ["large-crate"] }
```

`ignore_crates` 是一个可选的 crate 依赖名称列表，用于排除分析中的依赖项。

### Cursor 配置

1. 服务器启动时会打印其 MCP 配置
2. 使用提供的配置创建 `.cursor/mcp.json` 文件
3. Cursor 会自动检测并启用 MCP 服务器
4. 在 Cursor 设置的 MCP 部分检查服务器状态
5. 在聊天中选择 Agent 模式以访问开发工具

## 架构

项目采用模块化设计：

- `src/main.rs` - 主入口点，CLI 处理和服务器启动
- `src/context.rs` - 全局上下文管理，项目状态和通知
- `src/cargo_remote.rs` - Cargo 命令执行和输出解析
- `src/config_watcher.rs` - 配置文件监控和热重载
- `src/lsp/` - Rust Analyzer LSP 集成
- `src/mcp/` - 支持 SSE/HTTP 传输的 MCP 服务器实现
- `src/project.rs` - 项目抽象和 URI 处理

## 工作原理

- **LSP 功能**：为每个项目管理独立的 Rust Analyzer 实例
- **项目跟踪**：使用 DashMap 实现并发项目访问
- **配置管理**：自动保存/加载项目配置
- **通知系统**：提供索引和工具使用的实时更新
- **多传输支持**：支持 Stdio、SSE 和 Streamable HTTP 传输

## 作者

cupnfish

## 许可证

继承原项目许可证。
