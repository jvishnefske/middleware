"""CL6: the upload path's cores (CRC-32, SHA-256) against the model's
check values, then the whole request path against `upload.json`."""

from simtools import run


def test_cl6_crc32_sha256_cores():
    from migen import Module, Signal
    from mw5a75b.sha256 import Sha256
    from mw5a75b.upload import Crc32

    class Dut(Module):
        def __init__(self):
            self.submodules.crc = Crc32()
            self.submodules.sha = Sha256()

    d = Dut()
    ios = {d.crc.init, d.crc.stb, d.crc.din, d.crc.value}
    s = d.sha
    ios |= {s.start, s.valid, s.data, s.ready, s.finish, s.busy, s.done, s.digest}
    run(d, "cores", "tb_upload_cores", ios=ios)
