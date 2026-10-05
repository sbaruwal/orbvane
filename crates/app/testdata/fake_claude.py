#!/usr/bin/env python3
"""A scripted stand-in for `claude -p --input-format stream-json --output-format stream-json`,
for the Assistant's Claude Code bridge test.

Each message gets streamed thinking and text, a plan (TodoWrite), an Edit of the file the
prompt mentions ("hello" → "goodbye") and a Bash command, both asked for with `can_use_tool`
control requests, then a last message that didn't stream and the result. A message with
"expired" fails the way an expired sign-in does; one with "wait" waits for an interrupt;
one with "make a plan" proposes a plan (`ExitPlanMode`). Mode and model changes are control
requests. It prints its arguments' checks, MCP servers, modes and models on stderr.
"""
import json
import re
import sys

args = sys.argv[1:]
need = ["--input-format", "stream-json", "--permission-prompt-tool", "stdio", "--permission-mode", "default", "--allow-dangerously-skip-permissions"]
print("flags ok" if all(a in args for a in need) else "flags missing: %s" % args, file=sys.stderr, flush=True)
if "--mcp-config" in args:
    servers = json.loads(args[args.index("--mcp-config") + 1])["mcpServers"]
    print("mcp servers: %s" % sorted(servers), file=sys.stderr, flush=True)

next_request = 0
session = "s1"


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line)


def event(e):
    send({"type": "stream_event", "event": e, "parent_tool_use_id": None, "session_id": session})


def assistant(mid, content):
    send({"type": "assistant", "message": {"id": mid, "role": "assistant", "content": content}, "parent_tool_use_id": None, "session_id": session})


def tool_result(tid, text, error=False):
    send({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tid, "content": text, "is_error": error}]}, "session_id": session})


def result(ok=True, text="", **extra):
    msg = {"type": "result", "subtype": "success" if ok else "error_during_execution", "is_error": not ok, "result": text, "session_id": session}
    msg.update(extra)
    send(msg)


def control(msg):
    """The bridge's mode and model changes: True if it was one."""
    if msg.get("type") != "control_request":
        return False
    r = msg["request"]
    if r["subtype"] == "set_permission_mode":
        print("mode: %s" % r["mode"], file=sys.stderr, flush=True)
    elif r["subtype"] == "set_model":
        print("model: %s" % r["model"], file=sys.stderr, flush=True)
    else:
        return False
    send({"type": "control_response", "response": {"subtype": "success", "request_id": msg["request_id"]}})
    return True


def can_use(tool, tid, tool_input):
    """Asks to use a tool; True when allowed. Without an answer (allowed for the chat), the
    bridge answers at once, so this waits either way."""
    global next_request
    next_request += 1
    rid = "c%d" % next_request
    send({"type": "control_request", "request_id": rid, "request": {"subtype": "can_use_tool", "tool_name": tool, "input": tool_input, "tool_use_id": tid}})
    while True:
        msg = read()
        if control(msg):
            continue
        if msg.get("type") == "control_response" and msg["response"]["request_id"] == rid:
            return msg["response"]["response"]["behavior"] == "allow"


def turn(text, n):
    if "expired" in text:
        assistant("e%d" % n, [{"type": "text", "text": "Failed to authenticate: OAuth session expired"}])
        result(True, "Failed to authenticate: OAuth session expired and could not be refreshed", is_error=True, terminal_reason="api_error")
        return
    if "make a plan" in text:
        plan = {"plan": "1. Read it\n2. Change it"}
        assistant("p%d" % n, [{"type": "tool_use", "id": "plan%d" % n, "name": "ExitPlanMode", "input": plan}])
        approved = can_use("ExitPlanMode", "plan%d" % n, plan)
        assistant("q%d" % n, [{"type": "text", "text": "Approved." if approved else "Still planning."}])
        result(True, "")
        return
    if "wait" in text:
        while True:
            msg = read()
            if msg.get("type") == "control_request" and msg["request"]["subtype"] == "interrupt":
                send({"type": "control_response", "response": {"subtype": "success", "request_id": msg["request_id"]}})
                result(False, "")
                return
    mid = "m%d" % n
    event({"type": "message_start", "message": {"id": mid}})
    event({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Looking at it."}})
    event({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Using "}})
    event({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "tools"}})
    event({"type": "message_stop"})
    todos = {"todos": [{"content": "Read it", "status": "completed", "activeForm": "Reading"}, {"content": "Change it", "status": "in_progress", "activeForm": "Changing"}]}
    content = [{"type": "text", "text": "Using tools"}, {"type": "tool_use", "id": "todo%d" % n, "name": "TodoWrite", "input": todos}]
    found = re.search(r"The file open in the editor: (.+?)\)", text)
    edit = None
    if found:
        edit = {"file_path": found.group(1), "old_string": "hello", "new_string": "goodbye"}
        content.append({"type": "tool_use", "id": "edit%d" % n, "name": "Edit", "input": edit})
    assistant(mid, content)
    tool_result("todo%d" % n, "ok")
    if edit:
        if can_use("Edit", "edit%d" % n, edit):
            with open(edit["file_path"]) as f:
                old = f.read()
            with open(edit["file_path"], "w") as f:
                f.write(old.replace("hello", "goodbye", 1))
            tool_result("edit%d" % n, "The file was updated.")
        else:
            tool_result("edit%d" % n, "The user didn't allow it.", error=True)
    bash = {"command": "ls -1", "description": "List files"}
    assistant("b%d" % n, [{"type": "tool_use", "id": "bash%d" % n, "name": "Bash", "input": bash}])
    if can_use("Bash", "bash%d" % n, bash):
        tool_result("bash%d" % n, "notes.txt")
    else:
        tool_result("bash%d" % n, "Denied", error=True)
    assistant("d%d" % n, [{"type": "text", "text": "Done."}])
    result(True, "Done.")


turns = 0
print("fake claude ready", file=sys.stderr, flush=True)
while True:
    msg = read()
    if msg.get("type") == "control_request" and msg["request"]["subtype"] == "initialize":
        models = [{"value": "default", "displayName": "Default"}, {"value": "sonnet", "displayName": "Sonnet"}]
        send({"type": "control_response", "response": {"subtype": "success", "request_id": msg["request_id"], "response": {"commands": [], "models": models}}})
        send({"type": "system", "subtype": "init", "session_id": session, "model": "test"})
    elif control(msg):
        pass
    elif msg.get("type") == "user":
        turns += 1
        text = " ".join(b.get("text", "") for b in msg["message"]["content"])
        turn(text, turns)
