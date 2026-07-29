# AgentSeek Desktop

AgentSeek Desktop is the desktop UI for discovering AgentSeek templates, creating isolated instances, managing `.env` files, running lifecycle commands, and inspecting logs.

## Requirements

- Node.js 24 or newer.
- Rust stable and the Tauri system prerequisites.
- Python 3.12+ and `uv` for agentseek CLI installation.
- `agentseek` executable on `PATH` (or installed via `pipx install agentseek`).
- Docker and Docker Compose V2 only when Docker deployment is selected.

## Development

```bash
git clone https://github.com/agentseek-ai/agentseekd.git
cd agentseekd
npm install
npm run tauri dev
```

Use the browser preview when working only on the UI:

```bash
npm run dev
```

The preview uses local browser storage and simulated AgentSeek operations. Packaged Tauri builds use real CLI and filesystem commands.

## Build

```bash
npm run build
cargo check --manifest-path src-tauri/Cargo.toml
npm run tauri build
```

During repository development, the backend invokes the local checkout with `uv run --project <repo> agentseek`. Installed builds prefer an `agentseek` executable and otherwise fall back to `uvx agentseek`.

Set `AGENTSEEK_CLI` to an executable path to override this resolution.

## Testing

### Frontend Tests

```bash
npm run test          # run once
npm run test:watch    # watch mode
npm run test:coverage # with coverage report
```

Covers the Tauri API client, i18n helpers, and shared types.

### Rust Tests

```bash
cd src-tauri
cargo test
```

124 tests covering CLI resolution, env bridge, instance management, lifecycle patches (apt mirror, PyPI mirror, GitHub mirror, CORS, async shim, OpenVINO model conversion), and template rendering.

Template-based tests require cached cookiecutter templates at `~/.cookiecutters/agentseek/templates/`. If absent, these tests are skipped gracefully.

### E2E Template Tests

End-to-end tests that create a real agentseek instance for each template, configure `.env`, install dependencies, start services, and verify conversation via the API.

```bash
# Run all E2E tests (auto-starts Mock API server if no API key)
npm run test:e2e

# Skip Docker-dependent templates
npm run test:e2e:skip-docker

# CI mode (skip templates needing local hardware)
npm run test:e2e:ci
```

The built-in Mock API server (`tests/e2e/mock-api-server.py`) provides an OpenAI-compatible API with zero dependencies and zero cost. When no `E2E_API_KEY` is set, the script automatically starts the mock server.

To test with a real LLM, set these environment variables:

```bash
export E2E_API_KEY="sk-..."
export E2E_API_BASE="https://api.openai.com/v1"
export E2E_MODEL="openai:gpt-4o-mini"
...
npm run test:e2e
```

### Supported Templates

| Template | Protocol | Docker | Notes |
|---|---|---|---|
| `bub/default` | Bub AG-UI | No | |
| `deepagents/default` | Bub AG-UI | No | |
| `deepagents/research` | LangGraph | No | Needs `TAVILY_API_KEY` (mock provided) |
| `deepagents/sandbox` | LangGraph | No | Needs `DAYTONA_API_KEY` (optional) |
| `deepagents/content-builder` | LangGraph | No | |
| `langchain/default` | Bub AG-UI | Yes | Phoenix + SeekDB |
| `langchain/cli-remote` | LangGraph | No | |
| `langchain/agentic-rag` | LangGraph | Yes | |
| `langchain/agentic-rag-hybrid` | LangGraph | Yes | |
| `langchain/agentic-rag-openvino` | LangGraph | Yes | Downloads models via `task models` |
| `langchain/markdown-messages` | LangGraph | No | |
