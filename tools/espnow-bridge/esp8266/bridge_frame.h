// Serial frames between the host (mwctl) and the USB<->ESP-NOW bridge: a
// dependency-free C++ port of tools/mwctl/src/bridge.rs, which is the spec.
//
// Every message is one frame: the raw bytes below, then the CRC-16
// (CCITT-FALSE: poly 0x1021, init 0xFFFF, no reflection, no xorout) of
// everything before it, little-endian; the whole is COBS-stuffed and
// followed by one 0x00 delimiter.
//
//   type 1 Send  host->bridge  dst MAC 6, ESP-NOW frame (1..=250 bytes)
//   type 2 Recv  bridge->host  src MAC 6, ESP-NOW frame (1..=250 bytes)
//   type 3 Hello bridge->host  own MAC 6, channel u8, version u8 (1)
//   type 4 Query host->bridge  none; the bridge answers Hello then Stats
//   type 5 Stats bridge->host  six u32 LE counters: rx, rx_dropped, tx_ok,
//                              tx_fail, tx_dropped, serial_bad
//   type 6 Sent  bridge->host  status u8 (0 acknowledged, 1 failed), dst MAC 6
//
// Pure functions and one fixed-size class only (stdint/stddef, no heap, no
// recursion, every loop bounded by its input length), so test_bridge_frame.cpp
// checks them on the host against the golden frames in bridge.rs.
#pragma once

#include <stddef.h>
#include <stdint.h>

