#!/bin/bash
# Mock heddle worker that echoes the send message back as its response.
# Used to verify context threading in orchestrator tests.
# Supports: init, send (echoes message), status, shutdown.

while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)

  case "$type" in
    init)
      echo "{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"echo-sess-001\",\"protocol_version\":\"0.1.0\"}"
      ;;
    send)
      # Extract the message field and echo it back as the response
      message=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['message'])" 2>/dev/null)
      # Escape the message for JSON output
      escaped=$(echo "$message" | python3 -c "import sys,json; print(json.dumps(sys.stdin.read().strip()))" 2>/dev/null)
      echo "{\"type\":\"event\",\"event\":{\"event\":\"usage\",\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}"
      echo "{\"type\":\"result\",\"id\":\"$id\",\"status\":\"ok\",\"response\":$escaped,\"tool_calls_made\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15},\"iterations\":1}"
      ;;
    status)
      echo "{\"type\":\"status_ok\",\"id\":\"$id\",\"model\":\"mock/echo\",\"messages_count\":2,\"session_id\":\"echo-sess-001\",\"active\":true}"
      ;;
    shutdown)
      echo "{\"type\":\"shutdown_ok\",\"id\":\"$id\"}"
      exit 0
      ;;
    *)
      echo "{\"type\":\"event\",\"event\":{\"event\":\"error\",\"error\":\"unknown request type: $type\"}}" >&2
      ;;
  esac
done
