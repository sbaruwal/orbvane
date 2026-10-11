#!/usr/bin/env python3
"""The fake debug adapter as one that listens (like `rdbg --open --port N`): waits on the port
given as its first argument and runs fake_dap.py over the connection."""
import os, socket, subprocess, sys
port = int(sys.argv[1])
here = os.path.dirname(os.path.abspath(__file__))
with socket.create_server(("127.0.0.1", port)) as server:
    conn, _ = server.accept()
    with conn:
        subprocess.run([sys.executable, os.path.join(here, "fake_dap.py")], stdin=conn.fileno(), stdout=conn.fileno())
