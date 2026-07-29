We are proud to publish the first public release of AgentSeek Desktop.

AgentSeek Desktop v1.0.0 provides a desktop UI for discovering AgentSeek templates, creating isolated instances, and managing their full lifecycle — from dependency installation to runtime monitoring.

## Release Scope

This release ships the stable desktop application with full template management capabilities.

- Cross-platform desktop app for macOS (Apple Silicon + Intel), Linux, and Windows
- 11 cookiecutter template support with one-click instance creation
- Built-in Mock API Server for zero-cost, zero-secret E2E testing
- Embedded SQLite storage for desktop state (isolated from template instances)
- Comprehensive CI/CD pipeline with automated testing and release publishing

## Downloads

- **macOS (Apple Silicon)**: `.dmg` (aarch64)
- **macOS (Intel)**: `.dmg` (x86_64)
- **Linux**: `.deb` / `.AppImage`
- **Windows**: `.msi` / `.exe`

## Requirements

- **Node.js** 24 or newer
- **Python** 3.12 or newer
- **uv** (auto-installed by agentseek CLI)
- **Docker** (for Docker-based templates)

## Installation

Install the agentseek CLI:

```bash
pipx install agentseek
```

Download and install AgentSeek Desktop from the assets below for your platform.

## Quick Start

1. Launch AgentSeek Desktop
2. Browse the template catalog (11 templates available)
3. Create an instance with one click
4. Configure API credentials (or use the built-in Mock API Server)
5. Start, monitor, and manage your agent instances

## Supported Templates (11)

| Template | Protocol | Docker |
|---|---|---|
| bub/default | Bub AG-UI | No |
| deepagents/default | Bub AG-UI | No |
| deepagents/research | LangGraph | No |
| deepagents/sandbox | LangGraph | No |
| deepagents/content-builder | LangGraph | No |
| langchain/default | Bub AG-UI | Yes |
| langchain/cli-remote | LangGraph | No |
| langchain/agentic-rag | LangGraph | Yes |
| langchain/agentic-rag-hybrid | LangGraph | Yes |
| langchain/agentic-rag-openvino | LangGraph | Yes |
| langchain/markdown-messages | LangGraph | No |

## Feedback

If you encounter issues or have suggestions, please open an issue in the repository.
