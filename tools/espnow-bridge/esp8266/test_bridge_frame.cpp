// Host test for bridge_frame.h: the C++ bridge codec must be byte-identical
// to the host's Rust codec (tools/mwctl/src/bridge.rs). The golden wire
// frames below are copied from bridge.rs `golden()` (also reproduced by an
// independent Python COBS/CRC-16 script), so a mismatch means mwctl and the
// bridge firmware would not understand each other.
//
// Build and run: g++ -std=c++17 -Wall -Wextra -Werror test_bridge_frame.cpp && ./a.out
// (tools/espnow-bridge/check.sh does this).
#include <cstdio>
#include <cstring>

#include "bridge_frame.h"

static int failures = 0;
#define CHECK(cond)                                               \
  do {                                                            \
    if (!(cond)) {                                                \
      std::printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); \
      ++failures;                                                 \
    }                                                             \
  } while (0)

static const uint8_t kBridge[6] = {0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54};
static const uint8_t kDev[6] = {0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24};

// Parses a hex string into out; returns its byte length.
static size_t unhex(const char* hex, uint8_t* out, size_t cap) {
  const size_t len = std::strlen(hex) / 2;
  for (size_t i = 0; i < len && i < cap; ++i) {
    unsigned v = 0;
    std::sscanf(hex + 2 * i, "%2x", &v);
    out[i] = static_cast<uint8_t>(v);
  }
  return len;
}

static bool wire_is(const uint8_t* got, int n, const char* hex) {
  uint8_t want[bf::kMaxWire];
  const size_t len = unhex(hex, want, sizeof want);
  if (n != static_cast<int>(len) || std::memcmp(got, want, len) != 0) {
    std::printf("  got  ");
    for (int i = 0; i < n; ++i) std::printf("%02x", got[i]);
    std::printf("\n  want %s\n", hex);
    return false;
  }
  return true;
}

// Decodes a wire frame (delimiter included) copied into run.
static int decode_wire(const uint8_t* wire, int n, uint8_t* run, bf::Msg* msg) {
  if (n < 1) return n;
  std::memcpy(run, wire, static_cast<size_t>(n - 1));
  return bf::decode_run(run, static_cast<size_t>(n - 1), msg);
}

// Raw bytes plus their CRC, COBS-encoded into run (no delimiter).
static size_t with_crc_run(const uint8_t* body, size_t len, uint8_t* run, size_t cap) {
  uint8_t raw[bf::kMaxRaw + 8];
  std::memcpy(raw, body, len);
  const uint16_t crc = bf::crc16(body, len);
  raw[len] = static_cast<uint8_t>(crc & 0xFF);
  raw[len + 1] = static_cast<uint8_t>(crc >> 8);
  const int n = bf::cobs_encode(raw, len + 2, run, cap);
  return n < 0 ? 0 : static_cast<size_t>(n);
}

