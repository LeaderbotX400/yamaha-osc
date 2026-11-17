#!/usr/bin/env python3
"""OSC -> Yamaha TF console bridge.

Evolved from tf3.py. The proven core is unchanged: one persistent blocking TCP socket to the
console, a reconnect loop, the "only act on the console network" guard, and stateful mute/scene
reconciliation (mutes are re-asserted on top of a recalled scene). On top of that it adds a richer,
QLab-friendly OSC vocabulary — faders in dB, curved fades, sends, labels and colours — under a
`/ch|mix|dca|...` grammar, while the original `/tf/mute|unmute|scene` addresses keep working so
existing cues do not break.

Networking is deliberately blocking: on this hardware an async runtime proved unreliable on the
console's long-lived link, while blocking sockets (this script, nc) are rock-solid. The cue path is
bounded by the console's ~5 ms command pacing, not by Python.
"""

import json
import math
import os
import re
import shlex
import socket
import subprocess
import sys
import threading
import time

from pythonosc.dispatcher import Dispatcher
from pythonosc import osc_server

# Defaults match the venue install; env vars let a test instance run beside the live one.
TF_IP = os.environ.get("TF_IP", "192.168.50.2")
TF_PORT = int(os.environ.get("TF_PORT", "49280"))
OSC_IP = os.environ.get("TF_OSC_IP", "0.0.0.0")
OSC_PORT = int(os.environ.get("TF_OSC_PORT", "8000"))

HERE = os.path.dirname(os.path.abspath(__file__))
PRESETS_PATH = os.environ.get("TF_PRESETS", os.path.join(HERE, "presets.json"))
SNAPSHOTS_PATH = os.environ.get("TF_SNAPSHOTS", os.path.join(HERE, "snapshots.json"))
# Test-only escape hatches (never set on the booth Mac).
SKIP_NET_GUARD = os.environ.get("TF_SKIP_NET_GUARD") == "1"   # allow a non-192.168.50.* host
READER_ENABLED = os.environ.get("TF_READER", "1") != "0"      # kill-switch for the reply reader

MAX_INPUT_CH, MAX_MIX, MAX_MATRIX, MAX_DCA = 40, 20, 4, 8

RECONNECT_DELAY = 2.0
CONNECT_TIMEOUT = 2.0
CONNECTION_CHECK_INTERVAL = 1.0
SCENE_RECALL_DELAY = 0.3

# ---- level / fade constants ------------------------------------------------------------------
NEG_INF = -32768          # the console's -inf fader sentinel
FADER_MAX = 1000          # +10.00 dB, the fader ceiling
MIN_DB_RAW = -13800       # lowest real dB point; the floor a fade ramps to before -inf
FADE_TICK = 0.033         # 30 Hz fade stepping
# Console writes are paced: commands go out in chunks of BATCH_MAX lines per TCP write, with at
# least MIN_SEND_INTERVAL between chunks. Both are tunable without a code change.
MIN_SEND_INTERVAL = float(os.environ.get("TF_PACING_MS", "5")) / 1000.0
BATCH_MAX = max(1, int(os.environ.get("TF_BATCH_MAX", "8")))

# Strip word (OSC) -> RCP node name, and a generous instance ceiling (the console rejects the rest).
STRIP_PATHS = {
    "ch": "InCh", "input": "InCh",
    "stin": "StInCh",
    "fxrtn": "FxRtnCh",
    "dca": "DCA",
    "mix": "Mix",
    "mtrx": "Mtrx", "matrix": "Mtrx",
    "st": "St", "stereo": "St",
    "mono": "Mono",
}
STRIP_MAX = {
    "InCh": MAX_INPUT_CH, "StInCh": 20, "FxRtnCh": 4, "DCA": MAX_DCA,
    "Mix": MAX_MIX, "Mtrx": MAX_MATRIX, "St": 2, "Mono": 1,
}
LABEL_COLORS = {"purple", "pink", "red", "orange", "yellow", "blue", "skyblue", "green"}

sock = None
sock_lock = threading.Lock()
connection_event = threading.Event()
shutdown_event = threading.Event()
state_lock = threading.RLock()
connection_state = "DISCONNECTED"
connection_generation = 0

desired_mute_state = {}
applied_mute_state = {}
desired_scene = None
scene_generation = 0
applied_scene_generation = 0

# ---- write serialisation + pacing, fade tracking, fader shadow --------------------------------
write_lock = threading.Lock()
_last_send = [0.0]
fade_lock = threading.Lock()
active_fades = {}     # target key -> Fade, advanced together by one engine thread
fade_wake = threading.Event()
fader_shadow = {}     # target key -> last known raw level (fade start when no explicit from)
sync_wake = threading.Event()   # a desired mute/scene changed: apply it now, not on the next tick

pending_gets = {}     # (path, idx, sub) -> [[Event, value], ...] awaiting an `OK get` reply
pending_lock = threading.Lock()

presets, presets_mtime = {}, None
snapshots = {}
snap_lock = threading.Lock()

_tls = threading.local()   # per-thread "collect" context: a preset's writes go out as one batch


def log(msg):
    # One write per line: threads log concurrently and print() would split text from newline.
    sys.stdout.write(f"{msg}\n")
    sys.stdout.flush()


def set_connection_state(state):
    global connection_state
    with state_lock:
        if connection_state != state:
            connection_state = state
            log(f"TF STATE: {state}")


def get_local_ipv4_addresses():
    try:
        output = subprocess.check_output(
            ["ifconfig"],
            text=True,
            stderr=subprocess.DEVNULL
        )
    except Exception:
        return []

    out, iface = [], None

    for line in output.splitlines():
        if line and not line.startswith((" ", "\t")):
            iface = line.split(":")[0]
            continue

        line = line.strip()
        if line.startswith("inet "):
            parts = line.split()
            if len(parts) >= 2 and parts[1] != "127.0.0.1":
                out.append((iface, parts[1]))

    return out


def is_tf_network():
    if SKIP_NET_GUARD:
        return True
    return any(ip.startswith("192.168.50.") for _, ip in get_local_ipv4_addresses())


def configure_socket(s):
    s.settimeout(CONNECT_TIMEOUT)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
    try:
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPALIVE, 10)
    except Exception:
        pass
    return s


def get_socket():
    with sock_lock:
        return sock


def disconnect():
    global sock

    with sock_lock:
        old_sock, sock = sock, None

    if old_sock is not None:
        try:
            old_sock.shutdown(socket.SHUT_RDWR)
        except Exception:
            pass

        try:
            old_sock.close()
        except Exception:
            pass

    connection_event.clear()
    set_connection_state("DISCONNECTED")


