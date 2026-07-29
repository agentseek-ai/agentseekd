#!/usr/bin/env bash
#
# E2E conversation tests for all 11 AgentSeek templates.
#
# For each template this script:
#   1. Creates an instance via `agentseek create --no-input`
#   2. Configures .env with API keys from the environment
#   3. Installs dependencies via `agentseek task setup`
#   4. Starts services via `agentseek dev`
#   5. Waits for health checks to pass
#   6. Sends a test message and verifies a response
#   7. Stops the instance and cleans up
#
# Usage:
#   ./tests/e2e/test-templates.sh                    # test all templates
#   ./tests/e2e/test-templates.sh langchain/default  # test one template
#
# ---------------------------------------------------------------------------
# Secret strategy
# ---------------------------------------------------------------------------
#
# All templates use OpenAI-compatible APIs.  When no real API key is
# available the script automatically starts a built-in Mock API server
# (mock-api-server.py) that returns canned responses — zero cost, zero
# secrets, works in any CI environment.
#
# With real API key (optional, for testing actual LLM responses):
#   E2E_API_KEY    — An OpenAI-compatible API key
#   E2E_API_BASE   — API base URL (e.g. https://api.siliconflow.cn/v1)
#   E2E_MODEL      — Model ID (e.g. Qwen/Qwen2.5-7B-Instruct)
#
# Without API key (default, uses Mock API server — no cost, no secrets):
#   The script starts mock-api-server.py on 127.0.0.1:8899 and uses it.
#
# Optional (templates are skipped if the corresponding secret is missing):
#   TAVILY_API_KEY        — deepagents/research (Tavily search, free tier available)
#
# Auto-skipped in CI (no public alternative exists):
#   langchain/agentic-rag-openvino  — needs local OpenVINO model files
#   langchain/cli-remote            — needs an external LangGraph server URL
#
# Optional flags:
#   E2E_TIMEOUT           — Per-template timeout in seconds (default: 300)
#   E2E_WORK_DIR          — Working directory (default: /tmp/agentseek-e2e)
#   SKIP_DOCKER_TEMPLATES — Set to 1 to skip templates requiring Docker
#   SKIP_CI_ONLY          — Set to 1 to skip templates that need local
#                           hardware/external services (openvino, cli-remote)
#   MOCK_API_PORT         — Port for mock API server (default: 8899)

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

E2E_TIMEOUT="${E2E_TIMEOUT:-300}"
E2E_WORK_DIR="${E2E_WORK_DIR:-/tmp/agentseek-e2e}"
SKIP_DOCKER_TEMPLATES="${SKIP_DOCKER_TEMPLATES:-0}"
SKIP_CI_ONLY="${SKIP_CI_ONLY:-0}"
MOCK_API_PORT="${MOCK_API_PORT:-8899}"
TEST_MESSAGE="Hello, what can you help me with?"

# Track mock server PID for cleanup.
MOCK_API_PID=""

# Path to the mock API server script.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MOCK_API_SCRIPT="${SCRIPT_DIR}/mock-api-server.py"

# ---------------------------------------------------------------------------
# Mock API server management
# ---------------------------------------------------------------------------

# Start the mock API server if no real API key is provided.
start_mock_api() {
  if [[ -n "${E2E_API_KEY:-}${BUB_API_KEY:-}${OPENAI_API_KEY:-}${AGENTSEEK_API_KEY:-}" ]]; then
    return 0  # Real API key available, no need for mock.
  fi

  if [[ ! -f "$MOCK_API_SCRIPT" ]]; then
    log_fail "Mock API server not found: $MOCK_API_SCRIPT"
    exit 1
  fi

  log_info "Starting Mock API server (no real API key detected)..."
  # Kill any process using the mock API port.
  lsof -ti:"$MOCK_API_PORT" 2>/dev/null | xargs kill -9 2>/dev/null || true
  sleep 1
  python3 "$MOCK_API_SCRIPT" --port "$MOCK_API_PORT" &
  MOCK_API_PID=$!

  # Wait for the mock server to be ready.
  local elapsed=0
  while [ "$elapsed" -lt 10 ]; do
    if curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${MOCK_API_PORT}/health" 2>/dev/null | grep -q 200; then
      log_pass "  Mock API server ready (PID: $MOCK_API_PID, port: $MOCK_API_PORT)"
      return 0
    fi
    sleep 1
    elapsed=$((elapsed + 1))
  done

  log_fail "  Mock API server failed to start"
  exit 1
}

