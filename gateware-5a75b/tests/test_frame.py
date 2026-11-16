"""CL4: the frame core against the `serial::frame` golden vectors."""

from simtools import run


def test_cl4_frame_decoder():
    from mw5a75b.frame import FrameDecoder
    run(FrameDecoder(), "frame_decoder", "tb_frame_decoder")


def test_cl4_crc_appender():
    from mw5a75b.frame import CrcAppender
    run(CrcAppender(), "crc_appender", "tb_crc_appender")