def connect():
    global sock, connection_generation

    if get_socket() is not None:
        return True

    set_connection_state("CONNECTING")
    log(f"Connecting to TF3 {TF_IP}:{TF_PORT}...")

    s = None
    try:
        s = configure_socket(socket.socket(socket.AF_INET, socket.SOCK_STREAM))
        s.connect((TF_IP, TF_PORT))
        s.settimeout(2.0)

        with sock_lock:
            if sock is None:
                sock = s
                connection_generation += 1
                generation = connection_generation
            else:
                s.close()
                return True

        connection_event.set()
        set_connection_state("CONNECTED")
        log(f"TF CONNECTED (generation {generation})")
        if READER_ENABLED:
            threading.Thread(target=_reader, args=(s,), name=f"tf-reader-{generation}",
                             daemon=True).start()
        return True

    except Exception as e:
        if s is not None:
            try:
                s.close()
            except Exception:
                pass

        connection_event.clear()
        set_connection_state("DISCONNECTED")
        log(f"TF connection failed: {type(e).__name__}: {e}")
        return False


def _raw_send_many(cmds):
    """Write commands, BATCH_MAX lines per TCP write, paced to the console's command budget.
    Caller MUST hold write_lock."""
    s = get_socket()
    if s is None:
        return False
    for start in range(0, len(cmds), BATCH_MAX):
        chunk = cmds[start:start + BATCH_MAX]
        wait = MIN_SEND_INTERVAL - (time.monotonic() - _last_send[0])
        if wait > 0:
            time.sleep(wait)
        try:
            log(f"SEND x{len(chunk)}: {chunk[0]}" + (f" ... {chunk[-1]}" if len(chunk) > 1 else ""))
            s.sendall("".join(c + "\r\n" for c in chunk).encode("utf-8"))
            _last_send[0] = time.monotonic()
        except Exception as e:
            log(f"Send failed: {type(e).__name__}: {e}")
            disconnect()
            return False
    return True


def _raw_send(cmd):
    return _raw_send_many([cmd])


def send_many(cmds):
    """Write RCP commands now. Serialised across threads and paced, so concurrent fades and direct
    writes cannot interleave or outrun the console."""
    cmds = list(cmds)
    if not cmds:
        return True
    with write_lock:
        return _raw_send_many(cmds)


def send(cmd):
    return send_many([cmd])


def begin_collect():
    _tls.collect = {"cmds": [], "after": []}


def end_collect():
    """Flush what this thread collected as one batch, then run the deferred actions (fade starts)."""
    c = getattr(_tls, "collect", None)
    _tls.collect = None
    if c is None:
        return
    send_many(c["cmds"])
    for fn in c["after"]:
        fn()


def emit(cmds):
    """Handlers write through here: queued while a preset is collecting, otherwise sent now."""
    c = getattr(_tls, "collect", None)
    if c is not None:
        c["cmds"].extend(cmds)
        return True
    return send_many(cmds)


# ---- console replies -------------------------------------------------------------------------

def _handle_console_line(line):
    parts = line.split()
    if not parts:
        return
    if parts[0] == "ERROR":
        log(f"CONSOLE ERROR: {line}")
        return
    if parts[0] not in ("OK", "NOTIFY") or len(parts) < 6 or parts[1] not in ("get", "set"):
        return
    try:
        key = (parts[2], int(parts[3]), int(parts[4]))
        value = int(parts[5])
    except ValueError:
        return
    if parts[1] == "get":
        with pending_lock:
            waiters = pending_gets.pop(key, [])
        for w in waiters:
            w[1] = value
            w[0].set()
    # Track the console's own levels (desk moves) -- but not while one of our fades owns the target.
    if parts[2].endswith("/Level") and (parts[1] == "get" or parts[0] == "NOTIFY"):
        with fade_lock:
            owned = key in active_fades
        if not owned:
            fader_shadow[key] = value


def _reader(s):
    """Drain the console's replies for one connection: log rejected commands, learn real levels."""
    buf = b""
    while not shutdown_event.is_set() and get_socket() is s:
        try:
            data = s.recv(4096)
        except socket.timeout:
            continue
        except Exception:
            break
        if not data:
            break
        buf += data
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            _handle_console_line(line.decode("utf-8", "replace").strip())
    if get_socket() is s and not shutdown_event.is_set():
        log("Console link closed")
        disconnect()


def get_many(targets, timeout=1.0):
    """Read many (path, idx, sub) levels with one batched write; {target: int | None}.
    Anything the reader doesn't answer in time falls back to a short-lived read (few targets only)."""
    targets = list(dict.fromkeys(targets))
    out = {t: None for t in targets}
    if READER_ENABLED and targets and get_socket() is not None:
        waiters = {}
        with pending_lock:
            for t in targets:
                w = [threading.Event(), None]
                pending_gets.setdefault(t, []).append(w)
                waiters[t] = w
        send_many([f"get {p} {i} {s}" for p, i, s in targets])
        deadline = time.monotonic() + timeout
        for t, w in waiters.items():
            w[0].wait(max(0.0, deadline - time.monotonic()))
            out[t] = w[1]
        with pending_lock:
            for t, w in waiters.items():
                lst = pending_gets.get(t)
                if lst is not None:
                    if w in lst:
                        lst.remove(w)
                    if not lst:
                        pending_gets.pop(t, None)
    missing = [t for t in targets if out[t] is None]
    if 0 < len(missing) <= 8:
        for t in missing:
            out[t] = read_level_short(*t)
    return out


def tf_in_mute(ch, state):
    return f"set MIXER:Current/InCh/Fader/On {ch - 1} 0 {state}"


def tf_stin_pair_mute(pair_num, state):
    base = (pair_num - 1) * 2
    return [
        f"set MIXER:Current/StInCh/Fader/On {base} 0 {state}",
        f"set MIXER:Current/StInCh/Fader/On {base + 1} 0 {state}",
    ]


def tf_master_mute(state):
    return [
        f"set MIXER:Current/St/Fader/On 0 0 {state}",
        f"set MIXER:Current/St/Fader/On 1 0 {state}",
    ]


def tf_sub_mute(state):
    return f"set MIXER:Current/Mono/Fader/On 0 0 {state}"


def tf_mix_mute(mix_idx_zero_based, state):
    return f"set MIXER:Current/Mix/Fader/On {mix_idx_zero_based} 0 {state}"


def tf_matrix_mute(matrix_num, state):
    return f"set MIXER:Current/Mtrx/Fader/On {matrix_num - 1} 0 {state}"


