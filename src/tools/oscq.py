#!/usr/bin/env python3
"""OSC query: send a message and print any reply (QLab replies to the sending UDP port).
Usage: oscq.py host port /addr [args...]   (waits up to 2s for a reply)"""
import socket, struct, sys, time

def osc_string(s):
    b = s.encode() + b"\x00"
    if len(b) % 4: b += b"\x00" * (4 - len(b) % 4)
    return b

def is_int(s):
    try: int(s); return True
    except: return False
def is_float(s):
    try: float(s); return True
    except: return False

def build(addr, args):
    tags = ","; payload = b""
    for a in args:
        fs = a.startswith("=")
        v = a[1:] if fs else a
        if not fs and is_int(v): tags += "i"; payload += struct.pack(">i", int(v))
        elif not fs and is_float(v): tags += "f"; payload += struct.pack(">f", float(v))
        else: tags += "s"; payload += osc_string(v)
    return osc_string(addr) + osc_string(tags) + payload

def decode(data):
    # crude OSC decode: address, typetags, string args (ignore binary blobs)
    try:
        z = data.index(b"\x00"); addr = data[:z].decode()
        i = (len(data[:z]) // 4 + 1) * 4
        if i >= len(data) or data[i:i+1] != b",": return addr, data[i:]
        z2 = data.index(b"\x00", i); tags = data[i:z2].decode()
        j = ((z2 - i) // 4 + 1) * 4 + i
        out = []
        for t in tags[1:]:
            if t == 's':
                z3 = data.index(b"\x00", j); out.append(data[j:z3].decode(errors="replace"))
                j = ((z3 - j)//4 + 1)*4 + j
            elif t == 'i': out.append(struct.unpack(">i", data[j:j+4])[0]); j += 4
            elif t == 'f': out.append(struct.unpack(">f", data[j:j+4])[0]); j += 4
            else: out.append(f"<{t}>"); break
        return addr, out
    except Exception as e:
        return "<decode-err>", repr(data[:200])

host, port, addr = sys.argv[1], int(sys.argv[2]), sys.argv[3]
args = sys.argv[4:]
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("0.0.0.0", 0))
s.settimeout(2.0)
s.sendto(build(addr, args), (host, port))
print(f">> {addr} {args}")
t0 = time.time()
while time.time() - t0 < 2.0:
    try:
        data, src = s.recvfrom(65535)
        a, p = decode(data)
        print(f"<< {a}  {p}")
    except socket.timeout:
        break
