#!/bin/bash
# Mock heddle worker that reports confidence in its IPC result.
# Used by tests verifying confidence wire-through from worker → orb.

while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)

  case "$type" in
    init)
      echo "{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"mock-conf-sess\",\"protocol_version\":\"0.3.0\"}"
      ;;
    send)
      echo "{\"type\":\"result\",\"id\":\"$id\",\"status\":\"ok\",\"response\":\"answer\",\"tool_calls_made\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"iterations\":1,\"confidence\":0.73}"
      ;;
    status)
      echo "{\"type\":\"status_ok\",\"id\":\"$id\",\"model\":\"mock/test\",\"messages_count\":2,\"session_id\":\"mock-conf-sess\",\"active\":true}"
      ;;
    shutdown)
      echo "{\"type\":\"shutdown_ok\",\"id\":\"$id\"}"
      exit 0
      ;;
  esac
done
