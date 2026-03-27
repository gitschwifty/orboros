#!/bin/bash
# Mock heddle worker that supports cancel during send.
# When it receives a send, it waits for a cancel before responding.

while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)

  case "$type" in
    init)
      echo "{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"cancel-sess-001\",\"protocol_version\":\"0.2.0\"}"
      ;;
    send)
      SEND_ID="$id"
      # Don't respond yet — wait for cancel
      ;;
    cancel)
      # Respond to the original send with cancelled status
      echo "{\"type\":\"result\",\"id\":\"$SEND_ID\",\"status\":\"cancelled\",\"response\":null,\"tool_calls_made\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":0,\"total_tokens\":5},\"iterations\":0,\"error\":{\"code\":\"cancelled\",\"message\":\"Task was cancelled\",\"retryable\":false}}"
      ;;
    shutdown)
      echo "{\"type\":\"shutdown_ok\",\"id\":\"$id\"}"
      exit 0
      ;;
  esac
done