static void golden_frames_match_rust() {
  uint8_t out[bf::kMaxWire];
  uint8_t run[bf::kMaxWire];
  bf::Msg m{};
  const uint8_t send_data[] = {'M', 'R', 0x01, 0x01, 0x00, 0x00};
  const uint8_t recv_data[] = {0x00, 0xff, 0x00};

  int n = bf::encode_send(kDev, send_data, sizeof send_data, out, sizeof out);
  CHECK(wire_is(out, n, "0c01e4b06381d3244d520101010398c400"));
  CHECK(decode_wire(out, n, run, &m) == bf::kSend);
  CHECK(std::memcmp(m.mac, kDev, 6) == 0 && m.data_len == sizeof send_data &&
        std::memcmp(m.data, send_data, sizeof send_data) == 0);

  n = bf::encode_recv(kDev, recv_data, sizeof recv_data, out, sizeof out);
  CHECK(wire_is(out, n, "0802e4b06381d32402ff03fe8200"));
  CHECK(decode_wire(out, n, run, &m) == bf::kRecv);
  CHECK(std::memcmp(m.mac, kDev, 6) == 0 && m.data_len == sizeof recv_data &&
        std::memcmp(m.data, recv_data, sizeof recv_data) == 0);

  n = bf::encode_hello(kBridge, 1, bf::kVersion, out, sizeof out);
  CHECK(wire_is(out, n, "0c035ccf7f72f654010149d300"));
  CHECK(decode_wire(out, n, run, &m) == bf::kHello);
  CHECK(std::memcmp(m.mac, kBridge, 6) == 0 && m.channel == 1 && m.version == 1);

  n = bf::encode_query(out, sizeof out);
  CHECK(wire_is(out, n, "040474a100"));
  CHECK(decode_wire(out, n, run, &m) == bf::kQuery);

  const bf::Stats s = {1, 2, 3, 4, 5, 0x01020304};
  n = bf::encode_stats(s, out, sizeof out);
  CHECK(wire_is(out, n, "030501010102020101020301010204010102050101070403020124d100"));
  CHECK(decode_wire(out, n, run, &m) == bf::kStats);
  CHECK(m.stats.rx == 1 && m.stats.rx_dropped == 2 && m.stats.tx_ok == 3 && m.stats.tx_fail == 4 &&
        m.stats.tx_dropped == 5 && m.stats.serial_bad == 0x01020304);

  n = bf::encode_sent(false, kDev, out, sizeof out);
  CHECK(wire_is(out, n, "0b0601e4b06381d324c54000"));
  CHECK(decode_wire(out, n, run, &m) == bf::kSent);
  CHECK(!m.ok && std::memcmp(m.mac, kDev, 6) == 0);

  n = bf::encode_sent(true, kDev, out, sizeof out);
  CHECK(wire_is(out, n, "020609e4b06381d324a4f800"));
  CHECK(decode_wire(out, n, run, &m) == bf::kSent);
  CHECK(m.ok && std::memcmp(m.mac, kDev, 6) == 0);
}

static void host_frames_round_trip() {
  uint8_t out[bf::kMaxWire];
  uint8_t run[bf::kMaxWire];
  bf::Msg m{};
  // Largest Send, all zeros (worst case for COBS), fits kMaxWire.
  uint8_t big[bf::kMaxData] = {};
  int n = bf::encode_send(kDev, big, sizeof big, out, sizeof out);
  CHECK(n > 0 && static_cast<size_t>(n) <= bf::kMaxWire);
  for (int i = 0; i + 1 < n; ++i) CHECK(out[i] != 0);
  CHECK(out[n - 1] == 0);
  std::memcpy(run, out, static_cast<size_t>(n - 1));
  CHECK(bf::decode_host(run, static_cast<size_t>(n - 1), &m) == bf::kSend);
  CHECK(m.data_len == bf::kMaxData && std::memcmp(m.mac, kDev, 6) == 0);

  n = bf::encode_query(out, sizeof out);
  std::memcpy(run, out, static_cast<size_t>(n - 1));
  CHECK(bf::decode_host(run, static_cast<size_t>(n - 1), &m) == bf::kQuery);

  // Bridge->host frames are valid but not host frames.
  n = bf::encode_sent(true, kDev, out, sizeof out);
  std::memcpy(run, out, static_cast<size_t>(n - 1));
  CHECK(bf::decode_host(run, static_cast<size_t>(n - 1), &m) == bf::kErrNotHost);

  // A buffer one byte short is refused, not overrun.
  n = bf::encode_query(out, 5);
  CHECK(n == 5);
  CHECK(bf::encode_query(out, 4) == bf::kErrBufferTooSmall);
}

