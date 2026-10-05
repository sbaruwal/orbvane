#!/usr/bin/env python3
"""A fake debug adapter for orbvane's debugger tests: it stops at the first breakpoint,
steps a line at a time, and ends after the third resume."""
import sys, json, os
seq = [0]
def send(msg):
    seq[0] += 1
    msg["seq"] = seq[0]
    b = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
    sys.stdout.buffer.flush()
def resp(req, body=None, ok=True, message=None):
    m = {"type": "response", "request_seq": req["seq"], "command": req["command"], "success": ok, "body": body or {}}
    if message: m["message"] = message
    send(m)
def event(name, body=None):
    send({"type": "event", "event": name, "body": body or {}})
def read():
    h = b""
    while not h.endswith(b"\r\n\r\n"):
        c = sys.stdin.buffer.read(1)
        if not c: return None
        h += c
    n = int([l for l in h.split(b"\r\n") if l.lower().startswith(b"content-length")][0].split(b":")[1])
    return json.loads(sys.stdin.buffer.read(n))
bps = {}
program = None
line = 0
hits = 0
while True:
    req = read()
    if req is None: break
    cmd = req["command"]; args = req.get("arguments", {})
    if cmd == "initialize":
        resp(req, {"supportsConfigurationDoneRequest": True, "supportsConditionalBreakpoints": True, "supportsLogPoints": True,
                   "exceptionBreakpointFilters": [{"filter": "rust_panic", "label": "Rust Panic", "default": True}]})
    elif cmd == "launch":
        program = args.get("program")
        resp(req)
        event("initialized")
        event("output", {"category": "console", "output": "Launching: %s\n" % program})
    elif cmd == "setBreakpoints":
        path = args["source"]["path"]
        lines = [b["line"] for b in args["breakpoints"]]
        bps[path] = lines
        out = []
        for i, l in enumerate(lines):
            # Lines past 17 have "no code"; line 3 moves to 6 (the next line with code).
            if l > 17: out.append({"id": i + 1, "verified": False, "message": "No code at line"})
            elif l == 3: out.append({"id": i + 1, "verified": True, "line": 6})
            else: out.append({"id": i + 1, "verified": True, "line": l})
        resp(req, {"breakpoints": out})
    elif cmd == "setExceptionBreakpoints":
        resp(req)
    elif cmd == "configurationDone":
        resp(req)
        event("output", {"category": "stdout", "output": "hello from the program\n"})
        stops = sorted(l for ls in bps.values() for l in ls if l <= 17)
        if stops:
            line = stops[0]
            event("stopped", {"reason": "breakpoint", "description": "breakpoint 1.1", "threadId": 1, "allThreadsStopped": True})
        else:
            event("exited", {"exitCode": 0}); event("terminated")
    elif cmd == "threads":
        resp(req, {"threads": [{"id": 1, "name": "main"}, {"id": 2, "name": "worker"}]})
    elif cmd == "stackTrace":
        src = [p for p in bps][0] if bps else "/nowhere.rs"
        frames = [
            {"id": 1000, "name": "dbgdemo::add", "line": line, "column": 5, "source": {"name": os.path.basename(src), "path": src}},
            {"id": 1001, "name": "dbgdemo::main", "line": 16, "column": 17, "source": {"name": os.path.basename(src), "path": src}},
            {"id": 1002, "name": "core::ops::function::FnOnce::call_once", "line": 250, "column": 5, "presentationHint": "subtle"},
            {"id": 1003, "name": "start", "line": 0, "column": 0},
        ]
        resp(req, {"stackFrames": frames if args.get("threadId") == 1 else frames[2:], "totalFrames": 4})
    elif cmd == "scopes":
        f = args["frameId"]
        resp(req, {"scopes": [{"name": "Locals", "variablesReference": f * 10 + 1, "expensive": False},
                              {"name": "Registers", "variablesReference": f * 10 + 2, "expensive": True}]})
    elif cmd == "variables":
        r = args["variablesReference"]
        if r % 10 == 1:
            v = [{"name": "a", "value": str(hits), "type": "i32", "variablesReference": 0},
                 {"name": "b", "value": "2", "type": "i32", "variablesReference": 0},
                 {"name": "p", "value": "{x:3, y:4}", "type": "dbgdemo::Point", "variablesReference": 77},
                 {"name": "names", "value": "size=2", "type": "Vec<&str>", "variablesReference": 88},
                 {"name": "ok", "value": "true", "type": "bool", "variablesReference": 0}]
        elif r == 77:
            v = [{"name": "x", "value": "3", "variablesReference": 0}, {"name": "y", "value": "4", "variablesReference": 0}]
        elif r == 88:
            v = [{"name": "[0]", "value": "\"one\"", "variablesReference": 0}, {"name": "[1]", "value": "\"two\"", "variablesReference": 0}]
        else:
            v = [{"name": "rip", "value": "0x0000000100000f50", "variablesReference": 0}]
        resp(req, {"variables": v})
    elif cmd == "evaluate":
        e = args["expression"]
        if e == "a + b": resp(req, {"result": str(hits + 2), "type": "i32", "variablesReference": 0})
        elif e == "line": resp(req, {"result": "\"hovered\"", "type": "&str", "variablesReference": 0})
        elif e == "p": resp(req, {"result": "{x:3, y:4}", "type": "Point", "variablesReference": 77})
        else: resp(req, None, False, "use of undeclared identifier '%s'" % e)
    elif cmd in ("continue", "next", "stepIn", "stepOut"):
        resp(req, {"allThreadsContinued": True})
        hits += 1
        if cmd == "continue" and hits >= 3:
            event("output", {"category": "stdout", "output": "3 4 [\"one\", \"two\"] 3\n"})
            event("exited", {"exitCode": 0}); event("terminated")
        else:
            line = line + 1 if cmd != "continue" else line
            event("stopped", {"reason": "step" if cmd != "continue" else "breakpoint", "threadId": 1, "allThreadsStopped": True})
    elif cmd == "pause":
        resp(req); event("stopped", {"reason": "pause", "threadId": 1})
    elif cmd == "disconnect":
        resp(req); break
    else:
        resp(req, None, False, "unsupported: " + cmd)
