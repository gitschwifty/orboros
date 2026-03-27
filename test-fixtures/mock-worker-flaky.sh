#!/bin/bash
# Flaky mock heddle worker: fails first init, succeeds on retry.
# Uses a state file to track attempts. MOCK_STATE_FILE must be set.

STATE_FILE="${MOCK_STATE_FILE:?MOCK_STATE_FILE must be set}"

while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)

  case "$type" in
    init)
      if [ ! -f "$STATE_FILE" ]; then
        # First attempt: create state file and crash
        touch "$STATE_FILE"
        exit 1
      fi
      # Second attempt: succeed
      echo "{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"flaky-sess-001\",\"protocol_version\":\"0.2.0\"}"
      ;;
    send)
      echo "{\"type\":\"event\",\"event\":{\"event\":\"content_delta\",\"text\":\"Recovered\"},\"event_seq\":0,\"send_id\":\"$id\"}"
      echo "{\"type\":\"result\",\"id\":\"$id\",\"status\":\"ok\",\"response\":\"Recovered after retry\",\"tool_calls_made\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15},\"iterations\":1}"
      ;;
    shutdown)
      echo "{\"type\":\"shutdown_ok\",\"id\":\"$id\"}"
      exit 0
      ;;
  esac
done
