#!/usr/bin/env python3
"""Writes the transfers-v2 scenario transcripts S1-S7 (JSON) next to this file.

The transcripts are the shared contract test between the desk and the phone:
each side replays them against its own engine and asserts the same outputs.
This script uses nothing of either implementation (only hashlib), so a
transcript is also an independent check of both.

Run:  python3 generate.py   (rewrites the .json files in this directory)
"""
import hashlib, json, os

BLOCK = 1 << 20
CAP = 61440 - 40 - 32          # payload bytes per frame
ID = "0f8fad5b-d9cb-469f-a165-70867728950e"
TOKEN = "00112233445566778899aabbccddeeff"
SENDER, RECEIVER = "sender-device", "receiver-device"
SESSION, GENERATION = 9, 2
MTIME = 1700000000000
NAME = "scenario.bin"
FORMULA = "byte(i) = (((i * 2654435761) mod 2^32) >> 13) & 255"


def sha(b):
    return hashlib.sha256(b).digest()


def content(length, overrides=()):
    data = bytearray(((i * 2654435761) & 0xFFFFFFFF) >> 13 & 255 for i in range(length))
    for o in overrides:
        data[o["offset"]] ^= o["xor"]
    return bytes(data)


def blocks_of(data):
    return [data[i:i + BLOCK] for i in range(0, len(data), BLOCK)]


def frames_of(data, block, pass_, transfer_id=ID):
    """The frame messages of block `block`, in order."""
    b = blocks_of(data)[block]
    out, at = [], 0
    tid = bytes.fromhex(transfer_id.replace("-", ""))
    while at < len(b):
        n = min(CAP, len(b) - at)
        last = at + n == len(b)
        head = (b"DLF2" + bytes([1 if last else 0, pass_]) + b"\x00\x00" + tid
                + block.to_bytes(4, "big") + (block * BLOCK + at).to_bytes(8, "big")
                + n.to_bytes(4, "big"))
        out.append(head + b[at:at + n] + (sha(b) if last else b""))
        at += n
    return out


def block_frames(data, block, pass_):
    fr = frames_of(data, block, pass_)
    return {"pass": pass_, "block": block, "frames": len(fr),
            "sha256": sha(b"".join(fr)).hex()}


def common(action, who, transfer_id=ID):
    return {"protocolVersion": 2, "action": action, "transferId": transfer_id,
            "deviceId": who, "sessionId": SESSION, "connectionGeneration": GENERATION,
            "transferToken": TOKEN}


def offer(length, name=NAME, transfer_id=ID, extra=None):
    m = common("offer", SENDER, transfer_id)
    m.update({"filename": name, "totalSize": length, "mtime": MTIME,
              "blockSize": BLOCK, "maxFrameBytes": 61440})
    m.update(extra or {})
    return m


def accept(resume, prefix_hashes, transfer_id=ID):
    m = common("accept", RECEIVER, transfer_id)
    m.update({"resumeBlock": resume, "prefixSha256": sha(b"".join(prefix_hashes)).hex(),
              "maxFrameBytes": 61440})
    return m


def ack(index, h):
    m = common("block-ack", RECEIVER)
    m.update({"blockIndex": index, "sha256": h.hex()})
    return m


def done(hashes, length, spoil=False):
    m = common("done", SENDER)
    m.update({"fileHash": ("00" * 32) if spoil else sha(b"".join(hashes)).hex(),
              "blocks": len(hashes), "totalSize": length})
    return m


def complete(hashes):
    m = common("complete", RECEIVER)
    m["fileHash"] = sha(b"".join(hashes)).hex()
    return m


def S(frm, **kw):
    d = {"from": frm}
    d.update(kw)
    return d


def hashes_of(data):
    return [sha(b) for b in blocks_of(data)]


