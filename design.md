# middleware — design

Transport-agnostic messaging for embedded Linux, microcontrollers and cloud
services. Application code targets one set of traits; the transport is chosen
by cargo feature.

## Architecture

- **Functional core** (`no_std`, no heap, always compiled): keys, transport
  traits, codecs, typed client, BLE framing and advertising encoders, gRPC
  framing and Istio header logic.
- **Imperative shell** (feature gated): Zenoh session, tonic channel/server,
  and the radio traits that a BLE stack binding implements.

Keys are `/`-separated paths. Zenoh uses them as key expressions, gRPC maps a
`Service/Method` key to `/Service/Method`, and BLE hashes the key to a GATT
characteristic UUID and a 16-bit advertising id.

## Requirements

### Core

- [x] C1 With no features the crate is `no_std`, allocation free and builds
  for `thumbv6m-none-eabi` — `cargo build --target thumbv6m-none-eabi`
  and `scripts/check-features.sh`.
- [x] C2 Keys are validated: non-empty, ≤128 bytes, no empty segments, only
  `[A-Za-z0-9_.-]` — `key::tests::rejects_malformed`,
  `key::tests::accepts_paths_and_grpc_names`.
- [x] C3 A two-segment key splits into gRPC service and method —
  `key::tests::service_method_requires_two_segments`.
- [x] C4 Receiving into a too-small buffer is an error, never a truncation —
  `transport::tests::sample_into_refuses_to_truncate`.
- [x] C5 Handler errors have stable one-byte wire codes —
  `transport::tests::handler_error_codes_round_trip`.
- [x] C6 A typed client encodes requests and decodes responses through any
  codec and transport — `typed::tests::typed_request_encodes_and_decodes`,
  `typed::tests::typed_publish_then_recv`, `typed::tests::decode_failure_is_reported`.
- [x] C7 Codecs: raw bytes, postcard (no_std) and prost (protobuf) round trip
  and report undersized buffers — `codec::tests::raw_round_trip_and_errors`,
  `codec::postcard::tests::*`, `codec::prost::tests::*`.
- [x] C8 Subscriptions take a `KeyFilter` (exact key, or a segment-wise
  prefix: `app` selects `app`, `app/x`, `app/x/y`, never `apple`); runtime
  subscription is the `Subscribe` trait (Zenoh, gRPC bus, serial link,
  ESP-NOW; see C12) —
  `key::tests::filters_match_exact_and_segment_prefixes`,
  `tests/zenoh.rs`: `z6_prefix_subscription_delivers_app_x_but_not_apple`;
  `tests/grpc.rs`: `g5_prefix_subscription_delivers_app_x_but_not_apple`;
  `grpc::bus::tests::keyed_stream_prefix_selects_segments_not_apple`,
  `serial::link::tests::filtered_prefix_delivers_app_x_but_not_apple`.
- [x] C9 Samples carry `SampleMeta` (sender id, sequence) when the transport
  knows them; others report none —
  `tests/zenoh.rs`: `z7_no_self_delivery_and_sender_meta_present`,
  `z5_try_recv_is_none_when_empty_and_some_without_waiting`;
  `zenoh::tests::attachment_round_trips_and_foreign_ones_are_ignored`;
  `typed::tests::typed_try_recv_is_none_then_decodes_with_meta`;
  `serial::link::tests::try_recv_is_none_when_empty_and_some_without_waiting`
  (no meta); `g5_try_recv_is_none_when_empty_and_some_without_waiting` and
  `g8_samples_carry_the_publisher_and_self_delivery_is_refused` (sender
  only, see G8).
- [x] C10 Every subscriber has a non-blocking `try_recv` (`None` when
  nothing is queued, the sample otherwise, without awaiting), and every
  async method is cancel safe as stated in `transport`: a `recv` dropped
  before completion loses no sample —
  `z5_try_recv_is_none_when_empty_and_some_without_waiting`,
  `z8_cancelled_recv_loses_no_sample`,
  `g5_try_recv_is_none_when_empty_and_some_without_waiting`,
  `g6_cancelled_recv_loses_no_sample`,
  `serial::link::tests::try_recv_is_none_when_empty_and_some_without_waiting`,
  `serial::link::tests::cancelled_recv_keeps_partial_frame`,
  `serial::link::tests::cancelled_publish_is_completed_by_next_send`,
  `typed::tests::typed_try_recv_is_none_then_decodes_with_meta`.
- [x] C11 Each publisher states its `Limits` (largest key and payload) —
  `serial::link::tests::limits_admit_longest_key_with_largest_payload`
  (128-byte key + 891-byte payload fill one 1024-byte frame),
  `zenoh::tests::filters_map_to_key_expressions` (1 GiB − 1 KiB),
  `g7_limits_fit_tonic_default_message_size`,
  `g7_payload_at_limit_round_trips_through_subscription` (4 MiB − 138).
- [x] C12 Single-stream transports implement `Subscribe` with a subscriber
  that borrows the transport (`Subscriber<'a>`) and owns a copy of its
  filter (`KeyFilterBuf`, no heap), so the caller's key need not outlive
  it and the transport is usable again once the subscriber is dropped:
  the serial link (`serial::link::Filtered`, replacing
  `SerialLink::filtered`) and ESP-NOW (`espnow::EspNowSubscriber`, N13) —
  `serial::link::tests::c12_subscribe_borrows_the_link_and_owns_its_filter`,
  `serial::link::tests::c12_serial_link_is_usable_through_generic_subscribe`,
  `serial::link::tests::filtered_prefix_delivers_app_x_but_not_apple`,
  `espnow::tests::n13_transport_is_usable_through_generic_subscribe`.

### Feature isolation

- [x] F1 `ble` and `postcard` build for `thumbv6m-none-eabi` without `std` —
  `scripts/check-features.sh` (builds `ble`, `ble,postcard`, `postcard`, `serial`, `espnow`, `espnow,postcard` for `thumbv6m-none-eabi`, asserts `std` off).
- [x] F2 `zenoh`, `grpc` and `istio` build on the host without pulling in the
  `ble` module —
  `scripts/check-features.sh` (builds `zenoh`, `grpc`, `grpc,istio`, `zenoh,grpc,istio,prost`, asserts `ble` off).
- [x] F3 Enabling a cloud feature never enables an embedded feature and vice
  versa (checked by `scripts/check-features.sh` via `cargo tree`) —
  `scripts/check-features.sh` (`cargo tree` shows no zenoh/tonic/tokio/hyper/h2 in embedded sets, no `ble` in cloud sets).

### BLE GATT (`ble`)

- [x] B1 Keys map to stable, distinct 128-bit characteristic UUIDs and 16-bit
  advertising ids in a fixed namespace, hashed with the shared
  `dedup::fnv1a64` — `ble::uuid::tests::*`.
- [x] B2 Messages larger than one ATT payload are fragmented and reassembled;
  out-of-order, duplicate or oversized fragments are rejected —
  `ble::frame::tests::*` (`large_message_is_fragmented_and_reassembled`, `out_of_order_fragment_is_rejected`, `duplicate_fragment_is_rejected`, `oversized_message_is_rejected`, `interleaved_and_inconsistent_fragments_are_rejected`, `malformed_fragments_are_rejected`), `ble::gatt::tests::responder_rejects_duplicate_fragment_then_recovers`, `responder_rejects_oversized_request_and_tells_requester`.
