// USB-serial <-> ESP-NOW bridge for an ESP8266 (Arduino core 3.1.x).
//
// A dumb radio: every protocol decision lives on the host (mwctl,
// tools/mwctl/src/bridge.rs, which is the spec for the serial frames in
// bridge_frame.h). The bridge only moves frames:
//
// - Serial at 460800 baud. After the ROM banner (74880 baud, unavoidable)
//   it writes nothing but COBS frames ending in 0x00.
// - Wi-Fi station mode, unassociated, parked on channel 1 (as the ESP32
//   firmware), ESP-NOW role COMBO, broadcast peer added.
// - Hello {own STA MAC, channel, version 1} at boot; Query -> Hello + Stats.
// - Send {dst, data}: queued (depth 4, full -> tx_dropped); one esp_now_send
//   at a time, adding dst as a peer if needed (the bridge keeps the 6 peers
//   it added in LRU order and evicts the oldest when the driver's table is
//   full). The send callback, or 50 ms without one, becomes Sent {ok, dst}.
// - Every ESP-NOW frame heard (MW publications, MR rpc, anything) becomes
//   Recv {src, data}; the receive ring holds 8 (full -> rx_dropped).
// - Serial runs that fail to decode, or are not Send/Query, count as
//   serial_bad. Counters saturate at 2^32-1.
//
// Build (from the repo root):
//   ~/.local/bin/arduino-cli compile --fqbn esp8266:esp8266:generic tools/espnow-bridge/esp8266
// Codec host test and build: tools/espnow-bridge/check.sh
// Upload is done separately (not by these scripts).

#include <ESP8266WiFi.h>

extern "C" {
#include <espnow.h>
#include <user_interface.h>
}

#include "bridge_frame.h"

namespace {

constexpr uint32_t kBaud = 460800;
constexpr uint8_t kChannel = 1;
constexpr uint32_t kSendTimeoutMs = 50;
constexpr size_t kMaxSerialPerPass = 1024;

constexpr uint8_t kRxDepth = 8;    // power of two: indices are free-running u8
constexpr uint8_t kDoneDepth = 4;  // power of two
constexpr size_t kTxDepth = 4;
constexpr size_t kPeerSlots = 6;

uint8_t kBroadcast[bf::kMacLen] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};

inline void sat_inc(uint32_t& c) {
  if (c != UINT32_MAX) ++c;
}

bool same_mac(const uint8_t* a, const uint8_t* b) { return memcmp(a, b, bf::kMacLen) == 0; }

// ---- Wi-Fi context -> loop(): single producer, single consumer rings. ----

struct RxSlot {
  uint8_t mac[bf::kMacLen];
  uint8_t len;
  uint8_t data[bf::kMaxData];
};
RxSlot rx_ring[kRxDepth];
volatile uint8_t rx_head = 0;  // written by the callback
volatile uint8_t rx_tail = 0;  // written by loop()
volatile uint32_t rx_dropped = 0;

struct Done {
  uint8_t mac[bf::kMacLen];
  uint8_t status;
};
Done done_ring[kDoneDepth];
volatile uint8_t done_head = 0;
volatile uint8_t done_tail = 0;

void on_recv(uint8_t* mac, uint8_t* data, uint8_t len) {
  const uint8_t head = rx_head;
  if (static_cast<uint8_t>(head - rx_tail) >= kRxDepth || len == 0 || len > bf::kMaxData) {
    if (rx_dropped != UINT32_MAX) rx_dropped = rx_dropped + 1;
    return;
  }
  RxSlot& slot = rx_ring[head % kRxDepth];
  memcpy(slot.mac, mac, bf::kMacLen);
  memcpy(slot.data, data, len);
  slot.len = len;
  rx_head = static_cast<uint8_t>(head + 1);
}

void on_sent(uint8_t* mac, uint8_t status) {
  const uint8_t head = done_head;
  if (static_cast<uint8_t>(head - done_tail) >= kDoneDepth) return;  // loop() times the send out
  Done& d = done_ring[head % kDoneDepth];
  memcpy(d.mac, mac, bf::kMacLen);
  d.status = status;
  done_head = static_cast<uint8_t>(head + 1);
}