def scenario(name, title, replay, length, steps, final, **extra):
    d = {"scenario": name, "title": title, "replay": replay,
         "ids": {"transferId": ID, "token": TOKEN, "sender": SENDER, "receiver": RECEIVER,
                 "sessionId": SESSION, "generation": GENERATION},
         "file": {"name": NAME, "length": length, "formula": FORMULA, "mtime": MTIME},
         "ignore": ["message"]}
    d.update(extra)
    d["steps"] = steps
    d["final"] = final
    return d


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    out = {}

    # S1: a sender resumed at block 2 with no hashes in memory (a fresh process).
    L = 3 * BLOCK + 1000
    data = content(L); h = hashes_of(data)
    out["S1"] = scenario(
        "S1", "sender resumed at block 2 without hashes: it re-reads the prefix, keeps its pass, completes",
        ["sender", "receiver"], L, [
            S("sender", control=offer(L)),
            S("receiver", control=accept(2, h[:2])),
            S("sender", blockFrames=block_frames(data, 2, 0)),
            S("sender", blockFrames=block_frames(data, 3, 0)),
            S("receiver", control=ack(2, h[2])),
            S("receiver", control=ack(3, h[3])),
            S("sender", control=done(h, L)),
            S("receiver", control=complete(h)),
        ], {"receiverPublished": True, "slotsFree": True}, receiverHolds={"blocks": 2})

    # S2: the sender cannot reproduce the prefix (its file differs): it restarts at 0 under pass+1
    # and the receiver adopts the new pass from the first frame after the accept.
    ov = [{"offset": 5, "xor": 255}]
    orig = content(L); patched = content(L, ov)
    ho, hp = hashes_of(orig), hashes_of(patched)
    out["S2"] = scenario(
        "S2", "prefix mismatch: the sender restarts at block 0 under pass 1, the receiver adopts it",
        ["sender", "receiver"], L, [
            S("sender", control=offer(L)),
            S("receiver", control=accept(2, ho[:2])),
        ] + [S("sender", blockFrames=block_frames(patched, b, 1)) for b in range(4)] + [
            S("receiver", control=ack(b, hp[b])) for b in range(4)
        ] + [
            S("sender", control=done(hp, L)),
            S("receiver", control=complete(hp)),
        ], {"receiverPublished": True, "slotsFree": True, "publishedIsSenderFile": True},
        receiverHolds={"blocks": 2}, senderFile={"overrides": ov})

    # S3: pause (backlog) and the receiver's resume carry the pass through; stale frames of the old pass drop.
    L3 = 2 * BLOCK + 500
    d3 = content(L3); h3 = hashes_of(d3)
    out["S3"] = scenario(
        "S3", "backlog pause, then resume {1, prefix, pass 1}: the sender restarts at block 1 under pass 1",
        ["sender", "receiver"], L3, [
            S("sender", control=offer(L3)),
            S("receiver", control=accept(0, [])),
            S("sender", blockFrames=block_frames(d3, 0, 0)),
            S("receiver", control=ack(0, h3[0])),
            S("test", side="receiver", event="backlog-on"),
            S("receiver", control=dict(common("pause", RECEIVER), reason="backlog")),
            S("test", side="receiver", event="backlog-off"),
            S("receiver", control=dict(common("resume", RECEIVER), resumeBlock=1,
                                       prefixSha256=sha(h3[0]).hex(), pass_=1)),
            S("sender", blockFrames=block_frames(d3, 1, 0)),
            S("sender", blockFrames=block_frames(d3, 2, 0)),
            S("sender", blockFrames=block_frames(d3, 1, 1)),
            S("sender", blockFrames=block_frames(d3, 2, 1)),
            S("receiver", control=ack(1, h3[1])),
            S("receiver", control=ack(2, h3[2])),
            S("sender", control=done(h3, L3)),
            S("receiver", control=complete(h3)),
        ], {"receiverPublished": True, "slotsFree": True})
    # the resume carries `pass`, not `pass_`
    for s in out["S3"]["steps"]:
        c = s.get("control")
        if c and "pass_" in c:
            c["pass"] = c.pop("pass_")

    # S4: the final file hash disagrees (the sender's done is spoiled once): the receiver restarts the whole file once.
    L4 = 70000
    d4 = content(L4); h4 = hashes_of(d4)
    out["S4"] = scenario(
        "S4", "final hash mismatch: resume {0, SHA256(\"\"), pass 1}, the file is sent again once and completes",
        ["sender", "receiver"], L4, [
            S("sender", control=offer(L4)),
            S("receiver", control=accept(0, [])),
            S("test", side="sender", event="spoil-next-done"),
            S("sender", blockFrames=block_frames(d4, 0, 0)),
            S("receiver", control=ack(0, h4[0])),
            S("sender", control=done(h4, L4, spoil=True)),
            S("receiver", control=dict(common("resume", RECEIVER), resumeBlock=0,
                                       prefixSha256=sha(b"").hex(), pass_=1)),
            S("sender", blockFrames=block_frames(d4, 0, 1)),
            S("receiver", control=ack(0, h4[0])),
            S("sender", control=done(h4, L4)),
            S("receiver", control=complete(h4)),
        ], {"receiverPublished": True, "slotsFree": True})
    for s in out["S4"]["steps"]:
        c = s.get("control")
        if c and "pass_" in c:
            c["pass"] = c.pop("pass_")

    # S4b: the phone's older form: block-nack {0} after done. Sender side only (the desk receiver sends resume).
    s4b_steps = [
        S("sender", control=offer(L4)),
        S("receiver", control=accept(0, [])),
        S("test", side="sender", event="spoil-next-done"),
        S("sender", blockFrames=block_frames(d4, 0, 0)),
        S("receiver", control=ack(0, h4[0])),
        S("sender", control=done(h4, L4, spoil=True)),
        S("receiver", control=dict(common("block-nack", RECEIVER), blockIndex=0, pass_=1,
                                   reason="hash-mismatch")),
        S("sender", blockFrames=block_frames(d4, 0, 1)),
        S("receiver", control=ack(0, h4[0])),
        S("sender", control=done(h4, L4)),
        S("receiver", control=complete(h4)),
    ]
    for s in s4b_steps:
        c = s.get("control")
        if c and "pass_" in c:
            c["pass"] = c.pop("pass_")
    out["S4b"] = scenario(
        "S4b", "final hash mismatch, the phone's older form: block-nack {0, pass 1}; the sender accepts it too",
        ["sender"], L4, s4b_steps, {"slotsFree": True})

    # S5: a source read error ends with error{internal}: both slots are freed.
    L5 = 100000
    d5 = content(L5); h5 = hashes_of(d5)
    out["S5"] = scenario(
        "S5", "source read error: error{internal} (not cancel), both slots freed",
        ["sender", "receiver"], L5, [
            S("sender", control=offer(L5)),
            S("test", side="sender", event="source-read-error"),
            S("receiver", control=accept(0, [])),
            S("sender", control=dict(common("error", SENDER), code="internal", message="read error")),
        ], {"receiverPublished": False, "slotsFree": True})

    # S6: a re-offer of a live id is answered with accept (from where the receiver is).
    L6 = 2 * BLOCK
    d6 = content(L6); h6 = hashes_of(d6)
    out["S6"] = scenario(
        "S6", "a re-offer of a live transfer is answered with accept {resumeBlock, prefix}, never busy",
        ["receiver"], L6, [
            S("sender", control=offer(L6)),
            S("receiver", control=accept(0, [])),
            S("sender", blockFrames=block_frames(d6, 0, 0)),
            S("receiver", control=ack(0, h6[0])),
            S("sender", control=offer(L6)),
            S("receiver", control=accept(1, h6[:1])),
        ], {"slotsFree": False})

    # S7: busy ordering: with both slots taken, an unrequested Bucket file is refused (not busy); a plain third is busy.
    A, B, C, D = ("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                  "cccccccc-cccc-4ccc-8ccc-cccccccccccc", "dddddddd-dddd-4ddd-8ddd-dddddddddddd")
    err = lambda tid, code, msg=None: dict(common("error", RECEIVER, tid), code=code,
                                           **({"message": msg} if msg else {}))
    out["S7"] = scenario(
        "S7", "busy ordering: an unrequested Bucket file is refused before the busy check; a plain third offer is busy",
        ["receiver"], 10, [
            S("sender", control=offer(10, "a.bin", A)),
            S("receiver", control=accept(0, [], A)),
            S("sender", control=offer(10, "b.bin", B)),
            S("receiver", control=accept(0, [], B)),
            S("sender", control=offer(10, "c.txt", C, {"folder": "bucket", "relpath": "docs/c.txt",
                                                        "contentSha256": sha(b"c").hex()})),
            S("receiver", control=err(C, "refused", "folder-refused")),
            S("sender", control=offer(10, "d.bin", D)),
            S("receiver", control=err(D, "busy")),
        ], {"slotsFree": False})

    for name, doc in out.items():
        with open(os.path.join(here, f"{name}.json"), "w") as f:
            json.dump(doc, f, indent=2, sort_keys=False)
            f.write("\n")
        print(name, len(doc["steps"]), "steps")


if __name__ == "__main__":
    main()