static void bad_frames_are_rejected() {
  uint8_t out[bf::kMaxWire];
  uint8_t run[bf::kMaxWire];
  bf::Msg m{};
  uint8_t big[bf::kMaxData + 1] = {};
  CHECK(bf::encode_send(kDev, big, 0, out, sizeof out) == bf::kErrMalformed);
  CHECK(bf::encode_recv(kDev, big, bf::kMaxData + 1, out, sizeof out) == bf::kErrMalformed);

  // Corrupted CRC.
  int n = bf::encode_query(out, sizeof out);
  out[n - 2] ^= 0x01;
  CHECK(decode_wire(out, n, run, &m) == bf::kErrCrc);

  // Unknown type, Query with a body, Send with 0 and 251 data bytes, bad
  // Sent status, short Stats.
  const uint8_t unknown[] = {9};
  size_t r = with_crc_run(unknown, sizeof unknown, run, sizeof run);
  CHECK(bf::decode_run(run, r, &m) == bf::kErrUnknownType);
  const uint8_t query_body[] = {bf::kQuery, 0};
  r = with_crc_run(query_body, sizeof query_body, run, sizeof run);
  CHECK(bf::decode_host(run, r, &m) == bf::kErrMalformed);
  const uint8_t send_empty[] = {bf::kSend, 1, 2, 3, 4, 5, 6};
  r = with_crc_run(send_empty, sizeof send_empty, run, sizeof run);
  CHECK(bf::decode_host(run, r, &m) == bf::kErrMalformed);
  uint8_t send_long[1 + 6 + bf::kMaxData + 1] = {bf::kSend, 1, 2, 3, 4, 5, 6};
  for (size_t i = 7; i < sizeof send_long; ++i) send_long[i] = static_cast<uint8_t>(i);
  r = with_crc_run(send_long, sizeof send_long, run, sizeof run);
  CHECK(r > 0);
  CHECK(bf::decode_host(run, r, &m) == bf::kErrMalformed);
  const uint8_t sent_bad[] = {bf::kSent, 2, 1, 2, 3, 4, 5, 6};
  r = with_crc_run(sent_bad, sizeof sent_bad, run, sizeof run);
  CHECK(bf::decode_run(run, r, &m) == bf::kErrMalformed);
  const uint8_t stats_short[] = {bf::kStats, 0};
  r = with_crc_run(stats_short, sizeof stats_short, run, sizeof run);
  CHECK(bf::decode_run(run, r, &m) == bf::kErrMalformed);

  // Too short for a CRC.
  const uint8_t one[] = {1};
  CHECK(bf::decode_raw(one, sizeof one, &m) == bf::kErrMalformed);

  // Bad COBS: a run code pointing past the end.
  uint8_t bad_cobs[] = {0x05, 0x01};
  CHECK(bf::decode_run(bad_cobs, sizeof bad_cobs, &m) == bf::kErrCobs);
}

static void cobs_reference_vectors() {
  uint8_t enc[8];
  const uint8_t a[] = {0x11, 0x22, 0x00, 0x33};
  int n = bf::cobs_encode(a, sizeof a, enc, sizeof enc);
  const uint8_t a_want[] = {0x03, 0x11, 0x22, 0x02, 0x33};
  CHECK(n == 5 && std::memcmp(enc, a_want, 5) == 0);
  const uint8_t z[] = {0x00};
  n = bf::cobs_encode(z, 1, enc, sizeof enc);
  CHECK(n == 2 && enc[0] == 1 && enc[1] == 1);
  n = bf::cobs_encode(z, 0, enc, sizeof enc);
  CHECK(n == 1 && enc[0] == 1);
  const uint8_t three[] = {1, 2, 3};
  CHECK(bf::cobs_encode(three, 3, enc, 3) == bf::kErrBufferTooSmall);
  uint8_t embedded_zero[] = {0x02, 0x00};
  CHECK(bf::cobs_decode_in_place(embedded_zero, 2) == bf::kErrCobs);

  // Long runs (code 0xFF) round-trip.
  uint8_t src[600];
  for (size_t i = 0; i < sizeof src; ++i) src[i] = static_cast<uint8_t>(i % 300 < 260 ? 1 : i % 7);
  uint8_t buf[bf::cobs_max_encoded_len(sizeof src)];
  n = bf::cobs_encode(src, sizeof src, buf, sizeof buf);
  CHECK(n > 0);
  for (int i = 0; i < n; ++i) CHECK(buf[i] != 0);
  CHECK(bf::cobs_decode_in_place(buf, static_cast<size_t>(n)) == static_cast<int>(sizeof src));
  CHECK(std::memcmp(buf, src, sizeof src) == 0);
}