# Stop the mock API server if it was started.
stop_mock_api() {
  if [[ -n "$MOCK_API_PID" ]]; then
    log_info "Stopping Mock API server (PID: $MOCK_API_PID)..."
    kill "$MOCK_API_PID" 2>/dev/null || true
    wait "$MOCK_API_PID" 2>/dev/null || true
    MOCK_API_PID=""
  fi
}

# ---------------------------------------------------------------------------
# Unified API key resolution
#
# A single E2E_API_KEY is mapped to all template-specific key names.
# Individual keys (BUB_API_KEY, OPENAI_API_KEY, AGENTSEEK_API_KEY) take
# precedence if set, otherwise they fall back to E2E_API_KEY.
# If no real key is available, the Mock API server provides default values.
# ---------------------------------------------------------------------------

# Check whether the mock API server is running (PID is set).
_is_mock_mode() {
  [[ -n "$MOCK_API_PID" ]]
}

# Resolve an extra env var value (e.g. TAVILY_API_KEY).
# In mock mode, returns a dummy value so the template can start without real credentials.
resolve_extra_env_value() {
  local var_name="$1"
  local value="${!var_name:-}"
  if [[ -n "$value" ]]; then
    echo "$value"
    return
  fi
  if _is_mock_mode; then
    case "$var_name" in
      TAVILY_API_KEY) echo "mock-tavily-key" ;;
      *) echo "" ;;
    esac
    return
  fi
  echo ""
}

resolve_api_key() {
  local key_name="$1"
  local value=""
  case "$key_name" in
    BUB_API_KEY)       value="${BUB_API_KEY:-${E2E_API_KEY:-}}" ;;
    OPENAI_API_KEY)    value="${OPENAI_API_KEY:-${E2E_API_KEY:-}}" ;;
    AGENTSEEK_API_KEY) value="${AGENTSEEK_API_KEY:-${E2E_API_KEY:-}}" ;;
  esac
  # Fall back to mock key if no real key is available.
  if [[ -z "$value" ]] && _is_mock_mode; then
    value="mock-api-key"
  fi
  echo "$value"
}

resolve_api_base() {
  local key_name="$1"
  local value=""
  case "$key_name" in
    BUB_API_BASE)       value="${BUB_API_BASE:-${E2E_API_BASE:-}}" ;;
    OPENAI_API_BASE)    value="${OPENAI_API_BASE:-${E2E_API_BASE:-}}" ;;
    AGENTSEEK_API_BASE) value="${AGENTSEEK_API_BASE:-${E2E_API_BASE:-}}" ;;
  esac
  # Fall back to mock server URL if no real base is available.
  if [[ -z "$value" ]] && _is_mock_mode; then
    value="http://127.0.0.1:${MOCK_API_PORT}/v1"
  fi
  echo "$value"
}

resolve_model() {
  local key_name="$1"
  local value=""
  case "$key_name" in
    BUB_MODEL)          value="${BUB_MODEL:-${E2E_MODEL:-}}" ;;
    AGENTSEEK_MODEL)    value="${AGENTSEEK_MODEL:-${E2E_MODEL:-}}" ;;
    OPENAI_MODEL)       value="${OPENAI_MODEL:-${E2E_MODEL:-}}" ;;
  esac
  # Fall back to mock model if no real model is available.
  # The "openai:" prefix tells LangChain init_chat_model to use the OpenAI provider,
  # which routes to our mock server via OPENAI_API_BASE.
  if [[ -z "$value" ]] && _is_mock_mode; then
    value="openai:mock-gpt-4o-mini"
  fi
  echo "$value"
}

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Track results
declare -a PASSED=()
declare -a FAILED=()
declare -a SKIPPED=()

# ---------------------------------------------------------------------------
# Template definitions
#
# Format: "template_id|type|graph_id|needs_docker|extra_env|ci_only_skip"
#
# type:           "bub" (AG-UI gateway) or "langgraph" (LangGraph API)
# graph_id:       LangGraph graph key (empty for bub type)
# needs_docker:   1 if Docker is required, 0 otherwise
# extra_env:      Extra required env vars (comma-separated, empty if none)
# ci_only_skip:   Non-empty reason string if template should be skipped in CI
#                 (needs local hardware, external service, etc.)
# ---------------------------------------------------------------------------
#   Field format: tpl_id|tpl_type|graph_id|needs_docker|extra_env|ci_only_skip|tpl_exports
#   tpl_exports: comma-separated KEY=VALUE pairs exported when running tasks
TEMPLATES=(
  "bub/default|bub||0|||"
  "deepagents/default|bub||0|||"
  "deepagents/research|langgraph|research|0|TAVILY_API_KEY||"
  "deepagents/sandbox|langgraph|sandbox|0|DAYTONA_API_KEY||"
  "deepagents/content-builder|langgraph|content_builder|0|||"
  "langchain/default|bub||1|||"
  "langchain/cli-remote|langgraph|agent|0|||"
  "langchain/agentic-rag|langgraph|rag|1|||"
  "langchain/agentic-rag-hybrid|langgraph|hybrid-rag|1|||"
  "langchain/agentic-rag-openvino|langgraph|rag|1|||UV_EXTRA_INDEX_URL=https://download.pytorch.org/whl/cpu,UV_INDEX_STRATEGY=unsafe-best-match"
  "langchain/markdown-messages|langgraph|agent|0|||"
)

