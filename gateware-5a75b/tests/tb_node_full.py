"""cocotb: the whole node end to end, through its links and the flash
model only (CL6, CL8):

* `bridge.wat`, as the image the host compiles, uploaded over link 0
  (begin/chunk/commit), started:
  status Loading -> Stopped -> Running on `sys/payload/status`, its
  `bridge: up` log;
* its rules forward `demo/**` from link 0 to link 1 (byte for byte;
  `demox` not) and `back/ack` from link 1 to link 0;
* its tick publishes the demo rule's count on `bridge/stats` on both
  links; `ctl/bridge` "0" stops the rule, "1" reinstalls it;
* `sys/net/addr` reads and sets link 1's addresses; a configured peer
  receives link 1's publications;
* a reset boots from flash: the payload runs again (its rules forward)
  and the settings are back;
* a second payload goes into the other slot and boots after a reset; its
  header torn (magic missing), the first payload boots instead.
"""

import struct

import cocotb

from tb_common import reset, vectors
from tb_host import ZERO, boot, ip, publish

H0 = ("10.0.0.1", 5000)
H1 = ("10.0.1.2", 6000)
RUNNING, STOPPED, LOADING, EMPTY = 2, 1, 4, 0
SLOT_BASE = (0x100000, 0x105000)


async def upload(host, wasm, n=0, chunk=512):
    import zlib
    while True:
        code, _ = await host.request(n, "sys/loader/begin",
                                     struct.pack("<II", len(wasm), zlib.crc32(wasm)), *H0)
        if code != 6:
            break
        await host.clocks(2000)
    assert code == 0, code
    for off in range(0, len(wasm), chunk):
        code, _ = await host.request(n, "sys/loader/chunk",
                                     struct.pack("<I", off) + wasm[off:off + chunk], *H0)
        assert code == 0, (off, code)
    code, _ = await host.request(n, "sys/loader/commit", b"", *H0)
    assert code == 0, code


async def forwarded(host, src, dst, key, data, src_addr, timeout=3000):
    """Publishes on `src`; returns the frames that came out of `dst`."""
    seen = len(host.tx[dst].frames)
    await host.send(src, publish(key, data), *src_addr)
    await host.clocks(timeout)
    return [f for f in host.tx[dst].frames[seen:] if f[2][0] == 3 and f[2][3:3 + f[2][2]] == key]


def net_record(ipa, mac, peer, port):
    return (bytes(int(x) for x in ipa.split(".")) + bytes.fromhex(mac.replace(":", ""))
            + bytes(int(x) for x in peer.split(".")) + struct.pack("<H", port))