- [x] B3 The GATT server publishes as a notification on the key's
  characteristic; the GATT client's subscriber yields notifications received
  on it, at ATT MTU 23 and 247 —
  `ble::gatt::tests::publish_notifies_fragments_on_key_characteristic`, `subscribe_yields_notification_for_subscribed_key`, `subscribe_reports_small_buffer_and_bad_fragments`, `subscriber_drops_unrelated_events_up_to_a_bound`, `end_to_end_publish_subscribe_at_mtu_23_and_247`.
- [x] B4 Requests carry a correlation id; the response matching it is
  returned and stale responses are discarded. Handler errors reach the
  caller; the handler alone decides which keys exist (`NotFound`), with no
  second service list —
  `ble::gatt::tests::request_carries_correlation_id_and_discards_stale_responses`, `successive_requests_use_new_correlation_ids`, `handler_error_reaches_requester`, `responder_maps_failures_to_status_codes`, `end_to_end_request_at_mtu_23_and_247`.
- [x] B5 The radio is reached only through traits, so a host test drives the
  transport with an in-memory fake —
  all `ble::gatt::tests::*` drive `BleServer` / `BleClient` through the in-memory `ServerEnd` / `ClientEnd`; `att_mtu_is_range_checked`.
- [x] B6 Roles are separate traits: `GattServer` (notify, receive writes)
  and `GattClient` (write, receive notifications, non-blocking
  `try_receive`); `BleServer` implements `Publisher` + `Responder`,
  `BleClient` implements `Subscriber` + `Requester`, so no binding has to
  fail a call its role cannot make —
  `ble::gatt::tests::end_to_end_request_at_mtu_23_and_247`, `end_to_end_publish_subscribe_at_mtu_23_and_247` (fakes implement one role each).
- [x] B7 The ATT MTU is read per message from the connection, and fragments
  are capped by the const-generic buffer size `A` —
  `ble::gatt::tests::mtu_is_read_per_message_and_capped_by_buffer`.
- [x] B8 `serve_one` is cancel-safe: the partial request lives in the
  server, so a dropped call resumes; a disconnect, or fragment 0 of a new
  request id, discards a partial request; stray tails of abandoned or
  rejected requests are dropped instead of poisoning the next request —
  `ble::gatt::tests::serve_one_is_cancel_safe`, `disconnect_discards_partial_request`, `new_request_id_replaces_partial_request`, `responder_rejects_duplicate_fragment_then_recovers`.
- [x] B9 The client's `recv` is cancel-safe and loses no publication that
  completes during a request; `try_recv` returns without waiting; a
  disconnect discards a partial publication and fails a waiting request
  with `Disconnected`; a dropped request's late response is discarded —
  `ble::gatt::tests::recv_is_cancel_safe`, `publication_during_request_is_kept_for_recv`, `try_recv_returns_publications_without_waiting`, `disconnect_discards_partial_publication`, `requester_reports_small_buffer_link_errors_and_disconnect`, `dropped_request_late_response_is_discarded`.
- [x] B10 `Publisher::LIMITS` for the GATT server is the reassembly size `N`,
  bounded by the 16-bit length and 256 fragments at the minimum MTU —
  `ble::gatt::tests::publisher_limits_follow_reassembly_size`.

### BLE advertising (`ble`)

- [x] A1 A builder produces legacy advertising data (≤31 bytes): flags,
  complete or shortened local name, middleware service UUID, and manufacturer
  data. Data that does not fit is an error —
  `ble::advert::tests::builder_emits_all_fields`, `builder_uses_configured_company_id`, `long_name_is_shortened_or_rejected`, `oversized_fields_are_an_error`.
- [x] A2 Advertising data is parsed into AD structures; malformed lengths are
  rejected without panicking —
  `ble::advert::tests::parser_rejects_malformed_lengths`, `parser_stops_at_zero_padding`.
- [x] A3 Connectionless publish: a payload on a key is broadcast as
  manufacturer data `[short id][sequence][payload]`; `LIMITS` states the 21
  bytes the default template leaves and `max_payload` the room left by any
  template —
  `ble::advert::tests::publish_broadcasts_short_id_sequence_and_payload`, `publish_rejects_payload_that_does_not_fit`, `publisher_limits_match_the_template`.
- [x] A4 Scanning subscriber yields samples whose short id resolves to a
  subscribed key and drops repeated sequence numbers per (sender address,
  key), so two senders on one key never suppress each other; samples carry
  the sender and sequence in `SampleMeta` —
  `ble::advert::tests::scanning_subscriber_matches_keys_and_drops_repeats`, `dedup_is_per_sender`, `scanning_subscriber_errors`, `end_to_end_publish_to_scanning_subscriber`.
- [x] A5 Discovery: a scan report advertising the middleware service UUID is
  recognised as a middleware node, with its name —
  `ble::advert::tests::discovery_recognises_middleware_nodes`.
- [x] A6 `ShortIdTable` learns key texts at run time, resolves a short id to
  its key, and rejects a key whose short id is taken by another key
  (`Collision`) —
  `ble::advert::tests::short_id_table_learns_resolves_and_detects_collisions`.
- [x] A7 The advertising subscriber takes exact and prefix `KeyFilter`s,
  added or removed at run time and bounded by `K`; a prefix matches only
  learned keys, because adverts carry no key text —
  `ble::advert::tests::prefix_subscription_end_to_end`, `subscribe_key_is_atomic_and_bounded`.
- [x] A8 `Scanner::try_next_report` returns a queued report without waiting,
  and the subscriber's `try_recv` drains such reports, bounded per call —
  `ble::advert::tests::try_next_report_is_non_blocking`, `try_recv_drains_without_waiting`.
- [x] A9 An `Advertiser` may share the advertising set: a successful publish
  means accepted or queued, not on air, and a refusal does not advance the
  sequence; the publisher exposes the binding through `advertiser()` —
  `ble::advert::tests::queueing_advertiser_accepts_or_refuses`, `publisher_limits_match_the_template`.
- [x] A10 `AdvertisingSubscriber` bounds its learned keys (`K`) and its
  filters (`F`, default `K`) separately, and stores filters as owned
  `KeyFilterBuf`s, so a `KeyFilter` over a short-lived key or an owned
  `KeyFilterBuf` can be added and read back with `filters()` —
  `ble::advert::tests::key_and_filter_bounds_are_separate`, `filters_are_owned`.
- [x] A11 `ScanQueue` is a bounded, no-heap queue of scan reports: a radio
  callback pushes in place with `push_report(&mut self)` (newest wins when
  full, oversized data refused, both counted), a waiting `next_report` is
  woken by the push, and `try_next_report` pops without waiting —
  `ble::advert::tests::scan_queue_wakes_waiting_consumer`, `scan_queue_is_bounded_and_counts_drops`, `scan_queue_single_owner_pushes_in_place`.