namespace bf {

constexpr size_t kMacLen = 6;
constexpr size_t kMaxData = 250;
// Largest raw frame: type, MAC, data, CRC.
constexpr size_t kMaxRaw = 1 + kMacLen + kMaxData + 2;

constexpr size_t cobs_max_encoded_len(size_t n) { return n + n / 254 + 1; }

// Largest COBS run (no delimiter) and largest wire frame (with delimiter).
constexpr size_t kMaxCobs = cobs_max_encoded_len(kMaxRaw);
constexpr size_t kMaxWire = kMaxCobs + 1;

constexpr uint8_t kVersion = 1;
constexpr size_t kStatsCount = 6;

enum Type : uint8_t {
  kSend = 1,
  kRecv = 2,
  kHello = 3,
  kQuery = 4,
  kStats = 5,
  kSent = 6,
};

enum Error : int {
  kErrCobs = -1,         // bad COBS encoding
  kErrCrc = -2,          // CRC mismatch
  kErrMalformed = -3,    // wrong length for its type, data outside 1..=250
  kErrUnknownType = -4,  // unknown type byte
  kErrNotHost = -5,      // valid frame, but not one the host sends the bridge
  kErrBufferTooSmall = -6,
};

// Counters in Stats-frame order.
struct Stats {
  uint32_t rx;
  uint32_t rx_dropped;
  uint32_t tx_ok;
  uint32_t tx_fail;
  uint32_t tx_dropped;
  uint32_t serial_bad;
};

// A decoded frame. `data` points into the buffer it was decoded from.
struct Msg {
  Type type;
  uint8_t mac[kMacLen];  // Send dst, Recv src, Hello own MAC, Sent dst
  const uint8_t* data;   // Send/Recv
  size_t data_len;       // Send/Recv
  uint8_t channel;       // Hello
  uint8_t version;       // Hello
  bool ok;               // Sent
  Stats stats;           // Stats
};

inline uint16_t crc16(const uint8_t* bytes, size_t len) {
  uint16_t crc = 0xFFFF;
  for (size_t i = 0; i < len; ++i) {
    crc = static_cast<uint16_t>(crc ^ (static_cast<uint16_t>(bytes[i]) << 8));
    for (int bit = 0; bit < 8; ++bit) {
      crc = (crc & 0x8000) ? static_cast<uint16_t>((crc << 1) ^ 0x1021) : static_cast<uint16_t>(crc << 1);
    }
  }
  return crc;
}

// COBS-encodes src[0..len) into dst[0..cap) without a delimiter. Returns the
// encoded length, or kErrBufferTooSmall.
inline int cobs_encode(const uint8_t* src, size_t len, uint8_t* dst, size_t cap) {
  if (cap == 0) return kErrBufferTooSmall;
  size_t code_idx = 0;
  size_t out = 1;
  uint8_t code = 1;
  for (size_t i = 0; i < len; ++i) {
    const uint8_t b = src[i];
    if (b == 0) {
      dst[code_idx] = code;
      code_idx = out;
      if (out >= cap) return kErrBufferTooSmall;
      ++out;
      code = 1;
    } else {
      if (out >= cap) return kErrBufferTooSmall;
      dst[out++] = b;
      ++code;
      if (code == 0xFF) {
        dst[code_idx] = code;
        code_idx = out;
        if (out >= cap) return kErrBufferTooSmall;
        ++out;
        code = 1;
      }
    }
  }
  dst[code_idx] = code;
  return static_cast<int>(out);
}

// Decodes buf[0..len) (one run, delimiter removed) in place. Returns the
// decoded length, or kErrCobs for a malformed encoding (including any zero).
inline int cobs_decode_in_place(uint8_t* buf, size_t len) {
  size_t r = 0;
  size_t w = 0;
  while (r < len) {
    const uint8_t code = buf[r];
    if (code == 0) return kErrCobs;
    ++r;
    for (uint8_t k = 1; k < code; ++k) {
      if (r >= len) return kErrCobs;
      const uint8_t b = buf[r];
      if (b == 0) return kErrCobs;
      buf[w++] = b;
      ++r;
    }
    if (code != 0xFF && r < len) buf[w++] = 0;
  }
  return static_cast<int>(w);
}

// Appends the CRC to raw[0..len) (raw must have room for two more bytes),
// COBS-encodes it into out[0..cap) and adds the 0x00 delimiter. Returns the
// wire length or kErrBufferTooSmall.
inline int finish_wire(uint8_t* raw, size_t len, uint8_t* out, size_t cap) {
  const uint16_t crc = crc16(raw, len);
  raw[len] = static_cast<uint8_t>(crc & 0xFF);
  raw[len + 1] = static_cast<uint8_t>(crc >> 8);
  if (cap == 0) return kErrBufferTooSmall;
  const int n = cobs_encode(raw, len + 2, out, cap - 1);
  if (n < 0) return n;
  out[n] = 0;
  return n + 1;
}

inline void put_mac(uint8_t* dst, const uint8_t* mac) {
  for (size_t i = 0; i < kMacLen; ++i) dst[i] = mac[i];
}

inline void put_u32(uint8_t* dst, uint32_t v) {
  dst[0] = static_cast<uint8_t>(v);
  dst[1] = static_cast<uint8_t>(v >> 8);
  dst[2] = static_cast<uint8_t>(v >> 16);
  dst[3] = static_cast<uint8_t>(v >> 24);
}

inline uint32_t get_u32(const uint8_t* src) {
  return static_cast<uint32_t>(src[0]) | (static_cast<uint32_t>(src[1]) << 8) |
         (static_cast<uint32_t>(src[2]) << 16) | (static_cast<uint32_t>(src[3]) << 24);
}

// Send (type 1) or Recv (type 2): mac, then 1..=250 data bytes.
inline int encode_mac_data(Type type, const uint8_t* mac, const uint8_t* data, size_t len, uint8_t* out,
                           size_t cap) {
  if (len == 0 || len > kMaxData) return kErrMalformed;
  uint8_t raw[kMaxRaw];
  raw[0] = type;
  put_mac(raw + 1, mac);
  for (size_t i = 0; i < len; ++i) raw[1 + kMacLen + i] = data[i];
  return finish_wire(raw, 1 + kMacLen + len, out, cap);
}

// Each encoder writes the full wire frame (delimiter included) into
// out[0..cap) and returns its length, or a negative Error.
inline int encode_send(const uint8_t* dst, const uint8_t* data, size_t len, uint8_t* out, size_t cap) {
  return encode_mac_data(kSend, dst, data, len, out, cap);
}

inline int encode_recv(const uint8_t* src, const uint8_t* data, size_t len, uint8_t* out, size_t cap) {
  return encode_mac_data(kRecv, src, data, len, out, cap);
}

inline int encode_hello(const uint8_t* mac, uint8_t channel, uint8_t version, uint8_t* out, size_t cap) {
  uint8_t raw[1 + kMacLen + 2 + 2];
  raw[0] = kHello;
  put_mac(raw + 1, mac);
  raw[1 + kMacLen] = channel;
  raw[2 + kMacLen] = version;
  return finish_wire(raw, 3 + kMacLen, out, cap);
}

inline int encode_query(uint8_t* out, size_t cap) {
  uint8_t raw[1 + 2];
  raw[0] = kQuery;
  return finish_wire(raw, 1, out, cap);
}

inline int encode_stats(const Stats& s, uint8_t* out, size_t cap) {
  uint8_t raw[1 + 4 * kStatsCount + 2];
  raw[0] = kStats;
  const uint32_t v[kStatsCount] = {s.rx, s.rx_dropped, s.tx_ok, s.tx_fail, s.tx_dropped, s.serial_bad};
  for (size_t i = 0; i < kStatsCount; ++i) put_u32(raw + 1 + 4 * i, v[i]);
  return finish_wire(raw, 1 + 4 * kStatsCount, out, cap);
}

inline int encode_sent(bool ok, const uint8_t* dst, uint8_t* out, size_t cap) {
  uint8_t raw[2 + kMacLen + 2];
  raw[0] = kSent;
  raw[1] = ok ? 0 : 1;
  put_mac(raw + 2, dst);
  return finish_wire(raw, 2 + kMacLen, out, cap);
}

// Parses a raw frame raw[0..len) (COBS removed) into *msg. Returns the type
// (> 0) or a negative Error. Checks in the order bridge.rs does: CRC first,
// then type and layout.
inline int decode_raw(const uint8_t* raw, size_t len, Msg* msg) {
  if (len < 2) return kErrMalformed;
  const size_t body = len - 2;
  const uint16_t crc = static_cast<uint16_t>(raw[body] | (raw[body + 1] << 8));
  if (crc16(raw, body) != crc) return kErrCrc;
  if (body == 0) return kErrMalformed;
  const uint8_t t = raw[0];
  const uint8_t* rest = raw + 1;
  const size_t rest_len = body - 1;
  switch (t) {
    case kSend:
    case kRecv: {
      if (rest_len < kMacLen) return kErrMalformed;
      const size_t data_len = rest_len - kMacLen;
      if (data_len == 0 || data_len > kMaxData) return kErrMalformed;
      put_mac(msg->mac, rest);
      msg->data = rest + kMacLen;
      msg->data_len = data_len;
      break;
    }
    case kHello:
      if (rest_len != kMacLen + 2) return kErrMalformed;
      put_mac(msg->mac, rest);
      msg->channel = rest[kMacLen];
      msg->version = rest[kMacLen + 1];
      break;
    case kQuery:
      if (rest_len != 0) return kErrMalformed;
      break;
    case kStats:
      if (rest_len != 4 * kStatsCount) return kErrMalformed;
      msg->stats.rx = get_u32(rest);
      msg->stats.rx_dropped = get_u32(rest + 4);
      msg->stats.tx_ok = get_u32(rest + 8);
      msg->stats.tx_fail = get_u32(rest + 12);
      msg->stats.tx_dropped = get_u32(rest + 16);
      msg->stats.serial_bad = get_u32(rest + 20);
      break;
    case kSent:
      if (rest_len != 1 + kMacLen || rest[0] > 1) return kErrMalformed;
      msg->ok = rest[0] == 0;
      put_mac(msg->mac, rest + 1);
      break;
    default:
      return kErrUnknownType;
  }
  msg->type = static_cast<Type>(t);
  return t;
}

// COBS-decodes one delimited run run[0..len) in place and parses it; data in
// *msg then points into run. Returns the type (> 0) or a negative Error.
inline int decode_run(uint8_t* run, size_t len, Msg* msg) {
  const int n = cobs_decode_in_place(run, len);
  if (n < 0) return n;
  return decode_raw(run, static_cast<size_t>(n), msg);
}

// decode_run restricted to what the host sends the bridge (Send, Query);
// any other valid frame is kErrNotHost.
inline int decode_host(uint8_t* run, size_t len, Msg* msg) {
  const int t = decode_run(run, len, msg);
  if (t < 0) return t;
  if (t != kSend && t != kQuery) return kErrNotHost;
  return t;
}

// Outcome of feeding one byte to a Splitter.
enum Split : uint8_t {
  kPending,   // more bytes are needed
  kFrame,     // a delimiter ended a non-empty run; see frame()/frame_len()
  kOverflow,  // a run exceeded N bytes and was discarded up to its delimiter
};

// Splits a byte stream at 0x00 delimiters into an N-byte buffer, like
// middleware's serial::frame::Splitter. Empty runs (consecutive delimiters)
// are ignored. After kFrame the run stays in frame() (mutable, so it can be
// decoded in place) until the next push.
template <size_t N>
class Splitter {
 public:
  Split push(uint8_t byte) {
    if (ready_) {
      ready_ = false;
      len_ = 0;
    }
    if (byte == 0) {
      if (overflowed_) {
        overflowed_ = false;
        len_ = 0;
        return kOverflow;
      }
      if (len_ == 0) return kPending;
      ready_ = true;
      return kFrame;
    }
    if (overflowed_) return kPending;
    if (len_ < N) {
      buf_[len_++] = byte;
    } else {
      overflowed_ = true;
    }
    return kPending;
  }

  uint8_t* frame() { return buf_; }
  size_t frame_len() const { return ready_ ? len_ : 0; }

 private:
  uint8_t buf_[N] = {};
  size_t len_ = 0;
  bool ready_ = false;
  bool overflowed_ = false;
};

}  // namespace bf