// ---- loop()-owned state. ----

uint8_t own_mac[bf::kMacLen];
bf::Stats stats = {};
bf::Splitter<bf::kMaxCobs> splitter;

struct TxSlot {
  uint8_t dst[bf::kMacLen];
  uint8_t len;
  uint8_t data[bf::kMaxData];
};
TxSlot tx_queue[kTxDepth];
size_t tx_first = 0;
size_t tx_count = 0;

bool in_flight = false;
uint8_t in_flight_dst[bf::kMacLen];
uint32_t in_flight_at = 0;

// Peers this bridge added, least recently used first by stamp.
struct Peer {
  bool used;
  uint8_t mac[bf::kMacLen];
  uint32_t stamp;
};
Peer peers[kPeerSlots];
uint32_t peer_clock = 0;

void emit(const uint8_t* wire, int n) {
  if (n > 0) Serial.write(wire, static_cast<size_t>(n));
}

void emit_hello() {
  uint8_t out[bf::kMaxWire];
  emit(out, bf::encode_hello(own_mac, wifi_get_channel(), bf::kVersion, out, sizeof out));
}

void emit_stats() {
  uint8_t out[bf::kMaxWire];
  stats.rx_dropped = rx_dropped;
  emit(out, bf::encode_stats(stats, out, sizeof out));
}

void emit_sent(bool ok, const uint8_t* dst) {
  uint8_t out[bf::kMaxWire];
  emit(out, bf::encode_sent(ok, dst, out, sizeof out));
}

size_t peer_find(const uint8_t* mac) {
  for (size_t i = 0; i < kPeerSlots; ++i) {
    if (peers[i].used && same_mac(peers[i].mac, mac)) return i;
  }
  return kPeerSlots;
}

// Deletes the least recently used peer the bridge added; false if none.
bool peer_evict_lru() {
  size_t victim = kPeerSlots;
  for (size_t i = 0; i < kPeerSlots; ++i) {
    if (peers[i].used && (victim == kPeerSlots || peer_clock - peers[i].stamp > peer_clock - peers[victim].stamp)) {
      victim = i;
    }
  }
  if (victim == kPeerSlots) return false;
  esp_now_del_peer(peers[victim].mac);
  peers[victim].used = false;
  return true;
}

size_t peer_free_slot() {
  for (size_t i = 0; i < kPeerSlots; ++i) {
    if (!peers[i].used) return i;
  }
  return kPeerSlots;
}

// Makes dst usable with esp_now_send; false on a peer table error. A full
// table, or a driver that refuses the add, costs the least recently used
// peer this bridge added (then the add is retried once).
bool peer_ensure(uint8_t* dst) {
  if (same_mac(dst, kBroadcast)) return true;
  ++peer_clock;
  const size_t known = peer_find(dst);
  if (esp_now_is_peer_exist(dst) > 0) {
    if (known < kPeerSlots) peers[known].stamp = peer_clock;
    return true;
  }
  if (known < kPeerSlots) peers[known].used = false;  // the driver no longer has it
  if (peer_free_slot() == kPeerSlots) peer_evict_lru();
  if (esp_now_add_peer(dst, ESP_NOW_ROLE_COMBO, kChannel, nullptr, 0) != 0) {
    if (!peer_evict_lru()) return false;
    if (esp_now_add_peer(dst, ESP_NOW_ROLE_COMBO, kChannel, nullptr, 0) != 0) return false;
  }
  const size_t slot = peer_free_slot();
  if (slot < kPeerSlots) {
    peers[slot].used = true;
    memcpy(peers[slot].mac, dst, bf::kMacLen);
    peers[slot].stamp = peer_clock;
  }
  return true;
}

void tx_enqueue(const bf::Msg& m) {
  if (tx_count >= kTxDepth) {
    sat_inc(stats.tx_dropped);
    return;
  }
  TxSlot& slot = tx_queue[(tx_first + tx_count) % kTxDepth];
  memcpy(slot.dst, m.mac, bf::kMacLen);
  memcpy(slot.data, m.data, m.data_len);
  slot.len = static_cast<uint8_t>(m.data_len);
  ++tx_count;
}