# ---------------------------------------------------------------------------
# Utility functions
# ---------------------------------------------------------------------------

log_info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
log_pass()  { echo -e "${GREEN}[PASS]${NC}  $*"; }
log_fail()  { echo -e "${RED}[FAIL]${NC}  $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
log_step()  { echo -e "${BLUE}---${NC} $*"; }

# Check if a command exists
has_command() {
  command -v "$1" >/dev/null 2>&1
}

# Wait for an HTTP endpoint to return 200 (or any 2xx) status.
# Usage: wait_for_http <url> <timeout_seconds> <description>
wait_for_http() {
  local url="$1"
  local timeout="$2"
  local desc="$3"
  local elapsed=0
  while [ "$elapsed" -lt "$timeout" ]; do
    local status
    status=$(curl -s -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || echo "000")
    if [[ "$status" =~ ^2 ]]; then
      log_info "  $desc is ready (${status}s elapsed)"
      return 0
    fi
    sleep 3
    elapsed=$((elapsed + 3))
  done
  log_warn "  $desc not ready after ${timeout}s (last status: $status)"
  return 1
}

# Read a port value from .env file.
# Usage: get_env_port <env_file> <key>
get_env_value() {
  local env_file="$1"
  local key="$2"
  grep -E "^${key}=" "$env_file" 2>/dev/null | head -1 | cut -d'=' -f2-
}

# ---------------------------------------------------------------------------
# Prerequisites check
# ---------------------------------------------------------------------------

check_prerequisites() {
  log_step "Checking prerequisites"

  local missing=0

  if ! has_command agentseek; then
    log_fail "agentseek CLI not found in PATH"
    missing=1
  fi

  if ! has_command curl; then
    log_fail "curl not found in PATH"
    missing=1
  fi

  if ! has_command python3; then
    log_fail "python3 not found in PATH (required for mock API server)"
    missing=1
  fi

  if ! has_command docker; then
    log_warn "docker not found — Docker-dependent templates will be skipped"
  fi

  # Check for real API keys. If none is available, start the Mock API server.
  local has_any_key=0

  if [[ -n "${E2E_API_KEY:-}" ]]; then
    has_any_key=1
    log_info "Unified API key found (E2E_API_KEY)"
  elif [[ -n "${BUB_API_KEY:-}" || -n "${OPENAI_API_KEY:-}" || -n "${AGENTSEEK_API_KEY:-}" ]]; then
    has_any_key=1
    log_info "Individual API keys found (BUB/OPENAI/AGENTSEEK)"
  fi

  if [[ "$has_any_key" -eq 0 ]]; then
    log_info "No real API key found — starting Mock API server instead"
    start_mock_api
  fi

  # Resolve the effective model for display.
  local model
  model=$(resolve_model BUB_MODEL)
  if [[ -n "$model" ]]; then
    log_info "Effective model: $model"
  fi

  if [[ "$missing" -eq 1 ]]; then
    exit 1
  fi

  log_pass "Prerequisites OK"
}

# ---------------------------------------------------------------------------
# Template test logic
# ---------------------------------------------------------------------------

# Configure .env for a template instance.
# Copies .env.example to .env if it does not exist, then appends API keys.
# Usage: configure_env <instance_dir> <template_type> <extra_env_vars>
configure_env() {
  local dir="$1"
  local tpl_type="$2"
  local extra_env="$3"
  local env_file="$dir/.env"
  local env_example="$dir/.env.example"

  # agentseek create does NOT generate .env — only .env.example.
  # Copy .env.example to .env so the instance can load configuration.
  if [[ ! -f "$env_file" ]]; then
    if [[ -f "$env_example" ]]; then
      cp "$env_example" "$env_file"
    else
      touch "$env_file"
    fi
  fi

  echo "" >> "$env_file"
  echo "# E2E test: API configuration" >> "$env_file"

  # Always write all three key aliases — a single E2E_API_KEY covers them all.
  # This ensures both Bub and LangChain templates can find the key they expect.
  local bub_key bub_base bub_model
  local openai_key openai_base
  local agentseek_key agentseek_base agentseek_model

  bub_key=$(resolve_api_key BUB_API_KEY)
  bub_base=$(resolve_api_base BUB_API_BASE)
  bub_model=$(resolve_model BUB_MODEL)

  openai_key=$(resolve_api_key OPENAI_API_KEY)
  openai_base=$(resolve_api_base OPENAI_API_BASE)

  agentseek_key=$(resolve_api_key AGENTSEEK_API_KEY)
  agentseek_base=$(resolve_api_base AGENTSEEK_API_BASE)
  agentseek_model=$(resolve_model AGENTSEEK_MODEL)

  [[ -n "$bub_key" ]]       && echo "BUB_API_KEY=$bub_key" >> "$env_file"
  [[ -n "$bub_base" ]]      && echo "BUB_API_BASE=$bub_base" >> "$env_file"
  [[ -n "$bub_model" ]]     && echo "BUB_MODEL=$bub_model" >> "$env_file"

  [[ -n "$openai_key" ]]    && echo "OPENAI_API_KEY=$openai_key" >> "$env_file"
  [[ -n "$openai_base" ]]   && echo "OPENAI_API_BASE=$openai_base" >> "$env_file"

  [[ -n "$agentseek_key" ]]   && echo "AGENTSEEK_API_KEY=$agentseek_key" >> "$env_file"
  [[ -n "$agentseek_base" ]]  && echo "AGENTSEEK_API_BASE=$agentseek_base" >> "$env_file"
  [[ -n "$agentseek_model" ]] && echo "AGENTSEEK_MODEL=$agentseek_model" >> "$env_file"
  # LangChain init_chat_model accepts OPENAI_MODEL as alias.
  [[ -n "$agentseek_model" ]] && echo "OPENAI_MODEL=$agentseek_model" >> "$env_file"
  [[ -n "$agentseek_key" ]]   && echo "AGENTSEEK_MODEL_PROVIDER=openai" >> "$env_file"

  # Extra env vars (e.g. TAVILY_API_KEY for deepagents/research).
  # In mock mode, resolve_extra_env_value() provides dummy values.
  if [[ -n "$extra_env" ]]; then
    IFS=',' read -ra EXTRA_VARS <<< "$extra_env"
    for var in "${EXTRA_VARS[@]}"; do
      local value
      value=$(resolve_extra_env_value "$var")
      if [[ -n "$value" ]]; then
        echo "${var}=${value}" >> "$env_file"
      fi
    done
  fi
}

# Send a test message to a Bub AG-UI gateway and verify a response.
# The AG-UI protocol requires RunAgentInput with thread_id, run_id, and messages.
# The response is an SSE stream of events (RunStarted, TextMessageContent, etc.).
# Usage: send_message_bub <gateway_url>
send_message_bub() {
  local gateway_url="$1"
  local thread_id="e2e-$(date +%s)"
  local run_id="e2e-run-$(date +%s)"

  log_info "  Sending test message to Bub gateway: $gateway_url"

  # AG-UI protocol: POST to /agent with RunAgentInput.
  # Required fields: thread_id, run_id, state, messages (with id), tools, context, forwardedProps.
  local response
  response=$(curl -s -m 60 -X POST "$gateway_url" \
    -H "Content-Type: application/json" \
    -d "{\"thread_id\": \"${thread_id}\", \"run_id\": \"${run_id}\", \"state\": {}, \"messages\": [{\"id\": \"msg-1\", \"role\": \"user\", \"content\": \"${TEST_MESSAGE}\"}], \"tools\": [], \"context\": [], \"forwardedProps\": {}}" \
    2>/dev/null || echo "")

  if [[ -z "$response" ]]; then
    log_fail "  Empty response from gateway"
    return 1
  fi

  # The response should be an SSE stream with data: lines.
  local event_count
  event_count=$(echo "$response" | grep -c "^data:" 2>/dev/null || echo "0")
  
  # Check for error responses (JSON error, not SSE stream).
  if [[ "$event_count" -eq 0 ]]; then
    log_fail "  No SSE events in response (gateway may have rejected the message)"
    log_info "  Response: $(echo "$response" | head -3)"
    return 1
  fi
  
  # Check for error events in the SSE stream.
  if echo "$response" | grep -qi 'RUN_ERROR\|"type":"error"'; then
    log_fail "  Response contains error event (agent has API issues)"
    log_info "  Response: $(echo "$response" | head -5)"
    return 1
  fi
  
  log_info "  Response received ($event_count events, ${#response} bytes)"

  # Extract and display conversation content from SSE stream.
  local reply
  reply=$(echo "$response" | python3 -c "
import sys, json
deltas = []        # Bub TEXT_MESSAGE_CONTENT, LangGraph messages/content
last_msgs = []    # LangGraph messages/partial (keep only last to avoid duplication)
for line in sys.stdin:
    line = line.strip()
    if not line.startswith('data: '):
        continue
    try:
        data = json.loads(line[6:])
    except:
        continue
    # Bub AG-UI: TEXT_MESSAGE_CONTENT with 'delta' (streaming chunk)
    if isinstance(data.get('delta'), str):
        deltas.append(data['delta'])
    # LangGraph: messages/partial with 'messages' array (full state, keep last)
    elif isinstance(data.get('messages'), list):
        last_msgs = []
        for msg in data['messages']:
            if isinstance(msg, dict) and isinstance(msg.get('content'), str):
                last_msgs.append(msg['content'])
    # LangGraph: messages/content with 'content' (streaming chunk)
    elif isinstance(data.get('content'), str):
        deltas.append(data['content'])
# Prefer streaming deltas, fall back to last messages/partial snapshot
if deltas:
    print(''.join(deltas))
elif last_msgs:
    print(''.join(last_msgs))
else:
    print('(no text content extracted)')
" 2>/dev/null || echo "(failed to parse response)")
  log_info "  Assistant reply: ${reply}"
  if [[ "$reply" == "(no text content extracted)" || "$reply" == "(failed to parse response)" ]]; then
    log_fail "  No text content in assistant reply"
    return 1
  fi
  return 0
}

# Send a test message to a LangGraph API and verify a response.
# Usage: send_message_langgraph <base_url> <graph_id>
send_message_langgraph() {
  local base_url="$1"
  local graph_id="$2"

  log_info "  Sending test message to LangGraph API: $base_url (graph: $graph_id)"

  # Step 1: Create a thread.
  local thread_response
  thread_response=$(curl -s -m 10 -X POST "${base_url}/threads" \
    -H "Content-Type: application/json" \
    -d '{"metadata": {}}' 2>/dev/null || echo "")

  local thread_id
  thread_id=$(echo "$thread_response" | python3 -c "import sys,json; print(json.load(sys.stdin).get('thread_id',''))" 2>/dev/null || echo "")

  if [[ -z "$thread_id" ]]; then
    log_fail "  Failed to create thread"
    return 1
  fi

  log_info "  Thread created: $thread_id"

  # Step 2: Send a message via runs/stream (SSE stream).
  local run_response
  run_response=$(curl -s -m 60 -X POST "${base_url}/threads/${thread_id}/runs/stream" \
    -H "Content-Type: application/json" \
    -d "{\"assistant_id\": \"${graph_id}\", \"input\": {\"messages\": [{\"role\": \"user\", \"content\": \"${TEST_MESSAGE}\"}]}}" \
    2>/dev/null || echo "")

  if [[ -z "$run_response" ]]; then
    log_fail "  Empty response from LangGraph run"
    return 1
  fi

  # Check that we got some SSE events (data: lines).
  local event_count
  event_count=$(echo "$run_response" | grep -c "^event:\|^data:" 2>/dev/null || echo "0")

  if [[ "$event_count" -lt 1 ]]; then
    log_fail "  No SSE events in response"
    log_info "  Response: $(echo "$run_response" | head -5)"
    return 1
  fi

  log_info "  Response received ($event_count SSE events, ${#run_response} bytes)"

  # Extract and display conversation content from SSE stream.
  local reply
  reply=$(echo "$run_response" | python3 -c "
import sys, json
deltas = []        # Bub TEXT_MESSAGE_CONTENT, LangGraph messages/content
last_msgs = []    # LangGraph messages/partial (keep only last to avoid duplication)
for line in sys.stdin:
    line = line.strip()
    if not line.startswith('data: '):
        continue
    try:
        data = json.loads(line[6:])
    except:
        continue
    # Bub AG-UI: TEXT_MESSAGE_CONTENT with 'delta' (streaming chunk)
    if isinstance(data.get('delta'), str):
        deltas.append(data['delta'])
    # LangGraph: messages/partial with 'messages' array (full state, keep last)
    elif isinstance(data.get('messages'), list):
        last_msgs = []
        for msg in data['messages']:
            if isinstance(msg, dict) and isinstance(msg.get('content'), str):
                last_msgs.append(msg['content'])
    # LangGraph: messages/content with 'content' (streaming chunk)
    elif isinstance(data.get('content'), str):
        deltas.append(data['content'])
# Prefer streaming deltas, fall back to last messages/partial snapshot
if deltas:
    print(''.join(deltas))
elif last_msgs:
    print(''.join(last_msgs))
else:
    print('(no text content extracted)')
" 2>/dev/null || echo "(failed to parse response)")
  log_info "  Assistant reply: ${reply}"
  if [[ "$reply" == "(no text content extracted)" || "$reply" == "(failed to parse response)" ]]; then
    log_fail "  No text content in assistant reply"
    return 1
  fi
  return 0
}

# Kill a process and all its children by PID.
# Uses pkill -P to recursively kill child processes.
kill_tree() {
  local pid="$1"
  if [[ -z "$pid" ]]; then return; fi
  # Kill children first.
  local children
  children=$(pgrep -P "$pid" 2>/dev/null || true)
  for child in $children; do
    kill_tree "$child"
  done
  kill -9 "$pid" 2>/dev/null || true
}

# Clean up instance resources: kill dev process tree and remove directory.
# Usage: cleanup_instance <dev_pid> <instance_dir>
cleanup_instance() {
  local dev_pid="$1"
  local instance_dir="$2"

  log_info "  Stopping services..."
  kill_tree "$dev_pid"
  wait "$dev_pid" 2>/dev/null || true

  # Kill any lingering processes from the instance directory.
  if [[ -n "$instance_dir" && -d "$instance_dir" ]]; then
    local pids
    pids=$(lsof -t "$instance_dir" 2>/dev/null || true)
    if [[ -n "$pids" ]]; then
      echo "$pids" | xargs kill -9 2>/dev/null || true
    fi
    rm -rf "$instance_dir" 2>/dev/null || true
  fi
}

# Run E2E test for a single template.
# Usage: test_template "template_id|type|graph_id|needs_docker|extra_env|ci_only_skip"
test_template() {
  local entry="$1"
  IFS='|' read -r tpl_id tpl_type graph_id needs_docker extra_env ci_only_skip tpl_exports <<< "$entry"

  local instance_name="e2e-$(echo "$tpl_id" | tr '/' '-')"
  local instance_dir="${E2E_WORK_DIR}/${instance_name}"

  log_step "Testing template: $tpl_id"

  # Skip templates that need local hardware or external services.
  if [[ -n "$ci_only_skip" ]]; then
    if [[ "$SKIP_CI_ONLY" == "1" ]]; then
      log_warn "  Skipping (SKIP_CI_ONLY=1: $ci_only_skip)"
      SKIPPED+=("$tpl_id ($ci_only_skip)")
      return 0
    fi
  fi

  # Skip Docker templates if Docker is not available or SKIP_DOCKER_TEMPLATES=1.
  if [[ "$needs_docker" == "1" ]]; then
    if [[ "$SKIP_DOCKER_TEMPLATES" == "1" ]]; then
      log_warn "  Skipping (SKIP_DOCKER_TEMPLATES=1)"
      SKIPPED+=("$tpl_id (docker skipped)")
      return 0
    fi
    if ! has_command docker; then
      log_warn "  Skipping (docker not available)"
      SKIPPED+=("$tpl_id (no docker)")
      return 0
    fi
    if ! docker info >/dev/null 2>&1; then
      log_warn "  Skipping (docker daemon not running)"
      SKIPPED+=("$tpl_id (docker not running)")
      return 0
    fi
  fi

  # Check extra env vars (e.g. TAVILY_API_KEY).
  # In mock mode, resolve_extra_env_value() provides dummy values.
  if [[ -n "$extra_env" ]]; then
    IFS=',' read -ra EXTRA_VARS <<< "$extra_env"
    for var in "${EXTRA_VARS[@]}"; do
      local resolved
      resolved=$(resolve_extra_env_value "$var")
      if [[ -z "$resolved" ]]; then
        log_warn "  Skipping (missing optional env var: $var)"
        SKIPPED+=("$tpl_id (missing $var)")
        return 0
      fi
    done
  fi

  # Clean up any previous instance.
  rm -rf "$instance_dir"

  # Step 1: Create instance.
  log_info "  Creating instance..."
  # Record directories before creation to detect the new one.
  local before_dirs
  before_dirs=$(ls -d "$E2E_WORK_DIR"/*/ 2>/dev/null || true)
  if ! agentseek create "$tpl_id" --no-input --output-dir "$E2E_WORK_DIR" 2>&1 | tail -5; then
    log_fail "  Failed to create instance"
    FAILED+=("$tpl_id (create failed)")
    return 1
  fi

  # Find the newly created directory by comparing before/after.
  local created_dir=""
  while IFS= read -r d; do
    if [[ -f "${d}.agentseek/lifecycle.toml" ]]; then
      # Check if this directory existed before.
      if ! echo "$before_dirs" | grep -qF "$d"; then
        created_dir="${d%/}"
        break
      fi
    fi
  done < <(ls -d "$E2E_WORK_DIR"/*/ 2>/dev/null)

  # Fallback: find any directory with lifecycle.toml.
  if [[ -z "$created_dir" ]]; then
    for d in "$E2E_WORK_DIR"/*/; do
      if [[ -f "${d}.agentseek/lifecycle.toml" ]]; then
        created_dir="${d%/}"
        break
      fi
    done
  fi

  if [[ -z "$created_dir" ]]; then
    log_fail "  Could not find created instance directory"
    FAILED+=("$tpl_id (no instance dir)")
    return 1
  fi

  # Rename to our expected name.
  if [[ "$created_dir" != "$instance_dir" ]]; then
    mv "$created_dir" "$instance_dir"
  fi

  # Step 2: Configure .env.
  log_info "  Configuring .env..."
  configure_env "$instance_dir" "$tpl_type" "$extra_env"

  # Step 3: Install dependencies by running all lifecycle tasks.
  # `agentseek task --list` shows available tasks; each task is run by name.
  # Critical tasks (sync, frontend, models, seekdb) must succeed.
  # Non-critical tasks (ingest-sample, seekdb-skills) only produce a warning.
  local NON_CRITICAL_TASKS="ingest-sample|seekdb-skills"
  log_info "  Installing dependencies..."
  local setup_log="${instance_dir}/.e2e-setup.log"
  local task_list
  task_list=$(cd "$instance_dir" && agentseek task --list 2>&1 | awk '{print $1}' | grep -v '^$')
  # Export per-template environment variables (e.g. CPU-only PyTorch for openvino).
  local _export_cmd=""
  if [[ -n "$tpl_exports" ]]; then
    IFS=',' read -ra _EXPORTS <<< "$tpl_exports"
    for _exp in "${_EXPORTS[@]}"; do
      local _key="${_exp%%=*}"
      local _val="${_exp#*=}"
      export "$_key=$_val"
      log_info "  Exported: $_key"
    done
  fi
  if [[ -n "$task_list" ]]; then
    while IFS= read -r task_name; do
      log_info "    Running task: $task_name"
      if ! (cd "$instance_dir" && agentseek task "$task_name" >> "$setup_log" 2>&1); then
        if echo "$NON_CRITICAL_TASKS" | grep -qw "$task_name"; then
          log_warn "  Task '$task_name' failed (non-critical, continuing)"
        else
          log_fail "  Task '$task_name' failed (see $setup_log)"
          tail -10 "$setup_log" 2>/dev/null
          FAILED+=("$tpl_id (task $task_name failed)")
          rm -rf "$instance_dir"
          return 1
        fi
      fi
    done <<< "$task_list"
  fi
  log_info "  Setup completed"

  # Step 4: Parse service URLs from dry-run output.
  # `agentseek dev --dry-run` prints the startup plan with actual URLs.
  log_info "  Resolving service URLs..."
  local dry_run_output
  dry_run_output=$(cd "$instance_dir" && agentseek dev --dry-run 2>&1)

  # Extract URLs from dry-run output by service label.
  # Bub templates: "  Gateway: http://127.0.0.1:PORT/agent"
  # LangGraph templates: "  Backend: http://127.0.0.1:PORT" or "  Langgraph: http://127.0.0.1:PORT"
  local gateway_url=""
  local backend_url=""
  gateway_url=$(echo "$dry_run_output" | grep -iE '^\s*(gateway):' | grep -oE 'https?://127\.0\.0\.1:[0-9]+/agent' | head -1)
  backend_url=$(echo "$dry_run_output" | grep -iE '^\s*(backend|langgraph):' | grep -oE 'https?://127\.0\.0\.1:[0-9]+' | head -1)

  # Step 5: Start services.
  log_info "  Starting services (agentseek dev)..."
  local dev_log="${instance_dir}/.e2e-dev.log"
  # Export API credentials as real environment variables (not just .env).
  # Some templates (e.g. langchain/cli-remote) use pydantic-settings which reads
  # .env into model fields but does NOT export to os.environ. The OpenAI SDK
  # reads from os.environ, so we must set it here for the child process.
  local _openai_key _openai_base
  _openai_key=$(resolve_api_key OPENAI_API_KEY)
  _openai_base=$(resolve_api_base OPENAI_API_BASE)
  (cd "$instance_dir" && \
    OPENAI_API_KEY="$_openai_key" \
    OPENAI_API_BASE="$_openai_base" \
    agentseek dev > "$dev_log" 2>&1) &
  local dev_pid=$!
  log_info "  dev PID: $dev_pid"

  # Step 6: Wait for health checks.
  local health_urls=()
  if [[ "$tpl_type" == "bub" ]]; then
    if [[ -n "$gateway_url" ]]; then
      health_urls+=("${gateway_url}/health|gateway")
    fi
  elif [[ "$tpl_type" == "langgraph" ]]; then
    if [[ -n "$backend_url" ]]; then
      health_urls+=("${backend_url}|langgraph")
    fi
  fi

  if [[ ${#health_urls[@]} -eq 0 ]]; then
    log_fail "  Could not determine health check URL from dry-run output"
    log_info "  Dry-run output: $dry_run_output"
    cleanup_instance "$dev_pid" "$instance_dir"
    FAILED+=("$tpl_id (no health URL)")
    return 1
  fi

  # Wait for all health checks.
  local all_healthy=true
  for entry in "${health_urls[@]}"; do
    IFS='|' read -r url desc <<< "$entry"
    if ! wait_for_http "$url" "$E2E_TIMEOUT" "$desc"; then
      all_healthy=false
    fi
  done

  if [[ "$all_healthy" != "true" ]]; then
    log_fail "  Health checks failed"
    tail -20 "$dev_log" 2>/dev/null
    cleanup_instance "$dev_pid" "$instance_dir"
    FAILED+=("$tpl_id (health check failed)")
    return 1
  fi

  # Step 7: Send test message.
  local message_ok=false
  if [[ "$tpl_type" == "bub" ]]; then
    if [[ -n "$gateway_url" ]] && send_message_bub "$gateway_url"; then
      message_ok=true
    fi
  elif [[ "$tpl_type" == "langgraph" ]]; then
    if [[ -n "$backend_url" ]] && send_message_langgraph "$backend_url" "$graph_id"; then
      message_ok=true
    fi
  fi

  # Step 8: Clean up.
  cleanup_instance "$dev_pid" "$instance_dir"

  # Report result.
  if [[ "$message_ok" == "true" ]]; then
    log_pass "  $tpl_id — conversation test passed"
    PASSED+=("$tpl_id")
  else
    log_fail "  $tpl_id — conversation test failed"
    FAILED+=("$tpl_id (no response)")
  fi

  # Clean up per-template exports so they don't leak to the next template.
  if [[ -n "$tpl_exports" ]]; then
    IFS=',' read -ra _EXPORTS <<< "$tpl_exports"
    for _exp in "${_EXPORTS[@]}"; do
      unset "${_exp%%=*}"
    done
  fi
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

main() {
  echo "========================================"
  echo " AgentSeek E2E Template Tests"
  echo "========================================"
  echo ""

  check_prerequisites
  echo ""

  mkdir -p "$E2E_WORK_DIR"

  # If arguments are provided, only test those templates.
  local templates_to_test=()
  if [[ $# -gt 0 ]]; then
    for arg in "$@"; do
      for entry in "${TEMPLATES[@]}"; do
        if [[ "$entry" == "${arg}|"* ]]; then
          templates_to_test+=("$entry")
        fi
      done
    done
    if [[ ${#templates_to_test[@]} -eq 0 ]]; then
      log_fail "No matching templates found for: $*"
      exit 1
    fi
  else
    templates_to_test=("${TEMPLATES[@]}")
  fi

  local total=${#templates_to_test[@]}
  local current=0

  for entry in "${templates_to_test[@]}"; do
    current=$((current + 1))
    echo ""
    echo "[$current/$total]"
    # Continue to next template even if one fails.
    set +e
    test_template "$entry"
    set -e
  done

  # Summary
  echo ""
  echo "========================================"
  echo " Summary"
  echo "========================================"
  echo -e "${GREEN}Passed:${NC}   ${#PASSED[@]}"
  if [[ ${#PASSED[@]} -gt 0 ]]; then for t in "${PASSED[@]}"; do echo "  - $t"; done; fi
  echo -e "${RED}Failed:${NC}   ${#FAILED[@]}"
  if [[ ${#FAILED[@]} -gt 0 ]]; then for t in "${FAILED[@]}"; do echo "  - $t"; done; fi
  echo -e "${YELLOW}Skipped:${NC} ${#SKIPPED[@]}"
  if [[ ${#SKIPPED[@]} -gt 0 ]]; then for t in "${SKIPPED[@]}"; do echo "  - $t"; done; fi
  echo ""

  if [[ ${#FAILED[@]} -gt 0 ]]; then
    stop_mock_api
    exit 1
  fi
  stop_mock_api
  exit 0
}

main "$@"