- [x] A12 Receiving over a `ScanQueue` is cancel-safe: a dropped `recv` or
  `next_report` loses no report; producer and consumer can be split by the
  caller's own lock through `poll_report` —
  `ble::advert::tests::scan_queue_dropped_await_loses_no_report`, `scan_queue_splits_by_caller_lock`.
- [x] A13 `AdvertisingSubscriber` implements `Subscribe`: each subscription
  is a view borrowing the subscriber that delivers only its own filter's
  keys, without adding the filter to the subscriber —
  `ble::advert::tests::subscribe_hands_out_filtered_view`.
- [x] A14 The advertising publisher, subscriber, `ShortIdTable` and
  `ScanQueue` have `const fn` constructors and builders usable in `const`
  and `static` initialisers, and `Default` impls —
  `ble::advert::tests::defaults_and_const_builders`.

### ESP-NOW (`espnow`)

Broadcast transport ported from the conagotchi badge (`badge-mw`). One
ESP-NOW frame per publication: `MW` | version 1 | short id u16 LE | sequence
u8 | key length u8 | key | payload, at most 250 bytes.

- [x] N1 Frames have the v1 layout, round trip, and the largest key
  (64 bytes) with the largest payload (179 bytes) fills exactly one 250-byte
  ESP-NOW frame; the short id equals the BLE short id —
  `espnow::tests::n1_frame_round_trip_and_layout`, `n1_largest_frame_fits_espnow_v1`, `n1_short_id_is_fnv1a64_folded`.
- [x] N2 Oversized keys or payloads are refused before anything is sent and
  without consuming a sequence number; the transport advertises its limits
  as `Publisher::LIMITS` —
  `espnow::tests::n2_oversized_publications_are_refused`, `n2_limits_are_advertised`.
- [x] N3 Malformed `MW` frames are reported as frame errors; frames without
  the magic (other ESP-NOW users) are skipped —
  `espnow::tests::n3_malformed_frames_are_rejected`, `n3_malformed_mw_frame_is_an_error_foreign_frames_are_skipped`.
- [x] N4 Receivers select keys with `KeyFilter`s (exact or segment-wise
  prefix), added and removed at runtime in a fixed number of slots —
  `espnow::tests::n4_exact_and_prefix_filters_select_keys`, `n4_filters_change_at_runtime_and_are_bounded`.
- [x] N5 Repeats are dropped per (sender, key) with `dedup::SeenTable`, so
  two senders on one key never suppress each other —
  `espnow::tests::n5_duplicates_are_dropped_per_sender_and_key`.
- [x] N6 Samples carry `SampleMeta` with the sender (`SenderId::from_mac`)
  and the frame's sequence number —
  `espnow::tests::n6_samples_carry_sender_and_sequence`.
- [x] N7 Own frames are skipped and every receive call is bounded in frames
  (`MAX_IGNORED_FRAMES`) —
  `espnow::tests::n7_own_frames_are_skipped`, `n7_ignored_limit_bounds_recv_and_try_recv`.
- [x] N8 A sample that does not fit the caller's buffer is an error, never a
  truncation, and is kept for the next receive call —
  `espnow::tests::n8_buffer_too_small_keeps_the_sample`.
- [x] N9 `try_recv` returns a queued sample without waiting and `None` when
  the radio has none —
  `espnow::tests::n9_try_recv_returns_queued_samples_without_waiting`.
- [x] N10 Cancel safety: a dropped `recv` loses no sample; a dropped
  `publish` never makes the next message reuse its sequence number —
  `espnow::tests::n10_dropped_recv_loses_no_sample`, `n10_dropped_publish_never_reuses_a_sequence`.
- [x] N11 Publish to subscribe end to end over the in-memory radio —
  `espnow::tests::n11_end_to_end_publish_to_subscribe`.
- [x] N12 `espnow` and `espnow,postcard` build for `thumbv6m-none-eabi`
  without `std` or cloud dependencies — `scripts/check-features.sh`.
- [x] N13 `EspNowTransport` implements `Subscribe`: the `EspNowSubscriber`
  borrows the transport, owns its filter (a key over 64 bytes is refused
  with `KeyTooLong`) and selects with it instead of the filter slots, with
  own-address skipping and dedup unchanged; a kept sample stays kept until
  a receive whose filters select it —
  `espnow::tests::n13_subscribe_borrows_the_transport_with_its_own_filter`,
  `n13_transport_is_usable_through_generic_subscribe`,
  `n13_kept_sample_waits_for_a_receive_that_selects_it`.
- [x] N14 The kept-frame buffer is a const generic `P` (default
  `MAX_FRAME`, behaviour unchanged); `P = 0` shrinks the kept slot from
  258 to 8 bytes and a sample that does not fit the caller's buffer is
  then dropped rather than kept; `P > MAX_FRAME` does not compile —
  `espnow::tests::n14_keeping_is_optional_and_saves_a_frame_of_ram`,
  `n8_buffer_too_small_keeps_the_sample` (default `P`).

### Zenoh (`zenoh`)

- [x] Z1 Publish/subscribe round trip through a Zenoh session —
  `tests/zenoh.rs`: `z1_publish_subscribe_round_trip`,
  `z1_subscriber_buffer_too_small_is_error`.
- [x] Z2 Request is a Zenoh `get`; responder is a queryable running the pure
  handler — `z2_request_runs_handler_in_responder`,
  `z2_response_buffer_too_small_is_error`,
  `z2_request_without_responder_is_no_reply`,
  `zenoh::tests::run_handler_splits_scratch_and_reports_errors`.
- [x] Z3 Handler errors are returned as Zenoh reply errors and surface to the
  requester — `z3_handler_error_surfaces_at_requester`,
  `z3_oversized_request_is_buffer_too_small_reply`,
  `zenoh::tests::reply_error_payload_decodes_code_byte`.
- [x] Z4 Tests open sessions with multicast scouting disabled and connect only
  to explicit loopback endpoints —
  `z4_two_sessions_over_explicit_loopback_endpoint`,
  `zenoh::tests::config_rejects_unescapable_endpoints`.
- [x] Z5 `try_recv` dequeues from the subscriber's FIFO without waiting —
  `z5_try_recv_is_none_when_empty_and_some_without_waiting`.
- [x] Z6 `Subscribe` maps `Exact(k)` to the key expression `k` and
  `Prefix(k)` to `k/**`, which Zenoh matches against `k` itself too —
  `z6_prefix_subscription_delivers_app_x_but_not_apple`,
  `zenoh::tests::filters_map_to_key_expressions`.