void handle_run(uint8_t* run, size_t len) {
  bf::Msg m{};
  const int t = bf::decode_host(run, len, &m);
  if (t == bf::kSend) {
    tx_enqueue(m);
  } else if (t == bf::kQuery) {
    emit_hello();
    emit_stats();
  } else {
    sat_inc(stats.serial_bad);
  }
}

void pump_serial() {
  int avail = Serial.available();
  if (avail <= 0) return;
  const size_t n = static_cast<size_t>(avail) < kMaxSerialPerPass ? static_cast<size_t>(avail) : kMaxSerialPerPass;
  for (size_t i = 0; i < n; ++i) {
    const int c = Serial.read();
    if (c < 0) break;
    const bf::Split r = splitter.push(static_cast<uint8_t>(c));
    if (r == bf::kFrame) {
      handle_run(splitter.frame(), splitter.frame_len());
    } else if (r == bf::kOverflow) {
      sat_inc(stats.serial_bad);
    }
  }
}

void pump_rx() {
  uint8_t out[bf::kMaxWire];
  for (uint8_t i = 0; i < kRxDepth && rx_tail != rx_head; ++i) {
    const uint8_t tail = rx_tail;
    const RxSlot& slot = rx_ring[tail % kRxDepth];
    const int n = bf::encode_recv(slot.mac, slot.data, slot.len, out, sizeof out);
    rx_tail = static_cast<uint8_t>(tail + 1);
    if (n > 0) {
      sat_inc(stats.rx);
      emit(out, n);
    }
  }
}

void finish_send(bool ok) {
  in_flight = false;
  sat_inc(ok ? stats.tx_ok : stats.tx_fail);
  emit_sent(ok, in_flight_dst);
}

void pump_done() {
  for (uint8_t i = 0; i < kDoneDepth && done_tail != done_head; ++i) {
    const uint8_t tail = done_tail;
    const Done& d = done_ring[tail % kDoneDepth];
    // A late callback for a send already timed out does not match (or is
    // indistinguishable from one to the same dst); unmatched ones are dropped.
    if (in_flight && same_mac(d.mac, in_flight_dst)) finish_send(d.status == 0);
    done_tail = static_cast<uint8_t>(tail + 1);
  }
  if (in_flight && millis() - in_flight_at >= kSendTimeoutMs) finish_send(false);
}

// Starts the next queued send; frames that cannot be sent are reported and
// skipped, at most the queue depth per pass.
void pump_tx() {
  for (size_t i = 0; i < kTxDepth && !in_flight && tx_count > 0; ++i) {
    TxSlot& slot = tx_queue[tx_first];
    tx_first = (tx_first + 1) % kTxDepth;
    --tx_count;
    memcpy(in_flight_dst, slot.dst, bf::kMacLen);
    if (!peer_ensure(slot.dst)) {
      sat_inc(stats.tx_dropped);
      emit_sent(false, slot.dst);
      continue;
    }
    if (esp_now_send(slot.dst, slot.data, slot.len) != 0) {
      sat_inc(stats.tx_fail);
      emit_sent(false, slot.dst);
      continue;
    }
    in_flight = true;
    in_flight_at = millis();
  }
}

}  // namespace

void setup() {
  Serial.setRxBufferSize(1024);
  Serial.begin(kBaud);
  WiFi.mode(WIFI_STA);
  WiFi.disconnect();
  wifi_set_channel(kChannel);
  WiFi.macAddress(own_mac);
  if (esp_now_init() == 0) {
    esp_now_set_self_role(ESP_NOW_ROLE_COMBO);
    esp_now_register_recv_cb(on_recv);
    esp_now_register_send_cb(on_sent);
    esp_now_add_peer(kBroadcast, ESP_NOW_ROLE_COMBO, kChannel, nullptr, 0);
  }
  emit_hello();
}

void loop() {
  pump_serial();
  pump_rx();
  pump_done();
  pump_tx();
}
