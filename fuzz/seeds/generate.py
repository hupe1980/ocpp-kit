#!/usr/bin/env python3
"""Generate the raw seed inputs for the four fuzz targets.

    python3 -I fuzz/seeds/generate.py      # writes fuzz/seeds/out/<target>/
    # then merge-minimise into fuzz/corpus/<target>/ — the commands are in fuzz/README.md

Standard library only, deterministic, and synthetic: every identity, token and serial number
below is made up. Payloads are synthesised from the vendored OCA schemas in `schemas/`, so the
seeds follow the schemas when those are updated.
"""

import json
import os
import re
import shutil
import struct
import sys
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OUT = os.path.join(ROOT, "fuzz", "seeds", "out")
SCHEMAS = os.path.join(ROOT, "schemas")
TIMESTAMP = "2026-11-07T12:00:00Z"


# ---------------------------------------------------------------------------------------------
# schema -> instance
# ---------------------------------------------------------------------------------------------


def resolve(schema, root):
    while "$ref" in schema:
        path = schema["$ref"].lstrip("#/").split("/")
        node = root
        for part in path:
            node = node[part]
        schema = node
    return schema


def instance(schema, root, full, depth=0):
    """A value the schema accepts: required members only, or (full) every member."""
    schema = resolve(schema, root)
    if "enum" in schema:
        return schema["enum"][0]
    kind = schema.get("type", "object")
    if isinstance(kind, list):
        kind = kind[0]
    if kind == "object":
        out = {}
        required = set(schema.get("required", []))
        for name, member in schema.get("properties", {}).items():
            if name == "customData" and not full:
                continue
            if name in required or (full and depth < 4):
                out[name] = instance(member, root, full, depth + 1)
        return out
    if kind == "array":
        count = max(1, schema.get("minItems", 1))
        return [instance(schema.get("items", {}), root, full, depth + 1) for _ in range(count)]
    if kind == "string":
        if schema.get("format") == "date-time":
            return TIMESTAMP
        length = min(schema.get("maxLength", 8), 8)
        return "SYNTH-01"[:max(1, length)]
    if kind == "integer":
        return max(1, schema.get("minimum", 1))
    if kind == "number":
        return max(1.5, schema.get("minimum", 1.5))
    if kind == "boolean":
        return True
    return None


def schema_files(version):
    folder = os.path.join(SCHEMAS, version)
    return sorted(name for name in os.listdir(folder) if name.endswith(".json"))


def payload(version, name, full=False):
    with open(os.path.join(SCHEMAS, version, name), encoding="utf-8") as handle:
        schema = json.load(handle)
    return instance(schema, schema, full)


def request_files(version):
    if version == "v1_6":
        return [n for n in schema_files(version) if not n.endswith("Response.json")]
    return [n for n in schema_files(version) if n.endswith("Request.json")]


def action_of(version, file_name):
    stem = file_name[: -len(".json")]
    return stem[: -len("Request")] if version != "v1_6" else stem


def response_file(version, action):
    return f"{action}Response.json"


def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


# ---------------------------------------------------------------------------------------------
# targets
# ---------------------------------------------------------------------------------------------


def write(target, name, data):
    folder = os.path.join(OUT, target)
    os.makedirs(folder, exist_ok=True)
    if isinstance(data, str):
        data = data.encode("utf-8")
    with open(os.path.join(folder, name), "wb") as handle:
        handle.write(data)


def v2_1_action_order():
    """`v2_1::Action::ALL`, read from the generated source: the payload target's selector."""
    with open(os.path.join(ROOT, "src", "v2_1", "action.rs"), encoding="utf-8") as handle:
        text = handle.read()
    block = text.split("pub const ALL: &'static [Action] = &[", 1)[1].split("];", 1)[0]
    return re.findall(r"Action::(\w+),", block)


def payload_seeds():
    order = v2_1_action_order()
    # Every third action with its required members, and every fifteenth also with every
    # member and as a response: ~50 files. The fuzzer reaches the rest of the catalogue by
    # mutating the selector byte, which is why one seed per action is not needed.
    for index, file_name in enumerate(request_files("v2_1")):
        if index % 3 != 0:
            continue
        action = action_of("v2_1", file_name)
        selector = order.index(action)
        for full in (False, True) if index % 15 == 0 else (False,):
            body = compact(payload("v2_1", file_name, full))
            write("payload", f"{action}-req-{'full' if full else 'min'}", bytes([selector]) + body.encode())
        response = response_file("v2_1", action)
        if index % 15 == 0 and os.path.exists(os.path.join(SCHEMAS, "v2_1", response)):
            body = compact(payload("v2_1", response, False))
            write("payload", f"{action}-resp-min", bytes([selector]) + body.encode())
    # Near-valid shapes the lenient repair loop works on.
    boot = order.index("BootNotification")
    meter = order.index("MeterValues")
    write("payload", "quirk-offsetless-time", bytes([boot]) + b'{"reason":"PowerUp","chargingStation":{"model":"M","vendorName":"V"},"x":"2026-11-07T12:00:00"}')
    write("payload", "quirk-numeric-string", bytes([meter]) + b'{"evseId":"1","meterValue":[{"timestamp":"2026-11-07 12:00:00","sampledValue":[{"value":"20000.0"}]}]}')
    write("payload", "quirk-whole-float", bytes([meter]) + b'{"evseId":1.0,"meterValue":[{"timestamp":"2026-11-07T12:00:00Z","sampledValue":[{"value":2e4}]}]}')
    write("payload", "quirk-unknown-enum", bytes([boot]) + b'{"reason":"Reboot2","chargingStation":{"model":"M","vendorName":"V"}}')
    write("payload", "not-an-object", bytes([boot]) + b'[1,"2",null]')


