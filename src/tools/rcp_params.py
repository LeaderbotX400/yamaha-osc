#!/usr/bin/env python3
# Copyright (c) 2026 Eric Singer. All rights reserved.
# Proprietary and confidential; restricted distribution. No use without an explicit written grant - see LICENSE.
"""List the console's own RCP parameters (read-only: only sends `prminfo`).

Usage: rcp_params.py [pattern ...] [--host 192.168.50.2] [--port 49280] [--max 2000]

Prints every `prminfo` line whose text contains ALL the given patterns (case-insensitive), e.g.
    rcp_params.py dca                # everything DCA-related
    rcp_params.py inch dca           # input-channel parameters that mention DCA
With no pattern it prints everything. Use it to find the exact parameter path, index range and
value format before adding a console parameter to tf-bridge.py.
"""
import socket
import sys
import time


def main(argv):
    host, port, top, pats = "192.168.50.2", 49280, 2000, []
    it = iter(argv)
    for a in it:
        if a == "--host":
            host = next(it)
        elif a == "--port":
            port = int(next(it))
        elif a == "--max":
            top = int(next(it))
        else:
            pats.append(a.lower())

    try:
        s = socket.create_connection((host, port), timeout=3)
    except OSError as e:
        print(f"cannot reach console {host}:{port}: {e}", file=sys.stderr)
        return 1
    s.settimeout(0.5)

    for k in range(0, top, 100):          # small batches: the console is happy with ~100 lines
        s.sendall("".join(f"prminfo {i}\n" for i in range(k, min(k + 100, top))).encode())
        time.sleep(0.15)

    buf, quiet_since = b"", time.time()
    while time.time() - quiet_since < 2.0:
        try:
            data = s.recv(1 << 16)
        except socket.timeout:
            continue
        if not data:
            break
        buf += data
        quiet_since = time.time()

    shown = 0
    for line in buf.decode("utf-8", "replace").splitlines():
        low = line.lower()
        if "prminfo" in low and all(p in low for p in pats):
            print(line)
            shown += 1
    print(f"-- {shown} matching parameter(s)", file=sys.stderr)
    return 0 if shown else 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
