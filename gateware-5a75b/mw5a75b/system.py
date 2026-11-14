"""The system side of the node (CL6, CL8): system requests, the payload
lifecycle, the runtime's events and the flash store, shared by both
links. One request is handled at a time.

Requests (`req_*`, from a link's frame decoder; the body is read through
`buf_adr`/`buf_dat`, and `req_release` frees the decoder once it is):

    sys/info            DeviceInfo (status live, Caps::BRIDGE)
    sys/trust/info      unclaimed: [0][0 0 0 0]
    sys/loader/begin    \\
    sys/loader/chunk     > `upload.UploadState` (Loader::step)
    sys/loader/commit   /  then compile; store in flash
    sys/payload/start   compile from the image RAM, `init`
    sys/payload/stop
    sys/net/addr        [link] -> [link][ip 4][mac 6][peer ip 4][peer port u16];
                        [link][ip 4][mac 6][peer ip 4][peer port u16] -> empty
    anything else       NotFound (no `serve`: the payload answers no requests)

Bodies are checked as `Command::parse` does (a malformed body is
`InvalidRequest` and leaves the transfer alone). The status follows the
base firmware (`firmware/src/dispatch.rs`): a begin stops the payload
(Loading), a failed transfer returns to Stopped or Empty, a commit
installs (Stopped), `start` runs `init` (Running, or Trapped), `stop`
stops. The board compiles at commit: a module the loader refuses is
`InvalidRequest`, logged on `sys/log`, and the installed payload is put
back from flash. Stopping, trapping or replacing a payload removes its
bridge rules; its subscriptions and tick go at the next start.

Flash (`flash.Store`): a begin erases the inactive slot in the
background, a successful commit programs it (image, then header with
`seq + 1`); a begin while that program is pending answers `Unavailable`.
At reset the newest valid slot is copied into the image RAM (CRC-32
checked), compiled and started; the settings sector sets the links'
addresses. `sys/net/addr` changes are applied after the response (it
still leaves from the old address) and persisted.

Events for the runtime (`imports.Runtime`), one at a time: `init` from a
start, deliveries (`dlv_req[n]`, a subscribed publication in link n's
decoder buffer), the tick (`ev_tick` every millisecond, `now_ms`). A
trap in `tick` or `on_message` makes the payload Trapped.

Publications: `sys/payload/status` (one byte) on every status change and
`sys/log` lines (`payload installed sha256 <hex>`, `load refused: <error>`,
`payload trapped: <trap>`), on every link with a peer. Responses go back
to the requester (`tx_resp`).
"""

from migen import (
    Array, Cat, Constant, FSM, If, Memory, Module, Mux, NextState, NextValue, Signal,
)

from .flash import SETTINGS, SETTINGS_LEN, SET_MAGIC, MAGIC, SLOT_BASE, IMG_OFF, Store
from .imports import R_TRAP, Runtime
from .loader import ERRORS, Loader
from .sha256 import Sha256
from .upload import (
    BUFFER_TOO_SMALL, Crc32, EMPTY, INVALID_REQUEST, LOADING, NOT_FOUND, OK, RUNNING, STOPPED,
    TRAPPED, UNAVAILABLE, UploadState,
)
from .wasm import TRAPS

ABI_VERSION = 6
CAPACITY = 16 * 1024
CHIP = b"ecp5"
FIRMWARE = b"0.1.0"
CAP_BRIDGE = 2
CHUNK_MAX = 512
SIGNED_COMMIT_LEN = 68

KEYS = [b"sys/info", b"sys/trust/info", b"sys/loader/begin", b"sys/loader/chunk",
        b"sys/loader/commit", b"sys/payload/start", b"sys/payload/stop", b"sys/net/addr"]
(K_NONE, K_INFO, K_TRUST, K_BEGIN, K_CHUNK, K_COMMIT, K_START, K_STOP,
 K_NET) = range(len(KEYS) + 1)

B_NONE, B_INFO, B_TRUST, B_NET = 0, 1, 2, 3          # response bodies
E_RESP, E_STATUS, E_LOG = 0, 1, 2                    # emitter jobs

NET_LEN = 16
LOG_KEY = b"sys/log"
STATUS_KEY = b"sys/payload/status"
MSG_INSTALLED = b"payload installed"
MSG_SHA = b" sha256 "
MSG_REFUSED = b"load refused: "
MSG_TRAPPED = b"payload trapped: "


class _Rom:
    """Constant strings packed into one ROM, with their offsets."""

    def __init__(self):
        self.data = b""
        self.at = {}

    def add(self, s):
        if s not in self.at:
            self.at[s] = len(self.data)
            self.data += s
        return self.at[s], len(s)


def info_body(node_id, status):
    """`DeviceInfo` (status and node id live)."""
    body = []
    body += [Constant(b, 8) for b in ABI_VERSION.to_bytes(2, "little")]
    body += [Constant(b, 8) for b in CAPACITY.to_bytes(4, "little")]
    body += [status, Constant(len(CHIP), 8)] + [Constant(b, 8) for b in CHIP]
    body += [Constant(len(FIRMWARE), 8)] + [Constant(b, 8) for b in FIRMWARE]
    body += [node_id[8 * i:8 * i + 8] for i in range(8)]
    body += [Constant(b, 8) for b in CAP_BRIDGE.to_bytes(4, "little")]
    return body