def frame_seeds():
    versions = {"v1_6": "1.6", "v2_0_1": "2.0.1", "v2_1": "2.1"}
    for version, label in versions.items():
        files = request_files(version)
        for file_name in files[:: max(1, len(files) // 8)]:
            action = action_of(version, file_name)
            body = payload(version, file_name, False)
            write("frame", f"{label}-call-{action}", compact([2, f"{label}-{action}", action, body]))
            response = response_file(version, action)
            if os.path.exists(os.path.join(SCHEMAS, version, response)):
                write("frame", f"{label}-result-{action}", compact([3, f"{label}-{action}", payload(version, response, False)]))
        write("frame", f"{label}-callerror", compact([4, "e-1", "FormationViolation" if label == "1.6" else "FormatViolation", "bad", {}]))
        write("frame", f"{label}-callerror-empty", compact([4, "e-2", "GenericError", "", {}]))
    # 2.1-only message types.
    write("frame", "2.1-send", compact([6, "s-1", "NotifyPeriodicEventStream", {"id": 1, "pending": 0, "basetime": TIMESTAMP, "data": [{"t": 0.5, "v": "1"}]}]))
    write("frame", "2.1-callresulterror", compact([5, "r-1", "InternalError", "late", {"x": 1}]))
    # Near-valid frames: each breaks exactly one rule.
    near = {
        "id-number": '[2,1,"Heartbeat",{}]',
        "id-too-long": '[2,"' + "a" * 37 + '","Heartbeat",{}]',
        "missing-payload": '[2,"1","Heartbeat"]',
        "extra-element": '[2,"1","Heartbeat",{},{}]',
        "unknown-type": '[7,"1","Heartbeat",{}]',
        "type-string": '["2","1","Heartbeat",{}]',
        "payload-array": '[2,"1","Heartbeat",[]]',
        "payload-null": '[3,"1",null]',
        "trailing-garbage": '[2,"1","Heartbeat",{}]x',
        "not-array": '{"2":"1"}',
        "empty": "",
        "escapes": '[2,"\\u0031","Heart\\u0062eat",{"k":"\\ud83d\\ude00"}]',
        "error-details-null": '[4,"1","GenericError","d",null]',
        "error-description-null": '[4,"1","GenericError",null,{}]',
        "error-unknown-code": '[4,"1","NoSuchCode","d",{}]',
        "deep-nesting": '[2,"1","DataTransfer",{"vendorId":"v","data":' + "[" * 64 + "]" * 64 + "}]",
        "big-number": '[2,"1","MeterValues",{"connectorId":1e400,"meterValue":[]}]',
        "whitespace": ' \n[ 2 , "1" , "Heartbeat" , { } ] \t',
    }
    for name, text in near.items():
        write("frame", f"near-{name}", text)


def engine_seeds():
    hb = '[2,"{id}","Heartbeat",{{}}]'
    boot = compact([2, "b-1", "BootNotification", payload("v2_1", "BootNotificationRequest.json", False)])
    boot_ok = compact([3, "b-1", payload("v2_1", "BootNotificationResponse.json", False)])
    sequences = {
        "boot-then-heartbeats": [boot, hb.format(id="h-1"), hb.format(id="h-2")],
        "result-for-unknown-call": ['[3,"never-sent",{}]', '[4,"never-sent","GenericError","",{}]'],
        "duplicate-id": [hb.format(id="dup"), hb.format(id="dup")],
        "boot-result": [boot_ok, boot],
        "send-frame": ['[6,"s-1","NotifyPeriodicEventStream",{"id":1,"pending":0,"basetime":"2026-11-07T12:00:00Z","data":[]}]'],
        "callresulterror": ['[5,"x","GenericError","",{}]'],
        "garbage-then-call": ["not json", "[", '[2,"1"]', hb.format(id="ok")],
        "unknown-action": ['[2,"u-1","NoSuchAction",{}]'],
        "invalid-payload": ['[2,"i-1","BootNotification",{"reason":"Nope"}]'],
        "rejected-1.6-error-code": ['[4,"1","FormationViolation","",{}]'],
        "many-calls": [hb.format(id=f"m-{n}") for n in range(12)],
    }
    for name, lines in sequences.items():
        write("engine", name, "\n".join(lines) + "\n")
    # A selection of every 2.1 request, one per line, as a peer might send them.
    calls = []
    for file_name in request_files("v2_1")[::6]:
        action = action_of("v2_1", file_name)
        calls.append(compact([2, f"c-{action}"[:36], action, payload("v2_1", file_name, False)]))
    write("engine", "catalogue", "\n".join(calls) + "\n")


def ws_frame(opcode, payload_bytes, fin=True, mask=None, rsv1=False):
    head = (0x80 if fin else 0) | (0x40 if rsv1 else 0) | opcode
    length = len(payload_bytes)
    mask_bit = 0x80 if mask is not None else 0
    if length < 126:
        out = struct.pack("!BB", head, mask_bit | length)
    elif length < 65536:
        out = struct.pack("!BBH", head, mask_bit | 126, length)
    else:
        out = struct.pack("!BBQ", head, mask_bit | 127, length)
    if mask is not None:
        out += mask
        payload_bytes = bytes(b ^ mask[i % 4] for i, b in enumerate(payload_bytes))
    return out + payload_bytes


def deflate(data):
    compressor = zlib.compressobj(zlib.Z_DEFAULT_COMPRESSION, zlib.DEFLATED, -15)
    out = compressor.compress(data) + compressor.flush(zlib.Z_SYNC_FLUSH)
    return out[:-4]  # RFC 7692 §7.2.1: drop the trailing 0x00 0x00 0xff 0xff


def websocket_seeds():
    key = b"\x37\xfa\x21\x3d"
    text = b'[2,"1","Heartbeat",{}]'
    big = ('[2,"2","DataTransfer",{"vendorId":"v","data":"' + "x" * 300 + '"}]').encode()
    close = struct.pack("!H", 1000) + b"bye"
    for role, mask in (("client-to-server", key), ("server-to-client", None)):
        seeds = {
            "text": ws_frame(0x1, text, mask=mask),
            "text-16bit-length": ws_frame(0x1, big, mask=mask),
            "binary": ws_frame(0x2, b"\x00\x01\x02", mask=mask),
            "fragmented": ws_frame(0x1, text[:5], fin=False, mask=mask)
            + ws_frame(0x0, text[5:12], fin=False, mask=mask)
            + ws_frame(0x0, text[12:], mask=mask),
            "fragmented-with-ping": ws_frame(0x1, text[:5], fin=False, mask=mask)
            + ws_frame(0x9, b"p", mask=mask)
            + ws_frame(0x0, text[5:], mask=mask),
            "ping": ws_frame(0x9, b"ping", mask=mask),
            "pong": ws_frame(0xA, b"", mask=mask),
            "close": ws_frame(0x8, close, mask=mask),
            "close-empty": ws_frame(0x8, b"", mask=mask),
            "close-bad-code": ws_frame(0x8, struct.pack("!H", 1005), mask=mask),
            "close-one-byte": ws_frame(0x8, b"\x03", mask=mask),
            "deflate": ws_frame(0x1, deflate(big), mask=mask, rsv1=True),
            "deflate-fragmented": ws_frame(0x1, deflate(big)[:10], fin=False, mask=mask, rsv1=True)
            + ws_frame(0x0, deflate(big)[10:], mask=mask),
            "two-messages": ws_frame(0x1, text, mask=mask) + ws_frame(0x1, b"[3,\"1\",{}]", mask=mask),
            "invalid-utf8": ws_frame(0x1, b"\xc3\x28", mask=mask),
            "reserved-opcode": ws_frame(0x3, b"", mask=mask),
            "ping-too-long": ws_frame(0x9, b"x" * 126, mask=mask),
            "control-fragmented": ws_frame(0x9, b"p", fin=False, mask=mask),
            "continuation-first": ws_frame(0x0, text, mask=mask),
            "truncated": ws_frame(0x1, text, mask=mask)[:-3],
        }
        for name, data in seeds.items():
            write("websocket", f"{role}-{name}", data)
    # Length forms that no well-behaved peer sends.
    write("websocket", "length-64bit-small", struct.pack("!BBQ", 0x81, 0xFF, 5) + key + b"hello")
    write("websocket", "length-64bit-huge", struct.pack("!BBQ", 0x81, 0xFF, 1 << 62) + key)
    write("websocket", "length-64bit-msb", struct.pack("!BBQ", 0x81, 0xFF, 1 << 63) + key)


def main():
    if os.path.isdir(OUT):
        shutil.rmtree(OUT)
    payload_seeds()
    frame_seeds()
    engine_seeds()
    websocket_seeds()
    for target in sorted(os.listdir(OUT)):
        files = os.listdir(os.path.join(OUT, target))
        size = sum(os.path.getsize(os.path.join(OUT, target, f)) for f in files)
        print(f"{target}: {len(files)} seeds, {size} bytes", file=sys.stderr)


if __name__ == "__main__":
    main()
