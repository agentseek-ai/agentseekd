# AgentSeek Desktop

AgentSeek Desktop is the desktop UI for discovering AgentSeek templates, creating isolated instances, managing `.env` files, running lifecycle commands, and inspecting logs.

## Architecture

- `src/`: React and TypeScript desktop UI.
- `src/api.ts`: Tauri command client with a browser-only preview adapter.
- `src-tauri/src/lib.rs`: AgentSeek CLI runner, instance store, Env Center, process lifecycle, and log persistence.
- AgentSeek CLI remains the source of truth for template discovery and lifecycle execution.

## Requirements

- Node.js 20.19 or newer.
- Rust stable and the Tauri system prerequisites.
- `uv` or an `agentseek` executable on `PATH`.
- Docker and Docker Compose V2 only when Docker deployment is selected.

## Development

```bash
cd desktop/tauri
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

