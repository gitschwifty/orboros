"""Protocol fixture modeling ancestor context; not an implementation of Heddle."""
import json
import pathlib
import sys

for line in sys.stdin:
    request = json.loads(line)
    kind = request["type"]
    response = {"id": request["id"]}
    if kind == "init":
        response.update(type="init_ok", session_id="context-fixture", protocol_version="0.3.0")
    elif kind == "send":
        cwd = pathlib.Path.cwd()
        ancestors = list(reversed([cwd, *cwd.parents]))
        contexts = [str(path / "AGENTS.md") for path in ancestors if (path / "AGENTS.md").is_file()]
        response.update(type="result", status="ok", response=json.dumps({"cwd": str(cwd), "contexts": contexts}), tool_calls_made=[], iterations=1)
    elif kind == "shutdown":
        response.update(type="shutdown_ok")
    else:
        continue
    print(json.dumps(response), flush=True)
    if kind == "shutdown":
        break