// Feeds bytes to a splitter and decodes each run the way the bridge does:
// a run that fails to decode (or overflows) counts as bad.
struct Fed {
  int send = 0;
  int query = 0;
  int bad = 0;
};

static void feed(bf::Splitter<bf::kMaxCobs>& s, const uint8_t* bytes, size_t len, Fed* fed) {
  for (size_t i = 0; i < len; ++i) {
    const bf::Split r = s.push(bytes[i]);
    if (r == bf::kOverflow) {
      ++fed->bad;
    } else if (r == bf::kFrame) {
      bf::Msg m{};
      const int t = bf::decode_host(s.frame(), s.frame_len(), &m);
      if (t == bf::kSend) {
        ++fed->send;
      } else if (t == bf::kQuery) {
        ++fed->query;
      } else {
        ++fed->bad;
      }
    }
  }
}

static void splitter_finds_frames_in_noise() {
  uint8_t frame[bf::kMaxWire];
  const uint8_t data[] = {'M', 'W', 1};
  const int sn = bf::encode_send(kDev, data, sizeof data, frame, sizeof frame);
  uint8_t query[8];
  const int qn = bf::encode_query(query, sizeof query);

  // Noise with no delimiter shares a run with the first frame: that run is
  // bad, the next frame decodes.
  {
    bf::Splitter<bf::kMaxCobs> s;
    Fed fed;
    const char banner[] = "\r\n ets Jan  8 2013,rst cause:2\r\n";
    feed(s, reinterpret_cast<const uint8_t*>(banner), sizeof banner - 1, &fed);
    feed(s, frame, static_cast<size_t>(sn), &fed);
    feed(s, query, static_cast<size_t>(qn), &fed);
    CHECK(fed.bad == 1 && fed.send == 0 && fed.query == 1);
  }
  // Noise ended by a delimiter, then empty runs, then frames.
  {
    bf::Splitter<bf::kMaxCobs> s;
    Fed fed;
    const uint8_t noise[] = {'x', 'y', 0x05, 0, 0, 0};
    feed(s, noise, sizeof noise, &fed);
    feed(s, frame, static_cast<size_t>(sn), &fed);
    feed(s, query, static_cast<size_t>(qn), &fed);
    CHECK(fed.bad == 1 && fed.send == 1 && fed.query == 1);
  }
  // A run longer than the buffer is dropped up to its delimiter, once.
  {
    bf::Splitter<bf::kMaxCobs> s;
    Fed fed;
    uint8_t flood[bf::kMaxCobs + 40];
    for (size_t i = 0; i < sizeof flood; ++i) flood[i] = 0x41;
    feed(s, flood, sizeof flood, &fed);
    CHECK(fed.bad == 0);
    const uint8_t zero = 0;
    feed(s, &zero, 1, &fed);
    CHECK(fed.bad == 1);
    feed(s, frame, static_cast<size_t>(sn), &fed);
    CHECK(fed.bad == 1 && fed.send == 1);
  }
  // A maximal Send (250 non-zero bytes, a 0xFF COBS block) still fits.
  {
    bf::Splitter<bf::kMaxCobs> s;
    Fed fed;
    uint8_t big[bf::kMaxData];
    for (size_t i = 0; i < sizeof big; ++i) big[i] = 0x41;
    uint8_t wire[bf::kMaxWire];
    const int n = bf::encode_send(kBridge, big, sizeof big, wire, sizeof wire);
    CHECK(n > 0 && n <= static_cast<int>(bf::kMaxWire));
    feed(s, wire, static_cast<size_t>(n), &fed);
    CHECK(fed.send == 1 && fed.bad == 0);
  }
}

int main() {
  golden_frames_match_rust();
  host_frames_round_trip();
  bad_frames_are_rejected();
  cobs_reference_vectors();
  splitter_finds_frames_in_noise();
  if (failures != 0) {
    std::printf("%d bridge frame check(s) failed\n", failures);
    return 1;
  }
  std::printf("all bridge frame tests passed\n");
  return 0;
}