def tf_fx_mute(fx_num, state):
    if fx_num == 1:
        return [
            f"set MIXER:Current/FxRtnCh/Fader/On 0 0 {state}",
            f"set MIXER:Current/FxRtnCh/Fader/On 1 0 {state}",
        ]
    if fx_num == 2:
        return [
            f"set MIXER:Current/FxRtnCh/Fader/On 2 0 {state}",
            f"set MIXER:Current/FxRtnCh/Fader/On 3 0 {state}",
        ]
    return []


def tf_dca_mute(dca_num, state):
    idx = dca_num - 1
    return [
        f"set MIXER:Current/DCA/Fader/On {idx} 0 {state}",
        f"set MIXER:Current/DcaCh/Fader/On {idx} 0 {state}",
    ]


def tf_scene(bank, num):
    return f"ssrecall_ex scene_{bank.lower()} {num}"


def parse_range_list(spec, min_value, max_value):
    if not spec:
        return []

    spec = str(spec).lower().replace(" ", "")
    out, seen = [], set()

    for part in spec.split(","):
        if not part:
            continue

        if part == "all":
            for n in range(min_value, max_value + 1):
                if n not in seen:
                    out.append(n)
                    seen.add(n)
            continue

        if "-" in part and "/" not in part:
            try:
                a, b = map(int, part.split("-"))
            except Exception:
                continue
            if a > b:
                a, b = b, a
            nums = range(a, b + 1)
        else:
            try:
                nums = [int(part)]
            except Exception:
                continue

        for n in nums:
            if min_value <= n <= max_value and n not in seen:
                out.append(n)
                seen.add(n)

    return out


def parse_input_spec(spec):
    return parse_range_list(spec, 1, MAX_INPUT_CH)


def parse_matrix_spec(spec):
    return parse_range_list(spec, 1, MAX_MATRIX)


def parse_dca_spec(spec):
    if not spec:
        return []

    cleaned = []
    for part in str(spec).upper().split(","):
        token = re.sub(r"\bDCA\s*", "", part.strip(), flags=re.IGNORECASE).replace(" ", "")
        if token:
            cleaned.append(token)

    return parse_range_list(",".join(cleaned), 1, MAX_DCA)


def parse_st_spec(spec):
    if not spec:
        return []

    out, seen = [], set()
    for part in str(spec).upper().split(","):
        m = re.fullmatch(r"ST([12])", part.strip().replace(" ", ""))
        if m:
            n = int(m.group(1))
            if n not in seen:
                out.append(n)
                seen.add(n)
    return out


def parse_fx_spec(spec):
    if not spec:
        return []

    out, seen = [], set()
    for part in str(spec).upper().split(","):
        m = re.fullmatch(r"FX([12])", part.strip().replace(" ", ""))
        if m:
            n = int(m.group(1))
            if n not in seen:
                out.append(n)
                seen.add(n)
    return out


def expand_aux_token(token):
    token = token.strip().replace(" ", "")
    if not token:
        return []

    if token.lower() == "all":
        return list(range(1, MAX_MIX + 1))

    if token.isdigit():
        n = int(token)
        if 1 <= n <= 8:
            return [n]
        if n in {9, 11, 13, 15, 17, 19}:
            return [n, n + 1]

        stereo_seconds = {10: 9, 12: 11, 14: 13, 16: 15, 18: 17, 20: 19}
        if n in stereo_seconds:
            start = stereo_seconds[n]
            return [start, start + 1]

        return []

    m = re.fullmatch(r"(\d+)/(\d+)", token)
    if m:
        a, b = map(int, m.groups())
        valid_pairs = {(9, 10), (11, 12), (13, 14), (15, 16), (17, 18), (19, 20)}
        if (a, b) in valid_pairs:
            return [a, b]
        if (b, a) in valid_pairs:
            return [b, a]
        return []

    m = re.fullmatch(r"(\d+)/(\d+)-(\d+)/(\d+)", token)
    if m:
        a1, a2, b1, b2 = map(int, m.groups())
        stereo_pairs = [(9, 10), (11, 12), (13, 14), (15, 16), (17, 18), (19, 20)]
        try:
            i1, i2 = stereo_pairs.index((a1, a2)), stereo_pairs.index((b1, b2))
        except ValueError:
            return []
        if i1 > i2:
            i1, i2 = i2, i1
        out = []
        for p1, p2 in stereo_pairs[i1:i2 + 1]:
            out.extend([p1, p2])
        return out

    if "-" in token and "/" not in token:
        try:
            a, b = map(int, token.split("-"))
        except Exception:
            return []
        if a > b:
            a, b = b, a
        return [n for n in range(a, b + 1) if 1 <= n <= 8]

    return []


def parse_aux_spec(spec):
    if not spec:
        return []

    result, seen = [], set()
    for token in str(spec).split(","):
        for aux_num in expand_aux_token(token):
            if aux_num not in seen:
                result.append(aux_num)
                seen.add(aux_num)
    return result


def normalize_args(args):
    out = []
    for a in args:
        s = str(a).strip()
        if s:
            out.extend(t for t in s.replace(",", " ").split() if t)
    return out


def join_tokens(tokens):
    return ",".join(tokens)


def collect_number_tokens_after_prefix(tokens, prefix):
    out, i, prefix = [], 0, prefix.upper()

    while i < len(tokens):
        t = tokens[i].upper()
        m_full = re.fullmatch(rf"{prefix}([0-9]+)", t)
        if m_full:
            out.append(m_full.group(1))
            i += 1
            continue

        if t == prefix and i + 1 < len(tokens):
            nxt = str(tokens[i + 1]).strip().replace(",", "")
            if re.fullmatch(r"[0-9]+", nxt):
                out.append(nxt)
                i += 2
                continue

        i += 1

    return out


def commands_for_state(key, state):
    object_type, value = key
    return {
        "input": [tf_in_mute(value, state)],
        "aux": [tf_mix_mute(value - 1, state)],
        "matrix": [tf_matrix_mute(value, state)],
        "dca": tf_dca_mute(value, state),
        "fx": tf_fx_mute(value, state),
        "st": tf_stin_pair_mute(value, state),
        "master": tf_master_mute(state),
        "sub": [tf_sub_mute(state)],
    }.get(object_type, [])


def synchronize_states():
    """Apply every pending desired mute/on state as ONE burst (so a scene's re-asserted mutes land
    together instead of trickling out one command at a time)."""
    while not shutdown_event.is_set():
        if get_socket() is None:
            return False

        with state_lock:
            pending = [(k, d) for k, d in desired_mute_state.items()
                       if applied_mute_state.get(k) != d]
        if not pending:
            return True

        cmds = []
        for key, desired in pending:
            log(f"SYNC: {key} {applied_mute_state.get(key)} -> {desired}")
            cmds.extend(commands_for_state(key, desired))
        if not send_many(cmds):
            return False

        with state_lock:
            for key, desired in pending:
                if desired_mute_state.get(key) == desired:
                    applied_mute_state[key] = desired

    return False