class NetRegs:
    """One link's addresses: `ip`/`peer_ip` with the first octet in bits
    31..24, `mac` with the first byte in bits 47..40."""

    def __init__(self, n, ip, mac):
        self.ip = Signal(32, reset=ip, name=f"net{n}_ip")
        self.mac = Signal(48, reset=mac, name=f"net{n}_mac")
        self.peer_ip = Signal(32, name=f"net{n}_peer_ip")
        self.peer_port = Signal(16, name=f"net{n}_peer_port")

    def wire(self):
        """The 16-byte record, as Signals' byte slices."""
        be = lambda v, n: [v[8 * (n - 1 - k):8 * (n - k)] for k in range(n)]
        return (be(self.ip, 4) + be(self.mac, 6) + be(self.peer_ip, 4)
                + [self.peer_port[0:8], self.peer_port[8:16]])

    def load(self, b):
        """Statements setting the record from 16 byte expressions."""
        return [
            self.ip.eq(Cat(*reversed(b[0:4]))),
            self.mac.eq(Cat(*reversed(b[4:10]))),
            self.peer_ip.eq(Cat(*reversed(b[10:14]))),
            self.peer_port.eq(Cat(b[14], b[15])),
        ]


class System(Module):
    def __init__(self, bridge, node_id, ips, macs, with_sha256=True, flash_div=2,
                 ms_cycles=50_000, n_links=2):
        # Requests.
        self.req_valid = Signal()
        self.req_ready = Signal()          # pulse: taken
        self.req_link = Signal()
        self.req_id = Signal(8)
        self.req_key = Signal(4)           # K_*
        self.req_len = Signal(11)          # body bytes
        self.req_base = Signal(11)         # body offset in the buffer
        self.req_release = Signal()        # pulse: the buffer is free
        self.buf_adr = Signal(10)
        self.buf_dat = Signal(8)
        self.req_owner = Signal()          # the buffer port is the system's
        # Frames out (responses and system publications).
        self.tx_req = Signal()
        self.tx_link = Signal()
        self.tx_resp = Signal()            # 1: to the requester, 0: to the peer
        self.tx_valid = Signal()
        self.tx_data = Signal(8)
        self.tx_last = Signal()
        self.tx_ready = Signal()
        self.tx_len = Signal(16)           # frame body bytes (no CRC)
        self.peer_ok = Signal(n_links)
        # Deliveries.
        self.dlv_req = Signal(n_links)
        self.dlv_slot = [Signal(3, name=f"dlv{n}_slot") for n in range(n_links)]
        self.dlv_len = [Signal(10, name=f"dlv{n}_len") for n in range(n_links)]
        self.dlv_base = [Signal(10, name=f"dlv{n}_base") for n in range(n_links)]
        self.dlv_done = Signal(n_links)
        self.dlv_link = Signal()           # whose buffer the runtime reads
        self.dlv_adr = Signal(10)
        self.dlv_dat = Signal(8)
        # Payload and board.
        self.status = Signal(8)
        self.now_ms = Signal(32)
        self.led = Signal()
        self.led_claimed = Signal()
        self.button = Signal()
        self.net = [NetRegs(n, ips[n], macs[n]) for n in range(n_links)]
        # Flash pads.
        self.submodules.store = store = Store(flash_div)
        spi = store.spi
        self.flash_cs_n, self.flash_sclk = spi.cs_n, spi.sclk
        self.flash_mosi, self.flash_miso = spi.mosi, spi.miso

        # # #

        self.submodules.runtime = rt = Runtime(bridge=bridge)
        self.submodules.loader = ld = Loader(rt.core)
        self.submodules.upload = up = UploadState(CAPACITY)
        imp = rt.imports
        core = rt.core
        self.sub_match = imp.sub_match
        self.pub = imp
        self.comb += [
            imp.node_id.eq(node_id), imp.now_ms.eq(self.now_ms), imp.peer_ok.eq(self.peer_ok),
            imp.button.eq(self.button),
            self.led.eq(imp.led),
            self.led_claimed.eq(imp.led_enabled & (self.status == RUNNING)),
            rt.ld_mem_sel.eq(ld.busy & ld.mem_sel),
            rt.ld_mem_adr.eq(ld.mem_adr), rt.ld_mem_dat.eq(ld.mem_dat),
            rt.ld_mem_we.eq(ld.busy & ld.mem_we),
            rt.exp_init.eq(ld.exp_init), rt.exp_tick.eq(ld.exp_tick),
            rt.exp_alloc.eq(ld.exp_alloc), rt.exp_on_message.eq(ld.exp_on_message),
            core.ld_mem_len.eq(ld.mem_len), core.ld_mem_len_we.eq(ld.done),
        ]
        if with_sha256:
            self.submodules.sha = sha = Sha256()
        else:
            sha = None

        # --- millisecond clock ---------------------------------------------
        msc = Signal(max=ms_cycles)
        tick_pend = Signal()
        tick_clr = Signal()
        self.sync += [
            If(msc == ms_cycles - 1, msc.eq(0), self.now_ms.eq(self.now_ms + 1)
            ).Else(msc.eq(msc + 1)),
            If(msc == ms_cycles - 1, tick_pend.eq(1)).Elif(tick_clr, tick_pend.eq(0)),
        ]

        # --- events -----------------------------------------------------------
        lock = Signal()                    # the system FSM drives the runtime
        trap_flag = Signal()
        trap_code = Signal(4)
        trap_clr = Signal()
        ev_link = Signal()
        ev = FSM(reset_state="IDLE")
        self.submodules += ev
        ev_idle = Signal()
        self.comb += ev_idle.eq(ev.ongoing("IDLE"))
        running = Signal()
        self.comb += running.eq(self.status == RUNNING)
        ev_init = Signal()
        ev_reset = Signal()
        dlv_go = Signal()
        tick_go = Signal()
        self.comb += [
            rt.ev_init.eq(ev_init), rt.ev_reset.eq(ev_reset),
            rt.ev_deliver.eq(dlv_go), rt.ev_tick.eq(tick_go),
            rt.dlv_slot.eq(Array(self.dlv_slot)[ev_link]),
            rt.dlv_len.eq(Array(self.dlv_len)[ev_link]),
            rt.dlv_base.eq(Array(self.dlv_base)[ev_link]),
            self.dlv_link.eq(ev_link),
            self.dlv_adr.eq(rt.dlv_adr), rt.dlv_dat.eq(self.dlv_dat),
        ]
        dlv_ack = Signal(n_links)
        self.comb += self.dlv_done.eq(dlv_ack)
        ev.act("IDLE",
            If(~lock,
                If(self.dlv_req[0],
                    NextValue(ev_link, 0),
                    If(running, dlv_go.eq(1), NextState("RUN_DLV")).Else(dlv_ack[0].eq(1)),
                ).Elif(self.dlv_req[1],
                    NextValue(ev_link, 1),
                    If(running, dlv_go.eq(1), NextState("RUN_DLV")).Else(dlv_ack[1].eq(1)),
                ).Elif(tick_pend,
                    tick_clr.eq(1),
                    If(running & ~trap_flag, tick_go.eq(1), NextState("RUN_TICK")),
                ),
            ),
        )
        # Runtime results: R_TRAP + code is a trap.
        def result():
            return If(rt.ev_result >= R_TRAP,
                      NextValue(trap_code, rt.ev_result - R_TRAP))
        trap_set = Signal()
        self.sync += If(trap_clr, trap_flag.eq(0)).Elif(trap_set, trap_flag.eq(1))
        ev.act("RUN_DLV",
            If(rt.ev_done,
                result(), trap_set.eq(rt.ev_result >= R_TRAP),
                dlv_ack.eq(1 << ev_link),
                NextState("IDLE"),
            ),
        )
        ev.act("RUN_TICK",
            If(rt.ev_done,
                result(), trap_set.eq(rt.ev_result >= R_TRAP),
                NextState("IDLE"),
            ),
        )

        # --- ROM of the published strings -----------------------------------
        rom = _Rom()
        k_log = rom.add(LOG_KEY)
        k_status = rom.add(STATUS_KEY)
        m_inst = rom.add(MSG_INSTALLED)
        m_sha = rom.add(MSG_SHA)
        m_ref = rom.add(MSG_REFUSED)
        m_trap = rom.add(MSG_TRAPPED)
        err_at = [rom.add(ERRORS.get(i, "?").encode()) for i in range(16)]
        trap_at = [rom.add(TRAPS.get(i, "?").encode()) for i in range(16)]
        rmem = Memory(8, len(rom.data), init=list(rom.data))
        self.specials += rmem
        rport = rmem.get_port()
        self.specials += rport
        aw_rom = len(rport.adr)

        # --- emitter ----------------------------------------------------------
        em_go = Signal()
        em_kind = Signal(2)
        em_body = Signal(2)
        em_busy = Signal()
        em_code = Signal(8)
        em_id = Signal(8)
        em_link = Signal()                 # response link
        em_net = Signal()                  # the link a sys/net/addr get names
        # Log line: segment 1 + segment 2 (+ hex digest).
        log_pend = Signal()
        log_s1 = Signal(aw_rom)
        log_n1 = Signal(6)
        log_s2 = Signal(aw_rom)
        log_n2 = Signal(5)
        log_hex = Signal()
        status_pend = Signal()

        kind = Signal(2)
        body = Signal(2)
        code = Signal(8)
        rid = Signal(8)
        link = Signal()
        pos = Signal(8)
        elen = Signal(8)
        cur = Signal(8)
        use_rom = Signal()
        s1, n1, s2, n2, hx = (Signal(aw_rom), Signal(6), Signal(aw_rom), Signal(5), Signal())

        info_t = Array(info_body(node_id, self.status))
        net_t = Array(Array(r.wire()) for r in self.net)
        trust_n = 5
        body_len = Signal(8)
        self.comb += body_len.eq(Mux(body == B_INFO, len(info_body(node_id, self.status)),
                                     Mux(body == B_TRUST, trust_n,
                                         Mux(body == B_NET, 1 + NET_LEN, 0))))
        klen = Signal(8)
        kat = Signal(aw_rom)
        self.comb += [
            klen.eq(Mux(kind == E_LOG, k_log[1], k_status[1])),
            kat.eq(Mux(kind == E_LOG, k_log[0], k_status[0])),
        ]
        dig_nib = Signal(4)
        hex_i = Signal(6)
        if sha is not None:
            self.comb += dig_nib.eq((sha.digest >> Cat(Constant(0, 2), ~hex_i))[:4])
        hex_ch = Signal(8)
        self.comb += hex_ch.eq(Mux(dig_nib < 10, ord("0") + dig_nib, ord("a") - 10 + dig_nib))

        # Byte `pos` of the current frame: either `cur` (computed) or the
        # ROM at `radr`.
        radr = Signal(aw_rom)
        val = Signal(8)
        from_rom = Signal()
        p = pos
        d0 = Signal(8)                     # data offset (after the key)
        self.comb += d0.eq(3 + klen)
        resp_byte = Signal(8)
        self.comb += If(p == 0, resp_byte.eq(2)).Elif(p == 1, resp_byte.eq(rid)).Elif(
            p == 2, resp_byte.eq(code)).Elif(body == B_INFO, resp_byte.eq(info_t[(p - 3)[:5]])
        ).Elif(body == B_NET,
            If(p == 3, resp_byte.eq(em_net)).Else(
                resp_byte.eq(net_t[em_net][(p - 4)[:4]])),
        ).Else(resp_byte.eq(0))
        self.comb += [
            If(kind == E_RESP,
                val.eq(resp_byte),
            ).Elif(p == 0, val.eq(3)).Elif(p == 1, val.eq(0)).Elif(p == 2, val.eq(klen)
            ).Elif(p < d0, from_rom.eq(1), radr.eq(kat + (p - 3))
            ).Elif(kind == E_STATUS, val.eq(self.status)
            ).Elif(p < d0 + n1, from_rom.eq(1), radr.eq(s1 + (p - d0))
            ).Elif(p < d0 + n1 + n2, from_rom.eq(1), radr.eq(s2 + (p - d0 - n1))
            ).Else(val.eq(hex_ch)),
        ]
        self.comb += hex_i.eq((p - d0 - n1 - n2)[:6])
        self.comb += rport.adr.eq(radr)

        em = FSM(reset_state="IDLE")
        self.submodules += em
        self.comb += em_busy.eq(~em.ongoing("IDLE"))
        lens = Signal(8)
        self.comb += lens.eq(Mux(kind == E_RESP, 3 + body_len,
                                 Mux(kind == E_STATUS, 3 + klen + 1,
                                     3 + klen + n1 + n2 + Mux(hx, 64, 0))))
        self.comb += [
            self.tx_link.eq(link), self.tx_resp.eq(kind == E_RESP), self.tx_len.eq(lens),
        ]
        em.act("IDLE",
            If(em_go,
                NextValue(kind, E_RESP), NextValue(body, em_body), NextValue(code, em_code),
                NextValue(rid, em_id), NextValue(link, em_link), NextValue(pos, 0),
                NextState("FETCH"),
            ).Elif(log_pend,
                NextValue(kind, E_LOG), NextValue(s1, log_s1), NextValue(n1, log_n1),
                NextValue(s2, log_s2), NextValue(n2, log_n2), NextValue(hx, log_hex),
                NextValue(link, 0),
                NextState("PUB_NEXT"),
            ).Elif(status_pend,
                NextValue(kind, E_STATUS), NextValue(link, 0),
                NextState("PUB_NEXT"),
            ),
        )
        em_clear_log = Signal()
        em_clear_status = Signal()
        em.act("PUB_NEXT",
            NextValue(pos, 0),
            If((self.peer_ok >> link)[0], NextState("FETCH")
            ).Elif(link == 0, NextValue(link, 1)
            ).Else(NextState("PUB_END")),
        )
        em.act("PUB_END",
            If(kind == E_LOG, em_clear_log.eq(1)).Else(em_clear_status.eq(1)),
            NextState("IDLE"),
        )
        em.act("FETCH",
            self.tx_req.eq(1),
            NextValue(cur, val), NextValue(use_rom, from_rom),
            NextState("SEND"),
        )
        last = Signal()
        self.comb += last.eq(pos == lens - 1)
        em.act("SEND",
            self.tx_req.eq(1),
            self.tx_valid.eq(1),
            self.tx_data.eq(Mux(use_rom, rport.dat_r, cur)),
            self.tx_last.eq(last),
            If(self.tx_ready,
                NextValue(pos, pos + 1),
                If(last,
                    If(kind == E_RESP, NextState("IDLE")
                    ).Elif(link == 0, NextValue(link, 1), NextState("PUB_NEXT")
                    ).Else(NextState("PUB_END")),
                ).Else(NextState("FETCH")),
            ),
        )
        # While a frame is in FETCH the ROM address follows `pos`; the
        # registered ROM output is read in SEND with `pos` unchanged.

        # --- the system FSM ---------------------------------------------------
        st = self.status
        installed = Signal()
        active = Signal(reset=1)           # flash slot holding the installed payload
        seq = Signal(32)
        inst_len = Signal(15)
        inst_crc = Signal(32)
        prog_pend = Signal()
        prog_busy = Signal()
        erase_pend = Signal()
        net_dirty = Signal()
        # The request.
        r_link = Signal()
        r_id = Signal(8)
        r_key = Signal(4)
        r_len = Signal(11)
        r_base = Signal(11)
        bb = Array(Signal(8, name=f"body{i}") for i in range(17))
        ri = Signal(11)
        rn = Signal(11)
        ret = Signal(4)                    # where READ_BODY returns
        # Loading from flash.
        fl_slot = Signal()
        fl_len = Signal(15)
        fl_crc = Signal(32)
        fl_i = Signal(15)
        fl_boot = Signal()                 # boot (start after) or restore
        hdr = Array(Signal(8, name=f"hdr{i}") for i in range(16))
        h_len = [Signal(15, name=f"h{s}_len") for s in range(2)]
        h_crc = [Signal(32, name=f"h{s}_crc") for s in range(2)]
        h_seq = [Signal(32, name=f"h{s}_seq") for s in range(2)]
        h_ok = [Signal(name=f"h{s}_ok") for s in range(2)]
        tried = Signal(2)
        self.submodules.flcrc = flcrc = Crc32()

        self.comb += [
            self.buf_adr.eq((r_base + ri)[:10]),
            ld.img_rd_adr.eq(store.src_adr), ld.img_rd_sel.eq(prog_busy),
            store.src_dat.eq(ld.img_rd_dat),
        ]
        # Settings bytes for the store: link idx[4], byte idx[:4].
        self.comb += store.set_byte.eq(net_t[store.set_idx[4]][store.set_idx[:4]])

        # Programming done: the new slot is the installed one.
        set_active = Signal()
        set_seq = Signal()
        seq_in = Signal(32)
        self.sync += If(store.done & prog_busy,
            prog_busy.eq(0), active.eq(~active), seq.eq(seq + 1),
        ).Else(
            If(set_active, active.eq(fl_slot)),
            If(set_seq, seq.eq(seq_in)),
        )
        set_prog_busy = Signal()
        self.sync += If(set_prog_busy, prog_busy.eq(1))

        fsm = FSM(reset_state="BOOT")
        self.submodules += fsm
        # Image RAM writes: chunk copies and flash copies.
        img_adr = Signal(14)
        img_dat = Signal(8)
        img_we = Signal()
        self.comb += ld.img_adr.eq(img_adr), ld.img_dat.eq(img_dat), ld.img_we.eq(img_we)

        def respond(c, b=B_NONE):
            return [NextValue(em_code, c), NextValue(em_body, b), NextState("RESP")]

        status_set = Signal()
        log_set = Signal()

        def set_status(s):
            return [NextValue(st, s), status_set.eq(1)]

        def log(seg1, seg2=None, hexd=False, seg2_at=None, seg2_n=None):
            stmts = [log_set.eq(1), NextValue(log_s1, seg1[0]), NextValue(log_n1, seg1[1]),
                     NextValue(log_hex, hexd)]
            if seg2 is not None:
                stmts += [NextValue(log_s2, seg2[0]), NextValue(log_n2, seg2[1])]
            elif seg2_at is not None:
                stmts += [NextValue(log_s2, seg2_at), NextValue(log_n2, seg2_n)]
            else:
                stmts += [NextValue(log_n2, 0)]
            return stmts

        self.sync += [
            If(log_set, log_pend.eq(1)).Elif(em_clear_log, log_pend.eq(0)),
            If(status_set, status_pend.eq(1)).Elif(em_clear_status, status_pend.eq(0)),
        ]
        resting = Mux(installed, STOPPED, EMPTY)
        bridge_clear = bridge.clear

        # Boot: settings, then both headers, then the newest valid slot.
        rd_i = Signal(6)
        set_buf = Array(Signal(8, name=f"set{i}") for i in range(SETTINGS_LEN + 4))

        def flash_read(addr, n, nxt):
            return If(~store.busy,
                store.rd_go.eq(1), store.rd_addr.eq(addr), store.rd_n.eq(n),
                NextValue(rd_i, 0), NextState(nxt))

        fsm.act("BOOT", flash_read(SETTINGS, SETTINGS_LEN + 4, "BOOT_SET"))
        fsm.act("BOOT_SET",
            If(store.rd_stb, NextValue(set_buf[rd_i], store.rd_byte), NextValue(rd_i, rd_i + 1)),
            If(store.done, NextState("BOOT_SET2")),
        )
        smagic = Cat(*[set_buf[SETTINGS_LEN + k] for k in range(4)])
        fsm.act("BOOT_SET2",
            NextValue(fl_slot, 0),
            NextState("BOOT_HDR"),
        )
        for n, r in enumerate(self.net):
            rec = [set_buf[16 * n + k] for k in range(16)]
            self.sync += If(fsm.ongoing("BOOT_SET2") & (smagic == SET_MAGIC), *r.load(rec))
        fsm.act("BOOT_HDR",
            flash_read(Mux(fl_slot, SLOT_BASE[1], SLOT_BASE[0]), 16, "BOOT_HDR2"))
        fsm.act("BOOT_HDR2",
            If(store.rd_stb, NextValue(hdr[rd_i[:4]], store.rd_byte), NextValue(rd_i, rd_i + 1)),
            If(store.done, NextState("BOOT_HDR3")),
        )
        hl = Cat(*[hdr[k] for k in range(4)])
        hc = Cat(*[hdr[4 + k] for k in range(4)])
        hs = Cat(*[hdr[8 + k] for k in range(4)])
        hm = Cat(*[hdr[12 + k] for k in range(4)])
        for s in range(2):
            self.sync += If(fsm.ongoing("BOOT_HDR3") & (fl_slot == s),
                h_len[s].eq(hl[:15]), h_crc[s].eq(hc), h_seq[s].eq(hs),
                h_ok[s].eq((hm == MAGIC) & (hl != 0) & (hl <= CAPACITY)),
            )
        fsm.act("BOOT_HDR3",
            If(fl_slot == 0, NextValue(fl_slot, 1), NextState("BOOT_HDR")
            ).Else(NextValue(tried, 0), NextState("BOOT_PICK")),
        )
        # Newest valid untried slot.
        a_ok = h_ok[0] & ~tried[0]
        b_ok = h_ok[1] & ~tried[1]
        pick_b = b_ok & (~a_ok | ((h_seq[1] - h_seq[0])[31] == 0) & (h_seq[1] != h_seq[0]))
        fsm.act("BOOT_PICK",
            If(~a_ok & ~b_ok,
                NextValue(installed, 0), NextValue(st, EMPTY),
                NextState("IDLE"),
            ).Else(
                NextValue(fl_slot, pick_b),
                NextValue(tried, tried | Mux(pick_b, 2, 1)),
                NextValue(fl_len, Mux(pick_b, h_len[1], h_len[0])),
                NextValue(fl_crc, Mux(pick_b, h_crc[1], h_crc[0])),
                set_seq.eq(1), seq_in.eq(Mux(pick_b, h_seq[1], h_seq[0])),
                NextValue(fl_boot, 1),
                NextState("FL_COPY"),
            ),
        )
        # Copy slot `fl_slot` (fl_len bytes) into the image RAM, CRC checked,
        # then compile; boot starts it, a restore leaves it Stopped.
        self.comb += flcrc.din.eq(store.rd_byte)
        fsm.act("FL_COPY",
            flcrc.init.eq(1),
            flash_read(Mux(fl_slot, SLOT_BASE[1], SLOT_BASE[0]) + IMG_OFF, fl_len, "FL_COPY2"),
        )
        fsm.act("FL_COPY2",
            If(store.rd_stb,
                img_we.eq(1), img_adr.eq(fl_i[:14]), img_dat.eq(store.rd_byte),
                flcrc.stb.eq(1),
                NextValue(fl_i, fl_i + 1),
            ),
            If(store.done, NextValue(fl_i, 0), NextState("FL_CHECK")),
        )
        fsm.act("FL_CHECK",
            If(flcrc.value == fl_crc,
                ld.img_len.eq(fl_len), ld.start.eq(1),
                NextState("FL_COMPILE"),
            ).Elif(fl_boot, NextState("BOOT_PICK")
            ).Else(NextValue(installed, 0), *set_status(EMPTY), NextState("IDLE")),
        )
        fsm.act("FL_COMPILE",
            ld.img_len.eq(fl_len),
            If(ld.done,
                If(ld.error != 0,
                    If(fl_boot, NextState("BOOT_PICK")).Else(
                        NextValue(installed, 0), *set_status(EMPTY), NextState("IDLE")),
                ).Else(
                    NextValue(installed, 1), set_active.eq(1),
                    NextValue(inst_len, fl_len), NextValue(inst_crc, fl_crc),
                    If(fl_boot, NextState("RUN_RESET")).Else(*set_status(STOPPED),
                                                              NextState("IDLE")),
                ),
            ),
        )


        # IDLE: requests, traps, background flash jobs.
        fsm.act("IDLE",
            lock.eq(0),
            If(trap_flag,
                trap_clr.eq(1),
                If(st == RUNNING,
                    bridge_clear.eq(1),
                    *set_status(TRAPPED),
                    *log(m_trap, seg2_at=Array(Constant(a, aw_rom) for a, _ in trap_at)[trap_code],
                         seg2_n=Array(Constant(n, 5) for _, n in trap_at)[trap_code]),
                ),
            ).Elif(erase_pend & ~store.busy,
                store.erase_go.eq(1), store.slot.eq(~active),
                NextValue(erase_pend, 0),
            ).Elif(prog_pend & ~store.busy,
                store.prog_go.eq(1), store.slot.eq(~active),
                store.len.eq(inst_len), store.crc.eq(inst_crc), store.seq.eq(seq + 1),
                set_prog_busy.eq(1),
                NextValue(prog_pend, 0),
            ).Elif(net_dirty & ~store.busy,
                store.set_go.eq(1),
                NextValue(net_dirty, 0),
            ).Elif(self.req_valid & ~em_busy & ~log_pend & ~status_pend,
                self.req_ready.eq(1),
                NextValue(r_link, self.req_link), NextValue(r_id, self.req_id),
                NextValue(r_key, self.req_key), NextValue(r_len, self.req_len),
                NextValue(r_base, self.req_base),
                NextState("DISPATCH"),
            ),
        )
        self.comb += self.req_owner.eq(~fsm.ongoing("IDLE") & ~fsm.ongoing("BOOT"))
        self.cur_link = r_link
        released = Signal()

        def release():
            return [If(~released, self.req_release.eq(1)), NextValue(released, 1)]

        def read_body(n, then):
            return [NextValue(ri, 0), NextValue(rn, n), NextValue(ret, then),
                    NextState("RB_ADR")]

        R_BEGIN, R_CHUNK, R_NETGET, R_NETSET = 0, 1, 2, 3
        fsm.act("DISPATCH",
            NextValue(released, 0),
            If(r_key == K_INFO,
                If(r_len == 0, *respond(OK, B_INFO)).Else(*respond(INVALID_REQUEST)),
            ).Elif(r_key == K_TRUST,
                If(r_len == 0, *respond(OK, B_TRUST)).Else(*respond(INVALID_REQUEST)),
            ).Elif(r_key == K_NET,
                If(r_len == 1, *read_body(1, R_NETGET)
                ).Elif(r_len == 1 + NET_LEN, *read_body(1 + NET_LEN, R_NETSET)
                ).Else(*respond(INVALID_REQUEST)),
            ).Elif(r_key == K_BEGIN,
                If(r_len == 8, *read_body(8, R_BEGIN)).Else(*respond(INVALID_REQUEST)),
            ).Elif(r_key == K_CHUNK,
                If((r_len > 4) & (r_len <= 4 + CHUNK_MAX), *read_body(4, R_CHUNK)
                ).Else(*respond(INVALID_REQUEST)),
            ).Elif(r_key == K_COMMIT,
                If((r_len == 0) | (r_len == SIGNED_COMMIT_LEN), NextState("COMMIT")
                ).Else(*respond(INVALID_REQUEST)),
            ).Elif(r_key == K_START,
                If(r_len != 0, *respond(INVALID_REQUEST)
                ).Elif((st == EMPTY) | (st == LOADING), *respond(INVALID_REQUEST)
                ).Elif(st == RUNNING, *respond(OK)
                ).Else(NextState("START")),
            ).Elif(r_key == K_STOP,
                If(r_len != 0, *respond(INVALID_REQUEST)
                ).Elif((st == RUNNING) | (st == TRAPPED),
                    bridge_clear.eq(1), *set_status(STOPPED), *respond(OK),
                ).Else(*respond(OK)),
            ).Else(*respond(NOT_FOUND)),
        )
        # Body bytes into bb[0..rn); the buffer port answers a cycle later.
        fsm.act("RB_ADR", If(ri == rn, NextState("RB_DONE")).Else(NextState("RB_TAKE")))
        fsm.act("RB_TAKE",
            NextValue(bb[ri[:5]], self.buf_dat),
            NextValue(ri, ri + 1),
            NextState("RB_ADR"),
        )
        u32 = lambda o: Cat(bb[o], bb[o + 1], bb[o + 2], bb[o + 3])
        fsm.act("RB_DONE",
            If(ret == R_BEGIN, *release(), NextState("BEGIN")
            ).Elif(ret == R_CHUNK, NextState("CHUNK")
            ).Elif(ret == R_NETGET,
                *release(),
                If(bb[0] < n_links, NextValue(em_net, bb[0][0]), *respond(OK, B_NET)
                ).Else(*respond(INVALID_REQUEST)),
            ).Else(
                *release(),
                If(bb[0] < n_links, NextValue(em_net, bb[0][0]), *respond(OK),
                    NextValue(ret, R_NETSET),
                ).Else(NextValue(ret, 0), *respond(INVALID_REQUEST)),
            ),
        )
        # begin
        fsm.act("BEGIN",
            If(prog_pend | prog_busy, *respond(UNAVAILABLE)).Else(
                up.begin.eq(1), up.len_in.eq(u32(0)), up.crc_in.eq(u32(4)),
                If(up.code == OK,
                    If((st == RUNNING) | (st == TRAPPED), bridge_clear.eq(1)),
                    If(st != LOADING, *set_status(LOADING)),
                    NextValue(erase_pend, 1),
                    *([sha.start.eq(1)] if sha is not None else []),
                    *respond(OK),
                ).Else(
                    If(st == LOADING, *set_status(resting)),
                    *respond(up.code),
                ),
            ),
        )
        # chunk: the step, then the copy
        cpy_i = Signal(10)
        cpy_n = Signal(10)
        off = Signal(15)
        self.comb += [
            up.offset.eq(u32(0)), up.n.eq(r_len - 4),
        ]
        fsm.act("CHUNK",
            up.chunk.eq(1),
            If(up.write,
                NextValue(cpy_i, 0), NextValue(cpy_n, r_len - 4), NextValue(off, u32(0)[:15]),
                NextValue(ri, 4),
                NextState("CPY_ADR"),
            ).Else(
                *release(),
                If((up.code != OK) & (st == LOADING), *set_status(resting)),
                *respond(up.code),
            ),
        )
        sha_ready = sha.ready if sha is not None else Constant(1, 1)
        fsm.act("CPY_ADR", NextState("CPY_WR"))
        fsm.act("CPY_WR",
            If(sha_ready,
                img_we.eq(1), img_adr.eq((off + cpy_i)[:14]), img_dat.eq(self.buf_dat),
                up.wr_stb.eq(1), up.wr_byte.eq(self.buf_dat),
                *([sha.valid.eq(1), sha.data.eq(self.buf_dat)] if sha is not None else []),
                NextValue(cpy_i, cpy_i + 1), NextValue(ri, ri + 1),
                If(cpy_i + 1 == cpy_n, *release(), *respond(OK)).Else(NextState("CPY_ADR")),
            ),
        )
        # commit
        fsm.act("COMMIT",
            *release(),
            up.commit.eq(1),
            If(up.activate,
                *([sha.finish.eq(1)] if sha is not None else []),
                NextState("COMMIT_WAIT"),
            ).Else(
                If(st == LOADING, *set_status(resting)),
                *respond(INVALID_REQUEST),
            ),
        )
        fsm.act("COMMIT_WAIT",
            lock.eq(1),
            If(ev_idle & ~prog_busy,
                ld.img_len.eq(up.length), ld.start.eq(1),
                NextState("COMMIT_COMPILE"),
            ),
        )
        fsm.act("COMMIT_COMPILE",
            lock.eq(1),
            ld.img_len.eq(up.length),
            If(ld.done,
                If(ld.error == 0,
                    NextValue(installed, 1), NextValue(inst_len, up.length),
                    NextValue(inst_crc, up.crc),
                    NextValue(prog_pend, 1),
                    *set_status(STOPPED),
                    NextState("COMMIT_SHA"),
                ).Else(
                    *log(m_ref, seg2_at=Array(Constant(a, aw_rom) for a, _ in err_at)[ld.error],
                         seg2_n=Array(Constant(n, 5) for _, n in err_at)[ld.error]),
                    NextValue(em_code, INVALID_REQUEST), NextValue(em_body, B_NONE),
                    NextState("RESTORE"),
                ),
            ),
        )
        if sha is not None:
            fsm.act("COMMIT_SHA",
                If(~sha.busy,
                    *log(m_inst, m_sha, hexd=True),
                    *respond(OK),
                ),
            )
        else:
            fsm.act("COMMIT_SHA", *log(m_inst), *respond(OK))
        # A refused module: answer, then put the installed payload back.
        fsm.act("RESTORE",
            If(~em_busy,
                em_go.eq(1), em_id.eq(r_id), em_link.eq(r_link),
                If(installed,
                    NextValue(fl_slot, active), NextValue(fl_len, inst_len),
                    NextValue(fl_crc, inst_crc), NextValue(fl_boot, 0),
                    NextState("FL_COPY"),
                ).Else(*set_status(EMPTY), NextState("IDLE")),
            ),
        )
        # start: compile from the image RAM (a fresh instance), reset, init
        fsm.act("START",
            *release(),
            lock.eq(1),
            If(ev_idle & ~prog_busy,
                ld.img_len.eq(inst_len), ld.start.eq(1),
                NextState("START_COMPILE"),
            ),
        )
        fsm.act("START_COMPILE",
            lock.eq(1),
            ld.img_len.eq(inst_len),
            If(ld.done,
                NextValue(fl_boot, 0),
                If(ld.error == 0, NextState("RUN_RESET")).Else(
                    # Cannot happen: the image compiled when installed.
                    *set_status(TRAPPED), *respond(OK)),
            ),
        )
        fsm.act("RUN_RESET",
            lock.eq(1),
            bridge_clear.eq(1),
            ev_reset.eq(1),
            trap_clr.eq(1),
            NextState("RUN_INIT"),
        )
        fsm.act("RUN_INIT", lock.eq(1), ev_init.eq(1), NextState("RUN_INIT_W"))
        fsm.act("RUN_INIT_W",
            lock.eq(1),
            If(rt.ev_done,
                If(rt.ev_result >= R_TRAP,
                    bridge_clear.eq(1),
                    *set_status(TRAPPED),
                    *log(m_trap, seg2_at=Array(Constant(a, aw_rom) for a, _ in trap_at)[
                        (rt.ev_result - R_TRAP)[:4]],
                         seg2_n=Array(Constant(n, 5) for _, n in trap_at)[
                        (rt.ev_result - R_TRAP)[:4]]),
                ).Else(*set_status(RUNNING)),
                If(fl_boot, NextState("IDLE")).Else(*respond(OK)),
            ),
        )
        # The response (then a sys/net/addr set takes effect).
        fsm.act("RESP",
            *release(),
            If(~em_busy,
                em_go.eq(1), em_id.eq(r_id), em_link.eq(r_link),
                NextState("RESP_W"),
            ),
        )
        fsm.act("RESP_W",
            If(~em_busy,
                If((r_key == K_NET) & (ret == R_NETSET) & (em_code == OK),
                    NextValue(net_dirty, 1),
                ),
                NextValue(ret, 0),
                NextState("IDLE"),
            ),
        )
        for n, r in enumerate(self.net):
            rec = [bb[1 + k] for k in range(16)]
            self.sync += If(fsm.ongoing("RESP_W") & ~em_busy & (r_key == K_NET)
                            & (ret == R_NETSET) & (em_code == OK) & (em_net == n), *r.load(rec))

