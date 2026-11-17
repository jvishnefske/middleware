"""cocotb: every `upload.json` case replayed as request frames on link 0:
each response code and the payload status after it (from `sys/info`) as
`Loader::step` and the base firmware's dispatch give them; a successful
commit logs the image's SHA-256 on `sys/log`. A begin answered
`Unavailable` (the previous image still being programmed) is retried, as
a host does."""

import os

import cocotb

from tb_common import reset, vectors
from tb_host import ZERO, boot

UNAVAILABLE = 6


@cocotb.test()
async def cl6_upload_cases(dut):
    host, flash = await boot(dut)
    if os.environ.get("NODE_DEBUG"):
        from tb_debug import watch
        watch(dut, os.environ["NODE_DEBUG"].split(","))
    vec = vectors("upload.json")
    only = os.environ.get("UPLOAD_CASE")
    for case in vec["cases"]:
        if only and only not in case["name"]:
            continue
        flash.erase_all()
        await reset(dut, ZERO)
        for t in host.tx:
            t.frames.clear()
        for i, st in enumerate(case["steps"]):
            body = bytes.fromhex(st["body"])
            while True:
                code, _ = await host.request(0, st["key"], body)
                if not (code == UNAVAILABLE and st["key"] == "sys/loader/begin"):
                    break
                await host.clocks(2000)
            where = f"{case['name']} step {i} {st['key']}"
            assert code == st["code"], f"{where}: code {code} != {st['code']}"
            status = await host.status()
            assert status == st["status"], f"{where}: status {status} != {st['status']}"
        inst = case["installed"]
        logs = [d.decode() for _, _, _, d in host.tx[0].pubs("sys/log")]
        if inst is not None:
            want = f"payload installed sha256 {inst['sha256']}"
            assert want in logs, f"{case['name']}: {logs}"
        dut._log.info("case %s ok (%s)", case["name"], logs)