def process_pending_scene():
    global applied_scene_generation

    while not shutdown_event.is_set():
        if get_socket() is None:
            return False

        with state_lock:
            current_scene = desired_scene
            current_generation = scene_generation
            already_applied = applied_scene_generation

        if current_scene is None or current_generation == already_applied:
            return True

        bank, num = current_scene
        log(f"SCENE: {bank}{num}")

        if not send(tf_scene(bank, num)):
            return False

        with state_lock:
            applied_mute_state.clear()
            applied_scene_generation = current_generation

        shutdown_event.wait(SCENE_RECALL_DELAY)

        with state_lock:
            if scene_generation != current_generation:
                continue

        return True

    return False


def synchronize_with_tf():
    while not shutdown_event.is_set():
        if get_socket() is None:
            return False

        if not process_pending_scene():
            return False

        if not synchronize_states():
            return False

        with state_lock:
            scene_pending = desired_scene is not None and scene_generation != applied_scene_generation
            state_pending = any(
                desired_mute_state.get(key) != applied_mute_state.get(key)
                for key in desired_mute_state
            )

        if not scene_pending and not state_pending:
            return True

    return False


def update_state(key, state):
    with state_lock:
        old_state = desired_mute_state.get(key)
        desired_mute_state[key] = state

    if old_state != state:
        log(f"DESIRED: {key} {old_state} -> {state}")
        sync_wake.set()


def update_input_states(state, tokens):
    for ch in parse_input_spec(join_tokens(tokens)):
        update_state(("input", ch), state)


def update_st_states(state, tokens):
    nums = collect_number_tokens_after_prefix(tokens, "ST") or [
        str(n) for n in parse_st_spec(join_tokens(tokens))
    ]
    for n in nums:
        try:
            pair_num = int(n)
        except Exception:
            continue
        if 1 <= pair_num <= 2:
            update_state(("st", pair_num), state)


def update_aux_states(state, tokens):
    for aux_num in parse_aux_spec(join_tokens(tokens)):
        update_state(("aux", aux_num), state)


def update_matrix_states(state, tokens):
    for n in parse_matrix_spec(join_tokens(tokens)):
        update_state(("matrix", n), state)


def update_fx_states(state, tokens):
    nums = collect_number_tokens_after_prefix(tokens, "FX") or [
        str(n) for n in parse_fx_spec(join_tokens(tokens))
    ]
    for n in nums:
        try:
            fx_num = int(n)
        except Exception:
            continue
        if fx_num in (1, 2):
            update_state(("fx", fx_num), state)


def update_dca_states(state, tokens):
    for n in parse_dca_spec(join_tokens(tokens)):
        update_state(("dca", n), state)


def handle(state, tokens):
    first = tokens[0].lower()

    if len(tokens) == 1 and first == "master":
        update_state(("master", 0), state)
    elif len(tokens) == 1 and first == "sub":
        update_state(("sub", 0), state)
    elif first == "aux":
        update_aux_states(state, tokens[1:])
    elif first == "matrix":
        update_matrix_states(state, tokens[1:])
    elif first == "dca" or first.startswith("dca"):
        update_dca_states(state, tokens)
    elif first == "fx" or first.startswith("fx"):
        update_fx_states(state, tokens)
    elif first == "st" or first.startswith("st"):
        update_st_states(state, tokens)
    else:
        update_input_states(state, tokens)


def on_mute(addr, *args):
    tokens = normalize_args(args)
    if tokens:
        handle(0, tokens)


def on_unmute(addr, *args):
    tokens = normalize_args(args)
    if tokens:
        handle(1, tokens)


def set_desired_scene(bank, num):
    global desired_scene, scene_generation
    with state_lock:
        desired_scene = (bank, num)
        scene_generation += 1
        generation = scene_generation
    log(f"DESIRED SCENE: {bank}{num} (generation {generation})")
    sync_wake.set()


def on_scene(addr, *args):
    if not args:
        return

    raw = str(args[0]).strip().upper()
    m = re.fullmatch(r"([AB])(\d{1,2})", raw)
    if not m:
        log(f"INVALID SCENE: {raw}")
        return

    bank, num = m.group(1), int(m.group(2))
    if 0 <= num <= 99:
        set_desired_scene(bank, num)


# ============================================================================================
# New OSC vocabulary: /<strip>/<selector>/<param> ...  (direct / fire-and-forget, except scene)
# ============================================================================================

def db_to_raw(db):
    """dB float -> console raw units (hundredths of a dB), with the -inf sentinel and clamping.
    Fader ceiling is +10 dB (raw 1000); anything <= the -inf sentinel snaps to it."""
    if db is None:
        return None
    if db == float("-inf"):
        return NEG_INF
    raw = int(round(db * 100))
    if raw > FADER_MAX:
        raw = FADER_MAX
    if raw <= NEG_INF:
        raw = NEG_INF
    return raw


def parse_db_arg(arg):
    """An OSC arg meaning a dB level: a number, or the strings inf/-inf."""
    if isinstance(arg, str):
        s = arg.strip().lower()
        if s in ("-inf", "inf", "-infinity", "+inf"):
            return float("-inf") if s.startswith("-") else float("inf")
        try:
            return float(s)
        except ValueError:
            return None
    if isinstance(arg, (int, float)):
        return float(arg)
    return None


def curve_shape(name, t):
    n = (name or "linear").strip().lower()
    if n in ("cosine", "cos", "s", "scurve"):
        return 0.5 - 0.5 * math.cos(t * math.pi)
    if n in ("exp", "exponential"):
        return t * t
    return t  # linear / lin / anything else


def plan(from_raw, to_raw, duration_ms, curve):
    """Every value a fade writes, in order, ending exactly on to_raw."""
    tick_ms = int(FADE_TICK * 1000)
    count = duration_ms // max(tick_ms, 1)
    if count <= 1 or from_raw == to_raw:
        return [to_raw]

    ramp_from = MIN_DB_RAW if from_raw == NEG_INF else from_raw
    ramp_to = MIN_DB_RAW if to_raw == NEG_INF else to_raw
    span = ramp_to - ramp_from

    values = []
    for step in range(1, count):
        t = curve_shape(curve, step / count)
        values.append(ramp_from + int(round(span * t)))
    values.append(to_raw)
    return values