- [x] Z7 Publications carry a 12-byte attachment (`mw`, version, sender id
  hashed from the session's Zenoh id, sequence) surfaced as `SampleMeta`;
  subscribers use `allowed_origin(Remote)`, so a session never receives its
  own publications (`with_local_delivery(true)` opts back in) —
  `z7_no_self_delivery_and_sender_meta_present`,
  `zenoh::tests::attachment_round_trips_and_foreign_ones_are_ignored`.
- [x] Z8 Cancelling `recv` loses no sample —
  `z8_cancelled_recv_loses_no_sample`.

### gRPC (`grpc`)

- [x] G1 Unary request to key `Service/Method` is a gRPC call on
  `/Service/Method` carrying raw bytes (any protobuf codec on top) —
  `tests/grpc.rs`: `g1_unary_request_calls_service_method_with_raw_bytes`, `g1_non_service_method_key_is_rejected_before_sending`, `g1_reply_larger_than_buffer_is_an_error`, `g1_prost_codec_on_top_of_raw_bytes`; `grpc::client::tests::method_path_needs_two_segments`.
- [x] G2 A generic bus service provides publish (unary) and subscribe
  (server streaming) over gRPC —
  `g2_publish_reaches_matching_subscriber_only`, `g2_publish_without_subscribers_succeeds_and_small_buffer_fails`, `g2_bus_service_mounts_on_stock_tonic_router`, `grpc::bus::tests::publish_requires_valid_key_header`, `grpc::bus::tests::keyed_stream_filters_by_key`.
- [x] G3 Handler errors map to gRPC status codes and back —
  `g3_handler_errors_arrive_as_grpc_status_and_map_back`, `g3_request_larger_than_server_scratch_is_resource_exhausted`, `grpc::status::tests::handler_errors_round_trip_through_codes`, `grpc::status::tests::codes_map_back_to_handler_errors`.
- [x] G4 Tests run client and server in process on a loopback port —
  every `tests/grpc.rs` test binds `127.0.0.1:0`; `g4_responder_reports_closed_when_server_side_dropped`.
- [x] G5 `Subscribe` opens the bus stream for an exact or prefix filter
  (`mw-filter` metadata); the server filters with `KeyFilter::matches` and
  each stream item carries its key (`[key_len][key][payload]`, plus the
  sender for named clients, G8); `try_recv`
  polls the stream once without waiting —
  `g5_try_recv_is_none_when_empty_and_some_without_waiting`,
  `g5_prefix_subscription_delivers_app_x_but_not_apple`,
  `grpc::bus::tests::keyed_stream_prefix_selects_segments_not_apple`,
  `grpc::bus::tests::subscribe_rejects_unknown_filter_kind_and_bad_key`,
  `grpc::bus::tests::items_round_trip_and_reject_garbage`.
- [x] G6 Cancelling `recv` loses no sample — `g6_cancelled_recv_loses_no_sample`.
- [x] G7 `LIMITS` keep a publish body and its stream item within tonic's
  default 4 MiB message size — `g7_limits_fit_tonic_default_message_size`,
  `g7_payload_at_limit_round_trips_through_subscription`.
- [x] G8 Every `GrpcTransport` has a client id (`SenderId`, fresh per
  transport, settable with `with_client_id`, shared by clones) sent as
  `mw-client` metadata (16 hex digits) on publish and subscribe; the bus
  stamps it on the sample so delivered samples carry it in
  `SampleMeta.sender`, and, like Zenoh's `allowed_origin(Remote)`, does
  not deliver a client's own publications unless it subscribes with
  `mw-self: deliver` (`with_local_delivery(true)`); a malformed id or
  `mw-self` value is `INVALID_ARGUMENT` —
  `tests/grpc.rs`: `g8_samples_carry_the_publisher_and_self_delivery_is_refused`,
  `g8_local_delivery_and_shared_ids_follow_the_client_id`;
  `grpc::bus::tests::g8_bus_refuses_self_delivery_and_fills_the_sender`,
  `grpc::bus::tests::g8_client_ids_round_trip_as_hex_and_bad_ones_are_refused`,
  `grpc::client::tests::g8_fresh_client_ids_differ_and_ids_can_be_set`.
- [x] G9 Subscribers that send no `mw-client` (0.2 clients) keep the
  `[key_len][key][payload]` item format and receive every matching sample;
  named subscribers get `[key_len][key][sender_len][sender][payload]`, so
  `MAX_PAYLOAD` is 4 MiB − 138 —
  `grpc::bus::tests::g8_bus_refuses_self_delivery_and_fills_the_sender`
  (legacy stream), `grpc::bus::tests::g8_items_with_sender_round_trip_and_reject_garbage`,
  `g7_limits_fit_tonic_default_message_size`.

### Istio (`istio`)

- [x] I1 Builds the in-mesh authority `svc.ns.svc.cluster.local:port` —
  `grpc::istio::tests::mesh_authority_formats_cluster_dns`.
- [x] I2 Propagates `x-request-id`, W3C `traceparent`/`tracestate` and B3
  headers from an inbound request to outbound calls —
  `grpc::istio::tests::trace_context_extracts_and_injects_only_propagated_headers`, `grpc::istio::tests::interceptor_adds_trace_and_timeout`, `tests/grpc.rs`: `istio::i2_i5_interceptor_propagates_trace_and_channel_sets_mesh_authority`.
- [x] I3 Formats `grpc-timeout` deadlines per the gRPC spec —
  `grpc::istio::tests::grpc_timeout_picks_finest_unit_that_fits`.
- [x] I4 Classifies gRPC status codes as retriable using Istio's default
  `retryOn` set —
  `grpc::istio::tests::retriable_codes_match_istio_default_retry_on`.
- [x] I5 Plaintext h2c to the sidecar; mTLS is left to the mesh —
  `grpc::istio::tests::channel_builder_targets_sidecar_with_mesh_origin`, `istio::i2_i5_interceptor_propagates_trace_and_channel_sets_mesh_authority`.
## Device platform: ESP base firmware, WASM payloads, web tooling

A base firmware is flashed once onto an ESP32, ESP32-S3, ESP32-C3 or ESP8266
from the browser. Afterwards, application logic ships as WASM payloads loaded over the
same serial port, through a USB-serial bridge chip (CP2102, CH340, FTDI) or
the chip's native USB-Serial-JTAG. Payloads talk to the world through the
middleware ABI in `wasm_abi`.

Heap policy: the no-heap rule applies to the `middleware` crate and to RP2040
firmware. On ESP the base firmware may use a global allocator (`esp-alloc`),
because WASM linear memory and the wasmi interpreter require one.

### Serial protocol and loader (`serial`, `loader`, `wasm_abi`)

- [x] S1 COBS framing round trips arbitrary bytes, including long zero-free
  runs, and rejects malformed encodings — `serial::cobs::tests::*`.
- [x] S2 Frames (request, response, publish) carry a CRC-16 and round trip;
  corrupt, truncated, oversized and unknown frames are rejected —
  `serial::frame::tests::every_kind_round_trips_through_the_wire`,
  `corrupt_frames_are_rejected`, `oversized_frames_are_rejected`.
- [x] S3 Non-frame bytes on the line (ROM boot banners) are skipped —
  `serial::frame::tests::splitter_finds_frames_amid_noise`,
  `serial::link::tests::request_skips_noise_and_stale_responses`.
- [x] S4 `SerialLink` implements request/response with correlation ids,
  publish/subscribe and a responder over any `embedded-io-async` stream —
  `serial::link::tests::*`.
- [x] S5 The loader accepts an image only when chunks arrive in order, fit
  the declared length and the CRC-32 matches; any violation abandons the
  transfer — `loader::tests::complete_transfer_activates`,
  `out_of_order_oversized_and_corrupt_transfers_fail`.
- [x] S6 System commands and `sys/info` encode and parse symmetrically;
  application keys pass through, unknown `sys/` keys are `NotFound` —
  `loader::tests::commands_round_trip_through_parse`,
  `parse_rejects_bad_bodies_and_passes_app_keys`, `device_info_round_trips`.
- [x] S7 The payload ABI (imports, exports, error codes, bounds checks) is
  defined once in `wasm_abi` — `wasm_abi::tests::*`.

### WASM runtime (`wasm-host`)

- [x] W1 Loads a module, rejecting imports not in `wasm_abi::IMPORTS` or with
  wrong signatures, and a missing `memory` export —
  `wasm-host/tests/runtime.rs`: `w1_accepts_module_using_every_import`, `w1_rejects_unknown_import`, `w1_rejects_wrong_import_signature`, `w1_rejects_missing_memory`, `w1_rejects_bad_export_signature_and_garbage`, `w1_extra_exports_and_start_function_are_accepted`; `abi::tests::imports_must_be_known_functions_with_exact_signatures`.
- [x] W2 Runs `init`, `tick` on the requested period, and `on_message` for
  subscribed keys via `mw_alloc` —
  `w2_init_runs_once_on_load`, `w2_tick_runs_on_requested_period`, `w2_set_tick_zero_disables_and_tick_without_schedule_is_noop`, `w2_deliver_runs_on_message_for_exact_subscribed_keys`, `w2_alloc_refusal_means_not_delivered`, `abi::tests::tick_schedule_wraps_and_skips_missed_ticks`.
- [x] W3 Host imports validate pointers, pins, keys (no `sys/`), and the
  subscription limit, returning `AbiError` codes rather than trapping —
  `w3_imports_return_error_codes_instead_of_trapping`, `w3_subscription_limit_returns_no_resources`, `w3_log_truncates_at_max_log`, `abi::tests::keys_reject_sys_and_malformed`, `gpio_arguments_are_range_checked`, `link::tests::publish_checks_bounds_size_and_key`.
- [x] W4 Each call is fuel metered; an infinite loop traps and is reported,
  without hanging the host —
  `w4_infinite_loop_runs_out_of_fuel_and_poisons_payload`, `w4_infinite_loop_in_init_fails_load`, `w4_fuel_is_refilled_before_every_call`, `w4_other_traps_are_reported`.
- [x] W5 Memory is capped by a configurable byte limit (`Limits::bytes`,
  `Limits::pages`), at load (initial memory) and on `memory.grow`, for
  modules with 64 KiB pages and with 1-byte pages (custom-page-sizes) —
  `w5_limits_constructors_are_byte_based`, `w5_memory_grow_beyond_cap_fails`, `w5_initial_memory_over_cap_is_rejected`, `w5_one_byte_page_module_within_cap_loads_and_grows_to_it`, `w5_one_byte_page_module_over_cap_is_rejected`, `abi::tests::memory_cap_is_checked_in_bytes`, `abi::tests::initial_memory_bytes_reads_both_page_sizes`, `abi::tests::initial_memory_bytes_rejects_malformed_and_saturates`.
- [ ] W6 `no_std` + `alloc`; builds for the ESP targets and `wasm32`.
  Partial: builds for `wasm32-unknown-unknown`, `xtensa-esp32-none-elf`,
  `xtensa-esp32s3-none-elf`; fails on `riscv32imc-unknown-none-elf`
  (ESP32-C3 has no CAS atomics, wasmi needs `Arc`). Decision: wasmi is the
  only engine; the C3 firmware builds without the runtime (it installs and
  stores payloads but cannot run them).
- [x] W7 The desktop simulator runs a payload against a fake device —
  `tools/payload-sim/tests/sim.rs`: `w7_runs_payload_against_fake_device_in_virtual_time`, `w7_real_time_ticks`, `w7_reports_traps_and_load_errors`.
- [x] W8 When `init` traps, `Payload::load` returns `LoadError::Init { trap,
  log }` with the log lines `init` emitted before the trap, bounded to the
  last `INIT_LOG_LINES` (8) lines of at most `MAX_LOG` bytes, with a count
  of dropped earlier lines; the lines still reach `Device::log`, and the
  error's `Display` includes them. `payload-sim` prints them before the
  error —
  `wasm-host/tests/runtime.rs`: `w8_init_trap_returns_the_lines_logged_before_it`, `w8_init_log_is_bounded_to_the_last_lines`, `w8_init_lines_still_reach_the_device`; `error::tests::init_log_keeps_the_most_recent_lines`, `error::tests::init_error_display_includes_the_log`; `link::tests::log_is_recorded_while_init_runs`, `link::tests::log_rejects_bad_pointer`; `tools/payload-sim/tests/sim.rs`: `w8_prints_init_log_before_the_init_trap`.
- [x] W9 `Payload::tick` returns `Result<Ticked, Trap>` reporting whether
  the `tick` export ran: `Ran`, `NotDue`, `Disabled` (no period set) or
  `NoTickExport`; the decision is the pure `abi::tick_decision` —
  `abi::tests::w9_tick_decision_reports_why_nothing_ran`; `wasm-host/tests/runtime.rs`: `w9_tick_reports_whether_it_ran`, `w2_tick_runs_on_requested_period`, `w2_set_tick_zero_disables_and_tick_without_schedule_is_noop`.

### Payload SDK (`payloads/`)

- [x] P1 A `no_std` Rust SDK wraps the ABI safely (log, publish, subscribe,
  timers, GPIO) and provides the exports — `cargo test -p mw-payload
  --target x86_64-unknown-linux-gnu` in `payloads/` (15 tests: error codes,
  slots, oversized publish, lazy init, delivery buffer, re-entrancy, LogLine,
  GPIO). `unsafe` is confined to `payloads/sdk/src/sys.rs` (FFI imports) and
  the `payload!` export attributes; documented in the crate rustdoc.
- [x] P2 Example payloads (blink, echo) build for `wasm32-unknown-unknown`
  and pass ABI validation in the simulator —
  `cargo build --release` in `payloads/`; `abi-check` tests `rust_*`; blink and echo run in `payload-sim` (blink toggles GPIO 8 every 500 ms, echo republishes `app/echo/in` on `app/echo/out`).
- [x] P4 Payloads can be built with 1-byte pages and a byte-sized linear
  memory (default 16 KiB) for hosts that cannot spare a 64 KiB page; blink
  and echo are built and validated both ways —
  `payloads/scripts/build-byte-pages.sh` (run by `check-all.sh`); `abi-check` tests `rust_examples_with_one_byte_pages`, `default_build_uses_one_64k_page`, `wat_one_byte_page_module_validates`; `payload-sim --memory-bytes 16384` runs the byte-page blink.
- [x] P3 WAT and AssemblyScript templates exercise the same ABI and run in
  the simulator —
  `abi-check` tests `wat_*`; `payloads/scripts/check-all.sh` compiles the AssemblyScript templates and validates them; both AS payloads run in `payload-sim`.
- [x] P5 `mw_payload::payload!` works in a payload crate with
  `#![forbid(unsafe_code)]`: the expansion carries no `#[allow(unsafe_code)]`
  and its `#[unsafe(no_mangle)]` attributes are SDK tokens, which the lint
  does not report in the user crate. Every Rust example is
  `#![forbid(unsafe_code)]` and still exports `init`, `tick`, `mw_alloc`,
  `on_message` and `on_irq` —
  `abi-check` test `p5_forbid_unsafe_code_examples_export_the_abi` (run by `check-all.sh` after the release build).
- [x] P6 The byte-page payload build is reproducible: `build-byte-pages.sh`
  remaps the home directory, the cargo registry and the repository with
  `--remap-path-prefix`, and builds from two different directories are
  byte-identical —
  `payloads/scripts/check-reproducible.sh` (blink and echo, run by `check-all.sh`).

### ESP base firmware (`firmware/`)

- [x] E1 Builds for `esp32`, `esp32s3` and `esp32c3` from one source tree
  via cargo features —
  `firmware/build.sh esp32`, `esp32s3`, `esp32c3` release builds; `compile_error!` in `firmware/src/main.rs` unless exactly one chip feature is set.
- [ ] E2 Serves the protocol on UART0 (bridge chips) on all chips and on
  USB-Serial-JTAG on S3 and C3. Implemented (`firmware/src/shell/board.rs`,
  `link.rs`) and builds. Hardware evidence so far: UART0 behind a CH340 on an
  ESP8266EX (`firmware-esp8266`, 2026-10-25): `mwctl info`, `upload`,
  `reset` + `monitor` and `start` all ran against `/dev/ttyUSB0`, including
  recovery from the ROM's 74880-baud boot output. ESP32 family and
  USB-Serial-JTAG still need a hardware test.
- [x] E3 Answers `sys/info`; installs payloads with the loader into a flash
  region, verifying CRC-32; the payload survives reboot —
  `firmware` core tests `dispatch::tests::info_reports_device`, `full_transfer_installs_then_start_and_stop`, `abandoned_transfer_leaves_no_payload`, `oversized_begin_from_idle_keeps_installed_payload`, `resent_last_chunk_is_acknowledged_without_rewriting`; `store::tests::installed_image_survives_reopen_and_verifies`, `interrupted_transfer_leaves_no_image`, `corruption_is_detected`, `limits_and_ordering_are_enforced` (RAM flash model; silicon untested).
- [x] E4 Starts the stored payload on boot and on `sys/payload/start`; stops
  on `sys/payload/stop`; publishes status and log lines —
  `dispatch::tests::start_requires_an_installed_image`, `begin_while_running_stops_and_erases`, `storage_failure_and_traps_update_status`, `start_without_runtime_is_refused_but_loader_works`, `tick_delay_handles_wrap`.
- [x] E5 Bridges payload publish/subscribe to the serial link and GPIO to
  the chip, refusing pins used by flash, USB or the console UART —
  `chip::tests::console_uart_pins_are_reserved`, `flash_psram_and_usb_pins_are_refused`, `missing_pins_are_invalid`, `esp32_input_only_pins_refuse_output`, `general_purpose_pins_are_allowed`; `dispatch::tests::publications_route_only_to_running_subscribed_payloads`. Runs payloads on ESP32 and ESP32-S3 only; the C3 has no runtime.
- [x] E6 Produces merged flash images per chip for the web flasher —
  `firmware/build-images.sh` writes `web/public/firmware/{esp32,esp32s3,esp32c3}.bin` and `manifest.json`.

### ESP8266 base firmware (`firmware-esp8266/`)

Protocol only: about 80 KB of DRAM cannot hold wasmi plus a 64 KiB page, so
like the C3 build it stores payloads but cannot run them. It is a blocking
super-loop on esp8266-hal (no executor on the LX106). esp8266-hal's runtime
crate `xtensa-lx-rt` 0.12 is vendored and patched for current rustc
(`#[unsafe(naked)]`, `naked_asm!`, one `global_asm!` block for the exception
handlers); the unsafe asm stays in that vendored dependency.

- [x] E7 Builds for `xtensa-esp8266-none-elf` from `firmware-esp8266/`,
  reusing the `esp_base` core unchanged apart from the chip profile —
  `firmware-esp8266/build.sh` release build (`#![deny(unsafe_code)]` in the
  binary, `#![forbid(unsafe_code)]` in its library); `firmware-esp8266/test.sh`
  clippy `-D warnings` for host and chip; `esp_base` changes limited to
  `chip::ESP8266` and `Profile::runtime`, covered by
  `chip::tests::profiles_match_chips`, `heapless_profiles_have_no_linear_memory`,
  `missing_pins_are_invalid`, `general_purpose_pins_are_allowed`.
- [x] E8 Answers `sys/info`, installs payloads into flash at 0x100000 (above
  the 1 MB code window) with CRC-32 verification, keeps them across reboot,
  and refuses start —
  host: `controller::tests::fresh_boot_reports_empty_esp8266_without_runtime`, `upload_installs_and_survives_reboot`, `start_is_refused_and_stop_is_a_no_op`, `wrong_crc_commit_leaves_no_image`, `corrupted_stored_image_boots_empty`, `largest_image_fits_the_region`, `info_request_over_the_link`.
  Hardware (ESP8266EX, 4 MB flash, 2026-10-25) with `mwctl`: `info` reports
  esp8266, `0.1.0+no-wasm`, ABI 2, capacity 258048; `upload echo.wasm`
  installs (status Stopped); after `mwctl reset` the boot log reports the
  stored image and `info` still says Stopped; `start` answers NotFound;
  `upload --corrupt-chunk 3` fails at commit with InvalidRequest and leaves
  status Empty. A 252 KiB image also installed and survived reboot. Note:
  the payload store has one slot and `begin` erases it, so a rejected
  transfer leaves no payload rather than the previous one.
- [x] E9 The flash driver satisfies `NorFlash` semantics (4 KiB erase one
  sector per HAL call, 4-byte writes split at 256-byte pages, region-relative
  bounds) — `flash::tests::erased_reads_ff_and_writes_and_like_nor`, `addresses_are_region_relative`, `erase_issues_one_call_per_sector`, `writes_split_at_page_boundaries`, `aligned_page_sized_write_is_one_program`, `misaligned_requests_are_refused_without_device_access`, `out_of_region_requests_are_refused`, `whole_region_erases`, `device_failures_surface_as_other` (RAM model, `firmware-esp8266/src/flash/ram.rs`).
- [x] E10 `firmware/build-images.sh` emits `esp8266.bin` (merged, offset 0x0)
  and its manifest entry with `"wasm": false` —
  full four-chip `firmware/build-images.sh` run (2026-10-25) wrote
  `web/public/firmware/esp8266.bin` (78508 bytes, via
  `firmware-esp8266/image.sh`) and the `esp8266` manifest entry; the same
  merged image, written by `firmware-esp8266/flash.sh`, boots on the board.

### Host tool (`tools/mwctl`)

`mwctl --port <PATH> info|upload|start|stop|monitor|reset` drives a device
over one named serial port, synchronously, with the same pure frame and
loader code as the firmware. It never enumerates ports. It opens the port
without resetting the board (verified on the CH340 board: no boot output on
open).

- [x] T1 Protocol logic is tested against an in-memory device running the
  real splitter and loader —
  `device::tests::info_decodes_a_protocol_only_esp8266`, `upload_happy_path_installs_reports_progress_and_starts`, `dropped_chunk_reply_is_retried_and_the_device_deduplicates`, `dropped_request_and_late_reply_are_handled`, `exhausted_retries_time_out`, `corrupt_chunk_makes_commit_fail_on_the_device`, `start_without_runtime_is_a_clear_error`, `oversized_image_is_refused_at_begin`, `boot_garbage_before_the_first_reply_needs_no_retry`; `link::tests::garbage_without_delimiter_before_a_frame_is_skipped`, `garbage_with_delimiters_is_noise_and_frames_still_decode`; `plan::tests::*`, `cli::tests::*`.

### Web site (`web/`)

- [ ] U1 Flashes the base firmware from Chrome over Web Serial with
  esptool-js, detecting the chip and picking the matching image.
  Partial: manifest parsing and chip mapping, including `ESP8266` →
  `esp8266`, are tested (`web/test/manifest.test.ts`: "maps esptool-js chip
  names to manifest keys"); the esptool-js flash needs a manual run in
  Chrome against the ESP8266 board.
- [x] U2 Connects to the base firmware, shows device info, log and published
  messages, and publishes to the device —
  `web/test/upload.test.ts`: "reads sys/info after a boot banner and shows the banner as console text", "delivers device publications and sends host publications", "releases both stream locks on close"; `web/test/ports.test.ts` (mocked serial streams; not run against a real port).
- [x] U3 Uploads a `.wasm` payload with the loader protocol, with progress,
  then starts it —
  `web/test/upload.test.ts` upload flow against a fake device running the real Rust loader: in-order chunks + CRC + activate + start, lost request retried, lost response retried, refused step, over capacity, silent device; `loader::tests::resent_last_chunk_is_acknowledged_without_rewriting`.
- [x] U4 The browser uses the Rust protocol implementation compiled to WASM
  (`protocol-wasm`), not a second implementation —
  `protocol-wasm` `decoder::tests::*`, `encode::tests::*`, `loader::tests::*`; `web/test/protocol.test.ts` exercises the wasm-pack output, including ABI constants matching `wasm_abi`.
- [x] U5 In-browser editor for WAT and AssemblyScript that compiles to
  `.wasm` locally —
  `web/test/editor.test.ts` (WAT via wabt, AssemblyScript via asc without `env.abort`, error reporting, ABI pre-check); compiled in headless Chrome on localhost.
- [x] U6 In-browser simulator runs the payload with the same runtime as the
  device (wasm-host compiled to WASM), with virtual GPIO and message log —
  `protocol-wasm` `simulator::tests::simulator_runs_payload_with_virtual_gpio_and_messages`, `simulator_reports_rejection_and_traps`; `web/test/editor.test.ts` runs the blink/echo templates (WAT and AssemblyScript) in wasm-host compiled to WASM.
- [x] U7 Automated tests cover the protocol bindings and the upload flow
  against a mocked serial port —
  `npm test` in `web/` (58 tests) and `cargo test -p protocol-wasm` (17 tests).
- [x] U8 The manifest carries each image's `wasm` flag (absent means `true`)
  and the UI marks protocol-only images; the device page disables "start
  after upload" and Start for `+no-wasm` firmware —
  `web/test/manifest.test.ts`: "parses the wasm runtime flag, defaulting to true when absent"; `web/test/upload.test.ts`: "recognises protocol-only firmware by its +no-wasm version suffix".
### Interrupts and DMA (ABI 2)

Payloads handle GPIO edges, hardware timer alarms, DMA completion and
secondary-UART receive, and start SPI-master and UART DMA transfers.
Payload code never runs in interrupt context: ISRs timestamp events into a
bounded queue and the main loop dispatches them to the `on_irq` export. DMA
uses firmware-owned buffers copied to and from guest memory.

- [x] D1 ABI 2 is a superset of ABI 1 (new imports, optional `on_irq`
  export, `Busy` error) defined once in `wasm_abi` —
  `wasm_abi::tests::import_table_is_unique_and_searchable`,
  `error_codes_round_trip`.
- [x] D2 The event queue is bounded and never blocks: FIFO order, timer and
  UART RX coalescing, drop-and-count on overflow, per-source purge —
  `wasm_abi::events::tests::*`.
- [x] D3 Peripheral arguments (edges, timers, SPI config and lengths, UART
  config and lengths) are validated in one place —
  `wasm_abi::periph::tests::*`.
- [x] D4 `wasm-host` implements every ABI 2 import through `Device`, dispatches
  queued events to `on_irq` with fuel metering, and still loads ABI 1
  payloads — `wasm-host/tests/irq.rs` (`d4_*`, 16 tests, including
  `d4_abi1_module_still_loads_and_runs`,
  `d4_infinite_loop_in_on_irq_runs_out_of_fuel_and_poisons`); `abi::tests::
  spi_transfer_checks_arguments_before_pointer`, `uart_write_and_read_targets`.
- [x] D5 The firmware's GPIO, timer, DMA and UART ISRs only enqueue (bounded
  work, no allocation, no payload calls); overflow is reported via
  `irq_overflow` and `sys/log` —
  `firmware` `periph::tests::d5_gpio_event_respects_edge`, `d5_overflow_is_reported_once_per_rise`, `d5_isr_pushes_are_bounded_and_counted`, `claims::tests::d5_irq_pins_do_not_collide`; ISRs bound at compile time with `#[handler]` (`shell/gpio.rs`, `shell/timer.rs`); esp32/esp32s3 builds. Hardware delivery untested.
- [x] D6 SPI master transfers (bus 0) run on DMA with firmware-owned buffers;
  completion raises `DmaDone`; `dma_read` returns the received bytes —
  `claims::tests::d6_spi_pins_are_checked_against_the_chip`, `d6_reopening_spi_replaces_its_pins`, `dma::tests::d6_transfer_lifecycle_and_consuming_read`, `d6_errors_and_aborts_return_to_idle`, `periph::tests::d6_d7_completion_events`; SPI2 with DMA (`DMA_SPI2` on ESP32, GDMA on S3) in `shell/spi.rs`. Hardware untested.
- [x] D7 Secondary UART 1 transmits with DMA (or the chip's closest
  equivalent, documented per chip) and buffers received bytes, raising
  coalesced `UartRx` events —
  `claims::tests::d7_uart_pins_avoid_console_flash_usb_and_other_owners`, `dma::tests::d7_uart_tx_uses_the_same_machine`, `ring::tests::d7_ring_keeps_order_across_wrap`, `d7_ring_drops_and_counts_overflow`. Per chip: esp-hal 1.2 has no usable UART DMA (no UHCI on ESP32; S3 UHCI cannot share a gapless RX ring), so both use the interrupt-driven async FIFO driver, documented in `firmware/src/shell/uart1.rs`.
- [x] D8 Hardware timers 0–3 deliver one-shot and periodic alarms with
  microsecond timestamps; stopping a timer purges its pending events —
  `timers::tests::d8_one_shot_fires_once`, `d8_periodic_rearms_on_its_grid_and_counts_missed_periods`, `d8_four_timers_fire_in_deadline_order`, `d8_stop_and_restart`; four timers multiplexed on TIMG1 alarm (`shell/timer.rs`). Accuracy untested on hardware.
- [x] D9 Payload SDK exposes interrupts, timers, SPI and UART safely, with
  examples (button IRQ, SPI loopback, UART echo) and WAT/AssemblyScript
  templates — `mw-payload` `irq::tests::*`, `rt::tests::on_irq_*`,
  `rt::tests::reentrant_on_irq_is_dropped`, `dma::tests::*`,
  `spi::tests::*`, `uart::tests::*`, `timer::tests::*`,
  `gpio::tests::irq_helpers`; `abi-check` `tests::rejects_bad_on_irq_signature`,
  `tests::event_sources_require_on_irq` and `tests/payloads.rs`
  (`rust_button_example`, `rust_spi_loopback_example`,
  `rust_uart_echo_example`, `wat_button_template`, `wat_timer_template`,
  `as_button_template`, `as_spi_template`); run end to end in `payload-sim`.
- [x] D10 The desktop and browser simulators provide virtual GPIO edges,
  timers, an SPI loopback device and a UART terminal, so ABI 2 payloads run
  without hardware — desktop: `tools/payload-sim/tests/d10.rs` (`d10_*`);
  browser: `protocol-wasm` `d10_*` (timers, edges, SPI loopback/pattern, UART,
  overflow, bounded dispatch, trap in `on_irq`) and `web/test/abi2.test.ts`
  (including the real `payloads/templates` ABI 2 templates).

### PWM, ADC and I2C (ABI 3)

Payloads drive PWM outputs, sample analog inputs and talk to I2C targets.
The board owns the pin mapping: a PWM `(timer, channel)` and an I2C bus
name fixed pins, and an ADC read names a board pin. I2C transfers are
synchronous and bounded (`periph::MAX_I2C` = 64 bytes each way), so the
payload SDK can offer the blocking `embedded-hal` traits; PWM and ADC calls
complete immediately. None of these raise events.

- [x] X1 ABI 3 is a superset of ABI 2 (six imports appended to
  `wasm_abi::IMPORTS`, `ABI_VERSION` 3), and ABI 1 and ABI 2 payloads still
  load because imports are checked by name and signature only —
  `wasm_abi::tests::revisions_are_prefixes_of_the_import_table`,
  `import_table_is_unique_and_searchable`; `wasm-host/tests/abi3.rs`
  `x1_every_import_links_and_older_revisions_still_load`; `abi-check`
  `tests/payloads.rs` `abi2_examples_import_no_abi3_functions`,
  `abi1_examples_import_no_abi2_functions`.
- [x] X2 The new `Device` methods default to `Unsupported`, so existing
  boards compile unchanged, and device errors reach the payload as codes —
  `x2_device_errors_pass_through_and_defaults_are_unsupported`; the ESP
  firmware builds and passes `cargo test --lib` in `firmware/` without
  changes.
- [x] X3 PWM arguments are validated in one place (timer `< 6`, channel
  `< 4`, frequency 1 Hz to 1 MHz, duty a `u16` where 65535 is always on) —
  `wasm_abi::periph::tests::pwm_rules`;
  `x3_pwm_open_set_stop_and_bad_args`.
- [x] X4 `adc_read` returns a raw 12-bit value (device readings above 4095
  are clamped), rejects pins outside `u8`, and passes `Unsupported` through
  for pins without an analog input — `wasm_abi::periph::tests::adc_rules`;
  `x4_adc_read_returns_raw_values_clamped_and_errors`.
- [x] X5 `i2c_open` and `i2c_transfer` validate bus (`< 2`), clock
  (10 kHz to 1 MHz), 7-bit address and lengths (`<= MAX_I2C`, either may be
  0) before the guest pointers, bound both buffers, copy through host
  buffers, write guest memory only after the device succeeds, and report a
  missing acknowledge as `Transport` —
  `wasm_abi::periph::tests::i2c_rules`;
  `abi::tests::i2c_transfer_checks_arguments_before_pointers`;
  `link::tests::i2c_transfer_leaves_memory_alone_on_error`;
  `x5_i2c_open_and_bad_config`,
  `x5_i2c_write_then_write_read_round_trips_data`,
  `x5_i2c_errors_bounds_and_untouched_memory`.
- [x] X6 The payload SDK exposes PWM channels as `embedded-hal`
  `SetDutyCycle`, analog pins with integer millivolt conversion, and I2C as
  `embedded-hal` `I2c` (write, read, write-read, and transactions of the
  shape "writes, then reads"; other shapes are `Unsupported`), validating
  with the ABI parsers before calling the host, with `unsafe` still confined
  to `sys.rs`; the `dimmer` example builds for `wasm32` and validates —
  `mw-payload` `pwm::tests::*`, `adc::tests::*`, `i2c::tests::*`; `dimmer`
  `tests::*`; `abi-check` `tests::follows_the_import_table_including_abi3`,
  `tests/payloads.rs` `rust_dimmer_example`.
- [x] X7 The desktop simulator provides PWM (records duty), ADC (values
  set with `!adc <pin> <value>`) and an I2C register device at 0x50 on each
  bus — `payload-sim` `sim::periph::tests::pwm_records_duty_per_channel`,
  `adc_values_are_settable`, `i2c_register_device`;
  `sim::tests::abi3_peripherals`, `parses_commands`; `tests/x7.rs`
  `x7_adc_pwm_and_i2c_in_the_simulator`.
- [x] X8 `wasm-host` still builds for `thumbv7em-none-eabihf` (the STM32F407
  target that implements ABI 3 on hardware) —
  `cargo build -p wasm-host --target thumbv7em-none-eabihf`.

## Known limitations (0.3)

- **Publish and receive share one `&mut self`.** A transport that owns both directions of one radio cannot publish while a receive is pending. Callers serialise them or keep per-direction tasks behind channels. Split halves are deferred.
- **One borrowed subscriber at a time on single-stream transports.** Since 0.3 `Subscribe::Subscriber<'a>` may borrow the transport (serial C12, ESP-NOW N13, BLE advertising A13), so only one such view exists at once; independent concurrent subscriptions still need a multiplexing transport (Zenoh, gRPC bus) or the runtime filter slots. The GATT client (`BleClient`) does not implement `Subscribe` yet.
- **`SampleMeta.seq` is 8 bits** (documented on the field): it distinguishes repeats inside a dedup window, not order in a long stream; compare it with wrapping arithmetic.

Lifted in 0.3: owned keys (`KeyBuf<N>`, `KeyFilterBuf<N>`), runtime `Subscribe` on single-stream transports, and publisher identity on the gRPC bus (G8).
