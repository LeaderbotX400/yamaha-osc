#!/usr/bin/env python3
import re
import socket
import subprocess
import threading

from pythonosc.dispatcher import Dispatcher
from pythonosc import osc_server

TF_IP, TF_PORT = "192.168.50.2", 49280
OSC_IP, OSC_PORT = "0.0.0.0", 8000

MAX_INPUT_CH, MAX_MIX, MAX_MATRIX, MAX_DCA = 40, 20, 4, 8

RECONNECT_DELAY = 2.0
CONNECT_TIMEOUT = 2.0
CONNECTION_CHECK_INTERVAL = 1.0
SCENE_RECALL_DELAY = 0.3
STATE_COMMAND_DELAY = 0.01

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


def log(msg):
    print(msg, flush=True)


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


def send(cmd):
    s = get_socket()
    if s is None:
        return False

    try:
        log(f"SEND: {cmd}")
        s.sendall((cmd + "\r\n").encode("utf-8"))
        return True
    except Exception as e:
        log(f"Send failed: {type(e).__name__}: {e}")
        disconnect()
        return False


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


def apply_state_key(key):
    with state_lock:
        if key not in desired_mute_state:
            return True
        desired = desired_mute_state[key]
        applied = applied_mute_state.get(key)

    if applied == desired:
        return True

    for cmd in commands_for_state(key, desired):
        if not send(cmd):
            return False
        shutdown_event.wait(STATE_COMMAND_DELAY)

    with state_lock:
        if desired_mute_state.get(key) == desired:
            applied_mute_state[key] = desired

    return True


def synchronize_states():
    while not shutdown_event.is_set():
        if get_socket() is None:
            return False

        with state_lock:
            keys = list(desired_mute_state.keys())

        changed = False

        for key in keys:
            if get_socket() is None:
                return False

            with state_lock:
                desired = desired_mute_state.get(key)
                applied = applied_mute_state.get(key)

            if desired == applied:
                continue

            changed = True
            log(f"SYNC: {key} {applied} -> {desired}")

            if not apply_state_key(key):
                return False

        with state_lock:
            pending = any(
                desired_mute_state.get(key) != applied_mute_state.get(key)
                for key in desired_mute_state
            )

        if not pending or not changed:
            return True

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


def on_scene(addr, *args):
    global desired_scene, scene_generation

    if not args:
        return

    raw = str(args[0]).strip().upper()
    m = re.fullmatch(r"([AB])(\d{1,2})", raw)
    if not m:
        log(f"INVALID SCENE: {raw}")
        return

    bank, num = m.group(1), int(m.group(2))
    if 0 <= num <= 99:
        with state_lock:
            desired_scene = (bank, num)
            scene_generation += 1
            generation = scene_generation

        log(f"DESIRED SCENE: {bank}{num} (generation {generation})")


def on_unknown(addr, *args):
    log(f"UNMATCHED OSC: {addr} {args}")


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

        if not synchronize_with_tf():
            if get_socket() is None:
                continue
            log("Synchronization failed")
            disconnect()
            continue

        shutdown_event.wait(CONNECTION_CHECK_INTERVAL)


class ReusableOSCUDPServer(osc_server.ThreadingOSCUDPServer):
    allow_reuse_address = True


def main():
    log("Starting TF3 OSC server...")

    threading.Thread(
        target=reconnect_loop,
        name="TF3-ConnectionManager",
        daemon=True
    ).start()

    disp = Dispatcher()
    disp.map("/tf/mute", on_mute)
    disp.map("/tf/unmute", on_unmute)
    disp.map("/tf/scene", on_scene)
    disp.set_default_handler(on_unknown)

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
        log("TF3 OSC server stopped")


if __name__ == "__main__":
    main()
