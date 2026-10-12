#!/usr/bin/env python3
"""Binds 127.0.0.1:PORT, prints `ready`, then answers connections with `pong`."""
import socket
import sys

port = int(sys.argv[1])
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(8)
print("ready", flush=True)
while True:
    conn, _ = srv.accept()
    conn.sendall(b"pong")
    conn.close()
