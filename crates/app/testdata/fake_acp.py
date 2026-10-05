#!/usr/bin/env python3
"""A scripted Agent Client Protocol agent for the Assistant's tests.

For each prompt it streams a reply, reports a tool call proposing an edit to the file the
prompt links, asks for permission, and when allowed reads the file through the editor and
writes it back changed. A prompt saying "sign in" makes the next session/new need a sign-in.
"""
import json
import sys

next_id = 100
need_auth = False


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line)


def update(session, u):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": u}})


def call(method, params):
    """Sends a request to the editor and waits for its answer (others are handled meanwhile)."""
    global next_id
    next_id += 1
    rid = next_id
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    while True:
        msg = read()
        if msg.get("id") == rid and "method" not in msg:
            return msg
        handle(msg)


def prompt(rid, params):
    session = params["sessionId"]
    blocks = params["prompt"]
    text = " ".join(b.get("text", "") for b in blocks if b["type"] == "text")
    links = [b["uri"] for b in blocks if b["type"] == "resource_link"]
    update(session, {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Thinking about it."}})
    update(session, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "You said: "}})
    update(session, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})
    update(session, {"sessionUpdate": "plan", "entries": [
        {"content": "Read the file", "priority": "high", "status": "completed"},
        {"content": "Change it", "priority": "high", "status": "in_progress"},
    ]})
    if not links:
        send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
        return
    path = links[0][len("file://"):]
    got = call("fs/read_text_file", {"sessionId": session, "path": path})
    old = got["result"]["content"]
    new = old.replace("hello", "goodbye")
    update(session, {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit notes", "kind": "edit", "status": "pending",
                     "content": [{"type": "diff", "path": path, "oldText": old, "newText": new}]})
    answer = call("session/request_permission", {"sessionId": session, "toolCall": {"toolCallId": "t1"}, "options": [
        {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
        {"optionId": "no", "name": "Reject", "kind": "reject_once"},
    ]})
    outcome = answer["result"]["outcome"]
    if outcome.get("outcome") == "cancelled":
        send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "cancelled"}})
        return
    if outcome.get("optionId") == "yes":
        update(session, {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress"})
        call("fs/write_text_file", {"sessionId": session, "path": path, "content": new})
        update(session, {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"})
    else:
        update(session, {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed"})
    send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})


def handle(msg):
    global need_auth
    method = msg.get("method")
    rid = msg.get("id")
    params = msg.get("params") or {}
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": 1,
            "agentCapabilities": {"promptCapabilities": {"embeddedContext": True}},
            "agentInfo": {"name": "fake", "title": "Fake Agent", "version": "1"},
            "authMethods": [{"id": "token", "name": "Use a token"}],
        }})
    elif method == "session/new":
        if need_auth:
            send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": "Authentication required"}})
        else:
            send({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": "s1"}})
    elif method == "authenticate":
        need_auth = False
        send({"jsonrpc": "2.0", "id": rid, "result": {}})
    elif method == "session/prompt":
        if "sign in" in json.dumps(params["prompt"]):
            need_auth = True
        prompt(rid, params)
    elif method == "session/cancel":
        pass
    elif rid is not None:
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "unknown " + str(method)}})


print("fake agent ready", file=sys.stderr, flush=True)
while True:
    handle(read())
