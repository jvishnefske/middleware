"""cocotb: the node behind the minimal endpoint on link 0, as `soc.py
--only-port 0` builds it: ARP, ping and `sys/info` / `sys/net/addr` as
Ethernet frames."""

import struct

import cocotb

from flashmodel import FlashModel
from tb_host import request
from tb_net import (BCAST, HOST_IP, HOST_MAC, OUR_IP, OUR_MAC, PORT, Bench, arp, eth,
                    host_udp, icmp_echo, ip, ip_b, ipv4, parse_ipv4_udp)
from mw5a75b.frame import crc16_py


async def ask(b, rid, key, body=b"", sport=5000):
    seen = len(b.sent)
    await b.phy_send(host_udp(request(rid, key, body), sport=sport))
    await b.wait(lambda: len(b.sent) > seen, cycles=200_000, what=key)
    dmac, dip, dport, payload = parse_ipv4_udp(b.sent[seen])
    assert (dmac, dip, dport) == (HOST_MAC, ip(HOST_IP), sport)
    assert crc16_py(payload[:-2]) == int.from_bytes(payload[-2:], "little")
    assert payload[0] == 2 and payload[1] == rid, payload.hex()
    return payload[2], payload[3:-2]


@cocotb.test()
async def the_node_answers_over_ethernet(dut):
    FlashModel(dut)
    b = Bench(dut, link=False)
    await b.start()
    await b.idle(20_000)            # boot: the (erased) flash is read
    b.sent.clear()
    await b.phy_send(eth(BCAST, HOST_MAC, 0x0806, arp(1, HOST_MAC, HOST_IP, 0, OUR_IP)))
    await b.wait(lambda: len(b.sent) == 1, what="ARP reply")
    assert b.sent.pop()[14:42] == arp(2, OUR_MAC, OUR_IP, HOST_MAC, HOST_IP)
    req = icmp_echo(7, 1, bytes(range(56)))
    await b.phy_send(eth(OUR_MAC, HOST_MAC, 0x0800, ipv4(HOST_IP, OUR_IP, 1, req)))
    await b.wait(lambda: len(b.sent) == 1, what="echo reply")
    assert b.sent.pop()[38:] == req[4:]

    code, info = await ask(b, 1, b"sys/info")
    assert code == 0 and len(info) > 8, info.hex()
    code, rec = await ask(b, 2, b"sys/net/addr", b"\x00", sport=5005)
    assert code == 0
    assert rec[:11] == b"\x00" + ip_b(OUR_IP) + OUR_MAC.to_bytes(6, "big"), rec.hex()
    code, _ = await ask(b, 3, b"no/such/key")
    assert code != 0
