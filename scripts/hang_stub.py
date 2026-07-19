#!/usr/bin/env python3
# v4.1 (S-FIX-1) — a HANGING upstream for the proxy-outbound permit-bound gate.
# Accepts TCP connections on 127.0.0.1:18080 and holds them open WITHOUT ever
# sending a response byte, so a proxy guest's outbound connects fine then blocks
# on first-byte. Pre-fix that block is wasi-http's 600s default (the guest is
# parked in host I/O, so neither fuel nor the epoch trap fires and the fn_sem
# permit stays pinned); post-fix KeelHooks clamps the phase to the route's
# time_ms. The gate asserts the request returns bounded and the permit frees.
import socket
import threading
import time

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 18080))
srv.listen(128)


def hold(conn):
    # Never respond; just keep the socket open so the peer waits for a byte.
    try:
        time.sleep(3600)
    finally:
        try:
            conn.close()
        except OSError:
            pass


while True:
    conn, _ = srv.accept()
    threading.Thread(target=hold, args=(conn,), daemon=True).start()
