"""
Mock OpenAI-compatible API server for E2E template tests.

This server provides a free, no-auth alternative to real LLM APIs.
It responds to the OpenAI chat completions, embeddings, and models
endpoints with canned data so that AgentSeek templates can be
deployed and tested end-to-end without any API key or cost.

Usage:
    python3 mock-api-server.py [--port 8899] [--host 127.0.0.1]

The server is intentionally lightweight (stdlib only, no FastAPI/uvicorn)
so it can run in any CI environment without installing dependencies.
"""

import argparse
import json
import time
import uuid
from http.server import HTTPServer, BaseHTTPRequestHandler

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

MOCK_MODEL = "mock-gpt-4o-mini"
MOCK_EMBEDDING_MODEL = "mock-text-embedding-3-small"
MOCK_REPLY = "Hello! I am a mock assistant. This is an E2E test response."
MOCK_EMBEDDING = [0.01] * 1536  # 1536-dim vector like text-embedding-3-small


class MockAPIHandler(BaseHTTPRequestHandler):
    """Handles OpenAI-compatible API requests with canned responses."""

    def log_message(self, fmt, *args):
        # Suppress default logging; uncomment for debugging.
        # print(f"[mock-api] {self.command} {self.path}")
        pass

    def _send_json(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _send_sse(self, chunks):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.end_headers()
        for chunk in chunks:
            line = f"data: {json.dumps(chunk)}\n\n"
            self.wfile.write(line.encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_GET(self):
        if self.path == "/v1/models" or self.path == "/models":
            self._send_json(200, {
                "object": "list",
                "data": [
                    {"id": MOCK_MODEL, "object": "model", "created": 0},
                    {"id": MOCK_EMBEDDING_MODEL, "object": "model", "created": 0},
                ],
            })
        elif self.path == "/health" or self.path == "/":
            self._send_json(200, {"status": "ok"})
        else:
            self._send_json(404, {"error": {"message": "not found"}})

    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(content_length) if content_length > 0 else b"{}"

        try:
            body = json.loads(raw)
        except json.JSONDecodeError:
            body = {}

        path = self.path.rstrip("/")
        stream = body.get("stream", False)

        # --- Chat completions ---
        if path in ("/v1/chat/completions", "/chat/completions"):
            if stream:
                chunks = [
                    {
                        "id": f"chatcmpl-{uuid.uuid4().hex[:8]}",
                        "object": "chat.completion.chunk",
                        "created": int(time.time()),
                        "model": body.get("model", MOCK_MODEL),
                        "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}],
                    },
                    {
                        "id": f"chatcmpl-{uuid.uuid4().hex[:8]}",
                        "object": "chat.completion.chunk",
                        "created": int(time.time()),
                        "model": body.get("model", MOCK_MODEL),
                        "choices": [{"index": 0, "delta": {"content": MOCK_REPLY}, "finish_reason": None}],
                    },
                    {
                        "id": f"chatcmpl-{uuid.uuid4().hex[:8]}",
                        "object": "chat.completion.chunk",
                        "created": int(time.time()),
                        "model": body.get("model", MOCK_MODEL),
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    },
                ]
                self._send_sse(chunks)
            else:
                self._send_json(200, {
                    "id": f"chatcmpl-{uuid.uuid4().hex[:8]}",
                    "object": "chat.completion",
                    "created": int(time.time()),
                    "model": body.get("model", MOCK_MODEL),
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": MOCK_REPLY},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 12, "total_tokens": 22},
                })

        # --- Embeddings ---
        elif path in ("/v1/embeddings", "/embeddings"):
            inputs = body.get("input", "")
            if isinstance(inputs, str):
                inputs = [inputs]
            count = len(inputs) if isinstance(inputs, list) else 1
            self._send_json(200, {
                "object": "list",
                "data": [
                    {"object": "embedding", "index": i, "embedding": MOCK_EMBEDDING}
                    for i in range(count)
                ],
                "model": body.get("model", MOCK_EMBEDDING_MODEL),
                "usage": {"prompt_tokens": 5, "total_tokens": 5},
            })

        # --- Responses API (used by some newer OpenAI SDK versions) ---
        elif path in ("/v1/responses", "/responses"):
            self._send_json(200, {
                "id": f"resp-{uuid.uuid4().hex[:8]}",
                "object": "response",
                "created": int(time.time()),
                "model": body.get("model", MOCK_MODEL),
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": MOCK_REPLY}],
                    }
                ],
                "status": "completed",
            })

        else:
            self._send_json(404, {"error": {"message": f"unknown path: {self.path}"}})


def main():
    parser = argparse.ArgumentParser(description="Mock OpenAI-compatible API server")
    parser.add_argument("--host", default="127.0.0.1", help="Bind address (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=8899, help="Listen port (default: 8899)")
    args = parser.parse_args()

    server = HTTPServer((args.host, args.port), MockAPIHandler)
    print(f"[mock-api] Listening on http://{args.host}:{args.port}")
    print(f"[mock-api] Model: {MOCK_MODEL}")
    print(f"[mock-api] Endpoints: /v1/chat/completions, /v1/embeddings, /v1/models")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[mock-api] Shutting down")
        server.shutdown()


if __name__ == "__main__":
    main()
