#!/bin/bash
# Slow mock heddle worker for testing timeout handling.
# MOCK_DELAY: seconds to sleep before responding to init (default: 5)
# MOCK_SEND_DELAY: seconds to sleep before responding to send (default: 0)

INIT_DELAY="${MOCK_DELAY:-5}"
SEND_DELAY="${MOCK_SEND_DELAY:-0}"

while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)

  case "$type" in
    init)
      sleep "$INIT_DELAY"
      echo "{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"mock-sess-slow\",\"protocol_version\":\"0.2.0\"}"
      ;;
    send)
      sleep "$SEND_DELAY"
      echo "{\"type\":\"event\",\"event\":{\"event\":\"content_delta\",\"text\":\"Slow hello\"},\"event_seq\":0,\"send_id\":\"$id\"}"
      echo "{\"type\":\"result\",\"id\":\"$id\",\"status\":\"ok\",\"response\":\"Slow hello from mock\",\"tool_calls_made\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15},\"iterations\":1}"
      ;;
    shutdown)
      echo "{\"type\":\"shutdown_ok\",\"id\":\"$id\"}"
      exit 0
      ;;
    *)
      echo "{\"type\":\"event\",\"event\":{\"event\":\"error\",\"message\":\"unknown request type: $type\",\"code\":\"unknown_request\",\"retryable\":false},\"event_seq\":0,\"send_id\":\"\"}" >&2
      ;;
  esac
done