@cocotb.test()
async def cl8_node_end_to_end(dut):
    host, flash = await boot(dut)
    bridge_img = bytes.fromhex(vectors("wasm/bridge.json")["blob"])
    blink_img = bytes.fromhex(vectors("wasm/blink.json")["blob"])

    assert await host.status() == EMPTY
    await upload(host, bridge_img)
    code, _ = await host.request(0, "sys/payload/start", b"", *H0)
    assert code == 0
    assert await host.status() == RUNNING
    st = [d[0] for *_, d in host.tx[0].pubs("sys/payload/status")]
    assert st == [LOADING, STOPPED, RUNNING], st
    logs = [d.decode() for *_, d in host.tx[0].pubs("sys/log")]
    assert logs[-1] == "bridge: up", logs

    # Link 1 gets a peer; demo/** goes 0 -> 1, back/ack 1 -> 0.
    code, _ = await host.request(1, "sys/info", b"", *H1)
    assert code == 0
    pub = publish(b"demo/x", bytes(range(100)))
    got = await forwarded(host, 0, 1, b"demo/x", bytes(range(100)), H0)
    assert [(a, p, b) for a, p, b in got] == [(ip(H1[0]), H1[1], pub[:-2])], got
    assert await forwarded(host, 0, 1, b"demox", b"z", H0) == []
    assert await forwarded(host, 1, 0, b"demo/x", b"wrong way", H1) == []
    got = await forwarded(host, 1, 0, b"back/ack", b"ok", H1)
    assert [(a, p) for a, p, _ in got] == [(ip(H0[0]), H0[1])], got

    # bridge/stats on both links: the demo rule forwarded one frame.
    s0 = await host.wait_pub(0, "bridge/stats", timeout=200_000)
    s1 = await host.wait_pub(1, "bridge/stats", timeout=200_000)
    assert struct.unpack("<i", s0[-1][3])[0] == 1, s0
    assert struct.unpack("<i", s1[-1][3])[0] == 1, s1

    # ctl/bridge "0" stops the demo rule, "1" reinstalls it.
    await host.send(0, publish(b"ctl/bridge", b"0"), *H0)
    await host.clocks(3000)
    assert "bridge: stopped" in [d.decode() for *_, d in host.tx[0].pubs("sys/log")]
    assert await forwarded(host, 0, 1, b"demo/x", b"a", H0) == []
    await host.send(0, publish(b"ctl/bridge", b"1"), *H0)
    await host.clocks(3000)
    assert len(await forwarded(host, 0, 1, b"demo/x", b"b", H0)) == 1

    # sys/net/addr: defaults, then a configured peer on link 1.
    code, rec = await host.request(0, "sys/net/addr", b"\x01", *H0)
    assert code == 0
    assert rec == b"\x01" + net_record("192.168.2.50", "02:5a:75:b0:00:01", "0.0.0.0", 0), rec.hex()
    code, _ = await host.request(0, "sys/net/addr", b"\x05", *H0)
    assert code == 2
    code, _ = await host.request(0, "sys/net/addr", b"\x01\x02", *H0)
    assert code == 2
    want = net_record("192.168.2.51", "02:5a:75:b0:00:11", "10.0.1.9", 7000)
    code, _ = await host.request(0, "sys/net/addr", b"\x01" + want, *H0)
    assert code == 0
    code, rec = await host.request(0, "sys/net/addr", b"\x01", *H0)
    assert rec == b"\x01" + want, rec.hex()
    got = await forwarded(host, 0, 1, b"demo/x", b"c", H0)
    assert [(a, p) for a, p, _ in got] == [(ip("10.0.1.9"), 7000)], got
    # A request on link 1 from elsewhere is answered there, the peer stays.
    code, _ = await host.request(1, "sys/info", b"", "10.0.1.3", 6001)
    assert code == 0
    got = await forwarded(host, 0, 1, b"demo/x", b"d", H0)
    assert [(a, p) for a, p, _ in got] == [(ip("10.0.1.9"), 7000)], got
    await host.clocks(30_000)       # the settings reach the flash

    # Reset: the payload boots from flash, the settings are back.
    await reset(dut, ZERO)
    assert await host.status() == RUNNING
    code, rec = await host.request(0, "sys/net/addr", b"\x01", *H0)
    assert rec == b"\x01" + want, rec.hex()
    await host.clocks(2000)
    got = await forwarded(host, 0, 1, b"demo/x", b"e", H0)
    assert [(a, p) for a, p, _ in got] == [(ip("10.0.1.9"), 7000)], got

    # A second payload in the other slot boots after a reset.
    await upload(host, blink_img, chunk=300)
    code, _ = await host.request(0, "sys/payload/start", b"", *H0)
    assert code == 0
    assert await forwarded(host, 0, 1, b"demo/x", b"f", H0) == [], "rules gone with bridge.wat"
    while flash.busy or int(dut.flash_cs_n.value) == 0:
        await host.clocks(100)
    await host.clocks(60_000)
    hdr = [bytes(flash.mem[b:b + 16]) for b in SLOT_BASE]
    seqs = [struct.unpack("<IIII", h)[2] for h in hdr]
    assert sorted(seqs) == [1, 2], [h.hex() for h in hdr]
    await reset(dut, ZERO)
    assert await host.status() == RUNNING
    assert await forwarded(host, 0, 1, b"demo/x", b"g", H0) == []
    # Tear the newer header (its magic was written last): the first boots.
    newer = SLOT_BASE[seqs.index(2)]
    flash.mem[newer + 12:newer + 16] = b"\xff" * 4
    await reset(dut, ZERO)
    assert await host.status() == RUNNING
    await host.clocks(2000)
    assert len(await forwarded(host, 0, 1, b"demo/x", b"h", H0)) == 1
    # Erased flash: nothing installed.
    flash.erase_all()
    await reset(dut, ZERO)
    assert await host.status() == EMPTY
    dut._log.info("end to end: all steps passed at %s", cocotb.utils.get_sim_time("us"))
