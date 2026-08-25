#!/usr/bin/env python3
"""Minimal stdlib OSC/UDP sender. Usage: oscsend.py host port /addr arg1 arg2 ...
Arg typing: looks like int -> int32 'i'; has '.'/inf and numeric -> float32 'f'; else string 's'.
Force string with a leading '=' (e.g. =Blue) or quotes handled by shell."""
import socket, struct, sys

def pad(b):
    return b + b"\x00" * (4 - (len(b) % 4) if (len(b) % 4) else 4) if len(b) % 4 == 0 else b + b"\x00" * (4 - len(b) % 4)

def osc_string(s):
    b = s.encode() + b"\x00"
    if len(b) % 4:
        b += b"\x00" * (4 - len(b) % 4)
    return b

def is_int(s):
    try:
        int(s); return True
    except ValueError:
        return False

def is_float(s):
    try:
        float(s); return True
    except ValueError:
        return False

def build(addr, args):
    tags = ","
    payload = b""
    for a in args:
        force_str = a.startswith("=")
        v = a[1:] if force_str else a
        if not force_str and is_int(v):
            tags += "i"; payload += struct.pack(">i", int(v))
        elif not force_str and is_float(v):
            tags += "f"; payload += struct.pack(">f", float(v))
        else:
            tags += "s"; payload += osc_string(v)
    return osc_string(addr) + osc_string(tags) + payload

host, port, addr = sys.argv[1], int(sys.argv[2]), sys.argv[3]
args = sys.argv[4:]
msg = build(addr, args)
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.sendto(msg, (host, port))
print(f"sent {addr} {args} -> {host}:{port} ({len(msg)}B)")