def read_level_short(path, index, sub):
    """Read one raw level via a short-lived connection (blocking, reliable on this hardware).
    Used only to give a fade a start value when none is known."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(2.0)
        s.connect((TF_IP, TF_PORT))
        s.sendall(f"get {path} {index} {sub}\r\n".encode("utf-8"))
        deadline = time.monotonic() + 2.0
        buf = ""
        while time.monotonic() < deadline:
            data = s.recv(1024)
            if not data:
                break
            buf += data.decode("utf-8", "replace")
            for line in buf.splitlines():
                if line.startswith(f"OK get {path} {index} {sub}"):
                    s.close()
                    try:
                        return int(line.split()[-1])
                    except ValueError:
                        return None
        s.close()
    except Exception as e:
        log(f"read_level_short failed: {type(e).__name__}: {e}")
    return None


# ---- fade engine -----------------------------------------------------------------------------
# One daemon thread moves *every* active fade in lockstep and writes all of a tick's values as one
# batch, so a 24-channel fade costs the same as a 1-channel one. Fade values are a function of
# elapsed time (not a step counter), so load can never stretch a fade's duration.

class Fade:
    __slots__ = ("path", "idx", "sub", "from_raw", "to_raw", "t0", "dur", "curve", "last")

    def __init__(self, path, idx, sub, from_raw, to_raw, dur_ms, curve, t0=None):
        self.path, self.idx, self.sub = path, idx, sub
        self.from_raw, self.to_raw = from_raw, to_raw
        self.dur, self.curve = dur_ms, curve
        self.t0 = time.monotonic() if t0 is None else t0
        self.last = None


def fade_value(f, now):
    """(value, finished) for fade `f` at monotonic time `now`. Always ends exactly on to_raw; a ramp
    to/from -inf runs on the real dB scale (MIN_DB_RAW) and snaps to the sentinel at the end."""
    elapsed_ms = (now - f.t0) * 1000.0
    if f.dur <= FADE_TICK * 1000 or f.from_raw == f.to_raw or elapsed_ms >= f.dur:
        return f.to_raw, True
    ramp_from = MIN_DB_RAW if f.from_raw == NEG_INF else f.from_raw
    ramp_to = MIN_DB_RAW if f.to_raw == NEG_INF else f.to_raw
    t = curve_shape(f.curve, elapsed_ms / f.dur)
    return ramp_from + int(round((ramp_to - ramp_from) * t)), False


def fade_tick(now=None):
    """Advance every active fade one step and write the changed values as one batch.

    Takes write_lock *before* snapshotting active_fades. Anything that supersedes a fade pops it
    from active_fades (under fade_lock only) *before* it takes write_lock, so a cancelled fade can
    never write after the write that cancelled it. Lock order is always write_lock -> fade_lock."""
    with write_lock:
        now = time.monotonic() if now is None else now
        with fade_lock:
            items = list(active_fades.items())
        if not items:
            return False

        cmds, shadow, done = [], [], []
        for key, f in items:
            value, finished = fade_value(f, now)
            if value != f.last:
                cmds.append(f"set {f.path} {f.idx} {f.sub} {value}")
                shadow.append((key, value))
                f.last = value
            if finished:
                done.append((key, f))

        ok = _raw_send_many(cmds) if cmds else True

        with fade_lock:
            if not ok:
                active_fades.clear()        # link dropped: abandon the fades, like a cancel
            else:
                for key, f in done:
                    if active_fades.get(key) is f:
                        del active_fades[key]

    for key, value in shadow:
        fader_shadow[key] = value
    return True


def _fade_engine():
    while not shutdown_event.is_set():
        with fade_lock:
            busy = bool(active_fades)
        if not busy:
            fade_wake.wait(0.5)
            fade_wake.clear()
            continue
        started = time.monotonic()
        fade_tick()
        shutdown_event.wait(max(0.0, FADE_TICK - (time.monotonic() - started)))


def cancel_fade(key):
    with fade_lock:
        active_fades.pop(key, None)


def cancel_fades(keys):
    with fade_lock:
        for key in keys:
            active_fades.pop(key, None)


def cancel_fades_where(pred):
    with fade_lock:
        doomed = [k for k in active_fades if pred(k)]
        for k in doomed:
            del active_fades[k]
    return len(doomed)


def cancel_all_fades():
    with fade_lock:
        n = len(active_fades)
        active_fades.clear()
    return n


def start_fades(specs):
    """specs: (key, path, idx, sub, to_raw, duration_ms, curve, from_raw|None). Deferred until the
    enclosing preset's batch has been flushed, if there is one."""
    c = getattr(_tls, "collect", None)
    if c is not None:
        c["after"].append(lambda: start_fades_now(specs))
    else:
        start_fades_now(specs)


def start_fades_now(specs):
    # Unknown start levels: our own last write, else one batched read of the console.
    need = [s[0] for s in specs
            if s[7] is None and s[5] > 0 and fader_shadow.get(s[0]) is None]
    levels = get_many(need) if need else {}

    fades = []
    for key, path, idx, sub, to_raw, ms, curve, from_raw in specs:
        if from_raw is None:
            from_raw = fader_shadow.get(key)
        if from_raw is None:
            from_raw = levels.get(key)
        if from_raw is None:
            from_raw = to_raw        # last resort: a jump rather than a wrong-direction sweep
        fades.append((key, Fade(path, idx, sub, from_raw, to_raw, ms, curve)))

    with fade_lock:
        for key, f in fades:
            active_fades[key] = f    # replaces (= cancels) any fade already on this target
    fade_wake.set()


def set_levels_direct(items):
    """items: (path, idx, sub, raw). A direct write cancels any fade on the same target (last
    writer wins): the fades are popped *before* we take write_lock, see fade_tick."""
    if not items:
        return
    cancel_fades([(p, i, s) for p, i, s, _ in items])
    emit([f"set {p} {i} {s} {raw}" for p, i, s, raw in items])
    for p, i, s, raw in items:
        fader_shadow[(p, i, s)] = raw


def set_level_direct(path, index, sub, raw):
    set_levels_direct([(path, index, sub, raw)])


def rel_trim(cur_raw, delta_db):
    """Raw level after nudging by delta_db. -inf only leaves -inf for a positive nudge."""
    if cur_raw == NEG_INF:
        if delta_db <= 0:
            return NEG_INF
        cur_raw = MIN_DB_RAW
    new = cur_raw + int(round(delta_db * 100))
    if new > FADER_MAX:
        return FADER_MAX
    if new < MIN_DB_RAW:
        return NEG_INF
    return new


def current_levels(targets):
    """Best-known raw level per (path, idx, sub): our shadow, else one batched console read."""
    out, missing = {}, []
    for t in targets:
        v = fader_shadow.get(t)
        if v is None:
            missing.append(t)
        else:
            out[t] = v
    if missing:
        for t, v in get_many(missing).items():
            if v is not None:
                out[t] = v
                fader_shadow[t] = v
    return out


