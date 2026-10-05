#!/usr/bin/env python3
"""A scripted stand-in for `codex app-server`, for the Assistant's Codex bridge test.

For each turn it streams reasoning and a reply (naming the model the turn asked for), updates
the plan, proposes a change to the file the prompt mentions (its first line, "hello" →
"goodbye"), asks for approval, applies it when accepted, runs a command, and ends the turn.
A prompt with "fail" ends in an error; one with "wait" waits to be interrupted. Its threads
start with a model the account doesn't offer, so the bridge picks the default; like the real
one, it answers the model list after the thread has started. Each turn's approval policy,
sandbox and model are printed on stderr.
"""
import json
import re
import sys

next_id = 900
thread = "t1"
models_request = None
MODELS = {"data": [{"id": "m1", "displayName": "Model One", "isDefault": True}, {"id": "m2", "displayName": "Model Two", "isDefault": False}]}


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def note(method, params):
    send({"method": method, "params": params})


def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line)


def ask(method, params):
    """A request to the client; returns its answer (other messages are handled meanwhile)."""
    global next_id
    next_id += 1
    rid = next_id
    send({"id": rid, "method": method, "params": params})
    while True:
        msg = read()
        if msg.get("id") == rid and "method" not in msg:
            return msg
        handle(msg)


def turn(rid, params):
    text = " ".join(i.get("text", "") for i in params["input"])
    model = params.get("model", "default")
    sandbox = (params.get("sandboxPolicy") or {}).get("type")
    print("turn: approval=%s sandbox=%s model=%s" % (params.get("approvalPolicy"), sandbox, model), file=sys.stderr, flush=True)
    turn_id = "u%d" % rid
    send({"id": rid, "result": {"turn": {"id": turn_id, "items": [], "status": "inProgress"}}})
    note("turn/started", {"threadId": thread, "turn": {"id": turn_id, "status": "inProgress"}})
    ids = {"threadId": thread, "turnId": turn_id}
    if "fail" in text:
        error = {"message": json.dumps({"type": "error", "status": 400, "error": {"message": "Model is unavailable."}})}
        note("turn/completed", {"threadId": thread, "turn": {"id": turn_id, "status": "failed", "error": error}})
        return
    if "wait" in text:
        while True:
            msg = read()
            if msg.get("method") == "turn/interrupt":
                send({"id": msg["id"], "result": {}})
                note("turn/completed", {"threadId": thread, "turn": {"id": turn_id, "status": "interrupted"}})
                return
            handle(msg)
    note("item/reasoning/summaryTextDelta", dict(ids, itemId="r1", delta="Looking at the file.", summaryIndex=0))
    note("item/agentMessage/delta", dict(ids, itemId="m1", delta="Using "))
    note("item/agentMessage/delta", dict(ids, itemId="m1", delta="model=" + model))
    note("turn/plan/updated", dict(ids, plan=[{"step": "Read it", "status": "completed"}, {"step": "Change it", "status": "inProgress"}]))
    found = re.search(r"The file open in the editor: (.+?)\)", text)
    if found:
        path = found.group(1)
        with open(path) as f:
            old = f.read()
        first = old.split("\n")[0]
        new_first = first.replace("hello", "goodbye")
        diff = "--- a\n+++ b\n@@ -1,1 +1,1 @@\n-%s\n+%s\n" % (first, new_first)
        change = {"path": path, "kind": {"type": "update", "move_path": None}, "diff": diff}
        item = {"type": "fileChange", "id": "f1", "changes": [change], "status": "inProgress"}
        note("item/started", dict(ids, item=item, startedAtMs=0))
        answer = ask("item/fileChange/requestApproval", dict(ids, itemId="f1", reason=None))
        decision = answer["result"]["decision"]
        if decision == "cancel":
            note("turn/completed", {"threadId": thread, "turn": {"id": turn_id, "status": "interrupted"}})
            return
        if decision in ("accept", "acceptForSession"):
            with open(path, "w") as f:
                f.write(old.replace(first, new_first, 1))
            item["status"] = "completed"
        else:
            item["status"] = "declined"
        note("item/completed", dict(ids, item=item, completedAtMs=0))
    command = {"type": "commandExecution", "id": "c1", "command": "ls -1", "cwd": "/", "commandActions": [], "status": "inProgress"}
    note("item/started", dict(ids, item=command, startedAtMs=0))
    command.update(status="completed", exitCode=0, aggregatedOutput="notes.txt\n")
    note("item/completed", dict(ids, item=command, completedAtMs=0))
    note("item/completed", dict(ids, item={"type": "agentMessage", "id": "m2", "text": "Done."}, completedAtMs=0))
    note("turn/completed", {"threadId": thread, "turn": {"id": turn_id, "status": "completed"}})


def handle(msg):
    method = msg.get("method")
    rid = msg.get("id")
    params = msg.get("params") or {}
    if method == "initialize":
        send({"id": rid, "result": {"userAgent": "orbvane/9.9.9 (test)", "platformOs": "macos"}})
    elif method == "model/list":
        global models_request
        models_request = rid
    elif method == "account/read":
        send({"id": rid, "result": {"account": {"type": "chatgpt"}, "requiresOpenaiAuth": True}})
    elif method == "thread/start":
        servers = sorted((params.get("config") or {}).get("mcp_servers", {}).keys())
        print("mcp servers: %s" % servers, file=sys.stderr, flush=True)
        send({"id": rid, "result": {"thread": {"id": thread}, "model": "retired-model"}})
        if models_request is not None:
            send({"id": models_request, "result": MODELS})
    elif method == "turn/start":
        turn(rid, params)
    elif rid is not None and method is not None:
        send({"id": rid, "result": {}})


print("fake codex ready", file=sys.stderr, flush=True)
while True:
    handle(read())