# ---- on/off state ----------------------------------------------------------------------------
# on/off for input/mix/matrix/DCA strips goes through the same desired-state map as /tf/mute, so a
# scene recall re-asserts it. Other strips (stin, fxrtn, st, mono) are written directly.
STATE_KEY_NODE = {"InCh": "input", "Mix": "aux", "Mtrx": "matrix", "DCA": "dca"}


def set_on_pairs(node, pairs):
    """pairs: (0-based index, 0|1). Stateful strips are applied by the sync thread as one burst."""
    if not pairs:
        return
    kind = STATE_KEY_NODE.get(node)
    if kind:
        for idx, on in pairs:
            update_state((kind, idx + 1), on)
    else:
        emit([f"set MIXER:Current/{node}/Fader/On {idx} 0 {on}" for idx, on in pairs])


# ---- selectors, values -----------------------------------------------------------------------

def selector_to_indices(strip_node, selector):
    """Resolve a 1-based OSC selector (number, range, list) to 0-based console indices."""
    ones = parse_range_list(selector, 1, STRIP_MAX.get(strip_node, 96))
    return [n - 1 for n in ones]


def split_groups(node, selector):
    """'1-5|12-24' -> one index list per '|' group (kept positional, even when empty)."""
    return [selector_to_indices(node, g) for g in selector.split("|")]


def flat_values(args):
    """Split string args on spaces/commas so 'off on' and ('off', 'on') mean the same."""
    out = []
    for a in args:
        if isinstance(a, str):
            out.extend(a.replace(",", " ").split())
        else:
            out.append(a)
    return out


def norm_tokens(args):
    out = []
    for a in args:
        if isinstance(a, float) and a.is_integer():
            a = int(a)
        out.extend(t for t in str(a).replace(",", " ").split() if t)
    return out


def values_per_group(vals, n, addr=""):
    """Pair values to '|' groups by position; one value applies to every group."""
    if not vals:
        return [None] * n
    if len(vals) == 1:
        return list(vals) * n
    if len(vals) > n:
        log(f"{addr}: {len(vals)} values for {n} group(s); extras ignored: {vals[n:]}")
    return [vals[i] if i < len(vals) else None for i in range(n)]


def _truthy(arg):
    if isinstance(arg, str):
        s = arg.strip().lower()
        if s in ("on", "true", "yes", "t", "y"):
            return True
        try:
            return float(s) != 0
        except ValueError:
            return False
    return bool(arg)


def _level_path(node):
    return f"MIXER:Current/{node}/Fader/Level"


def on_osc(addr, *args):
    """Default handler: the /<strip>/<sel>/<param> grammar plus /scene, /mutegroup, /preset,
    /snapshot and /panic. A message's console writes go out as one batch."""
    seg = addr.rstrip("/").strip("/").split("/")

    try:
        if seg == ["panic"] or seg == ["fade", "stop"]:
            log(f"FADES STOPPED: {cancel_all_fades()}")
            return

        # /scene/recall <a|b> <n>   (stateful, like /tf/scene)
        if seg[:2] == ["scene", "recall"]:
            if len(args) >= 2:
                bank = str(args[0]).strip().upper()[:1]
                num = int(float(args[1]))
                if bank in ("A", "B") and 0 <= num <= 99:
                    set_desired_scene(bank, num)
            return

        # /mutegroup/<sel> <0|1>   (direct; '|' groups take one value each)
        if len(seg) == 2 and seg[0] == "mutegroup":
            groups = [parse_range_list(g, 1, 6) for g in seg[1].split("|")]
            vals = values_per_group(flat_values(args), len(groups), addr)
            cmds = []
            for g, v in zip(groups, vals):
                if v is not None:
                    on = 1 if _truthy(v) else 0
                    cmds += [f"set MIXER:Current/MuteMaster/On {n - 1} 0 {on}" for n in g]
            emit(cmds)
            return

        if len(seg) == 2 and seg[0] == "preset":
            run_preset(seg[1])
            return

        if len(seg) == 2 and seg[0] == "snapshot":
            snapshot_command(seg[1], args)
            return

        if len(seg) < 3:
            log(f"UNMATCHED OSC: {addr} {args}")
            return

        strip_word, selector, rest = seg[0], seg[1], seg[2:]
        node = STRIP_PATHS.get(strip_word)
        if node is None:
            log(f"UNMATCHED OSC (strip): {addr}")
            return
        groups = split_groups(node, selector)
        flat_idx = list(dict.fromkeys(i for g in groups for i in g))
        if not flat_idx:
            log(f"UNMATCHED OSC (selector): {addr}")
            return

        param = rest[0]
        path = _level_path(node)

        if param == "fader" and rest == ["fader"]:
            vals = values_per_group(flat_values(args), len(groups), addr)
            items = []
            for g, v in zip(groups, vals):
                if v is None:
                    continue
                db = parse_db_arg(v)
                if db is None:
                    log(f"BAD fader arg: {addr} {args}")
                    return
                items += [(path, i, 0, db_to_raw(db)) for i in g]
            set_levels_direct(items)

        elif param == "fader" and rest == ["fader", "raw"]:
            vals = values_per_group(flat_values(args), len(groups), addr)
            items = []
            for g, v in zip(groups, vals):
                if v is not None:
                    raw = max(NEG_INF, min(FADER_MAX, int(float(v))))   # same ceiling as dB writes
                    items += [(path, i, 0, raw) for i in g]
            set_levels_direct(items)

        elif param == "fader" and rest == ["fader", "rel"]:
            vals = values_per_group(flat_values(args), len(groups), addr)
            deltas = {}
            for g, v in zip(groups, vals):
                if v is None:
                    continue
                d = parse_db_arg(v)
                if d is None or d in (float("inf"), float("-inf")):
                    log(f"BAD fader/rel arg: {addr} {args}")
                    return
                for i in g:
                    deltas[(path, i, 0)] = d
            cur = current_levels(list(deltas))
            items = [(p, i, s, rel_trim(cur[(p, i, s)], d))
                     for (p, i, s), d in deltas.items() if (p, i, s) in cur]
            if len(items) != len(deltas):
                log(f"{addr}: no known level for {len(deltas) - len(items)} target(s); skipped")
            set_levels_direct(items)

        elif param == "fade" and rest == ["fade", "stop"]:
            idxset, prefix = set(flat_idx), f"MIXER:Current/{node}/"
            n = cancel_fades_where(lambda k: k[0].startswith(prefix) and k[1] in idxset)
            log(f"FADES STOPPED: {n}")

        elif param == "fade" and rest == ["fade"]:
            spec = parse_fade_args(args)
            if spec is None:
                log(f"BAD fade args: {addr} {args}")
                return
            to_raw, ms, curve, from_raw = spec
            start_fades([((path, i, 0), path, i, 0, to_raw, ms, curve, from_raw)
                         for i in flat_idx])

        elif param in ("on", "mute") and len(rest) == 1:
            vals = values_per_group(flat_values(args), len(groups), addr)
            pairs = []
            for g, v in zip(groups, vals):
                if v is None:
                    continue
                on = 1 if _truthy(v) else 0
                if param == "mute":
                    on = 1 - on          # mute 1 == on 0
                pairs += [(i, on) for i in g]
            set_on_pairs(node, pairs)

        elif param == "only" and len(rest) == 1:
            # Everything in the scope goes off except the listed numbers, which go on.
            if len(groups) != 1:
                log(f"{addr}: 'only' takes a single selector, not '|' groups")
                return
            keep = parse_range_list(join_tokens(norm_tokens(args)), 1, STRIP_MAX.get(node, 96))
            scope = groups[0]
            scope_ones = {i + 1 for i in scope}
            stray = [n for n in keep if n not in scope_ones]
            if stray:
                log(f"{addr}: {stray} outside the selector; ignored")
            keepset = set(keep) & scope_ones
            set_on_pairs(node, [(i, 1 if i + 1 in keepset else 0) for i in scope])

        elif param == "name" and len(rest) == 1:
            if not args:
                return
            vals = values_per_group(list(args), len(groups), addr)
            cmds = []
            for g, v in zip(groups, vals):
                if v is not None:
                    text = str(v).replace('"', "'")
                    cmds += [f'set MIXER:Current/{node}/Label/Name {i} 0 "{text}"' for i in g]
            emit(cmds)

        elif param in ("color", "colour") and len(rest) == 1:
            vals = values_per_group(flat_values(args), len(groups), addr)
            cmds = []
            for g, v in zip(groups, vals):
                if v is None:
                    continue
                col = str(v).strip()
                if col.lower() not in LABEL_COLORS:
                    log(f"BAD colour: {col}")
                    return
                cmds += [f'set MIXER:Current/{node}/Label/Color {i} 0 "{col}"' for i in g]
            emit(cmds)

        elif param == "send" and len(rest) == 4:
            _handle_send(node, groups, flat_idx, rest, args, addr)

        else:
            log(f"UNMATCHED OSC (param): {addr}")

    except Exception as e:
        log(f"OSC handler error on {addr} {args}: {type(e).__name__}: {e}")


SEND_LEAF = {
    "InCh": "ToMix", "StInCh": "ToMix", "FxRtnCh": "ToMix",
    "Mix": "ToMtrx", "St": "ToMtrx", "Mono": "ToMtrx",
}


def _handle_send(node, groups, flat_idx, rest, args, addr):
    _, bus, dest_sel, leaf = rest
    leaf_node = SEND_LEAF.get(node)
    if leaf_node is None:
        log(f"{addr}: {node} strips have no sends")
        return
    expected_bus = "mix" if leaf_node == "ToMix" else "mtrx"
    if bus not in (expected_bus, "matrix" if expected_bus == "mtrx" else expected_bus):
        log(f"{addr}: this strip sends to {expected_bus}, not {bus!r}")
        return
    dest_max = MAX_MIX if leaf_node == "ToMix" else MAX_MATRIX
    dests = [d - 1 for d in parse_range_list(dest_sel, 1, dest_max)]
    lpath = f"MIXER:Current/{node}/{leaf_node}/Level"

    if leaf == "fader":
        vals = values_per_group(flat_values(args), len(groups), addr)
        items = []
        for g, v in zip(groups, vals):
            if v is None:
                continue
            db = parse_db_arg(v)
            if db is None:
                log(f"BAD send level: {addr} {args}")
                return
            items += [(lpath, i, d, db_to_raw(db)) for i in g for d in dests]
        set_levels_direct(items)
    elif leaf == "on":
        vals = values_per_group(flat_values(args), len(groups), addr)
        cmds = []
        for g, v in zip(groups, vals):
            if v is not None:
                on = 1 if _truthy(v) else 0
                cmds += [f"set MIXER:Current/{node}/{leaf_node}/On {i} {d} {on}"
                         for i in g for d in dests]
        emit(cmds)
    elif leaf == "fade":
        spec = parse_fade_args(args)
        if spec is None:
            log(f"BAD fade args: {addr} {args}")
            return
        to_raw, ms, curve, from_raw = spec
        start_fades([((lpath, i, d), lpath, i, d, to_raw, ms, curve, from_raw)
                     for i in flat_idx for d in dests])
    else:
        log(f"UNMATCHED send leaf: {addr}")


CURVE_NAMES = {"linear", "lin", "cosine", "cos", "s", "scurve", "exp", "exponential"}


def parse_fade_args(args):
    """<to_db> <duration_ms> [curve] [from_db], curve/from in either order. -> (to_raw, ms, curve, from_raw)."""
    if len(args) < 2 or len(args) > 4:
        return None
    to_db = parse_db_arg(args[0])
    if to_db is None:
        return None
    try:
        ms = int(float(args[1]))
    except (ValueError, TypeError):
        return None
    curve = "linear"
    from_raw = None
    for extra in args[2:]:
        if isinstance(extra, str) and extra.strip().lower() in CURVE_NAMES:
            curve = extra
        else:
            fdb = parse_db_arg(extra)
            if fdb is not None:
                from_raw = db_to_raw(fdb)
    return db_to_raw(to_db), ms, curve, from_raw


# ---- presets: named lists of OSC lines, fired as one batch -------------------------------------
# presets.json: {"act1-sc2": ["/ch/1-24/only 3,7,12", "/dca/1/fade 0 1500"], ...}

_NUM_RE = re.compile(r"[+-]?\d+(\.\d+)?")


def _coerce(tok):
    if _NUM_RE.fullmatch(tok):
        return float(tok) if "." in tok else int(tok)
    return tok


def parse_osc_line(line):
    """'/ch/5/name "Lead Vox"' -> ('/ch/5/name', ['Lead Vox']); numeric tokens become numbers."""
    parts = shlex.split(line)
    if not parts or not parts[0].startswith("/"):
        raise ValueError(f"not an OSC line: {line!r}")
    return parts[0], [_coerce(p) for p in parts[1:]]


def dispatch(addr, *args):
    if addr == "/tf/mute":
        on_mute(addr, *args)
    elif addr == "/tf/unmute":
        on_unmute(addr, *args)
    elif addr == "/tf/scene":
        on_scene(addr, *args)
    else:
        on_osc(addr, *args)


def load_presets(force=False):
    global presets, presets_mtime
    try:
        mtime = os.stat(PRESETS_PATH).st_mtime
    except OSError:
        return
    if not force and mtime == presets_mtime:
        return
    presets_mtime = mtime
    try:
        with open(PRESETS_PATH, encoding="utf-8") as fh:
            data = json.load(fh)
        if not isinstance(data, dict) or not all(
                isinstance(v, list) and all(isinstance(x, str) for x in v) for v in data.values()):
            raise ValueError("expected {name: [osc line, ...]}")
        presets = data
        log(f"PRESETS: loaded {len(presets)} from {PRESETS_PATH}")
    except Exception as e:
        log(f"PRESETS: {PRESETS_PATH} not loaded ({type(e).__name__}: {e}); keeping previous")


def run_preset(name):
    if name == "reload":
        load_presets(force=True)
        return
    if getattr(_tls, "collect", None) is not None:
        log(f"PRESET {name}: nested presets are not allowed")
        return
    load_presets()
    lines = presets.get(name)
    if lines is None:
        log(f"PRESET {name}: not found")
        return
    log(f"PRESET {name}: {len(lines)} line(s)")
    begin_collect()
    try:
        for line in lines:
            try:
                addr, args = parse_osc_line(line)
            except ValueError as e:
                log(f"PRESET {name}: {e}")
                continue
            if addr.startswith("/preset/"):
                log(f"PRESET {name}: nested presets are not allowed")
                continue
            dispatch(addr, *args)
    finally:
        end_collect()


# ---- snapshots: save levels + on-states, recall (optionally as a crossfade) -------------------

def load_snapshots():
    global snapshots
    try:
        with open(SNAPSHOTS_PATH, encoding="utf-8") as fh:
            snapshots = json.load(fh)
        log(f"SNAPSHOTS: loaded {len(snapshots)} from {SNAPSHOTS_PATH}")
    except FileNotFoundError:
        pass
    except Exception as e:
        log(f"SNAPSHOTS: {SNAPSHOTS_PATH} not loaded ({type(e).__name__}: {e})")


def _store_snapshots():
    tmp = SNAPSHOTS_PATH + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(snapshots, fh, indent=1)
    os.replace(tmp, SNAPSHOTS_PATH)


def snapshot_command(action, args):
    if action == "save":
        vals = flat_values(args)
        if len(vals) < 2:
            log("SNAPSHOT save: need <name> <strip/sel> ...")
            return
        name, targets = str(vals[0]), []
        for tok in vals[1:]:
            word, _, sel = str(tok).strip("/").partition("/")
            node = STRIP_PATHS.get(word)
            if node is None or not sel:
                log(f"SNAPSHOT save: bad target {tok!r} (want e.g. ch/1-24)")
                continue
            for g in split_groups(node, sel):
                targets += [(node, i) for i in g]
        targets = list(dict.fromkeys(targets))
        queries = []
        for node, i in targets:
            queries += [(_level_path(node), i, 0), (f"MIXER:Current/{node}/Fader/On", i, 0)]
        got = get_many(queries)
        items = [[node, i, got.get((_level_path(node), i, 0)),
                  got.get((f"MIXER:Current/{node}/Fader/On", i, 0))] for node, i in targets]
        with snap_lock:
            snapshots[name] = {"items": items}
            _store_snapshots()
        log(f"SNAPSHOT saved {name!r}: {len(items)} strip(s)")

    elif action == "recall":
        if not args:
            return
        name = str(args[0])
        with snap_lock:
            snap = snapshots.get(name)
        if snap is None:
            log(f"SNAPSHOT {name!r}: not found")
            return
        ms, curve = 0, "linear"
        for extra in args[1:]:
            if isinstance(extra, str) and extra.strip().lower() in CURVE_NAMES:
                curve = extra
            else:
                try:
                    ms = int(float(extra))
                except (TypeError, ValueError):
                    pass
        levels, fades, on_by_node = [], [], {}
        for node, i, level, on in snap["items"]:
            path = _level_path(node)
            if level is not None:
                if ms > 0:
                    fades.append(((path, i, 0), path, i, 0, level, ms, curve, None))
                else:
                    levels.append((path, i, 0, level))
            if on is not None:
                on_by_node.setdefault(node, []).append((i, 1 if on else 0))
        set_levels_direct(levels)
        start_fades(fades)
        for node, pairs in on_by_node.items():
            set_on_pairs(node, pairs)
        log(f"SNAPSHOT recalled {name!r} ({'fade ' + str(ms) + ' ms' if ms > 0 else 'jump'})")

    else:
        log(f"UNMATCHED snapshot action: {action}")


def on_unknown(addr, *args):
    on_osc(addr, *args)


def reconnect_loop():
    global applied_scene_generation

    last_connection_generation = 0

    log("TF3 connection manager started")
    log(f"Target TF3: {TF_IP}:{TF_PORT}")
    log(f"OSC: {OSC_IP}:{OSC_PORT}")

    while not shutdown_event.is_set():
        if not is_tf_network():
            disconnect()
            set_connection_state("NO_TF_NETWORK")
            shutdown_event.wait(RECONNECT_DELAY)
            continue

        if get_socket() is None and not connect():
            shutdown_event.wait(RECONNECT_DELAY)
            continue

        current_generation = connection_generation

        if current_generation != last_connection_generation:
            log("NEW TF3 CONNECTION: invalidating applied state")

            with state_lock:
                applied_mute_state.clear()
                applied_scene_generation = 0

            last_connection_generation = current_generation

            if not synchronize_with_tf():
                log("Initial synchronization failed")
                disconnect()
                continue

        sync_wake.clear()
        if not synchronize_with_tf():
            if get_socket() is None:
                continue
            log("Synchronization failed")
            disconnect()
            continue

        sync_wake.wait(CONNECTION_CHECK_INTERVAL)


class ReusableOSCUDPServer(osc_server.ThreadingOSCUDPServer):
    allow_reuse_address = True


def main():
    log("Starting TF bridge OSC server...")

    threading.Thread(
        target=reconnect_loop,
        name="TF3-ConnectionManager",
        daemon=True
    ).start()

    threading.Thread(target=_fade_engine, name="TF3-FadeEngine", daemon=True).start()
    load_presets()
    load_snapshots()

    disp = Dispatcher()
    disp.map("/tf/mute", on_mute)
    disp.map("/tf/unmute", on_unmute)
    disp.map("/tf/scene", on_scene)
    disp.set_default_handler(on_osc)

    server = ReusableOSCUDPServer((OSC_IP, OSC_PORT), disp)
    log(f"OSC server listening on {OSC_IP}:{OSC_PORT}")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        log("Shutdown requested")
    finally:
        shutdown_event.set()
        server.server_close()
        disconnect()
        log("TF bridge OSC server stopped")


if __name__ == "__main__":
    main()
